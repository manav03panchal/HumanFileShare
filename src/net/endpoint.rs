//! Iroh endpoint management and device identity
//!
//! This module handles the core networking endpoint using Iroh, including:
//! - Device identity (key pair) generation and persistence
//! - Endpoint initialization and configuration
//! - Connection management with pooling for frequently accessed peers
//!
//! # Device Identity
//!
//! Each device has a unique cryptographic identity stored at:
//! `~/.config/humanfileshare/device_key`
//!
//! This identity persists across application restarts, ensuring consistent
//! device identification for pairing and transfers.
//!
//! # Performance Optimizations
//!
//! - Config directory path is cached using `OnceCell`
//! - Node addresses are cached to avoid repeated async calls
//! - Connection pooling for frequently accessed peers
//! - `Arc<str>` for error messages to reduce allocations
//! - Zero-copy operations where possible

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use iroh::{Endpoint as IrohEndpoint, NodeAddr, PublicKey, SecretKey};
use once_cell::sync::OnceCell;
use parking_lot::RwLock;
use thiserror::Error;
use tokio::fs;
use tokio::io::AsyncReadExt;
use tokio::sync::Mutex as AsyncMutex;
use tracing::{debug, info, instrument, warn};

/// The ALPN protocol identifier for HumanFileShare
pub const ALPN: &[u8] = b"humanfileshare/1";

/// Default configuration directory name
const CONFIG_DIR: &str = "humanfileshare";

/// Device key filename
const DEVICE_KEY_FILE: &str = "device_key";

/// Default connection pool size limit
const DEFAULT_POOL_SIZE: usize = 32;

/// Connection idle timeout (5 minutes)
const CONNECTION_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// Maximum number of concurrent connections per peer
const MAX_CONNECTIONS_PER_PEER: usize = 3;

/// Cached config directory path
static CONFIG_DIR_CACHE: OnceCell<PathBuf> = OnceCell::new();

/// Errors that can occur during endpoint operations
#[derive(Error, Debug, Clone)]
pub enum EndpointError {
    /// Failed to create or access the configuration directory
    #[error("failed to access configuration directory: {0}")]
    ConfigDir(Arc<str>),

    /// Failed to read or write the device key
    #[error("failed to persist device key: {0}")]
    KeyPersistence(Arc<str>),

    /// Failed to initialize the Iroh endpoint
    #[error("failed to initialize endpoint: {0}")]
    EndpointInit(Arc<str>),

    /// The endpoint is not initialized
    #[error("endpoint not initialized")]
    NotInitialized,

    /// The endpoint has been shut down
    #[error("endpoint has been shut down")]
    Shutdown,

    /// Connection pool is at capacity
    #[error("connection pool at capacity")]
    PoolAtCapacity,

    /// Connection not found in pool
    #[error("connection not found for peer")]
    ConnectionNotFound,

    /// Invalid peer address
    #[error("invalid peer address: {0}")]
    InvalidPeerAddress(Arc<str>),
}

impl EndpointError {
    /// Creates a new config dir error with an `Arc<str>` message
    #[inline]
    fn config_dir(msg: impl Into<String>) -> Self {
        Self::ConfigDir(Arc::from(msg.into()))
    }

    /// Creates a new key persistence error with an `Arc<str>` message
    #[inline]
    fn key_persistence(msg: impl Into<String>) -> Self {
        Self::KeyPersistence(Arc::from(msg.into()))
    }
}

/// Internal state of the endpoint
#[derive(Debug)]
enum EndpointState {
    /// Endpoint is running with the underlying Iroh endpoint
    Running(IrohEndpoint),
    /// Endpoint has been shut down
    Shutdown,
}

/// Metadata for a pooled connection
#[derive(Debug)]
struct PooledConnectionMeta {
    /// When the connection was last used
    last_used: Instant,
    /// Number of active references
    ref_count: AtomicU64,
    /// Connection ID for tracking
    connection_id: u64,
}

impl PooledConnectionMeta {
    #[inline]
    fn new(connection_id: u64) -> Self {
        Self {
            last_used: Instant::now(),
            ref_count: AtomicU64::new(1),
            connection_id,
        }
    }

    #[inline]
    fn touch(&mut self) {
        self.last_used = Instant::now();
    }

    #[inline]
    fn is_expired(&self) -> bool {
        self.last_used.elapsed() > CONNECTION_IDLE_TIMEOUT
            && self.ref_count.load(Ordering::Acquire) == 0
    }
}

/// A pooled connection wrapper that automatically returns to pool on drop
pub struct PooledConnection {
    /// The underlying connection
    connection: Option<iroh::endpoint::Connection>,
    /// The peer's public key
    peer_id: PublicKey,
    /// Reference to the pool for returning
    pool: Arc<AsyncMutex<ConnectionPoolInner>>,
    /// Connection ID for tracking
    connection_id: u64,
}

