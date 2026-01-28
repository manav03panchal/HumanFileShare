//! File transfer logic using iroh-blobs
//!
//! This module handles the actual transfer of files between devices using
//! Iroh's blob transfer protocol.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use iroh::NodeAddr;
use iroh::PublicKey;
use iroh::endpoint::Connection;
use iroh::protocol::{ProtocolHandler, Router};
use iroh_blobs::downloader::DownloadRequest;
use iroh_blobs::net_protocol::Blobs;
use iroh_blobs::store::mem::Store as MemStore;
use iroh_blobs::store::{ExportMode, ImportMode, ImportProgress, ReadableStore, Store};
use iroh_blobs::ticket::BlobTicket;
use iroh_blobs::util::progress::IgnoreProgressSender;
use iroh_blobs::{BlobFormat, HashAndFormat};
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
    PeerNotConnected(String),

    #[error("transfer cancelled")]
    Cancelled,

    #[error("transfer failed: {0}")]
    Failed(String),

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
    Failed { reason: String },
    Cancelled,
}

/// Progress information for a transfer
#[derive(Debug, Clone)]
pub struct TransferProgress {
    pub transfer_id: TransferId,
    pub direction: TransferDirection,
    pub peer_id: PublicKey,
    pub file_name: String,
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
        ((self.transferred_bytes as f64 / self.total_bytes as f64) * 100.0) as u8
    }

    pub fn is_complete(&self) -> bool {
        matches!(self.state, TransferState::Completed)
    }

    pub fn is_failed(&self) -> bool {
        matches!(self.state, TransferState::Failed { .. })
    }
}

/// Unique identifier for a transfer
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TransferId(u64);

impl TransferId {
    fn new() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(1);
        Self(COUNTER.fetch_add(1, Ordering::Relaxed))
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
    progress_rx: Arc<tokio::sync::Mutex<mpsc::Receiver<TransferProgress>>>,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
}

impl TransferHandle {
    pub fn id(&self) -> TransferId {
        self.id
    }

    pub async fn next_progress(&self) -> Option<TransferProgress> {
        self.progress_rx.lock().await.recv().await
    }

