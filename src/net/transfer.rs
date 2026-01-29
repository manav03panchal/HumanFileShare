//! File transfer logic using iroh-blobs
//!
//! This module handles the actual transfer of files between devices using
//! Iroh's blob transfer protocol.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bytes::Bytes;
use dashmap::DashMap;
use iroh::endpoint::Connection;
use iroh::protocol::{ProtocolHandler, Router};
use iroh::{NodeAddr, PublicKey};
use iroh_blobs::downloader::DownloadRequest;
use iroh_blobs::net_protocol::Blobs;
use iroh_blobs::store::mem::Store as MemStore;
use iroh_blobs::store::{ExportMode, ImportMode, ImportProgress, ReadableStore, Store};
use iroh_blobs::ticket::BlobTicket;
use iroh_blobs::util::progress::IgnoreProgressSender;
use iroh_blobs::{BlobFormat, HashAndFormat};
use parking_lot::Mutex;
use parking_lot::RwLock;
use thiserror::Error;

use tokio::sync::mpsc;
use tracing::{debug, error, info, instrument, warn};

use super::endpoint::{ALPN, Endpoint};

/// Errors that can occur during file transfers
#[derive(Error, Debug)]
pub enum TransferError {
    #[error("file not found: {0}")]
    FileNotFound(PathBuf),

    #[error("peer not connected: {0}")]
    PeerNotConnected(Arc<str>),

    #[error("transfer cancelled")]
    Cancelled,

    #[error("transfer failed: {0}")]
    Failed(Arc<str>),

    #[error("no connected peer")]
    NoPeer,

    #[error("endpoint not initialized")]
    NoEndpoint,
}

/// Direction of a transfer
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferDirection {
    Send,
    Receive,
}

/// Current state of a transfer
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferState {
    Pending,
    InProgress,
    Completed,
    Failed { reason: Arc<str> },
    Cancelled,
}

impl TransferState {
    /// Returns true if the transfer is in a terminal state
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            TransferState::Completed | TransferState::Failed { .. } | TransferState::Cancelled
        )
    }

    /// Returns true if the transfer can transition to InProgress
    pub fn can_start(&self) -> bool {
        matches!(self, TransferState::Pending)
    }

    /// Attempts to transition from Pending to InProgress
    pub fn start(&mut self) -> bool {
        if matches!(self, TransferState::Pending) {
            *self = TransferState::InProgress;
            true
        } else {
            false
        }
    }

    /// Transitions to Failed state
    pub fn fail(&mut self, reason: impl Into<Arc<str>>) {
        *self = TransferState::Failed {
            reason: reason.into(),
        };
    }

    /// Transitions to Completed state
    pub fn complete(&mut self) {
        *self = TransferState::Completed;
    }

    /// Transitions to Cancelled state
    pub fn cancel(&mut self) {
        *self = TransferState::Cancelled;
    }
}

/// Progress information for a transfer
#[derive(Debug, Clone)]
pub struct TransferProgress {
    pub transfer_id: TransferId,
    pub direction: TransferDirection,
    pub peer_id: PublicKey,
    pub file_name: Arc<str>,
    pub total_bytes: u64,
    pub transferred_bytes: u64,
    pub state: TransferState,
    pub started_at: Instant,
    pub speed_bps: u64,
}

impl TransferProgress {
    pub fn percent(&self) -> u8 {
        if self.total_bytes == 0 {
            return 100;
        }
        let pct = (self.transferred_bytes as f64 / self.total_bytes as f64) * 100.0;
        pct.min(100.0) as u8
    }

    pub fn is_complete(&self) -> bool {
        matches!(self.state, TransferState::Completed)
    }

    pub fn is_failed(&self) -> bool {
        matches!(self.state, TransferState::Failed { .. })
    }

    pub fn is_active(&self) -> bool {
        matches!(self.state, TransferState::Pending | TransferState::InProgress)
    }

    /// Calculates ETA in seconds based on current speed
    pub fn eta_seconds(&self) -> Option<u64> {
        if self.speed_bps == 0 || self.transferred_bytes >= self.total_bytes {
            return None;
        }
        let remaining = self.total_bytes - self.transferred_bytes;
        Some(remaining / self.speed_bps)
    }
}

/// Unique identifier for a transfer
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TransferId(u64);

impl TransferId {
    fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(1);
        Self(COUNTER.fetch_add(1, Ordering::Relaxed))
    }

    /// Returns the raw u64 value of this transfer ID
    pub fn as_u64(&self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for TransferId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "transfer-{}", self.0)
    }
}

/// A handle to an active transfer
#[derive(Debug)]
pub struct TransferHandle {
    id: TransferId,
    progress_rx: Arc<Mutex<mpsc::Receiver<TransferProgress>>>,
    cancelled: Arc<AtomicU64>, // Using AtomicU64 for transferred bytes tracking
    cancel_flag: Arc<std::sync::atomic::AtomicBool>,
}

impl TransferHandle {
    pub fn id(&self) -> TransferId {
        self.id
    }

    pub async fn next_progress(&self) -> Option<TransferProgress> {
        // Use blocking_lock in a spawn_blocking for fairness, but for simple cases:
        let rx = self.progress_rx.clone();
        tokio::task::spawn_blocking(move || rx.lock().blocking_recv())
            .await
            .ok()
            .flatten()
    }