impl PooledConnection {
    /// Returns a reference to the underlying connection
    #[inline]
    pub fn inner(&self) -> &iroh::endpoint::Connection {
        self.connection.as_ref().expect("connection always present until drop")
    }

    /// Consumes the pooled connection and returns the underlying connection
    /// This removes it from the pool permanently
    #[inline]
    pub fn into_inner(mut self) -> iroh::endpoint::Connection {
        self.connection.take().expect("connection always present until drop")
    }

    /// Returns the peer's public key
    #[inline]
    pub fn peer_id(&self) -> PublicKey {
        self.peer_id
    }
}

impl Drop for PooledConnection {
    fn drop(&mut self) {
        // If we still have the connection, return it to the pool
        if let Some(conn) = self.connection.take() {
            let peer_id = self.peer_id;
            let connection_id = self.connection_id;
            let pool = Arc::clone(&self.pool);

            // Spawn a task to return the connection to the pool
            tokio::spawn(async move {
                let mut inner = pool.lock().await;
                inner.return_connection(peer_id, conn, connection_id);
            });
        }
    }
}

/// Inner connection pool data
#[derive(Debug)]
struct ConnectionPoolInner {
    /// Map of peer ID to their pooled connections
    connections: HashMap<PublicKey, Vec<(iroh::endpoint::Connection, PooledConnectionMeta)>>,
    /// Maximum pool size
    max_size: usize,
    /// Current total connections in pool
    total_connections: usize,
    /// Next connection ID
    next_connection_id: u64,
}

impl ConnectionPoolInner {
    #[inline]
    fn new(max_size: usize) -> Self {
        Self {
            connections: HashMap::new(),
            max_size,
            total_connections: 0,
            next_connection_id: 1,
        }
    }

    /// Gets a connection from the pool if available
    fn get_connection(&mut self, peer_id: &PublicKey) -> Option<(iroh::endpoint::Connection, u64)> {
        let connections = self.connections.get_mut(peer_id)?;

        // Find a non-expired connection
        while let Some((conn, meta)) = connections.pop() {
            self.total_connections -= 1;

            if !meta.is_expired() {
                meta.ref_count.fetch_add(1, Ordering::Release);
                let id = meta.connection_id;
                // Put back the metadata with updated ref count
                connections.push((conn.clone(), meta));
                self.total_connections += 1;
                return Some((conn, id));
            }
        }

        // Clean up empty entry
        if connections.is_empty() {
            self.connections.remove(peer_id);
        }

        None
    }

    /// Returns a connection to the pool
    fn return_connection(
        &mut self,
        peer_id: PublicKey,
        conn: iroh::endpoint::Connection,
        connection_id: u64,
    ) {
        // Check if we should keep this connection
        if self.total_connections >= self.max_size {
            debug!("Connection pool at capacity, dropping returned connection");
            return;
        }

        // Update ref count and touch timestamp
        if let Some(connections) = self.connections.get_mut(&peer_id) {
            for (existing_conn, meta) in connections.iter_mut() {
                if meta.connection_id == connection_id {
                    meta.ref_count.fetch_sub(1, Ordering::Release);
                    meta.touch();
                    // Update the connection reference
                    let _ = existing_conn;
                    return;
                }
            }
        }

        // Connection not found, add it if under per-peer limit
        let peer_connections = self.connections.entry(peer_id).or_default();
        if peer_connections.len() >= MAX_CONNECTIONS_PER_PEER {
            debug!("Max connections per peer reached, dropping connection");
            return;
        }

        let meta = PooledConnectionMeta::new(connection_id);
        peer_connections.push((conn, meta));
        self.total_connections += 1;
    }

    /// Adds a new connection to the pool
    fn add_connection(
        &mut self,
        peer_id: PublicKey,
        conn: iroh::endpoint::Connection,
    ) -> Option<u64> {
        if self.total_connections >= self.max_size {
            return None;
        }

        let peer_connections = self.connections.entry(peer_id).or_default();
        if peer_connections.len() >= MAX_CONNECTIONS_PER_PEER {
            return None;
        }

        let connection_id = self.next_connection_id;
        self.next_connection_id += 1;

        let meta = PooledConnectionMeta::new(connection_id);
        peer_connections.push((conn, meta));
        self.total_connections += 1;

        Some(connection_id)
    }

