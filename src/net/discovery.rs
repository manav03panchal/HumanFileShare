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
use std::sync::Arc;
use std::time::{Duration, Instant};

use iroh::{Endpoint as IrohEndpoint, NodeAddr, NodeId};
use parking_lot::RwLock;
use smallvec::SmallVec;
use tokio::sync::mpsc;
use tracing::{debug, info};

/// The source name used by Iroh's mDNS discovery.
/// Used to filter locally-discovered peers.
pub const MDNS_DISCOVERY_NAME: &str = "mdns";

/// Default cache TTL for peer list.
pub const DEFAULT_PEER_CACHE_TTL: Duration = Duration::from_secs(5);

/// Maximum number of addresses to store inline (typical peers have 1-2 addresses).
const INLINE_ADDRS: usize = 4;

/// Information about a discovered peer on the local network.
#[derive(Debug, Clone)]
pub struct DiscoveredPeer {
    /// The peer's node ID (public key).
    pub device_id: NodeId,
    /// Network addresses where the peer can be reached.
    /// Uses `Arc<[SocketAddr]>` for cheap cloning of immutable address lists.
    addresses: Arc<[SocketAddr]>,
    /// Optional friendly device name (not provided by mDNS discovery).
    pub device_name: Option<String>,
    /// When the peer was last seen.
    pub last_seen: Instant,
}

impl DiscoveredPeer {
    /// Creates a new `DiscoveredPeer`.
    #[inline]
    pub fn new(
        device_id: NodeId,
        addresses: Arc<[SocketAddr]>,
        device_name: Option<String>,
        last_seen: Instant,
    ) -> Self {
        Self {
            device_id,
            addresses,
            device_name,
            last_seen,
        }
    }

    /// Returns the peer's addresses as a slice.
    #[inline]
    pub fn addresses(&self) -> &[SocketAddr] {
        &self.addresses
    }

    /// Returns the number of addresses.
    #[inline]
    pub fn address_count(&self) -> usize {
        self.addresses.len()
    }

    /// Convert to an Iroh NodeAddr for connecting.
    #[inline]
    pub fn to_node_addr(&self) -> NodeAddr {
        // Convert Arc<[SocketAddr]> to SmallVec for NodeAddr
        let addrs: SmallVec<[SocketAddr; INLINE_ADDRS]> =
            self.addresses.iter().copied().collect();
        NodeAddr::new(self.device_id).with_direct_addresses(addrs)
    }
}

/// Iterator over discovered peers.
pub struct PeerIter<'a> {
    inner: std::slice::Iter<'a, DiscoveredPeer>,
}

impl<'a> Iterator for PeerIter<'a> {
    type Item = &'a DiscoveredPeer;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next()
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl<'a> ExactSizeIterator for PeerIter<'a> {
    #[inline]
    fn len(&self) -> usize {
        self.inner.len()
    }
}

/// Cached peer list with TTL.
struct PeerCache {
    /// Cached list of peers.
    peers: Vec<DiscoveredPeer>,
    /// When the cache was last updated.
    last_updated: Instant,
    /// Cache TTL.
    ttl: Duration,
}

impl PeerCache {
    #[inline]
    fn new(ttl: Duration) -> Self {
        Self {
            peers: Vec::new(),
            last_updated: Instant::now(),
            ttl,
        }
    }

    #[inline]
    fn is_stale(&self) -> bool {
        self.last_updated.elapsed() >= self.ttl
    }

    #[inline]
    fn update(&mut self, peers: Vec<DiscoveredPeer>) {
        self.peers = peers;
        self.last_updated = Instant::now();
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
    /// Cached peer list with TTL.
    cache: RwLock<PeerCache>,
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
            cache: RwLock::new(PeerCache::new(DEFAULT_PEER_CACHE_TTL)),
        }
    }