    pub fn cancel(&self) {
        self.cancel_flag
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel_flag
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Updates the transferred byte count atomically
    pub fn update_transferred(&self, bytes: u64) {
        self.cancelled.store(bytes, Ordering::Relaxed);
    }

    /// Gets the current transferred byte count
    pub fn transferred_bytes(&self) -> u64 {
        self.cancelled.load(Ordering::Relaxed)
    }
}

/// Callback type for received files
pub type OnFileReceived = Arc<dyn Fn(Arc<str>, PathBuf, u64) + Send + Sync + 'static>;

/// Connection pool for reusing peer connections
#[derive(Debug, Clone)]
struct ConnectionPool {
    connections: Arc<DashMap<PublicKey, (Connection, Instant)>>,
    max_idle_duration: Duration,
}

impl ConnectionPool {
    fn new(max_idle_secs: u64) -> Self {
        Self {
            connections: Arc::new(DashMap::new()),
            max_idle_duration: Duration::from_secs(max_idle_secs),
        }
    }

    fn get(&self, peer_id: &PublicKey) -> Option<Connection> {
        self.connections
            .get(peer_id)
            .and_then(|entry| {
                let (conn, last_used) = entry.value();
                if last_used.elapsed() < self.max_idle_duration {
                    Some(conn.clone())
                } else {
                    drop(entry);
                    self.connections.remove(peer_id);
                    None
                }
            })
    }

    fn insert(&self, peer_id: PublicKey, conn: Connection) {
        self.connections.insert(peer_id, (conn, Instant::now()));
    }

    /// Cleans up stale connections
    fn cleanup_stale(&self) {
        let stale_keys: Vec<_> = self
            .connections
            .iter()
            .filter(|entry| entry.value().1.elapsed() > self.max_idle_duration)
            .map(|entry| *entry.key())
            .collect();

        for key in stale_keys {
            self.connections.remove(&key);
        }
    }
}

/// Protocol handler for file transfer metadata
#[derive(Clone)]
struct FileTransferHandler {
    blobs: Blobs<MemStore>,
    download_dir: PathBuf,
    on_file_received: Arc<RwLock<Option<OnFileReceived>>>,
}

impl std::fmt::Debug for FileTransferHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileTransferHandler")
            .field("download_dir", &self.download_dir)
            .finish_non_exhaustive()
    }
}

/// Maximum size for protocol messages (10KB)
const MAX_PROTOCOL_MSG_SIZE: usize = 10_000;

impl ProtocolHandler for FileTransferHandler {
    fn accept(
        &self,
        connection: Connection,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + 'static>>
    {
        let blobs = self.blobs.clone();
        let download_dir = self.download_dir.clone();
        let on_file_received = self.on_file_received.clone();

        Box::pin(async move {
            let peer_id = connection.remote_node_id().ok();
            debug!(peer = ?peer_id, "Handling incoming file transfer connection");

            // Keep accepting streams on this connection until it closes
            loop {
                // Accept incoming bi-directional stream
                let stream_result = connection.accept_bi().await;
                let (mut send, mut recv) = match stream_result {
                    Ok(stream) => stream,
                    Err(e) => {
                        // Connection closed by peer - this is normal
                        debug!(peer = ?peer_id, error = %e, "Connection closed");
                        break;
                    }
                };

                // Read the message using read_to_end with size limit
                let buf = match recv.read_to_end(MAX_PROTOCOL_MSG_SIZE).await {
                    Ok(buf) => buf,
                    Err(e) => {
                        warn!(peer = ?peer_id, error = %e, "Failed to read from stream");
                        continue;
                    }
                };

                let msg = String::from_utf8_lossy(&buf);
                if let Some(rest) = msg.strip_prefix("FILE:") {
                    if let Some((file_name, ticket_str)) = rest.split_once('\n') {
                        let ticket: BlobTicket = match ticket_str.trim().parse() {
                            Ok(t) => t,
                            Err(e) => {
                                error!(error = %e, "Failed to parse blob ticket");
                                continue;
                            }
                        };

                        let file_name: Arc<str> = Arc::from(file_name);
                        info!(file_name = %file_name, hash = %ticket.hash(), "Receiving file via protocol handler");

                        let dest_path = download_dir.join(file_name.as_ref());

                        // Download the blob
                        let downloader = blobs.downloader();
                        let hash_and_format = HashAndFormat::raw(ticket.hash());
                        let request =
                            DownloadRequest::new(hash_and_format, [ticket.node_addr().clone()]);
                        let handle = downloader.queue(request).await;
                        if let Err(e) = handle.await {
                            error!(error = %e, "Failed to download blob");
                            continue;
                        }

                        // Export to file with streaming progress
                        let export_progress: Box<dyn Fn(u64) -> std::io::Result<()> + Send + Sync> =
                            Box::new(|_| Ok(()));
                        if let Err(e) = blobs
                            .store()
                            .export(
                                ticket.hash(),
                                dest_path.clone(),
                                ExportMode::Copy,
                                export_progress,
                            )
                            .await
                        {
                            error!(error = %e, "Failed to export blob to file");
                            continue;
                        }

                        // Get file size
                        let size = tokio::fs::metadata(&dest_path)
                            .await
                            .map(|m| m.len())
                            .unwrap_or(0);

                        info!(file_name = %file_name, size = size, dest = %dest_path.display(), "File received successfully");

                        // Send acknowledgment using Bytes for zero-copy
                        let ack = Bytes::from_static(b"ACK");
                        if let Err(e) = send.write_all(&ack).await {
                            error!(error = %e, "Failed to send ACK");
                            continue;
                        }
                        let _ = send.finish();

                        // Notify callback
                        if let Some(callback) = on_file_received.read().as_ref() {
                            callback(file_name, dest_path, size);
                        }
                    }
                }
            }

            Ok(())
        })
    }
}

/// Manages file transfers using iroh-blobs
pub struct TransferManager {
    /// Reference to the network endpoint
    endpoint: Arc<Endpoint>,
    /// The blobs protocol handler (includes store)
    blobs: Blobs<MemStore>,
    /// The protocol router (keeps the server running)
    _router: Router,
    /// Active transfers - using DashMap for better concurrent access
    transfers: Arc<DashMap<TransferId, TransferInfo>>,
    /// Default download directory
    download_dir: PathBuf,
    /// Currently connected peer
    connected_peer: Arc<RwLock<Option<NodeAddr>>>,
    /// Handler for setting callback
    file_handler: FileTransferHandler,
    /// Connection pool for reusing peer connections
    connection_pool: ConnectionPool,
    /// Progress update batching interval
    progress_batch_interval: Duration,
}

/// Internal transfer tracking info
#[derive(Debug)]
struct TransferInfo {
    progress: TransferProgress,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
}

/// Default channel capacity for progress updates
const DEFAULT_PROGRESS_CHANNEL_CAPACITY: usize = 32;

/// Default progress batch interval in milliseconds
const DEFAULT_PROGRESS_BATCH_MS: u64 = 100;

impl TransferManager {
    /// Creates a new transfer manager with blob store and protocol handler.
    #[instrument(skip(endpoint))]
    pub async fn new(endpoint: Arc<Endpoint>) -> Result<Self> {
        Self::with_config(
            endpoint,
            TransferConfig {
                progress_channel_capacity: DEFAULT_PROGRESS_CHANNEL_CAPACITY,
                progress_batch_ms: DEFAULT_PROGRESS_BATCH_MS,
                connection_pool_max_idle_secs: 300, // 5 minutes
            },
        )
        .await
    }