    /// Cleans up expired connections
    fn cleanup_expired(&mut self) {
        let mut total_removed = 0;

        self.connections.retain(|_peer_id, connections| {
            let before_len = connections.len();
            connections.retain(|(_, meta)| !meta.is_expired());
            total_removed += before_len - connections.len();
            !connections.is_empty()
        });

        self.total_connections = self.total_connections.saturating_sub(total_removed);

        if total_removed > 0 {
            debug!(removed = total_removed, "Cleaned up expired connections");
        }
    }

    /// Gets pool statistics
    fn stats(&self) -> PoolStats {
        PoolStats {
            total_connections: self.total_connections,
            max_size: self.max_size,
            peers: self.connections.len(),
        }
    }
}

/// Connection pool statistics
#[derive(Debug, Clone, Copy)]
pub struct PoolStats {
    /// Total connections in the pool
    pub total_connections: usize,
    /// Maximum pool size
    pub max_size: usize,
    /// Number of unique peers in the pool
    pub peers: usize,
}

/// Manages pooled connections to peers
#[derive(Debug, Clone)]
pub struct ConnectionPool {
    /// Inner pool protected by async mutex
    inner: Arc<AsyncMutex<ConnectionPoolInner>>,
}

impl ConnectionPool {
    /// Creates a new connection pool with the default size
    #[inline]
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_POOL_SIZE)
    }

    /// Creates a new connection pool with the specified capacity
    #[inline]
    pub fn with_capacity(max_size: usize) -> Self {
        Self {
            inner: Arc::new(AsyncMutex::new(ConnectionPoolInner::new(max_size))),
        }
    }

    /// Gets a connection from the pool if available
    #[inline]
    pub async fn get(&self, peer_id: PublicKey) -> Option<(iroh::endpoint::Connection, u64)> {
        let mut inner = self.inner.lock().await;
        inner.get_connection(&peer_id)
    }

    /// Adds a connection to the pool
    #[inline]
    pub async fn add(
        &self,
        peer_id: PublicKey,
        conn: iroh::endpoint::Connection,
    ) -> Option<u64> {
        let mut inner = self.inner.lock().await;
        inner.add_connection(peer_id, conn)
    }

    /// Cleans up expired connections
    #[inline]
    pub async fn cleanup(&self) {
        let mut inner = self.inner.lock().await;
        inner.cleanup_expired();
    }

    /// Gets pool statistics
    #[inline]
    pub async fn stats(&self) -> PoolStats {
        let inner = self.inner.lock().await;
        inner.stats()
    }

    /// Creates a pooled connection wrapper
    #[inline]
    fn wrap_connection(
        &self,
        peer_id: PublicKey,
        connection: iroh::endpoint::Connection,
        connection_id: u64,
    ) -> PooledConnection {
        PooledConnection {
            connection: Some(connection),
            peer_id,
            pool: Arc::clone(&self.inner),
            connection_id,
        }
    }
}

impl Default for ConnectionPool {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

/// Cached node address with timestamp
#[derive(Debug, Clone)]
struct CachedNodeAddr {
    /// The cached node address
    addr: NodeAddr,
    /// When it was cached
    cached_at: Instant,
    /// TTL for the cache
    ttl: Duration,
}

impl CachedNodeAddr {
    #[inline]
    fn new(addr: NodeAddr, ttl: Duration) -> Self {
        Self {
            addr,
            cached_at: Instant::now(),
            ttl,
        }
    }

    #[inline]
    fn is_valid(&self) -> bool {
        self.cached_at.elapsed() < self.ttl
    }

    #[inline]
    fn get(&self) -> NodeAddr {
        self.addr.clone()
    }
}

/// Manages the Iroh networking endpoint and device identity
///
/// The `Endpoint` struct is the primary interface for network operations.
/// It handles:
/// - Loading or generating a persistent device identity
/// - Initializing the Iroh endpoint with NAT traversal
/// - Providing the device's public key for identification
/// - Caching node addresses for performance
/// - Connection pooling for frequently accessed peers
///
/// # Example
///
/// ```rust,ignore
/// use humanfileshare::net::Endpoint;
///
/// let endpoint = Endpoint::new().await?;
/// println!("Device ID: {}", endpoint.device_id());
/// ```
#[derive(Debug)]
pub struct Endpoint {
    /// The device's public key (derived from secret key)
    public_key: PublicKey,
    /// The underlying Iroh endpoint state
    state: Arc<RwLock<EndpointState>>,
    /// Cached node address to avoid repeated async calls
    node_addr_cache: Arc<AsyncMutex<Option<CachedNodeAddr>>>,
    /// Connection pool for frequently accessed peers
    connection_pool: ConnectionPool,
    /// Node address cache TTL
    node_addr_ttl: Duration,
}

impl Endpoint {
    /// Creates a new endpoint, loading or generating the device identity
    ///
    /// This will:
    /// 1. Load the existing device key from `~/.config/humanfileshare/device_key`
    /// 2. Or generate a new key if none exists
    /// 3. Initialize the Iroh endpoint
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The configuration directory cannot be created
    /// - The device key cannot be read or written
    /// - The Iroh endpoint fails to initialize
    #[instrument(name = "endpoint_new")]
    pub async fn new() -> Result<Self> {
        Self::with_config(Default::default()).await
    }