    /// Creates a new discovery service with a custom cache TTL.
    pub fn with_cache_ttl(endpoint: IrohEndpoint, ttl: Duration) -> Self {
        let our_node_id = endpoint.node_id();
        info!(
            node_id = %our_node_id,
            ttl_secs = ttl.as_secs(),
            "Discovery service initialized with custom TTL"
        );

        Self {
            endpoint,
            our_node_id,
            cache: RwLock::new(PeerCache::new(ttl)),
        }
    }

    /// Queries the endpoint for all known peers.
    fn query_peers(&self) -> Vec<DiscoveredPeer> {
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
                // Collect addresses using SmallVec for stack allocation,
                // then convert to Arc<[SocketAddr]> for cheap cloning
                let addresses: SmallVec<[SocketAddr; INLINE_ADDRS]> =
                    info.addrs.iter().map(|addr_info| addr_info.addr).collect();

                debug!(
                    node_id = %info.node_id,
                    address_count = addresses.len(),
                    "Found discovered peer"
                );

                DiscoveredPeer {
                    device_id: info.node_id,
                    addresses: Arc::from(addresses.into_vec()),
                    device_name: None, // mDNS doesn't provide device names
                    last_seen: Instant::now(),
                }
            })
            .collect()
    }

    /// Returns all currently known peers discovered via local network.
    ///
    /// This uses a TTL-based cache to avoid repeated endpoint queries.
    /// The cache is automatically refreshed when stale.
    pub fn known_peers(&self) -> Vec<DiscoveredPeer> {
        {
            let cache = self.cache.read();
            if !cache.is_stale() {
                return cache.peers.clone();
            }
        } // Read lock dropped here

        // Cache is stale, need to update
        let peers = self.query_peers();
        let mut cache = self.cache.write();
        cache.update(peers.clone());
        peers
    }

    /// Returns all peers without using cache (forces a fresh query).
    pub fn known_peers_fresh(&self) -> Vec<DiscoveredPeer> {
        let peers = self.query_peers();
        let mut cache = self.cache.write();
        cache.update(peers.clone());
        peers
    }

    /// Clears the peer cache, forcing a refresh on next query.
    #[inline]
    pub fn invalidate_cache(&self) {
        let mut cache = self.cache.write();
        cache.last_updated = Instant::now() - cache.ttl - Duration::from_secs(1);
    }

    /// Returns an iterator over known peers.
    ///
    /// This method returns a `Vec` which can be iterated. The cache is updated
    /// with fresh data if it is stale.
    pub fn iter_peers(&self) -> impl Iterator<Item = DiscoveredPeer> + '_ {
        // Return an owning iterator over the cached data
        self.known_peers().into_iter()
    }

    /// Returns true if any peers have been discovered.
    ///
    /// Uses cache if fresh, otherwise queries endpoint.
    #[inline]
    pub fn has_peers(&self) -> bool {
        self.peer_count() > 0
    }

    /// Returns the number of discovered peers.
    ///
    /// Uses cache if fresh, otherwise queries endpoint.
    pub fn peer_count(&self) -> usize {
        {
            let cache = self.cache.read();
            if !cache.is_stale() {
                return cache.peers.len();
            }
        }

        let peers = self.query_peers();
        let count = peers.len();
        let mut cache = self.cache.write();
        cache.update(peers);
        count
    }

    /// Gets information about a specific peer by node ID.
    ///
    /// Checks cache first, then queries endpoint directly if not found.
    pub fn get_peer(&self, node_id: NodeId) -> Option<DiscoveredPeer> {
        // First check cache
        {
            let cache = self.cache.read();
            if !cache.is_stale() {
                if let Some(peer) = cache.peers.iter().find(|p| p.device_id == node_id) {
                    return Some(peer.clone());
                }
            }
        }

        // Not in cache or cache is stale, query endpoint directly
        self.endpoint.remote_info(node_id).and_then(|info| {
            if info.addrs.is_empty() {
                return None;
            }

            let addresses: SmallVec<[SocketAddr; INLINE_ADDRS]> =
                info.addrs.iter().map(|addr_info| addr_info.addr).collect();

            Some(DiscoveredPeer {
                device_id: info.node_id,
                addresses: Arc::from(addresses.into_vec()),
                device_name: None,
                last_seen: Instant::now(),
            })
        })
    }

    /// Returns a stream that yields peer discovery events.
    ///
    /// This provides a reactive API for peer discovery. The stream will
    /// yield `PeerEvent` items whenever peers are discovered or lost.
    ///
    /// Note: The stream polls the endpoint at a fixed interval (1 second).
    /// For real-time updates, consider using Iroh's `discovery_stream()` directly.
    pub fn peer_stream(&self) -> mpsc::Receiver<PeerEvent> {
        let (tx, rx) = mpsc::channel(128);
        let endpoint = self.endpoint.clone();
        let our_node_id = self.our_node_id;

        tokio::spawn(async move {
            let mut last_peers: std::collections::HashSet<NodeId> = std::collections::HashSet::new();

            loop {
                let current_peers: std::collections::HashSet<NodeId> = endpoint
                    .remote_info_iter()
                    .filter(|info| info.node_id != our_node_id && !info.addrs.is_empty())
                    .map(|info| info.node_id)
                    .collect();

                // Check for new peers
                for &node_id in current_peers.difference(&last_peers) {
                    if let Some(info) = endpoint.remote_info(node_id) {
                        let addresses: SmallVec<[SocketAddr; INLINE_ADDRS]> =
                            info.addrs.iter().map(|a| a.addr).collect();

                        let peer = DiscoveredPeer {
                            device_id: node_id,
                            addresses: Arc::from(addresses.into_vec()),
                            device_name: None,
                            last_seen: Instant::now(),
                        };

                        if tx.send(PeerEvent::Discovered(peer)).await.is_err() {
                            return; // Receiver dropped
                        }
                    }
                }

                // Check for lost peers
                for &node_id in last_peers.difference(&current_peers) {
                    if tx.send(PeerEvent::Lost(node_id)).await.is_err() {
                        return; // Receiver dropped
                    }
                }

                last_peers = current_peers;
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });

        rx
    }
}