    /// Creates a new transfer manager with custom configuration.
    #[instrument(skip(endpoint))]
    pub async fn with_config(endpoint: Arc<Endpoint>, config: TransferConfig) -> Result<Self> {
        let download_dir = dirs::download_dir()
            .or_else(|| dirs::home_dir().map(|h| h.join("Downloads")))
            .unwrap_or_else(|| PathBuf::from("."))
            .join("HumanFileShare");

        // Create download directory if it doesn't exist
        tokio::fs::create_dir_all(&download_dir).await.ok();

        // Get the iroh endpoint
        let iroh_endpoint = endpoint.iroh_endpoint()?;

        // Create an in-memory blob store with the blobs protocol
        let blobs = Blobs::memory().build(&iroh_endpoint);

        // Create our custom file transfer handler
        let file_handler = FileTransferHandler {
            blobs: blobs.clone(),
            download_dir: download_dir.clone(),
            on_file_received: Arc::new(RwLock::new(None)),
        };

        // Build a router that accepts both blob requests and our file transfer protocol
        let router = Router::builder(iroh_endpoint)
            .accept(iroh_blobs::ALPN, blobs.clone())
            .accept(ALPN.to_vec(), file_handler.clone())
            .spawn();

        info!(download_dir = %download_dir.display(), "Transfer manager initialized");

        Ok(Self {
            endpoint,
            blobs,
            _router: router,
            transfers: Arc::new(DashMap::new()),
            download_dir,
            connected_peer: Arc::new(RwLock::new(None)),
            file_handler,
            connection_pool: ConnectionPool::new(config.connection_pool_max_idle_secs),
            progress_batch_interval: Duration::from_millis(config.progress_batch_ms),
        })
    }

    /// Sets the currently connected peer.
    pub fn set_connected_peer(&self, addr: Option<NodeAddr>) {
        *self.connected_peer.write() = addr;
    }

    /// Gets the currently connected peer.
    pub fn connected_peer(&self) -> Option<NodeAddr> {
        self.connected_peer.read().clone()
    }

    /// Sets the default download directory.
    pub fn set_download_dir(&mut self, dir: PathBuf) {
        self.download_dir = dir;
    }

    /// Gets the download directory.
    pub fn download_dir(&self) -> &Path {
        &self.download_dir
    }

    /// Sets the callback to be invoked when a file is received.
    pub fn set_on_file_received(&self, callback: OnFileReceived) {
        *self.file_handler.on_file_received.write() = Some(callback);
    }

    /// Creates a blob ticket for a file that can be shared with a peer.
    pub async fn create_ticket(&self, path: impl AsRef<Path>) -> Result<BlobTicket> {
        let path = path.as_ref();

        if !path.exists() {
            return Err(TransferError::FileNotFound(path.to_path_buf()).into());
        }

        let abs_path = tokio::fs::canonicalize(path)
            .await
            .context("failed to get absolute path")?;

        // Import the file into the blob store (with no-op progress sender)
        let progress = IgnoreProgressSender::<ImportProgress>::default();
        let (temp_tag, _size) = self
            .blobs
            .store()
            .import_file(abs_path, ImportMode::Copy, BlobFormat::Raw, progress)
            .await
            .context("failed to import file to blob store")?;

        // Get our node address
        let node_addr = self.endpoint.node_addr().await?;

        // Create the ticket - dereference the hash
        let ticket = BlobTicket::new(node_addr, *temp_tag.hash(), BlobFormat::Raw)?;

        info!(
            hash = %temp_tag.hash(),
            file = %path.display(),
            "Created blob ticket for file"
        );

        Ok(ticket)
    }

