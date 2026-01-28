//! Local network discovery via mDNS
//!
//! This module handles discovering other HumanFileShare devices on the local
//! network using multicast DNS (mDNS).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use flume::RecvTimeoutError;
use iroh::{NodeAddr, PublicKey};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use parking_lot::RwLock;
use thiserror::Error;
use tokio::sync::mpsc;
use tracing::{debug, error, info, instrument, warn};

/// The mDNS service type for HumanFileShare
pub const SERVICE_TYPE: &str = "_humanfileshare._tcp.local.";

/// Errors that can occur during discovery operations
#[derive(Error, Debug)]
pub enum DiscoveryError {
    #[error("failed to initialize mDNS: {0}")]
    InitFailed(String),

    #[error("failed to register service: {0}")]
    RegistrationFailed(String),

    #[error("discovery is not running")]
    NotRunning,
}

/// Information about a discovered peer
#[derive(Debug, Clone)]
pub struct DiscoveredPeer {
    /// The peer's public key (device ID)
    pub device_id: PublicKey,
    /// Network addresses where the peer can be reached
    pub addresses: Vec<SocketAddr>,
    /// Optional friendly device name
    pub device_name: Option<String>,
    /// When the peer was last seen
    pub last_seen: std::time::Instant,
    /// The iroh port for QUIC connections
    pub iroh_port: u16,
}

impl DiscoveredPeer {
    /// Convert to an iroh NodeAddr for connecting
    pub fn to_node_addr(&self) -> NodeAddr {
        NodeAddr::new(self.device_id).with_direct_addresses(self.addresses.clone())
    }
}

/// Manages local network discovery via mDNS
pub struct Discovery {
    /// This device's public key
    device_id: PublicKey,
    /// The port our iroh endpoint is listening on
    iroh_port: u16,
    /// Optional friendly name for this device
    device_name: Option<String>,
    /// Currently known peers
    peers: Arc<RwLock<HashMap<PublicKey, DiscoveredPeer>>>,
    /// Channel for peer discovery events
    peer_tx: mpsc::Sender<DiscoveredPeer>,
    /// Receiver for peer discovery events
    peer_rx: Arc<tokio::sync::Mutex<mpsc::Receiver<DiscoveredPeer>>>,
    /// The mDNS daemon
    mdns: Option<ServiceDaemon>,
    /// Whether we're currently running
    running: Arc<RwLock<bool>>,
}

impl Discovery {
    /// Creates a new discovery service
    #[instrument(skip(device_id), fields(device_id = %device_id))]
    pub fn new(device_id: PublicKey, iroh_port: u16) -> Self {
        let (peer_tx, peer_rx) = mpsc::channel(100);

        info!("Discovery service created");

        Self {
            device_id,
            iroh_port,
            device_name: None,
            peers: Arc::new(RwLock::new(HashMap::new())),
            peer_tx,
            peer_rx: Arc::new(tokio::sync::Mutex::new(peer_rx)),
            mdns: None,
            running: Arc::new(RwLock::new(false)),
        }
    }

    /// Sets a friendly name for this device
    pub fn with_device_name(mut self, name: impl Into<String>) -> Self {
        self.device_name = Some(name.into());
        self
    }