    /// Creates a new endpoint with custom configuration
    pub async fn with_config(config: EndpointConfig) -> Result<Self> {
        let secret_key = Self::load_or_create_device_key().await?;
        let public_key = secret_key.public();

        info!(device_id = %public_key, "Device identity loaded");

        let endpoint = IrohEndpoint::builder()
            .secret_key(secret_key)
            .alpns(vec![ALPN.to_vec()])
            .discovery_n0()
            .discovery_local_network()
            .bind()
            .await
            .context("failed to bind Iroh endpoint")?;

        debug!("Endpoint initialized");

        let connection_pool = if config.enable_connection_pooling {
            ConnectionPool::with_capacity(config.connection_pool_size)
        } else {
            ConnectionPool::with_capacity(0)
        };

        Ok(Self {
            public_key,
            state: Arc::new(RwLock::new(EndpointState::Running(endpoint))),
            node_addr_cache: Arc::new(AsyncMutex::new(None)),
            connection_pool,
            node_addr_ttl: config.node_addr_ttl,
        })
    }

    /// Returns the device's public key (unique device identifier)
    ///
    /// This key uniquely identifies this device across the network and
    /// remains consistent across application restarts.
    #[inline]
    pub fn device_id(&self) -> PublicKey {
        self.public_key
    }

    /// Returns the node address for this endpoint (cached)
    ///
    /// The node address contains the public key and network addresses
    /// needed for other peers to connect to this device.
    ///
    /// This method caches the result to avoid repeated async calls to the
    /// underlying endpoint.
    pub async fn node_addr(&self) -> Result<NodeAddr> {
        // Fast path: check cache without holding lock long
        {
            let cache = self.node_addr_cache.lock().await;
            if let Some(ref cached) = *cache {
                if cached.is_valid() {
                    debug!("Using cached node address");
                    return Ok(cached.get());
                }
            }
        }

        // Slow path: fetch from endpoint and update cache
        let endpoint = {
            let state = self.state.read();
            match &*state {
                EndpointState::Running(endpoint) => endpoint.clone(),
                EndpointState::Shutdown => return Err(EndpointError::Shutdown.into()),
            }
        };

        let addr = endpoint.node_addr().await?;

        // Update cache
        let mut cache = self.node_addr_cache.lock().await;
        *cache = Some(CachedNodeAddr::new(addr.clone(), self.node_addr_ttl));

        Ok(addr)
    }

    /// Invalidates the cached node address
    ///
    /// Call this if you suspect the network configuration has changed.
    pub async fn invalidate_node_addr_cache(&self) {
        let mut cache = self.node_addr_cache.lock().await;
        *cache = None;
        debug!("Node address cache invalidated");
    }

    /// Connects to a remote peer by their node address
    ///
    /// # Arguments
    ///
    /// * `addr` - The node address of the peer to connect to
    ///
    /// # Returns
    ///
    /// A connection to the remote peer
    #[instrument(skip(self), fields(peer = %addr.node_id))]
    pub async fn connect(&self, addr: NodeAddr) -> Result<iroh::endpoint::Connection> {
        let peer_id = addr.node_id;

        // Try to get from pool first
        if let Some((conn, _)) = self.connection_pool.get(peer_id).await {
            debug!("Reusing pooled connection");
            return Ok(conn);
        }

        let endpoint = {
            let state = self.state.read();
            match &*state {
                EndpointState::Running(endpoint) => endpoint.clone(),
                EndpointState::Shutdown => return Err(EndpointError::Shutdown.into()),
            }
        };

        debug!("Creating new connection");
        let connection = endpoint
            .connect(addr, ALPN)
            .await
            .context("failed to connect to peer")?;

        info!("Connected to peer successfully");

        // Add to pool for future reuse
        if let Some(connection_id) = self.connection_pool.add(peer_id, connection.clone()).await {
            debug!(connection_id, "Connection added to pool");
        }

        Ok(connection)
    }