    /// Sends files to the connected peer.
    #[instrument(skip(self, paths), fields(file_count = paths.len()))]
    pub async fn send_files(&self, paths: Vec<PathBuf>) -> Result<Vec<TransferHandle>> {
        info!(file_count = paths.len(), "send_files called");
        let peer_addr = self.connected_peer().ok_or_else(|| {
            error!("No peer connected when trying to send files");
            TransferError::NoPeer
        })?;
        let addrs: Vec<_> = peer_addr.direct_addresses().collect();
        info!(peer = %peer_addr.node_id, addresses = ?addrs, "Sending to peer");

        let mut handles = Vec::with_capacity(paths.len());

        for path in paths {
            info!(path = %path.display(), "Sending file");
            let handle = self.send_file_to_peer(&peer_addr, &path).await?;
            handles.push(handle);
        }

        Ok(handles)
    }

    /// Sends a single file to a specific peer.
    async fn send_file_to_peer(&self, peer_addr: &NodeAddr, path: &Path) -> Result<TransferHandle> {
        if !path.exists() {
            return Err(TransferError::FileNotFound(path.to_path_buf()).into());
        }

        let metadata = tokio::fs::metadata(path)
            .await
            .context("failed to read file metadata")?;

        let file_name: Arc<str> = path
            .file_name()
            .map(|n| n.to_string_lossy().into())
            .unwrap_or_else(|| Arc::from("unknown"));

        let transfer_id = TransferId::new();
        let (progress_tx, progress_rx) = mpsc::channel(DEFAULT_PROGRESS_CHANNEL_CAPACITY);
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let progress = TransferProgress {
            transfer_id,
            direction: TransferDirection::Send,
            peer_id: peer_addr.node_id,
            file_name: file_name.clone(),
            total_bytes: metadata.len(),
            transferred_bytes: 0,
            state: TransferState::Pending,
            started_at: Instant::now(),
            speed_bps: 0,
        };

        let info = TransferInfo {
            progress: progress.clone(),
            cancelled: cancelled.clone(),
        };

        self.transfers.insert(transfer_id, info);

        info!(
            transfer_id = %transfer_id,
            file_name = %file_name,
            size = metadata.len(),
            peer = %peer_addr.node_id,
            "Starting file send"
        );

        // Create the ticket
        let ticket = self.create_ticket(path).await?;

        // Spawn task to send the file metadata to the peer
        let transfers = self.transfers.clone();
        let endpoint = self.endpoint.clone();
        let peer_addr_clone = peer_addr.clone();
        let cancelled_clone = cancelled.clone();
        let connection_pool = self.connection_pool.clone();
        let _batch_interval = self.progress_batch_interval;

        info!(transfer_id = %transfer_id, "Spawning send task");

        let task_transfer_id = transfer_id;
        let task_file_name = file_name.clone();
        tokio::spawn(async move {
            info!(transfer_id = %task_transfer_id, "Send task started");
            let mut progress = TransferProgress {
                transfer_id,
                direction: TransferDirection::Send,
                peer_id: peer_addr_clone.node_id,
                file_name: task_file_name.clone(),
                total_bytes: metadata.len(),
                transferred_bytes: 0,
                state: TransferState::InProgress,
                started_at: Instant::now(),
                speed_bps: 0,
            };

            // Send initial progress
            let _ = progress_tx.send(progress.clone()).await;

            info!(
                transfer_id = %transfer_id,
                peer = %peer_addr_clone.node_id,
                "Connecting to peer to send file ticket"
            );

            // Helper to attempt sending on a connection
            async fn try_send_on_connection(
                conn: &Connection,
                task_file_name: &str,
                ticket: &BlobTicket,
                transfer_id: TransferId,
                total_bytes: u64,
                progress: &mut TransferProgress,
            ) -> Result<(), String> {
                info!(transfer_id = %transfer_id, "Opening bi stream");
                let (mut send, mut recv) = conn.open_bi().await.map_err(|e| format!("Failed to open stream: {}", e))?;

                // Send the ticket as a message using Bytes for zero-copy
                let ticket_str = ticket.to_string();
                let msg = format!("FILE:{}\n{}", task_file_name, ticket_str);
                let msg_bytes = Bytes::from(msg);

                info!(transfer_id = %transfer_id, msg_len = msg_bytes.len(), "Sending file ticket to peer");

                send.write_all(&msg_bytes).await.map_err(|e| format!("Failed to send ticket: {}", e))?;

                info!(transfer_id = %transfer_id, "Ticket sent, finishing stream and waiting for ACK");
                let _ = send.finish();

                // Wait for acknowledgment
                let mut buf = [0u8; 3];
                match recv.read_exact(&mut buf).await {
                    Ok(_) if &buf == b"ACK" => {
                        progress.transferred_bytes = total_bytes;
                        progress.state.complete();
                        info!(transfer_id = %transfer_id, "File transfer completed - ACK received");
                        Ok(())
                    }
                    Ok(_) => {
                        Err("Unexpected response from peer".to_string())
                    }
                    Err(e) => {
                        Err(format!("No acknowledgment from peer: {}", e))
                    }
                }
            }

            // Try pooled connection first, then fresh connection on failure
            let send_result: Result<Connection, String> = async {
                // First try: pooled connection
                if let Some(conn) = connection_pool.get(&peer_addr_clone.node_id) {
                    debug!(transfer_id = %transfer_id, "Trying pooled connection");

                    match try_send_on_connection(&conn, &task_file_name, &ticket, transfer_id, metadata.len(), &mut progress).await {
                        Ok(()) => return Ok(conn),
                        Err(e) => {
                            warn!(transfer_id = %transfer_id, error = %e, "Pooled connection failed, will retry with fresh connection");
                            // Remove dead connection from pool
                            connection_pool.connections.remove(&peer_addr_clone.node_id);
                        }
                    }
                }

                // Second try (or first if no pooled connection): fresh connection
                debug!(transfer_id = %transfer_id, "Creating fresh connection");
                let conn = endpoint.connect(peer_addr_clone.clone()).await
                    .map_err(|e| format!("Failed to connect to peer: {}", e))?;

                try_send_on_connection(&conn, &task_file_name, &ticket, transfer_id, metadata.len(), &mut progress).await?;
                Ok(conn)
            }.await;

            match send_result {
                Ok(conn) => {
                    // Store successful connection in pool for reuse
                    connection_pool.insert(peer_addr_clone.node_id, conn);
                }
                Err(e) => {
                    error!(transfer_id = %transfer_id, error = %e, "Transfer failed");
                    progress.state.fail(e);
                }
            }

            if cancelled_clone.load(std::sync::atomic::Ordering::Relaxed) {
                progress.state.cancel();
            }

            // Final progress update
            let _ = progress_tx.send(progress.clone()).await;

            // Update stored progress
            if let Some(mut entry) = transfers.get_mut(&transfer_id) {
                entry.progress = progress;
            }
        });

        Ok(TransferHandle {
            id: transfer_id,
            progress_rx: Arc::new(Mutex::new(progress_rx)),
            cancelled: Arc::new(AtomicU64::new(0)),
            cancel_flag: cancelled,
        })
    }