    /// Starts broadcasting presence and listening for peers
    #[instrument(skip(self))]
    pub fn start(&mut self) -> Result<()> {
        if *self.running.read() {
            debug!("Discovery already running");
            return Ok(());
        }

        info!("Starting mDNS discovery");

        // Create mDNS daemon
        let mdns = ServiceDaemon::new().map_err(|e| DiscoveryError::InitFailed(e.to_string()))?;

        // Register our service
        let instance_name = format!("hfs-{}", &self.device_id.to_string()[..8]);
        let host_name = format!("{}.local.", hostname::get()?.to_string_lossy());

        let mut properties = HashMap::new();
        properties.insert("device_id".to_string(), self.device_id.to_string());
        if let Some(ref name) = self.device_name {
            properties.insert("name".to_string(), name.clone());
        }

        let service_info = ServiceInfo::new(
            SERVICE_TYPE,
            &instance_name,
            &host_name,
            "",
            self.iroh_port,
            properties,
        )
        .map_err(|e| DiscoveryError::RegistrationFailed(e.to_string()))?
        .enable_addr_auto();

        mdns.register(service_info)
            .map_err(|e| DiscoveryError::RegistrationFailed(e.to_string()))?;

        info!(instance_name = %instance_name, "Registered mDNS service");

        // Browse for other services
        let receiver = mdns
            .browse(SERVICE_TYPE)
            .map_err(|e| DiscoveryError::InitFailed(e.to_string()))?;

        // Set running flag BEFORE spawning the task
        *self.running.write() = true;

        // Spawn task to handle discovery events
        let peers = self.peers.clone();
        let peer_tx = self.peer_tx.clone();
        let running = self.running.clone();
        let our_device_id = self.device_id;

        tokio::task::spawn_blocking(move || {
            debug!("mDNS browse task started");
            while *running.read() {
                match receiver.recv_timeout(Duration::from_millis(100)) {
                    Ok(event) => {
                        match event {
                            ServiceEvent::ServiceResolved(info) => {
                                // Parse the device_id from properties
                                if let Some(device_id_str) = info.get_property_val_str("device_id")
                                {
                                    // Skip our own service
                                    if device_id_str == our_device_id.to_string() {
                                        continue;
                                    }

                                    match device_id_str.parse::<PublicKey>() {
                                        Ok(device_id) => {
                                            let addresses: Vec<SocketAddr> = info
                                                .get_addresses()
                                                .iter()
                                                .map(|ip| SocketAddr::new(*ip, info.get_port()))
                                                .collect();

                                            let device_name =
                                                info.get_property_val_str("name").map(String::from);

                                            // Merge addresses with existing peer if present
                                            let mut peers_guard = peers.write();
                                            let peer = if let Some(existing) =
                                                peers_guard.get_mut(&device_id)
                                            {
                                                // Merge addresses, avoiding duplicates
                                                for addr in addresses {
                                                    if !existing.addresses.contains(&addr) {
                                                        existing.addresses.push(addr);
                                                    }
                                                }
                                                existing.last_seen = std::time::Instant::now();
                                                if device_name.is_some() {
                                                    existing.device_name = device_name;
                                                }
                                                existing.clone()
                                            } else {
                                                let peer = DiscoveredPeer {
                                                    device_id,
                                                    addresses,
                                                    device_name,
                                                    last_seen: std::time::Instant::now(),
                                                    iroh_port: info.get_port(),
                                                };
                                                peers_guard.insert(device_id, peer.clone());
                                                peer
                                            };
                                            drop(peers_guard);

                                            debug!(
                                                peer_id = %device_id,
                                                address_count = peer.addresses.len(),
                                                "Updated peer addresses via mDNS"
                                            );

                                            let _ = peer_tx.try_send(peer);
                                        }
                                        Err(e) => {
                                            warn!("Invalid device_id in mDNS: {}", e);
                                        }
                                    }
                                }
                            }
                            ServiceEvent::ServiceRemoved(_, fullname) => {
                                debug!(service = %fullname, "Service removed");
                            }
                            ServiceEvent::SearchStarted(service_type) => {
                                debug!(service_type = %service_type, "mDNS search started");
                            }
                            ServiceEvent::ServiceFound(service_type, fullname) => {
                                debug!(service_type = %service_type, fullname = %fullname, "Service found, resolving...");
                            }
                            _ => {
                                debug!("Other mDNS event received");
                            }
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        // Normal timeout, continue
                    }
                    Err(RecvTimeoutError::Disconnected) => {
                        debug!("mDNS receiver disconnected");
                        break;
                    }
                }

                // Clean up stale peers (not seen in last 2 minutes)
                let now = std::time::Instant::now();
                let stale_timeout = Duration::from_secs(120);
                peers
                    .write()
                    .retain(|_, peer| now.duration_since(peer.last_seen) < stale_timeout);
            }
        });

        self.mdns = Some(mdns);

        info!("mDNS discovery started");
        Ok(())
    }

    /// Stops the discovery service
    #[instrument(skip(self))]
    pub fn stop(&mut self) {
        *self.running.write() = false;
        if let Some(mdns) = self.mdns.take() {
            if let Err(e) = mdns.shutdown() {
                error!("Error shutting down mDNS: {:?}", e);
            }
        }
        info!("mDNS discovery stopped");
    }

    /// Returns the next discovered peer
    pub async fn next_peer(&self) -> Option<DiscoveredPeer> {
        self.peer_rx.lock().await.recv().await
    }

    /// Returns all currently known peers
    pub fn known_peers(&self) -> Vec<DiscoveredPeer> {
        self.peers.read().values().cloned().collect()
    }

    /// Checks if the discovery service is running
    pub fn is_running(&self) -> bool {
        *self.running.read()
    }

    /// Manually adds a peer (useful for testing or direct connections)
    #[instrument(skip(self, peer), fields(peer_id = %peer.device_id))]
    pub fn add_peer(&self, peer: DiscoveredPeer) {
        debug!("Manually adding peer");
        self.peers.write().insert(peer.device_id, peer.clone());
        let _ = self.peer_tx.try_send(peer);
    }
}

impl Drop for Discovery {
    fn drop(&mut self) {
        self.stop();
    }
}
