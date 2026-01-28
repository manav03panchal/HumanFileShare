//! Local network discovery via mDNS
//!
//! This module handles discovering other HumanFileShare devices on the local
//! network using multicast DNS (mDNS). This enables:
//!
//! - Zero-configuration discovery of nearby devices
//! - Automatic peer detection without manual IP entry
//! - Fast local transfers without relay servers
//!
//! # Protocol
//!
//! Devices advertise themselves using the service type `_humanfileshare._tcp.local.`
//! The TXT record contains the device's public key for secure identification.
//!
//! # Example
//!
//! ```rust,ignore
//! use humanfileshare::net::Discovery;
//!
//! let discovery = Discovery::new(device_id).await?;
//! discovery.start_broadcasting()?;
//!
//! // Listen for discovered peers
//! while let Some(peer) = discovery.next_peer().await {
//!     println!("Found peer: {} at {:?}", peer.device_id, peer.addresses);
//! }
//! ```

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use iroh::PublicKey;
use parking_lot::RwLock;
use thiserror::Error;
use tokio::sync::mpsc;
use tracing::{debug, info, instrument, warn};

/// The mDNS service type for HumanFileShare
pub const SERVICE_TYPE: &str = "_humanfileshare._tcp.local.";

/// Default mDNS port
pub const MDNS_PORT: u16 = 5353;

/// How often to re-announce presence (in seconds)
const ANNOUNCE_INTERVAL: Duration = Duration::from_secs(30);

/// Errors that can occur during discovery operations
#[derive(Error, Debug)]
pub enum DiscoveryError {
    /// Failed to initialize mDNS
    #[error("failed to initialize mDNS: {0}")]
    InitFailed(String),

    /// Failed to register service
    #[error("failed to register service: {0}")]
    RegistrationFailed(String),

    /// Failed to browse for services
    #[error("failed to browse for services: {0}")]
    BrowseFailed(String),

    /// Discovery is not running
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
}

/// Internal state of the discovery service
#[derive(Debug)]
enum DiscoveryState {
    /// Not yet started
    Stopped,
    /// Running and discovering
    Running {
        /// Handle to stop broadcasting
        _broadcast_handle: tokio::task::JoinHandle<()>,
    },
}

/// Manages local network discovery via mDNS
///
/// The `Discovery` struct handles advertising this device on the local network
/// and discovering other HumanFileShare devices.
#[derive(Debug)]
pub struct Discovery {
    /// This device's public key
    device_id: PublicKey,
    /// Optional friendly name for this device
    device_name: Option<String>,
    /// Currently known peers
    peers: Arc<RwLock<HashMap<PublicKey, DiscoveredPeer>>>,
    /// Channel for peer discovery events
    peer_tx: mpsc::Sender<DiscoveredPeer>,
    /// Receiver for peer discovery events
    peer_rx: Arc<tokio::sync::Mutex<mpsc::Receiver<DiscoveredPeer>>>,
    /// Current state
    state: Arc<RwLock<DiscoveryState>>,
}

impl Discovery {
    /// Creates a new discovery service for the given device
    ///
    /// # Arguments
    ///
    /// * `device_id` - The public key of this device
    ///
    /// # Returns
    ///
    /// A new `Discovery` instance, not yet started
    #[instrument(skip(device_id), fields(device_id = %device_id))]
    pub fn new(device_id: PublicKey) -> Self {
        let (peer_tx, peer_rx) = mpsc::channel(100);

        info!("Discovery service created");

        Self {
            device_id,
            device_name: None,
            peers: Arc::new(RwLock::new(HashMap::new())),
            peer_tx,
            peer_rx: Arc::new(tokio::sync::Mutex::new(peer_rx)),
            state: Arc::new(RwLock::new(DiscoveryState::Stopped)),
        }
    }

    /// Sets a friendly name for this device
    ///
    /// This name will be advertised to other peers and displayed
    /// in their device lists.
    pub fn with_device_name(mut self, name: impl Into<String>) -> Self {
        self.device_name = Some(name.into());
        self
    }