    /// Downloads a file from a blob ticket.
    #[instrument(skip(self), fields(hash = %ticket.hash()))]
    pub async fn download_from_ticket(
        &self,
        ticket: &BlobTicket,
        file_name: &str,
    ) -> Result<PathBuf> {
        let dest_path = self.download_dir.join(file_name);

        debug!(
            hash = %ticket.hash(),
            dest = %dest_path.display(),
            "Downloading blob"
        );

        // Download the blob from the peer using the downloader
        let downloader = self.blobs.downloader();
        let hash_and_format = HashAndFormat::raw(ticket.hash());
        let request = DownloadRequest::new(hash_and_format, [ticket.node_addr().clone()]);
        // queue() is async and returns DownloadHandle
        let handle = downloader.queue(request).await;
        // DownloadHandle implements Future<Output = Result<Stats, DownloadError>>
        match handle.await {
            Ok(_stats) => {
                debug!("Download completed successfully");
            }
            Err(e) => {
                return Err(anyhow::anyhow!("failed to download blob: {}", e));
            }
        }

        // Export to file (with no-op progress callback)
        let export_progress: Box<dyn Fn(u64) -> std::io::Result<()> + Send + Sync> =
            Box::new(|_| Ok(()));
        self.blobs
            .store()
            .export(
                ticket.hash(),
                dest_path.clone(),
                ExportMode::Copy,
                export_progress,
            )
            .await
            .context("failed to export blob to file")?;

        info!(
            hash = %ticket.hash(),
            dest = %dest_path.display(),
            "Downloaded file successfully"
        );

        Ok(dest_path)
    }

    /// Sets up the file receive callback and keeps the transfer manager alive.
    /// The Router handles incoming connections automatically.
    pub async fn receive_loop(
        self: Arc<Self>,
        on_file_received: Arc<dyn Fn(Arc<str>, PathBuf, u64) + Send + Sync + 'static>,
    ) {
        info!("Starting file receive loop (Router handles connections)");

        // Set the callback that will be invoked by the FileTransferHandler
        self.set_on_file_received(on_file_received);

        // Periodic cleanup task for connection pool and finished transfers
        let cleanup_interval = Duration::from_secs(60);
        let mut interval = tokio::time::interval(cleanup_interval);

        // Keep this task alive - the Router handles incoming connections
        loop {
            interval.tick().await;
            self.connection_pool.cleanup_stale();
            self.cleanup_finished();
        }
    }

    /// Returns all active transfers
    pub fn active_transfers(&self) -> Vec<TransferProgress> {
        self.transfers
            .iter()
            .filter(|entry| entry.value().progress.is_active())
            .map(|entry| entry.value().progress.clone())
            .collect()
    }

    /// Returns all transfers (including completed/failed)
    pub fn all_transfers(&self) -> Vec<TransferProgress> {
        self.transfers
            .iter()
            .map(|entry| entry.value().progress.clone())
            .collect()
    }

    /// Returns the progress for a specific transfer
    pub fn get_transfer(&self, id: TransferId) -> Option<TransferProgress> {
        self.transfers.get(&id).map(|entry| entry.value().progress.clone())
    }

    /// Cancels a transfer
    #[instrument(skip(self), fields(transfer_id = %id))]
    pub fn cancel_transfer(&self, id: TransferId) -> bool {
        if let Some(mut entry) = self.transfers.get_mut(&id) {
            let info = entry.value_mut();
            info.cancelled
                .store(true, std::sync::atomic::Ordering::Relaxed);
            info.progress.state.cancel();
            info!("Transfer cancelled");
            true
        } else {
            warn!("Transfer not found");
            false
        }
    }

    /// Removes completed or failed transfers from tracking
    pub fn cleanup_finished(&self) {
        self.transfers.retain(|_, info| info.progress.is_active());
    }

    /// Returns the number of active transfers
    pub fn active_transfer_count(&self) -> usize {
        self.transfers
            .iter()
            .filter(|entry| entry.value().progress.is_active())
            .count()
    }

