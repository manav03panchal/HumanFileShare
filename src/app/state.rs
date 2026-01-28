//! Core application state.

use std::path::PathBuf;
use std::sync::Arc;

use gpui::Global;
use iroh::PublicKey;
use parking_lot::RwLock;

use crate::net::{Endpoint, TransferManager, TransferProgress};

/// The current state of the file sharing portal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PortalState {
    /// Idle, waiting for user interaction.
    #[default]
    Idle,
    /// Searching for nearby devices.
    Searching,
    /// Connected to a peer, ready to transfer.
    Connected,
    /// Actively transferring a file.
    Transferring,
}

/// Represents a connected peer device.
#[derive(Debug, Clone)]
pub struct PeerInfo {
    /// The peer's public key (device ID).
    pub id: PublicKey,
    /// Display name (if known).
    pub name: Option<String>,
}

/// A received file that's ready to be dragged to a destination.
#[derive(Debug, Clone)]
pub struct ReceivedFile {
    /// The file name.
    pub name: String,
    /// Path where the file is temporarily stored.
    pub path: PathBuf,
    /// File size in bytes.
    pub size: u64,
    /// When the file was received.
    pub received_at: std::time::Instant,
}

/// Shared application state accessible across the app.
/// This is stored as a GPUI global for easy access.
#[derive(Clone)]
pub struct AppState {
    inner: Arc<RwLock<AppStateInner>>,
}

struct AppStateInner {
    /// Current state of the portal.
    portal_state: PortalState,
    /// Transfer progress (0.0 - 1.0) when in Transferring state.
    transfer_progress: f32,
    /// The network endpoint (initialized async).
    endpoint: Option<Arc<Endpoint>>,
    /// The transfer manager (initialized after endpoint).
    transfer_manager: Option<Arc<TransferManager>>,
    /// Currently connected peer.
    connected_peer: Option<PeerInfo>,
    /// Files received and available for dragging out.
    received_files: Vec<ReceivedFile>,
    /// Active transfer progress updates.
    active_transfers: Vec<TransferProgress>,
    /// Files pending to be sent (dropped onto portal).
    pending_send_files: Vec<PathBuf>,
}

impl AppState {
    /// Creates a new app state.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(AppStateInner {
                portal_state: PortalState::Idle,
                transfer_progress: 0.0,
                endpoint: None,
                transfer_manager: None,
                connected_peer: None,
                received_files: Vec::new(),
                active_transfers: Vec::new(),
                pending_send_files: Vec::new(),
            })),
        }
    }

    /// Gets the current portal state.
    pub fn portal_state(&self) -> PortalState {
        self.inner.read().portal_state
    }

    /// Sets the portal state.
    pub fn set_portal_state(&self, state: PortalState) {
        self.inner.write().portal_state = state;
    }

    /// Gets the transfer progress.
    pub fn transfer_progress(&self) -> f32 {
        self.inner.read().transfer_progress
    }

    /// Sets the transfer progress.
    pub fn set_transfer_progress(&self, progress: f32) {
        self.inner.write().transfer_progress = progress;
    }

    /// Sets the network endpoint.
    pub fn set_endpoint(&self, endpoint: Arc<Endpoint>) {
        self.inner.write().endpoint = Some(endpoint);
    }

    /// Gets the network endpoint if initialized.
    pub fn endpoint(&self) -> Option<Arc<Endpoint>> {
        self.inner.read().endpoint.clone()
    }

    /// Sets the transfer manager.
    pub fn set_transfer_manager(&self, manager: Arc<TransferManager>) {
        self.inner.write().transfer_manager = Some(manager);
    }

    /// Gets the transfer manager if initialized.
    pub fn transfer_manager(&self) -> Option<Arc<TransferManager>> {
        self.inner.read().transfer_manager.clone()
    }

    /// Sets the connected peer.
    pub fn set_connected_peer(&self, peer: Option<PeerInfo>) {
        let mut inner = self.inner.write();
        inner.connected_peer = peer;
        inner.portal_state = if inner.connected_peer.is_some() {
            PortalState::Connected
        } else {
            PortalState::Idle
        };
    }

    /// Gets the connected peer if any.
    pub fn connected_peer(&self) -> Option<PeerInfo> {
        self.inner.read().connected_peer.clone()
    }

    /// Adds a received file.
    pub fn add_received_file(&self, file: ReceivedFile) {
        self.inner.write().received_files.push(file);
    }

    /// Gets all received files.
    pub fn received_files(&self) -> Vec<ReceivedFile> {
        self.inner.read().received_files.clone()
    }

    /// Removes a received file by index.
    pub fn remove_received_file(&self, index: usize) -> Option<ReceivedFile> {
        let mut inner = self.inner.write();
        if index < inner.received_files.len() {
            Some(inner.received_files.remove(index))
        } else {
            None
        }
    }

    /// Updates active transfer progress.
    pub fn update_transfers(&self, transfers: Vec<TransferProgress>) {
        self.inner.write().active_transfers = transfers;
    }

    /// Gets active transfers.
    pub fn active_transfers(&self) -> Vec<TransferProgress> {
        self.inner.read().active_transfers.clone()
    }

    /// Returns the device ID if endpoint is initialized.
    pub fn device_id(&self) -> Option<PublicKey> {
        self.inner.read().endpoint.as_ref().map(|e| e.device_id())
    }

    /// Queue files to be sent.
    pub fn queue_files_for_send(&self, files: Vec<PathBuf>) {
        let mut inner = self.inner.write();
        inner.pending_send_files = files;
        inner.portal_state = PortalState::Transferring;
    }

    /// Take pending files to send (clears the queue).
    pub fn take_pending_send_files(&self) -> Vec<PathBuf> {
        let mut inner = self.inner.write();
        std::mem::take(&mut inner.pending_send_files)
    }

    /// Check if there are files pending to send.
    pub fn has_pending_send_files(&self) -> bool {
        !self.inner.read().pending_send_files.is_empty()
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

impl Global for AppState {}

/// Legacy App struct for compatibility.
pub struct App {
    /// Current state of the portal.
    pub portal_state: PortalState,
    /// Transfer progress (0.0 - 1.0) when in Transferring state.
    pub transfer_progress: f32,
}

impl App {
    pub fn new() -> Self {
        Self {
            portal_state: PortalState::Idle,
            transfer_progress: 0.0,
        }
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}