    /// Starts broadcasting presence and listening for peers
    ///
    /// This will:
    /// 1. Register this device as an mDNS service
    /// 2. Start browsing for other HumanFileShare services
    /// 3. Begin emitting peer discovery events
    ///
    /// # Errors
    ///
    /// Returns an error if mDNS initialization fails
    #[instrument(skip(self))]
    pub fn start(&self) -> Result<()> {
        let mut state = self.state.write();
        if matches!(*state, DiscoveryState::Running { .. }) {
            debug!("Discovery already running");
            return Ok(());
        }

        info!("Starting mDNS discovery");

        // TODO: Implement actual mDNS using mdns-sd crate
        // For now, create a placeholder task that periodically logs
        let device_id = self.device_id;
        let device_name = self.device_name.clone();
        let peers = self.peers.clone();
        let _peer_tx = self.peer_tx.clone();

        let broadcast_handle = tokio::spawn(async move {
            debug!(
                device_id = %device_id,
                device_name = ?device_name,
                "mDNS broadcast task started (placeholder)"
            );

            // Placeholder: In production, this would use mdns-sd to:
            // 1. Register our service with TXT record containing device_id
            // 2. Browse for other _humanfileshare._tcp services
            // 3. Parse discovered services and emit DiscoveredPeer events

            let mut interval = tokio::time::interval(ANNOUNCE_INTERVAL);
            loop {
                interval.tick().await;
                debug!("mDNS announce tick (placeholder)");

                // Clean up stale peers (not seen in last 2 minutes)
                let now = std::time::Instant::now();
                let stale_timeout = Duration::from_secs(120);
                peers
                    .write()
                    .retain(|_, peer| now.duration_since(peer.last_seen) < stale_timeout);
            }
        });

        *state = DiscoveryState::Running {
            _broadcast_handle: broadcast_handle,
        };

        info!("mDNS discovery started");
        Ok(())
    }

    /// Stops the discovery service
    ///
    /// This will:
    /// 1. Unregister the mDNS service
    /// 2. Stop browsing for peers
    /// 3. Clean up resources
    #[instrument(skip(self))]
    pub fn stop(&self) {
        let mut state = self.state.write();
        if let DiscoveryState::Running { _broadcast_handle } =
            std::mem::replace(&mut *state, DiscoveryState::Stopped)
        {
            _broadcast_handle.abort();
            info!("mDNS discovery stopped");
        }
    }

    /// Returns the next discovered peer
    ///
    /// This is an async stream of peer discovery events. Call this
    /// in a loop to receive notifications of new or updated peers.
    pub async fn next_peer(&self) -> Option<DiscoveredPeer> {
        self.peer_rx.lock().await.recv().await
    }

    /// Returns all currently known peers
    ///
    /// This returns a snapshot of all peers discovered on the local network.
    /// Peers that haven't been seen recently may be stale.
    pub fn known_peers(&self) -> Vec<DiscoveredPeer> {
        self.peers.read().values().cloned().collect()
    }

    /// Checks if the discovery service is running
    pub fn is_running(&self) -> bool {
        matches!(&*self.state.read(), DiscoveryState::Running { .. })
    }

    /// Manually adds a peer (useful for testing or direct connections)
    ///
    /// # Arguments
    ///
    /// * `peer` - The peer information to add
    #[instrument(skip(self, peer), fields(peer_id = %peer.device_id))]
    pub fn add_peer(&self, peer: DiscoveredPeer) {
        debug!("Manually adding peer");
        self.peers.write().insert(peer.device_id, peer.clone());

        // Try to notify listeners, but don't block if channel is full
        let _ = self.peer_tx.try_send(peer);
    }
}

impl Drop for Discovery {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_service_type_is_valid() {
        assert!(SERVICE_TYPE.ends_with(".local."));
        assert!(SERVICE_TYPE.starts_with('_'));
    }
}