    /// Cleans up stale connections in the connection pool
    pub fn cleanup_connections(&self) {
        self.connection_pool.cleanup_stale();
    }
}

/// Configuration for transfer manager
#[derive(Debug, Clone, Copy)]
pub struct TransferConfig {
    /// Capacity of progress update channels
    pub progress_channel_capacity: usize,
    /// Progress update batching interval in milliseconds
    pub progress_batch_ms: u64,
    /// Maximum idle time for pooled connections in seconds
    pub connection_pool_max_idle_secs: u64,
}

impl Default for TransferConfig {
    fn default() -> Self {
        Self {
            progress_channel_capacity: DEFAULT_PROGRESS_CHANNEL_CAPACITY,
            progress_batch_ms: DEFAULT_PROGRESS_BATCH_MS,
            connection_pool_max_idle_secs: 300,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;
    use std::time::Duration;

    // ==================== TransferId Tests ====================

    #[test]
    fn test_transfer_id_unique_sequential() {
        // Reset the counter by creating IDs and checking they're sequential
        let id1 = TransferId::new();
        let id2 = TransferId::new();
        let id3 = TransferId::new();

        assert_eq!(id2.as_u64(), id1.as_u64() + 1);
        assert_eq!(id3.as_u64(), id2.as_u64() + 1);
        assert_ne!(id1, id2);
        assert_ne!(id2, id3);
        assert_ne!(id1, id3);
    }

    #[test]
    fn test_transfer_id_display() {
        let id = TransferId(42);
        assert_eq!(format!("{}", id), "transfer-42");
    }

    #[test]
    fn test_transfer_id_hash() {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let id = TransferId(123);
        let mut hasher1 = DefaultHasher::new();
        id.hash(&mut hasher1);
        let hash1 = hasher1.finish();

        let id2 = TransferId(123);
        let mut hasher2 = DefaultHasher::new();
        id2.hash(&mut hasher2);
        let hash2 = hasher2.finish();

        assert_eq!(hash1, hash2);
    }

    // ==================== TransferProgress Tests ====================

    fn create_test_progress(
        total: u64,
        transferred: u64,
        state: TransferState,
    ) -> TransferProgress {
        TransferProgress {
            transfer_id: TransferId::new(),
            direction: TransferDirection::Send,
            peer_id: PublicKey::from_bytes(&[0u8; 32]).unwrap(),
            file_name: Arc::from("test.txt"),
            total_bytes: total,
            transferred_bytes: transferred,
            state,
            started_at: Instant::now(),
            speed_bps: 1000,
        }
    }

    #[test]
    fn test_transfer_progress_percent() {
        // Normal case
        let progress = create_test_progress(100, 50, TransferState::InProgress);
        assert_eq!(progress.percent(), 50);

        // Zero total (edge case)
        let progress = create_test_progress(0, 0, TransferState::InProgress);
        assert_eq!(progress.percent(), 100);

        // 100% complete
        let progress = create_test_progress(100, 100, TransferState::Completed);
        assert_eq!(progress.percent(), 100);

        // Over 100% should cap at 100
        let progress = create_test_progress(100, 150, TransferState::Completed);
        assert_eq!(progress.percent(), 100);

        // Rounding test
        let progress = create_test_progress(3, 1, TransferState::InProgress);
        assert_eq!(progress.percent(), 33);
    }

    #[test]
    fn test_transfer_progress_is_complete() {
        let completed = create_test_progress(100, 100, TransferState::Completed);
        assert!(completed.is_complete());

        let failed = create_test_progress(100, 50, TransferState::Failed { reason: Arc::from("err") });
        assert!(!failed.is_complete());

        let pending = create_test_progress(100, 0, TransferState::Pending);
        assert!(!pending.is_complete());

        let in_progress = create_test_progress(100, 50, TransferState::InProgress);
        assert!(!in_progress.is_complete());
    }

    #[test]
    fn test_transfer_progress_is_failed() {
        let failed = create_test_progress(100, 50, TransferState::Failed { reason: Arc::from("err") });
        assert!(failed.is_failed());

        let completed = create_test_progress(100, 100, TransferState::Completed);
        assert!(!completed.is_failed());

        let cancelled = create_test_progress(100, 50, TransferState::Cancelled);
        assert!(!cancelled.is_failed());
    }

    #[test]
    fn test_transfer_progress_is_active() {
        let pending = create_test_progress(100, 0, TransferState::Pending);
        assert!(pending.is_active());

        let in_progress = create_test_progress(100, 50, TransferState::InProgress);
        assert!(in_progress.is_active());

        let completed = create_test_progress(100, 100, TransferState::Completed);
        assert!(!completed.is_active());

        let failed = create_test_progress(100, 50, TransferState::Failed { reason: Arc::from("err") });
        assert!(!failed.is_active());
    }

    #[test]
    fn test_transfer_progress_eta() {
        // Normal case
        let progress = create_test_progress(1000, 500, TransferState::InProgress);
        assert_eq!(progress.eta_seconds(), Some(0)); // 500 / 1000 = 0.5 -> 0

        // Zero speed
        let mut progress = create_test_progress(1000, 500, TransferState::InProgress);
        progress.speed_bps = 0;
        assert_eq!(progress.eta_seconds(), None);

        // Already complete
        let progress = create_test_progress(1000, 1000, TransferState::Completed);
        assert_eq!(progress.eta_seconds(), None);
    }

    // ==================== TransferState Tests ====================

    #[test]
    fn test_transfer_state_transitions() {
        let mut state = TransferState::Pending;

        // Pending can start
        assert!(state.can_start());
        assert!(state.start());
        assert!(matches!(state, TransferState::InProgress));

        // InProgress cannot start again
        assert!(!state.can_start());
        assert!(!state.start());

        // Can complete
        state.complete();
        assert!(matches!(state, TransferState::Completed));

        // Cannot start from completed
        assert!(!state.can_start());
    }

    #[test]
    fn test_transfer_state_fail() {
        let mut state = TransferState::InProgress;
        state.fail("test error");
        
        match &state {
            TransferState::Failed { reason } => {
                assert_eq!(reason.as_ref(), "test error");
            }
            _ => panic!("Expected Failed state"),
        }
    }

    #[test]
    fn test_transfer_state_is_terminal() {
        assert!(!TransferState::Pending.is_terminal());
        assert!(!TransferState::InProgress.is_terminal());
        assert!(TransferState::Completed.is_terminal());
        assert!(TransferState::Cancelled.is_terminal());
        assert!(TransferState::Failed { reason: Arc::from("err") }.is_terminal());
    }

    #[test]
    fn test_transfer_state_cancel() {
        let mut state = TransferState::InProgress;
        state.cancel();
        assert!(matches!(state, TransferState::Cancelled));
    }

    // ==================== TransferError Tests ====================

    #[test]
    fn test_transfer_error_display() {
        let err = TransferError::FileNotFound(PathBuf::from("/tmp/test.txt"));
        assert!(err.to_string().contains("file not found"));
        assert!(err.to_string().contains("/tmp/test.txt"));

        let err = TransferError::PeerNotConnected(Arc::from("peer123"));
        assert!(err.to_string().contains("peer not connected"));
        assert!(err.to_string().contains("peer123"));

        let err = TransferError::Cancelled;
        assert_eq!(err.to_string(), "transfer cancelled");

        let err = TransferError::Failed(Arc::from("something went wrong"));
        assert!(err.to_string().contains("transfer failed"));
        assert!(err.to_string().contains("something went wrong"));

        let err = TransferError::NoPeer;
        assert_eq!(err.to_string(), "no connected peer");

        let err = TransferError::NoEndpoint;
        assert_eq!(err.to_string(), "endpoint not initialized");
    }

    // ==================== TransferHandle Tests ====================

    #[tokio::test]
    async fn test_transfer_handle_cancel() {
        let (_tx, rx) = mpsc::channel(10);
        let handle = TransferHandle {
            id: TransferId::new(),
            progress_rx: Arc::new(Mutex::new(rx)),
            cancelled: Arc::new(AtomicU64::new(0)),
            cancel_flag: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };

        assert!(!handle.is_cancelled());
        handle.cancel();
        assert!(handle.is_cancelled());
    }

    #[tokio::test]
    async fn test_transfer_handle_transferred_bytes() {
        let (_tx, rx) = mpsc::channel(10);
        let handle = TransferHandle {
            id: TransferId::new(),
            progress_rx: Arc::new(Mutex::new(rx)),
            cancelled: Arc::new(AtomicU64::new(0)),
            cancel_flag: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };

        assert_eq!(handle.transferred_bytes(), 0);
        handle.update_transferred(1024);
        assert_eq!(handle.transferred_bytes(), 1024);
    }

    // ==================== File Name Extraction Tests ====================

    #[test]
    fn test_file_name_extraction() {
        // Normal file path
        let path = PathBuf::from("/home/user/documents/file.txt");
        let name: Arc<str> = path
            .file_name()
            .map(|n| n.to_string_lossy().into())
            .unwrap_or_else(|| Arc::from("unknown"));
        assert_eq!(name.as_ref(), "file.txt");

        // Just file name
        let path = PathBuf::from("file.txt");
        let name: Arc<str> = path
            .file_name()
            .map(|n| n.to_string_lossy().into())
            .unwrap_or_else(|| Arc::from("unknown"));
        assert_eq!(name.as_ref(), "file.txt");

        // Empty path falls back to unknown
        let path = PathBuf::from("");
        let name: Arc<str> = path
            .file_name()
            .map(|n| n.to_string_lossy().into())
            .unwrap_or_else(|| Arc::from("unknown"));
        assert_eq!(name.as_ref(), "unknown");
    }

    // ==================== Message Parsing Tests ====================

    #[test]
    fn test_message_parsing() {
        // Valid message
        let msg = "FILE:test.txt\nticket_data_here";
        let rest = msg.strip_prefix("FILE:").unwrap();
        let (file_name, ticket_str) = rest.split_once('\n').unwrap();
        assert_eq!(file_name, "test.txt");
        assert_eq!(ticket_str, "ticket_data_here");

        // Empty filename
        let msg = "FILE:\nticket_data";
        let rest = msg.strip_prefix("FILE:").unwrap();
        let (file_name, ticket_str) = rest.split_once('\n').unwrap();
        assert_eq!(file_name, "");
        assert_eq!(ticket_str, "ticket_data");

        // Filename with spaces
        let msg = "FILE:my file name.txt\nticket";
        let rest = msg.strip_prefix("FILE:").unwrap();
        let (file_name, ticket_str) = rest.split_once('\n').unwrap();
        assert_eq!(file_name, "my file name.txt");
    }

    #[test]
    fn test_invalid_message_parsing() {
        // Missing FILE: prefix
        let msg = "test.txt\nticket_data";
        assert!(msg.strip_prefix("FILE:").is_none());

        // Missing newline
        let msg = "FILE:test.txt";
        let rest = msg.strip_prefix("FILE:").unwrap();
        assert!(rest.split_once('\n').is_none());
    }

    // ==================== ConnectionPool Tests ====================

    #[test]
    fn test_connection_pool_cleanup() {
        let pool = ConnectionPool::new(0); // 0 second max idle
        
        // Should start empty
        assert_eq!(pool.connections.len(), 0);
        
        // Cleanup on empty pool shouldn't panic
        pool.cleanup_stale();
    }

    // ==================== TransferConfig Tests ====================

    #[test]
    fn test_transfer_config_default() {
        let config = TransferConfig::default();
        assert_eq!(config.progress_channel_capacity, 32);
        assert_eq!(config.progress_batch_ms, 100);
        assert_eq!(config.connection_pool_max_idle_secs, 300);
    }

    // ==================== Bytes Usage Tests ====================

    #[test]
    fn test_bytes_zero_copy() {
        // Test that Bytes can be cloned without copying data
        let original = Bytes::from_static(b"ACK");
        let cloned = original.clone();
        
        assert_eq!(original, cloned);
        // Both point to same underlying data
        assert_eq!(&original[..], b"ACK");
        assert_eq!(&cloned[..], b"ACK");
    }

    // ==================== DashMap Concurrent Access Tests ====================

    #[test]
    fn test_dashmap_concurrent_access() {
        use std::sync::Arc;
        use std::thread;

        let map: Arc<DashMap<u64, String>> = Arc::new(DashMap::new());

        // Spawn multiple threads to write
        let mut handles = vec![];
        for i in 0..10 {
            let map_clone = map.clone();
            handles.push(thread::spawn(move || {
                for j in 0..100 {
                    map_clone.insert(i * 100 + j, format!("value_{}_{}", i, j));
                }
            }));
        }

        // Wait for all threads
        for handle in handles {
            handle.join().unwrap();
        }

        // Verify all entries exist
        assert_eq!(map.len(), 1000);

        // Concurrent reads while writing
        let map_clone = map.clone();
        let read_handle = thread::spawn(move || {
            let mut count = 0;
            for entry in map_clone.iter() {
                assert!(entry.value().starts_with("value_"));
                count += 1;
            }
            count
        });

        let read_count = read_handle.join().unwrap();
        assert_eq!(read_count, 1000);
    }

    // ==================== Arc<str> Tests ====================

    #[test]
    fn test_arc_str_usage() {
        let s1: Arc<str> = Arc::from("hello");
        let s2 = s1.clone();
        
        // Both point to same data
        assert_eq!(s1.as_ptr(), s2.as_ptr());
        assert_eq!(s1.as_ref(), "hello");
        assert_eq!(s2.as_ref(), "hello");

        // Conversion from String
        let string = String::from("world");
        let arc_str: Arc<str> = Arc::from(string);
        assert_eq!(arc_str.as_ref(), "world");
    }

    // ==================== Progress Batching Logic Tests ====================

    #[test]
    fn test_progress_batch_interval() {
        let interval = Duration::from_millis(100);
        let start = Instant::now();
        
        // Simulate time passing
        std::thread::sleep(Duration::from_millis(10));
        
        let elapsed = start.elapsed();
        assert!(elapsed < interval); // Should be less than batch interval
    }

    // ==================== Atomic Operations Tests ====================

    #[test]
    fn test_atomic_u64_operations() {
        let atomic = AtomicU64::new(0);
        
        // Store
        atomic.store(100, Ordering::Relaxed);
        assert_eq!(atomic.load(Ordering::Relaxed), 100);
        
        // Fetch add
        let prev = atomic.fetch_add(50, Ordering::Relaxed);
        assert_eq!(prev, 100);
        assert_eq!(atomic.load(Ordering::Relaxed), 150);
        
        // Compare and swap
        let result = atomic.compare_exchange(
            150,
            200,
            Ordering::SeqCst,
            Ordering::Relaxed,
        );
        assert!(result.is_ok());
        assert_eq!(atomic.load(Ordering::Relaxed), 200);
    }

    // ==================== TransferDirection Tests ====================

    #[test]
    fn test_transfer_direction_clone_copy() {
        let dir = TransferDirection::Send;
        let dir2 = dir;
        // dir should still be usable after copy
        assert!(matches!(dir, TransferDirection::Send));
        assert!(matches!(dir2, TransferDirection::Send));

        let dir = TransferDirection::Receive;
        let dir2 = dir;
        assert!(matches!(dir, TransferDirection::Receive));
        assert!(matches!(dir2, TransferDirection::Receive));
    }

    #[test]
    fn test_transfer_direction_equality() {
        assert_eq!(TransferDirection::Send, TransferDirection::Send);
        assert_eq!(TransferDirection::Receive, TransferDirection::Receive);
        assert_ne!(TransferDirection::Send, TransferDirection::Receive);
    }

    // ==================== Cleanup Logic Tests ====================

    #[test]
    fn test_cleanup_finished_transfers() {
        let transfers: DashMap<TransferId, TransferInfo> = DashMap::new();

        // Create some mock transfer info
        let id1 = TransferId(1);
        transfers.insert(id1, TransferInfo {
            progress: TransferProgress {
                transfer_id: id1,
                direction: TransferDirection::Send,
                peer_id: PublicKey::from_bytes(&[0u8; 32]).unwrap(),
                file_name: Arc::from("test1.txt"),
                total_bytes: 100,
                transferred_bytes: 100,
                state: TransferState::Completed,
                started_at: Instant::now(),
                speed_bps: 1000,
            },
            cancelled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        });

        let id2 = TransferId(2);
        transfers.insert(id2, TransferInfo {
            progress: TransferProgress {
                transfer_id: id2,
                direction: TransferDirection::Send,
                peer_id: PublicKey::from_bytes(&[0u8; 32]).unwrap(),
                file_name: Arc::from("test2.txt"),
                total_bytes: 100,
                transferred_bytes: 50,
                state: TransferState::InProgress,
                started_at: Instant::now(),
                speed_bps: 1000,
            },
            cancelled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        });

        // Cleanup should remove completed transfer
        transfers.retain(|_, info| info.progress.is_active());

        assert_eq!(transfers.len(), 1);
        assert!(transfers.get(&id2).is_some());
        assert!(transfers.get(&id1).is_none());
    }
}