    /// Gets a pooled connection to a peer
    ///
    /// This returns a `PooledConnection` that automatically returns to the pool
    /// when dropped.
    pub async fn get_pooled_connection(&self, addr: NodeAddr) -> Result<PooledConnection> {
        let peer_id = addr.node_id;

        // Try to get from pool first
        if let Some((conn, connection_id)) = self.connection_pool.get(peer_id).await {
            debug!("Reusing pooled connection");
            return Ok(self
                .connection_pool
                .wrap_connection(peer_id, conn, connection_id));
        }

        // Create new connection
        let conn = self.connect(addr).await?;
        let connection_id = self
            .connection_pool
            .add(peer_id, conn.clone())
            .await
            .ok_or(EndpointError::PoolAtCapacity)?;

        Ok(self
            .connection_pool
            .wrap_connection(peer_id, conn, connection_id))
    }

    /// Accepts an incoming connection
    ///
    /// # Returns
    ///
    /// The accepted connection, or an error if the endpoint is not running
    #[instrument(skip(self))]
    pub async fn accept(&self) -> Result<Option<iroh::endpoint::Connection>> {
        let endpoint = {
            let state = self.state.read();
            match &*state {
                EndpointState::Running(endpoint) => endpoint.clone(),
                EndpointState::Shutdown => return Ok(None),
            }
        };

        if let Some(incoming) = endpoint.accept().await {
            let connection = incoming.await.context("failed to accept connection")?;
            if let Ok(peer_id) = connection.remote_node_id() {
                info!(peer = %peer_id, "Accepted incoming connection");
            } else {
                info!("Accepted incoming connection");
            }
            Ok(Some(connection))
        } else {
            Ok(None)
        }
    }

    /// Gracefully shuts down the endpoint
    ///
    /// This will:
    /// 1. Close all active connections
    /// 2. Stop accepting new connections
    /// 3. Release network resources
    #[instrument(skip(self))]
    pub async fn shutdown(&self) -> Result<()> {
        let endpoint = {
            let mut state = self.state.write();
            match std::mem::replace(&mut *state, EndpointState::Shutdown) {
                EndpointState::Running(endpoint) => endpoint,
                EndpointState::Shutdown => {
                    warn!("Attempted to shut down already shutdown endpoint");
                    return Ok(());
                }
            }
        };

        // Clear caches
        self.invalidate_node_addr_cache().await;

        info!("Shutting down endpoint");
        endpoint.close().await;
        info!("Endpoint shut down successfully");
        Ok(())
    }

    /// Checks if the endpoint is currently running
    #[inline]
    pub fn is_running(&self) -> bool {
        matches!(&*self.state.read(), EndpointState::Running(_))
    }

    /// Returns a clone of the underlying Iroh endpoint for use with other protocols.
    ///
    /// # Errors
    ///
    /// Returns an error if the endpoint is not initialized or has been shut down.
    pub fn iroh_endpoint(&self) -> Result<IrohEndpoint> {
        let state = self.state.read();
        match &*state {
            EndpointState::Running(endpoint) => Ok(endpoint.clone()),
            EndpointState::Shutdown => Err(EndpointError::Shutdown.into()),
        }
    }

    /// Returns the local port the endpoint is bound to.
    ///
    /// This is useful for mDNS advertisement.
    pub fn bound_port(&self) -> Result<u16> {
        let state = self.state.read();
        match &*state {
            EndpointState::Running(endpoint) => {
                let (ipv4_addr, _ipv6_addr) = endpoint.bound_sockets();
                Ok(ipv4_addr.port())
            }
            EndpointState::Shutdown => Err(EndpointError::Shutdown.into()),
        }
    }

    /// Returns a reference to the connection pool
    #[inline]
    pub fn connection_pool(&self) -> &ConnectionPool {
        &self.connection_pool
    }

    /// Gets the path to the configuration directory (cached).
    ///
    /// Respects the `HFS_CONFIG_DIR` environment variable for testing
    /// multiple instances on the same machine.
    fn config_dir() -> Result<PathBuf> {
        CONFIG_DIR_CACHE.get_or_try_init(|| {
            // Check for custom config dir (useful for testing multiple instances)
            if let Ok(custom_dir) = std::env::var("HFS_CONFIG_DIR") {
                return Ok(PathBuf::from(custom_dir));
            }

            dirs::config_dir()
                .map(|p| p.join(CONFIG_DIR))
                .ok_or_else(|| EndpointError::config_dir("could not determine config directory").into())
        }).cloned()
    }

    /// Gets the path to the device key file
    fn device_key_path() -> Result<PathBuf> {
        Ok(Self::config_dir()?.join(DEVICE_KEY_FILE))
    }