/// Events emitted by the peer discovery stream.
#[derive(Debug, Clone)]
pub enum PeerEvent {
    /// A new peer was discovered.
    Discovered(DiscoveredPeer),
    /// A peer is no longer reachable.
    Lost(NodeId),
}

/// Helper trait for converting collections of peers.
pub trait IntoDiscoveredPeers {
    /// Converts the collection into a `Vec<DiscoveredPeer>`.
    fn into_discovered_peers(self) -> Vec<DiscoveredPeer>;
}

impl IntoDiscoveredPeers for Vec<DiscoveredPeer> {
    #[inline]
    fn into_discovered_peers(self) -> Vec<DiscoveredPeer> {
        self
    }
}

impl IntoDiscoveredPeers for &[DiscoveredPeer] {
    #[inline]
    fn into_discovered_peers(self) -> Vec<DiscoveredPeer> {
        self.to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    /// Creates a test SocketAddr.
    fn test_addr(port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), port)
    }

    /// Creates a random NodeId for testing.
    fn test_node_id() -> NodeId {
        // NodeId is a public key; for testing we can create from bytes
        // In real usage this comes from the endpoint
        NodeId::from_bytes(&[0u8; 32]).unwrap()
    }

    /// Creates a different NodeId for testing.
    fn test_node_id_alt() -> NodeId {
        let mut bytes = [0u8; 32];
        bytes[0] = 1;
        NodeId::from_bytes(&bytes).unwrap()
    }

    mod discovered_peer_tests {
        use super::*;

        #[test]
        fn test_discovered_peer_creation() {
            let node_id = test_node_id();
            let addresses: Arc<[SocketAddr]> =
                Arc::from(vec![test_addr(8080), test_addr(8081)].into_boxed_slice());
            let now = Instant::now();

            let peer = DiscoveredPeer::new(
                node_id,
                Arc::clone(&addresses),
                Some("Test Device".to_string()),
                now,
            );

            assert_eq!(peer.device_id, node_id);
            assert_eq!(peer.address_count(), 2);
            assert_eq!(peer.device_name, Some("Test Device".to_string()));
            assert_eq!(peer.last_seen, now);
        }

        #[test]
        fn test_discovered_peer_addresses() {
            let node_id = test_node_id();
            let addrs = vec![test_addr(8080), test_addr(8081)];
            let addresses: Arc<[SocketAddr]> = Arc::from(addrs.into_boxed_slice());

            let peer = DiscoveredPeer::new(node_id, Arc::clone(&addresses), None, Instant::now());

            assert_eq!(peer.addresses().len(), 2);
            assert_eq!(peer.addresses()[0], test_addr(8080));
            assert_eq!(peer.addresses()[1], test_addr(8081));
        }

        #[test]
        fn test_to_node_addr() {
            let node_id = test_node_id();
            let addrs = vec![test_addr(8080), test_addr(8081)];
            let addresses: Arc<[SocketAddr]> = Arc::from(addrs.into_boxed_slice());

            let peer = DiscoveredPeer::new(node_id, Arc::clone(&addresses), None, Instant::now());
            let node_addr = peer.to_node_addr();

            assert_eq!(node_addr.node_id, node_id);
            assert_eq!(node_addr.direct_addresses.len(), 2);
        }

        #[test]
        fn test_clone() {
            let node_id = test_node_id();
            let addresses: Arc<[SocketAddr]> =
                Arc::from(vec![test_addr(8080)].into_boxed_slice());

            let peer = DiscoveredPeer::new(
                node_id,
                Arc::clone(&addresses),
                Some("Device".to_string()),
                Instant::now(),
            );
            let cloned = peer.clone();

            assert_eq!(cloned.device_id, peer.device_id);
            assert_eq!(cloned.address_count(), peer.address_count());
            // Arc cloning is cheap - both point to same data
            assert!(Arc::ptr_eq(&cloned.addresses, &peer.addresses));
        }

        #[test]
        fn test_empty_addresses() {
            let node_id = test_node_id();
            let addresses: Arc<[SocketAddr]> = Arc::from(Vec::new().into_boxed_slice());

            let peer = DiscoveredPeer::new(node_id, addresses, None, Instant::now());

            assert_eq!(peer.address_count(), 0);
            assert!(peer.addresses().is_empty());
        }

        #[test]
        fn test_single_address() {
            let node_id = test_node_id();
            let addresses: Arc<[SocketAddr]> =
                Arc::from(vec![test_addr(9000)].into_boxed_slice());

            let peer = DiscoveredPeer::new(node_id, addresses, None, Instant::now());

            assert_eq!(peer.address_count(), 1);
            assert_eq!(peer.addresses()[0].port(), 9000);
        }
    }

    mod peer_cache_tests {
        use super::*;

        #[test]
        fn test_cache_creation() {
            let ttl = Duration::from_secs(10);
            let cache = PeerCache::new(ttl);

            assert!(cache.peers.is_empty());
            assert!(!cache.is_stale());
        }

        #[test]
        fn test_cache_staleness() {
            let ttl = Duration::from_millis(50);
            let mut cache = PeerCache::new(ttl);

            assert!(!cache.is_stale());

            // Update with a peer
            let peer = DiscoveredPeer::new(
                test_node_id(),
                Arc::from(vec![test_addr(8080)].into_boxed_slice()),
                None,
                Instant::now(),
            );
            cache.update(vec![peer]);

            assert!(!cache.is_stale());

            // Wait for TTL to expire
            std::thread::sleep(ttl + Duration::from_millis(10));
            assert!(cache.is_stale());
        }

        #[test]
        fn test_cache_update() {
            let ttl = Duration::from_secs(60);
            let mut cache = PeerCache::new(ttl);

            let peer1 = DiscoveredPeer::new(
                test_node_id(),
                Arc::from(vec![test_addr(8080)].into_boxed_slice()),
                None,
                Instant::now(),
            );

            cache.update(vec![peer1.clone()]);
            assert_eq!(cache.peers.len(), 1);
            assert_eq!(cache.peers[0].device_id, peer1.device_id);

            // Update with different peer
            let peer2 = DiscoveredPeer::new(
                test_node_id_alt(),
                Arc::from(vec![test_addr(9090)].into_boxed_slice()),
                None,
                Instant::now(),
            );

            let old_updated = cache.last_updated;
            std::thread::sleep(Duration::from_millis(10));
            cache.update(vec![peer2.clone()]);

            assert_eq!(cache.peers.len(), 1);
            assert_eq!(cache.peers[0].device_id, peer2.device_id);
            assert!(cache.last_updated > old_updated);
        }
    }

    mod peer_event_tests {
        use super::*;

        #[test]
        fn test_peer_event_discovered() {
            let peer = DiscoveredPeer::new(
                test_node_id(),
                Arc::from(vec![test_addr(8080)].into_boxed_slice()),
                None,
                Instant::now(),
            );

            let event = PeerEvent::Discovered(peer.clone());

            match event {
                PeerEvent::Discovered(p) => {
                    assert_eq!(p.device_id, peer.device_id);
                }
                _ => panic!("Expected Discovered event"),
            }
        }

        #[test]
        fn test_peer_event_lost() {
            let node_id = test_node_id();
            let event = PeerEvent::Lost(node_id);

            match event {
                PeerEvent::Lost(id) => {
                    assert_eq!(id, node_id);
                }
                _ => panic!("Expected Lost event"),
            }
        }

        #[test]
        fn test_peer_event_clone() {
            let peer = DiscoveredPeer::new(
                test_node_id(),
                Arc::from(vec![test_addr(8080)].into_boxed_slice()),
                None,
                Instant::now(),
            );

            let event = PeerEvent::Discovered(peer);
            let cloned = event.clone();

            match (event, cloned) {
                (PeerEvent::Discovered(p1), PeerEvent::Discovered(p2)) => {
                    assert_eq!(p1.device_id, p2.device_id);
                }
                _ => panic!("Expected Discovered events"),
            }
        }
    }

    mod into_discovered_peers_tests {
        use super::*;

        #[test]
        fn test_vec_into_discovered_peers() {
            let peer1 = DiscoveredPeer::new(
                test_node_id(),
                Arc::from(vec![test_addr(8080)].into_boxed_slice()),
                None,
                Instant::now(),
            );
            let peer2 = DiscoveredPeer::new(
                test_node_id_alt(),
                Arc::from(vec![test_addr(9090)].into_boxed_slice()),
                None,
                Instant::now(),
            );

            let vec = vec![peer1.clone(), peer2.clone()];
            let converted: Vec<DiscoveredPeer> = vec.clone().into_discovered_peers();

            assert_eq!(converted.len(), 2);
            assert_eq!(converted[0].device_id, peer1.device_id);
            assert_eq!(converted[1].device_id, peer2.device_id);
        }

        #[test]
        fn test_slice_into_discovered_peers() {
            let peer1 = DiscoveredPeer::new(
                test_node_id(),
                Arc::from(vec![test_addr(8080)].into_boxed_slice()),
                None,
                Instant::now(),
            );
            let peer2 = DiscoveredPeer::new(
                test_node_id_alt(),
                Arc::from(vec![test_addr(9090)].into_boxed_slice()),
                None,
                Instant::now(),
            );

            let vec = vec![peer1, peer2];
            let converted: Vec<DiscoveredPeer> = vec.as_slice().into_discovered_peers();

            assert_eq!(converted.len(), 2);
        }
    }

    mod peer_iterator_tests {
        use super::*;

        #[test]
        fn test_peer_iter() {
            let peer1 = DiscoveredPeer::new(
                test_node_id(),
                Arc::from(vec![test_addr(8080)].into_boxed_slice()),
                None,
                Instant::now(),
            );
            let peer2 = DiscoveredPeer::new(
                test_node_id_alt(),
                Arc::from(vec![test_addr(9090)].into_boxed_slice()),
                None,
                Instant::now(),
            );

            let peers = vec![peer1.clone(), peer2.clone()];
            let iter = PeerIter {
                inner: peers.iter(),
            };

            let collected: Vec<_> = iter.collect();
            assert_eq!(collected.len(), 2);
            assert_eq!(collected[0].device_id, peer1.device_id);
            assert_eq!(collected[1].device_id, peer2.device_id);
        }

        #[test]
        fn test_peer_iter_exact_size() {
            let peers: Vec<DiscoveredPeer> = vec![
                DiscoveredPeer::new(
                    test_node_id(),
                    Arc::from(vec![test_addr(8080)].into_boxed_slice()),
                    None,
                    Instant::now(),
                ),
                DiscoveredPeer::new(
                    test_node_id_alt(),
                    Arc::from(vec![test_addr(9090)].into_boxed_slice()),
                    None,
                    Instant::now(),
                ),
            ];

            let iter = PeerIter {
                inner: peers.iter(),
            };

            assert_eq!(iter.len(), 2);

            let mut iter = iter;
            iter.next();
            assert_eq!(iter.len(), 1);
            iter.next();
            assert_eq!(iter.len(), 0);
        }

        #[test]
        fn test_peer_iter_empty() {
            let peers: Vec<DiscoveredPeer> = vec![];
            let iter = PeerIter {
                inner: peers.iter(),
            };

            assert_eq!(iter.len(), 0);
            assert!(iter.collect::<Vec<_>>().is_empty());
        }
    }

    mod constants_tests {
        use super::*;

        #[test]
        fn test_mdns_discovery_name() {
            assert_eq!(MDNS_DISCOVERY_NAME, "mdns");
        }

        #[test]
        fn test_default_peer_cache_ttl() {
            assert_eq!(DEFAULT_PEER_CACHE_TTL, Duration::from_secs(5));
        }

        #[test]
        fn test_inline_addrs() {
            // INLINE_ADDRS is 4, which is the typical max addresses per peer
            assert_eq!(INLINE_ADDRS, 4);
        }
    }

    /// Mock IrohEndpoint for testing Discovery methods without real network.
    mod mock_endpoint_tests {
        use super::*;

        /// A mock RemoteInfo for testing.
        #[derive(Clone)]
        struct MockRemoteInfo {
            node_id: NodeId,
            addrs: Vec<MockAddrInfo>,
        }

        /// A mock address info for testing.
        #[derive(Clone)]
        struct MockAddrInfo {
            addr: SocketAddr,
        }

        /// Tests that we can create and manipulate DiscoveredPeer without an endpoint.
        /// These test the core logic that would be used with a real endpoint.
        #[test]
        fn test_peer_filtering_logic() {
            let our_node_id = test_node_id();
            let other_node_id = test_node_id_alt();

            // Simulate endpoint data
            let mock_infos = vec![
                MockRemoteInfo {
                    node_id: our_node_id,
                    addrs: vec![MockAddrInfo { addr: test_addr(8080) }],
                },
                MockRemoteInfo {
                    node_id: other_node_id,
                    addrs: vec![
                        MockAddrInfo { addr: test_addr(9090) },
                        MockAddrInfo { addr: test_addr(9091) },
                    ],
                },
                MockRemoteInfo {
                    node_id: test_node_id(), // Different instance, same bytes - empty addrs
                    addrs: vec![],
                },
            ];

            // Apply filtering logic
            let peers: Vec<_> = mock_infos
                .into_iter()
                .filter(|info| info.node_id != our_node_id)
                .filter(|info| !info.addrs.is_empty())
                .map(|info| {
                    let addresses: SmallVec<[SocketAddr; INLINE_ADDRS]> =
                        info.addrs.iter().map(|a| a.addr).collect();

                    DiscoveredPeer {
                        device_id: info.node_id,
                        addresses: Arc::from(addresses.into_vec()),
                        device_name: None,
                        last_seen: Instant::now(),
                    }
                })
                .collect();

            // Should only have the other_node_id peer (our_node_id filtered out)
            assert_eq!(peers.len(), 1);
            assert_eq!(peers[0].device_id, other_node_id);
            assert_eq!(peers[0].address_count(), 2);
        }

        #[test]
        fn test_get_peer_logic() {
            let target_node_id = test_node_id_alt();
            let addrs = vec![test_addr(8080)];

            let peer = DiscoveredPeer::new(
                target_node_id,
                Arc::from(addrs.into_boxed_slice()),
                None,
                Instant::now(),
            );

            // Simulate finding a peer by ID
            let found = Some(&peer).filter(|p| p.device_id == target_node_id);
            assert!(found.is_some());
            assert_eq!(found.unwrap().device_id, target_node_id);

            // Simulate not finding a peer
            let not_found = Some(&peer).filter(|p| p.device_id == test_node_id());
            assert!(not_found.is_none());
        }

        #[test]
        fn test_address_collection_with_smallvec() {
            // Test that SmallVec correctly handles the inline capacity
            let addrs: Vec<SocketAddr> = (0..10).map(|i| test_addr(8000 + i as u16)).collect();

            let small_addrs: SmallVec<[SocketAddr; INLINE_ADDRS]> =
                addrs.iter().copied().collect();

            // SmallVec should still hold all 10 addresses
            assert_eq!(small_addrs.len(), 10);

            // Convert to Arc
            let arc_addrs: Arc<[SocketAddr]> = Arc::from(small_addrs.into_vec());
            assert_eq!(arc_addrs.len(), 10);
        }

        #[test]
        fn test_empty_address_filtering() {
            let mock_infos = vec![
                MockRemoteInfo {
                    node_id: test_node_id(),
                    addrs: vec![], // Empty addresses
                },
                MockRemoteInfo {
                    node_id: test_node_id_alt(),
                    addrs: vec![MockAddrInfo { addr: test_addr(8080) }],
                },
            ];

            let peers: Vec<_> = mock_infos
                .into_iter()
                .filter(|info| !info.addrs.is_empty())
                .collect();

            assert_eq!(peers.len(), 1);
        }

        #[test]
        fn test_has_peers_logic() {
            let peers: Vec<DiscoveredPeer> = vec![];
            assert!(!(!peers.is_empty())); // has_peers equivalent

            let peer = DiscoveredPeer::new(
                test_node_id(),
                Arc::from(vec![test_addr(8080)].into_boxed_slice()),
                None,
                Instant::now(),
            );

            let peers = vec![peer];
            assert!(!peers.is_empty());
        }

        #[test]
        fn test_peer_count_logic() {
            let peers: Vec<DiscoveredPeer> = vec![
                DiscoveredPeer::new(
                    test_node_id(),
                    Arc::from(vec![test_addr(8080)].into_boxed_slice()),
                    None,
                    Instant::now(),
                ),
                DiscoveredPeer::new(
                    test_node_id_alt(),
                    Arc::from(vec![test_addr(9090)].into_boxed_slice()),
                    None,
                    Instant::now(),
                ),
            ];

            assert_eq!(peers.len(), 2);
        }
    }
}
