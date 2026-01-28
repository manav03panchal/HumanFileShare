//! Local network discovery using Iroh's built-in mDNS.
//!
//! This module provides a simple wrapper around Iroh's `discovery-local-network`
//! feature which handles mDNS-based peer discovery automatically.
//!
//! When the endpoint is created with `.discovery_local_network()`, Iroh will:
//! - Broadcast our presence on the local network
//! - Discover other Iroh nodes and add them to the node map
//! - Provide discovered peers via `remote_info_iter()` and `discovery_stream()`

use std::net::SocketAddr;
use std::time::Instant;

use iroh::{Endpoint as IrohEndpoint, NodeAddr, NodeId};
use tracing::{debug, info};

/// The source name used by Iroh's mDNS discovery.
/// Used to filter locally-discovered peers.
pub const MDNS_DISCOVERY_NAME: &str = "mdns";

/// Information about a discovered peer on the local network.
#[derive(Debug, Clone)]
pub struct DiscoveredPeer {
    /// The peer's node ID (public key).
    pub device_id: NodeId,
    /// Network addresses where the peer can be reached.
    pub addresses: Vec<SocketAddr>,
    /// Optional friendly device name (not provided by mDNS discovery).
    pub device_name: Option<String>,
    /// When the peer was last seen.
    pub last_seen: Instant,
}

impl DiscoveredPeer {
    /// Convert to an Iroh NodeAddr for connecting.
    pub fn to_node_addr(&self) -> NodeAddr {
        NodeAddr::new(self.device_id).with_direct_addresses(self.addresses.clone())
    }
}

/// Discovery service that wraps Iroh's built-in local network discovery.
///
/// This is a lightweight wrapper that queries the Iroh endpoint for
/// discovered peers. The actual mDNS discovery is handled by Iroh
/// when the endpoint is configured with `.discovery_local_network()`.
pub struct Discovery {
    /// Reference to the Iroh endpoint for querying discovered nodes.
    endpoint: IrohEndpoint,
    /// Our own node ID (to filter out self-discovery).
    our_node_id: NodeId,
}

impl Discovery {
    /// Creates a new discovery service wrapping an Iroh endpoint.
    ///
    /// The endpoint must have been created with `.discovery_local_network()`
    /// for mDNS discovery to work.
    pub fn new(endpoint: IrohEndpoint) -> Self {
        let our_node_id = endpoint.node_id();
        info!(node_id = %our_node_id, "Discovery service initialized (using Iroh's built-in mDNS)");

        Self {
            endpoint,
            our_node_id,
        }
    }

    /// Returns all currently known peers discovered via local network.
    ///
    /// This queries Iroh's internal node map for peers that were
    /// discovered through mDNS.
    pub fn known_peers(&self) -> Vec<DiscoveredPeer> {
        self.endpoint
            .remote_info_iter()
            .filter(|info| {
                // Filter out ourselves
                info.node_id != self.our_node_id
            })
            .filter(|info| {
                // Only include peers with addresses (locally discovered)
                !info.addrs.is_empty()
            })
            .map(|info| {
                let addresses: Vec<SocketAddr> =
                    info.addrs.iter().map(|addr_info| addr_info.addr).collect();

                debug!(
                    node_id = %info.node_id,
                    address_count = addresses.len(),
                    "Found discovered peer"
                );

                DiscoveredPeer {
                    device_id: info.node_id,
                    addresses,
                    device_name: None, // mDNS doesn't provide device names
                    last_seen: Instant::now(),
                }
            })
            .collect()
    }

    /// Returns true if any peers have been discovered.
    pub fn has_peers(&self) -> bool {
        self.endpoint
            .remote_info_iter()
            .any(|info| info.node_id != self.our_node_id && !info.addrs.is_empty())
    }

    /// Returns the number of discovered peers.
    pub fn peer_count(&self) -> usize {
        self.endpoint
            .remote_info_iter()
            .filter(|info| info.node_id != self.our_node_id && !info.addrs.is_empty())
            .count()
    }

    /// Gets information about a specific peer by node ID.
    pub fn get_peer(&self, node_id: NodeId) -> Option<DiscoveredPeer> {
        self.endpoint.remote_info(node_id).and_then(|info| {
            if info.addrs.is_empty() {
                return None;
            }

            let addresses: Vec<SocketAddr> =
                info.addrs.iter().map(|addr_info| addr_info.addr).collect();

            Some(DiscoveredPeer {
                device_id: info.node_id,
                addresses,
                device_name: None,
                last_seen: Instant::now(),
            })
        })
    }
}