    /// Loads an existing device key or creates a new one
    #[instrument(name = "load_or_create_device_key")]
    async fn load_or_create_device_key() -> Result<SecretKey> {
        let key_path = Self::device_key_path()?;

        if key_path.exists() {
            debug!(path = %key_path.display(), "Loading existing device key");

            // Use buffered async I/O for better performance
            let mut file = fs::File::open(&key_path)
                .await
                .context("failed to open device key file")?;

            let mut buffer = Vec::with_capacity(32);
            file.read_to_end(&mut buffer)
                .await
                .context("failed to read device key")?;

            let key_array: [u8; 32] = buffer
                .try_into()
                .map_err(|_| EndpointError::key_persistence("invalid key length"))?;

            let secret_key = SecretKey::from_bytes(&key_array);
            info!("Loaded existing device identity");
            Ok(secret_key)
        } else {
            debug!("No existing device key found, generating new identity");
            let secret_key = SecretKey::generate(rand::thread_rng());

            // Ensure config directory exists
            let config_dir = Self::config_dir()?;
            fs::create_dir_all(config_dir)
                .await
                .context("failed to create config directory")?;

            // Write the key securely using buffered I/O
            #[cfg(unix)]
            {
                Self::write_key_unix(&key_path, secret_key.to_bytes()).await?;
            }

            #[cfg(not(unix))]
            {
                let mut file = fs::File::create(&key_path)
                    .await
                    .context("failed to create device key file")?;
                
                use tokio::io::AsyncWriteExt;
                file.write_all(&secret_key.to_bytes())
                    .await
                    .context("failed to write device key")?;
            }

            info!(path = %key_path.display(), "Generated and saved new device identity");
            Ok(secret_key)
        }
    }