    pub fn cancel(&self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Callback type for received files
pub type OnFileReceived = Arc<dyn Fn(String, PathBuf, u64) + Send + Sync + 'static>;

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

            // Accept incoming bi-directional stream
            let (mut send, mut recv) = connection.accept_bi().await?;

            // Read the message (FILE:filename\nticket)
            let mut buf = Vec::new();
            let mut chunk = [0u8; 1024];
            loop {
                match recv.read(&mut chunk).await? {
                    Some(n) => buf.extend_from_slice(&chunk[..n]),
                    None => break,
                }
                if buf.len() > 10000 {
                    break;
                }
            }

            let msg = String::from_utf8_lossy(&buf);
            if let Some(rest) = msg.strip_prefix("FILE:") {
                if let Some((file_name, ticket_str)) = rest.split_once('\n') {
                    let ticket: BlobTicket = ticket_str
                        .trim()
                        .parse()
                        .context("failed to parse blob ticket")?;

                    info!(file_name = %file_name, hash = %ticket.hash(), "Receiving file via protocol handler");

                    let dest_path = download_dir.join(file_name);

                    // Download the blob
                    let downloader = blobs.downloader();
                    let hash_and_format = HashAndFormat::raw(ticket.hash());
                    let request =
                        DownloadRequest::new(hash_and_format, [ticket.node_addr().clone()]);
                    let handle = downloader.queue(request).await;
                    handle.await.context("failed to download blob")?;

                    // Export to file
                    let export_progress: Box<dyn Fn(u64) -> std::io::Result<()> + Send + Sync> =
                        Box::new(|_| Ok(()));
                    blobs
                        .store()
                        .export(
                            ticket.hash(),
                            dest_path.clone(),
                            ExportMode::Copy,
                            export_progress,
                        )
                        .await
                        .context("failed to export blob to file")?;

                    // Get file size
                    let size = tokio::fs::metadata(&dest_path)
                        .await
                        .map(|m| m.len())
                        .unwrap_or(0);

                    info!(file_name = %file_name, size = size, dest = %dest_path.display(), "File received successfully");

                    // Send acknowledgment
                    send.write_all(b"ACK").await?;
                    let _ = send.finish();

                    // Notify callback
                    if let Some(callback) = on_file_received.read().as_ref() {
                        callback(file_name.to_string(), dest_path, size);
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
    /// Active transfers
    transfers: Arc<RwLock<HashMap<TransferId, TransferInfo>>>,
    /// Default download directory
    download_dir: PathBuf,
    /// Currently connected peer
    connected_peer: Arc<RwLock<Option<NodeAddr>>>,
    /// Handler for setting callback
    file_handler: FileTransferHandler,
}

/// Internal transfer tracking info
#[derive(Debug)]
struct TransferInfo {
    progress: TransferProgress,
    progress_tx: mpsc::Sender<TransferProgress>,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
}

impl TransferManager {
    /// Creates a new transfer manager with blob store and protocol handler.
    #[instrument(skip(endpoint))]
    pub async fn new(endpoint: Arc<Endpoint>) -> Result<Self> {
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
            transfers: Arc::new(RwLock::new(HashMap::new())),
            download_dir,
            connected_peer: Arc::new(RwLock::new(None)),
            file_handler,
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
        let peer_addr = self.connected_peer().ok_or(TransferError::NoPeer)?;

        let mut handles = Vec::new();

        for path in paths {
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

        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "unknown".to_string());

        let transfer_id = TransferId::new();
        let (progress_tx, progress_rx) = mpsc::channel(100);
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
            progress_tx: progress_tx.clone(),
            cancelled: cancelled.clone(),
        };

        self.transfers.write().insert(transfer_id, info);

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

        tokio::spawn(async move {
            let mut progress = TransferProgress {
                transfer_id,
                direction: TransferDirection::Send,
                peer_id: peer_addr_clone.node_id,
                file_name: file_name.clone(),
                total_bytes: metadata.len(),
                transferred_bytes: 0,
                state: TransferState::InProgress,
                started_at: Instant::now(),
                speed_bps: 0,
            };

            let _ = progress_tx.send(progress.clone()).await;

            // Connect to peer and send the ticket
            match endpoint.connect(peer_addr_clone.clone()).await {
                Ok(conn) => {
                    // Open a bidirectional stream to send the ticket
                    match conn.open_bi().await {
                        Ok((mut send, mut recv)) => {
                            // Send the ticket as a message
                            let ticket_str = ticket.to_string();
                            let msg = format!("FILE:{}\n{}", file_name, ticket_str);

                            if let Err(e) = send.write_all(msg.as_bytes()).await {
                                warn!("Failed to send ticket: {}", e);
                                progress.state = TransferState::Failed {
                                    reason: format!("Failed to send ticket: {}", e),
                                };
                            } else {
                                let _ = send.finish();
                                // Wait for acknowledgment
                                let mut buf = [0u8; 3];
                                match recv.read_exact(&mut buf).await {
                                    Ok(_) if &buf == b"ACK" => {
                                        // Transfer complete - peer will fetch the blob
                                        progress.transferred_bytes = metadata.len();
                                        progress.state = TransferState::Completed;
                                        info!(transfer_id = %transfer_id, "File transfer completed");
                                    }
                                    _ => {
                                        progress.state = TransferState::Failed {
                                            reason: "No acknowledgment from peer".to_string(),
                                        };
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            progress.state = TransferState::Failed {
                                reason: format!("Failed to open stream: {}", e),
                            };
                        }
                    }
                }
                Err(e) => {
                    progress.state = TransferState::Failed {
                        reason: format!("Failed to connect to peer: {}", e),
                    };
                }
            }

            if cancelled_clone.load(std::sync::atomic::Ordering::Relaxed) {
                progress.state = TransferState::Cancelled;
            }

            let _ = progress_tx.send(progress.clone()).await;

            if let Some(info) = transfers.write().get_mut(&transfer_id) {
                info.progress = progress;
            }
        });

        Ok(TransferHandle {
            id: transfer_id,
            progress_rx: Arc::new(tokio::sync::Mutex::new(progress_rx)),
            cancelled,
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
        on_file_received: Arc<dyn Fn(String, PathBuf, u64) + Send + Sync + 'static>,
    ) {
        info!("Starting file receive loop (Router handles connections)");

        // Set the callback that will be invoked by the FileTransferHandler
        self.set_on_file_received(on_file_received);

        // Keep this task alive - the Router handles incoming connections
        // This loop just keeps the transfer manager from being dropped
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
        }
    }

    /// Returns all active transfers
    pub fn active_transfers(&self) -> Vec<TransferProgress> {
        self.transfers
            .read()
            .values()
            .filter(|info| {
                matches!(
                    info.progress.state,
                    TransferState::Pending | TransferState::InProgress
                )
            })
            .map(|info| info.progress.clone())
            .collect()
    }

    /// Returns the progress for a specific transfer
    pub fn get_transfer(&self, id: TransferId) -> Option<TransferProgress> {
        self.transfers
            .read()
            .get(&id)
            .map(|info| info.progress.clone())
    }

    /// Cancels a transfer
    #[instrument(skip(self), fields(transfer_id = %id))]
    pub fn cancel_transfer(&self, id: TransferId) -> bool {
        if let Some(info) = self.transfers.write().get_mut(&id) {
            info.cancelled
                .store(true, std::sync::atomic::Ordering::Relaxed);
            info.progress.state = TransferState::Cancelled;
            info!("Transfer cancelled");
            true
        } else {
            warn!("Transfer not found");
            false
        }
    }

    /// Removes completed or failed transfers from tracking
    pub fn cleanup_finished(&self) {
        self.transfers.write().retain(|_, info| {
            matches!(
                info.progress.state,
                TransferState::Pending | TransferState::InProgress
            )
        });
    }
}