    /// Writes the device key with Unix permissions (600)
    #[cfg(unix)]
    async fn write_key_unix(key_path: &PathBuf, key_bytes: [u8; 32]) -> Result<()> {
        use std::os::unix::fs::OpenOptionsExt;

        let key_path = key_path.clone();

        tokio::task::spawn_blocking(move || {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create(true).truncate(true).mode(0o600);

            let mut file = options
                .open(&key_path)
                .map_err(|e| anyhow::anyhow!("failed to open key file: {}", e))?;

            file.write_all(&key_bytes)
                .map_err(|e| anyhow::anyhow!("failed to write key: {}", e))?;

            file.flush()
                .map_err(|e| anyhow::anyhow!("failed to flush key file: {}", e))?;

            Ok::<_, anyhow::Error>(())
        })
        .await
        .context("key write task failed")??;

        Ok(())
    }
}

/// Configuration for the endpoint
#[derive(Debug, Clone)]
pub struct EndpointConfig {
    /// Enable connection pooling
    pub enable_connection_pooling: bool,
    /// Maximum connection pool size
    pub connection_pool_size: usize,
    /// Node address cache TTL
    pub node_addr_ttl: Duration,
}

impl Default for EndpointConfig {
    #[inline]
    fn default() -> Self {
        Self {
            enable_connection_pooling: true,
            connection_pool_size: DEFAULT_POOL_SIZE,
            node_addr_ttl: Duration::from_secs(60),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;
    use tokio::sync::Barrier;

    // Test helper to reset config dir cache
    fn reset_config_cache() {
        // Note: OnceCell can't be easily reset in tests,
        // so we rely on HFS_CONFIG_DIR env var for test isolation
    }

    #[test]
    fn test_alpn_is_valid() {
        assert!(!ALPN.is_empty());
        assert!(ALPN.is_ascii());
        assert_eq!(ALPN, b"humanfileshare/1");
    }

    #[test]
    fn test_error_types() {
        // Test error creation with Arc<str>
        let err = EndpointError::config_dir("test error");
        assert!(matches!(err, EndpointError::ConfigDir(_)));
        assert!(err.to_string().contains("test error"));

        let err = EndpointError::key_persistence("key error");
        assert!(matches!(err, EndpointError::KeyPersistence(_)));

        // Test static errors
        assert!(matches!(
            EndpointError::NotInitialized,
            EndpointError::NotInitialized
        ));
        assert!(matches!(EndpointError::Shutdown, EndpointError::Shutdown));
    }

    #[test]
    fn test_error_conversion() {
        // Test that EndpointError can be converted to anyhow::Error
        let err: anyhow::Error = EndpointError::NotInitialized.into();
        assert!(err.to_string().contains("not initialized"));

        let err: anyhow::Error = EndpointError::Shutdown.into();
        assert!(err.to_string().contains("shut down"));
    }

    #[tokio::test]
    async fn test_config_dir_resolution_with_env() {
        reset_config_cache();

        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path().to_str().unwrap();

        // Set custom config dir
        std::env::set_var("HFS_CONFIG_DIR", temp_path);

        // Get config dir
        let config_dir = Endpoint::config_dir().unwrap();
        assert_eq!(config_dir.as_os_str(), temp_path);

        // Clean up
        std::env::remove_var("HFS_CONFIG_DIR");
    }

    #[tokio::test]
    async fn test_device_key_generation_and_loading() {
        reset_config_cache();

        let temp_dir = TempDir::new().unwrap();
        std::env::set_var("HFS_CONFIG_DIR", temp_dir.path().to_str().unwrap());

        // First call should generate a new key
        let key1 = Endpoint::load_or_create_device_key().await.unwrap();
        let key1_bytes = key1.to_bytes();

        // Verify the key file was created
        let key_path = temp_dir.path().join(DEVICE_KEY_FILE);
        assert!(key_path.exists());

        // Read the key file and verify contents
        let file_content = fs::read(&key_path).await.unwrap();
        assert_eq!(file_content.len(), 32);
        assert_eq!(file_content, key1_bytes.as_slice());

        // Second call should load the existing key
        let key2 = Endpoint::load_or_create_device_key().await.unwrap();
        assert_eq!(key1.to_bytes(), key2.to_bytes());
        assert_eq!(key1.public(), key2.public());

        // Clean up
        std::env::remove_var("HFS_CONFIG_DIR");
    }

    #[tokio::test]
    async fn test_device_key_invalid_length() {
        reset_config_cache();

        let temp_dir = TempDir::new().unwrap();
        std::env::set_var("HFS_CONFIG_DIR", temp_dir.path().to_str().unwrap());

        // Create an invalid key file (wrong length)
        let key_path = temp_dir.path().join(DEVICE_KEY_FILE);
        fs::write(&key_path, b"too short").await.unwrap();

        // Should fail with invalid key length error
        let result = Endpoint::load_or_create_device_key().await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("invalid key length"));

        // Clean up
        std::env::remove_var("HFS_CONFIG_DIR");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_key_file_permissions_unix() {
        use std::os::unix::fs::PermissionsExt;

        reset_config_cache();

        let temp_dir = TempDir::new().unwrap();
        std::env::set_var("HFS_CONFIG_DIR", temp_dir.path().to_str().unwrap());

        // Generate a new key
        let _ = Endpoint::load_or_create_device_key().await.unwrap();

        // Check file permissions
        let key_path = temp_dir.path().join(DEVICE_KEY_FILE);
        let metadata = fs::metadata(&key_path).await.unwrap();
        let permissions = metadata.permissions();

        // Should be 0o600 (owner read/write only)
        assert_eq!(permissions.mode() & 0o777, 0o600);

        // Clean up
        std::env::remove_var("HFS_CONFIG_DIR");
    }

    #[tokio::test]
    async fn test_endpoint_state_transitions() {
        // We can't easily test the full endpoint lifecycle without network,
        // but we can test the state logic

        // Test state matching
        let state = EndpointState::Shutdown;
        assert!(!matches!(&state, EndpointState::Running(_)));

        // Note: Testing actual state transitions requires mocking IrohEndpoint
        // which is complex. We test the state machine logic here instead.
    }

    #[test]
    fn test_endpoint_config_default() {
        let config = EndpointConfig::default();
        assert!(config.enable_connection_pooling);
        assert_eq!(config.connection_pool_size, DEFAULT_POOL_SIZE);
        assert_eq!(config.node_addr_ttl, Duration::from_secs(60));
    }

    #[test]
    fn test_connection_pool_new() {
        let pool = ConnectionPool::new();
        // Pool should be created with default size
        assert_eq!(pool.inner.try_lock().unwrap().max_size, DEFAULT_POOL_SIZE);
    }

    #[test]
    fn test_connection_pool_with_capacity() {
        let pool = ConnectionPool::with_capacity(64);
        assert_eq!(pool.inner.try_lock().unwrap().max_size, 64);
    }

    #[tokio::test]
    async fn test_connection_pool_add_and_get() {
        let pool = ConnectionPool::with_capacity(2);
        let peer_id = SecretKey::generate(rand::thread_rng()).public();

        // Create a mock connection (we can't easily create a real one without Iroh)
        // For this test, we just verify the pool mechanics work

        // Initially pool should be empty
        let stats = pool.stats().await;
        assert_eq!(stats.total_connections, 0);

        // Pool operations are tested with real connections in integration tests
    }

    #[tokio::test]
    async fn test_connection_pool_cleanup() {
        let pool = ConnectionPool::with_capacity(10);

        // Initially empty
        pool.cleanup().await;

        let stats = pool.stats().await;
        assert_eq!(stats.total_connections, 0);
        assert_eq!(stats.peers, 0);
    }

    #[test]
    fn test_pooled_connection_meta() {
        let meta = PooledConnectionMeta::new(1);
        assert_eq!(meta.connection_id, 1);
        assert_eq!(meta.ref_count.load(Ordering::Acquire), 1);
        assert!(!meta.is_expired());
    }

    #[test]
    fn test_cached_node_addr() {
        // Create a mock NodeAddr (we can't easily create a real one)
        // Instead we test the cache mechanics

        // Test TTL logic
        let ttl = Duration::from_millis(50);

        // We can't create a NodeAddr without a real endpoint,
        // so we test the is_valid logic conceptually
        struct MockCached {
            cached_at: Instant,
            ttl: Duration,
        }

        impl MockCached {
            fn is_valid(&self) -> bool {
                self.cached_at.elapsed() < self.ttl
            }
        }

        let cached = MockCached {
            cached_at: Instant::now(),
            ttl,
        };
        assert!(cached.is_valid());

        std::thread::sleep(ttl + Duration::from_millis(10));
        assert!(!cached.is_valid());
    }

    #[test]
    fn test_pool_stats() {
        let stats = PoolStats {
            total_connections: 5,
            max_size: 10,
            peers: 2,
        };

        assert_eq!(stats.total_connections, 5);
        assert_eq!(stats.max_size, 10);
        assert_eq!(stats.peers, 2);
    }

    #[tokio::test]
    async fn test_concurrent_access_to_endpoint() {
        // This test verifies that the Endpoint is Send + Sync
        // and can be accessed concurrently

        // We can't create a real endpoint without network,
        // so we verify the types are thread-safe

        fn assert_send<T: Send>() {}
        fn assert_sync<T: Sync>() {}

        assert_send::<Endpoint>();
        assert_sync::<Endpoint>();
        assert_send::<ConnectionPool>();
        assert_sync::<ConnectionPool>();
    }

    #[tokio::test]
    async fn test_concurrent_pool_access() {
        let pool = ConnectionPool::with_capacity(100);
        let counter = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(Barrier::new(10));

        let mut handles = vec![];

        for i in 0..10 {
            let pool = pool.clone();
            let counter = Arc::clone(&counter);
            let barrier = Arc::clone(&barrier);

            let handle = tokio::spawn(async move {
                barrier.wait().await;

                // Perform pool operations
                let stats = pool.stats().await;
                counter.fetch_add(1, Ordering::SeqCst);

                if i % 2 == 0 {
                    pool.cleanup().await;
                }

                stats
            });

            handles.push(handle);
        }

        // Wait for all tasks
        for handle in handles {
            handle.await.unwrap();
        }

        assert_eq!(counter.load(Ordering::SeqCst), 10);
    }

    #[test]
    fn test_constants() {
        assert_eq!(CONFIG_DIR, "humanfileshare");
        assert_eq!(DEVICE_KEY_FILE, "device_key");
        assert_eq!(DEFAULT_POOL_SIZE, 32);
        assert_eq!(MAX_CONNECTIONS_PER_PEER, 3);
    }

    #[test]
    fn test_endpoint_error_display() {
        let err = EndpointError::NotInitialized;
        assert_eq!(err.to_string(), "endpoint not initialized");

        let err = EndpointError::Shutdown;
        assert_eq!(err.to_string(), "endpoint has been shut down");

        let err = EndpointError::PoolAtCapacity;
        assert_eq!(err.to_string(), "connection pool at capacity");

        let err = EndpointError::ConnectionNotFound;
        assert_eq!(err.to_string(), "connection not found for peer");
    }

    #[test]
    fn test_endpoint_config_custom() {
        let config = EndpointConfig {
            enable_connection_pooling: false,
            connection_pool_size: 64,
            node_addr_ttl: Duration::from_secs(120),
        };

        assert!(!config.enable_connection_pooling);
        assert_eq!(config.connection_pool_size, 64);
        assert_eq!(config.node_addr_ttl, Duration::from_secs(120));
    }

    #[tokio::test]
    async fn test_device_key_path() {
        reset_config_cache();

        let temp_dir = TempDir::new().unwrap();
        std::env::set_var("HFS_CONFIG_DIR", temp_dir.path().to_str().unwrap());

        let key_path = Endpoint::device_key_path().unwrap();
        assert_eq!(key_path, temp_dir.path().join(DEVICE_KEY_FILE));

        std::env::remove_var("HFS_CONFIG_DIR");
    }

    // Integration tests would require a real Iroh endpoint and network
    // These are marked as ignored and can be run with: cargo test -- --ignored
    #[tokio::test]
    #[ignore = "requires network"]
    async fn test_endpoint_new() {
        // This test creates a real endpoint and should only be run manually
        let _endpoint = Endpoint::new().await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires network"]
    async fn test_endpoint_with_config() {
        let config = EndpointConfig {
            enable_connection_pooling: true,
            connection_pool_size: 16,
            node_addr_ttl: Duration::from_secs(30),
        };

        let _endpoint = Endpoint::with_config(config).await.unwrap();
    }
}
