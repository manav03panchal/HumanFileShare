//! Core application state.

use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

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

impl PortalState {
    /// Returns true if the portal is in a state where transfers can occur.
    #[inline]
    #[must_use]
    pub const fn can_transfer(self) -> bool {
        matches!(self, PortalState::Connected | PortalState::Transferring)
    }

    /// Returns true if the portal is in an active state (searching, connected, or transferring).
    #[inline]
    #[must_use]
    pub const fn is_active(self) -> bool {
        !matches!(self, PortalState::Idle)
    }
}

/// Represents a connected peer device.
#[derive(Debug, Clone)]
pub struct PeerInfo {
    /// The peer's public key (device ID).
    pub id: PublicKey,
    /// Display name (if known).
    pub name: Option<Arc<str>>,
}

impl PeerInfo {
    /// Creates a new PeerInfo with the given id and optional name.
    #[inline]
    #[must_use]
    pub fn new(id: PublicKey, name: Option<impl AsRef<str>>) -> Self {
        Self {
            id,
            name: name.map(|n| Arc::from(n.as_ref())),
        }
    }

    /// Returns the display name of the peer, or a shortened ID if no name is set.
    #[inline]
    #[must_use]
    pub fn display_name(&self) -> String {
        self.name
            .as_ref()
            .map(|n| n.to_string())
            .unwrap_or_else(|| format!("{:.8}", self.id))
    }
}

/// A received file that's ready to be dragged to a destination.
#[derive(Debug, Clone)]
pub struct ReceivedFile {
    /// The file name.
    pub name: Arc<str>,
    /// Path where the file is temporarily stored.
    pub path: PathBuf,
    /// File size in bytes.
    pub size: u64,
    /// When the file was received.
    pub received_at: Instant,
}

impl ReceivedFile {
    /// Creates a new ReceivedFile.
    #[inline]
    #[must_use]
    pub fn new(name: impl AsRef<str>, path: PathBuf, size: u64) -> Self {
        Self {
            name: Arc::from(name.as_ref()),
            path,
            size,
            received_at: Instant::now(),
        }
    }

    /// Returns the age of the received file.
    #[inline]
    #[must_use]
    pub fn age(&self) -> std::time::Duration {
        self.received_at.elapsed()
    }
}

/// Shared application state accessible across the app.
/// This is stored as a GPUI global for easy access.
#[derive(Clone)]
pub struct AppState {
    inner: Arc<RwLock<AppStateInner>>,
    /// Shutdown signal for graceful termination.
    shutdown: Arc<AtomicBool>,
}

/// Default capacity for received files collection.
const DEFAULT_RECEIVED_FILES_CAPACITY: usize = 16;
/// Default capacity for active transfers collection.
const DEFAULT_ACTIVE_TRANSFERS_CAPACITY: usize = 8;
/// Default capacity for pending send files collection.
const DEFAULT_PENDING_FILES_CAPACITY: usize = 16;

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
    /// Creates a new app state with default capacities.
    #[inline]
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(
            DEFAULT_RECEIVED_FILES_CAPACITY,
            DEFAULT_ACTIVE_TRANSFERS_CAPACITY,
            DEFAULT_PENDING_FILES_CAPACITY,
        )
    }

    /// Creates a new app state with specified collection capacities.
    #[must_use]
    pub fn with_capacity(
        received_files_cap: usize,
        active_transfers_cap: usize,
        pending_files_cap: usize,
    ) -> Self {
        Self {
            inner: Arc::new(RwLock::new(AppStateInner {
                portal_state: PortalState::Idle,
                transfer_progress: 0.0,
                endpoint: None,
                transfer_manager: None,
                connected_peer: None,
                received_files: Vec::with_capacity(received_files_cap),
                active_transfers: Vec::with_capacity(active_transfers_cap),
                pending_send_files: Vec::with_capacity(pending_files_cap),
            })),
            shutdown: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Signal shutdown to all components.
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
    }

    /// Check if shutdown has been signaled.
    #[inline]
    #[must_use]
    pub fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }

    /// Gets the current portal state.
    #[inline]
    #[must_use]
    pub fn portal_state(&self) -> PortalState {
        self.inner.read().portal_state
    }

    /// Sets the portal state.
    #[inline]
    pub fn set_portal_state(&self, state: PortalState) {
        self.inner.write().portal_state = state;
    }

    /// Gets the transfer progress.
    #[inline]
    #[must_use]
    pub fn transfer_progress(&self) -> f32 {
        self.inner.read().transfer_progress
    }

    /// Sets the transfer progress (clamped to 0.0 - 1.0).
    #[inline]
    pub fn set_transfer_progress(&self, progress: f32) {
        self.inner.write().transfer_progress = progress.clamp(0.0, 1.0);
    }

    /// Sets the network endpoint.
    #[inline]
    pub fn set_endpoint(&self, endpoint: Arc<Endpoint>) {
        self.inner.write().endpoint = Some(endpoint);
    }

    /// Gets the network endpoint if initialized.
    #[inline]
    #[must_use]
    pub fn endpoint(&self) -> Option<Arc<Endpoint>> {
        self.inner.read().endpoint.clone()
    }

    /// Sets the transfer manager.
    #[inline]
    pub fn set_transfer_manager(&self, manager: Arc<TransferManager>) {
        self.inner.write().transfer_manager = Some(manager);
    }

    /// Gets the transfer manager if initialized.
    #[inline]
    #[must_use]
    pub fn transfer_manager(&self) -> Option<Arc<TransferManager>> {
        self.inner.read().transfer_manager.clone()
    }

    /// Sets the connected peer and updates portal state accordingly.
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
    #[inline]
    #[must_use]
    pub fn connected_peer(&self) -> Option<PeerInfo> {
        self.inner.read().connected_peer.clone()
    }

    /// Returns true if a peer is connected.
    #[inline]
    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.inner.read().connected_peer.is_some()
    }

    /// Adds a received file.
    pub fn add_received_file(&self, file: ReceivedFile) {
        let mut inner = self.inner.write();
        // Ensure capacity to reduce reallocations
        if inner.received_files.len() == inner.received_files.capacity() {
            inner.received_files.reserve(DEFAULT_RECEIVED_FILES_CAPACITY);
        }
        inner.received_files.push(file);
    }

    /// Adds multiple received files in a single operation (reduces lock contention).
    pub fn add_received_files(&self, files: impl IntoIterator<Item = ReceivedFile>) {
        let mut inner = self.inner.write();
        let iter = files.into_iter();
        let (min, _) = iter.size_hint();
        inner.received_files.reserve(min);
        inner.received_files.extend(iter);
    }

    /// Gets all received files as a boxed slice (frozen collection).
    #[inline]
    #[must_use]
    pub fn received_files(&self) -> Box<[ReceivedFile]> {
        self.inner.read().received_files.clone().into_boxed_slice()
    }

    /// Gets the count of received files.
    #[inline]
    #[must_use]
    pub fn received_files_count(&self) -> usize {
        self.inner.read().received_files.len()
    }

    /// Removes a received file by index.
    /// Uses NonZeroUsize for the index to ensure validity.
    pub fn remove_received_file(&self, index: NonZeroUsize) -> Option<ReceivedFile> {
        let mut inner = self.inner.write();
        let idx = index.get() - 1; // Convert from 1-based to 0-based
        if idx < inner.received_files.len() {
            Some(inner.received_files.remove(idx))
        } else {
            None
        }
    }

    /// Removes a received file by index (usize version for convenience).
    /// Returns None if index is out of bounds.
    pub fn remove_received_file_at(&self, index: usize) -> Option<ReceivedFile> {
        let mut inner = self.inner.write();
        if index < inner.received_files.len() {
            Some(inner.received_files.remove(index))
        } else {
            None
        }
    }

    /// Takes all received files, clearing the collection in one operation.
    #[must_use]
    pub fn take_received_files(&self) -> Vec<ReceivedFile> {
        std::mem::take(&mut self.inner.write().received_files)
    }

    /// Clears all received files without returning them.
    #[inline]
    pub fn clear_received_files(&self) {
        self.inner.write().received_files.clear();
    }

    /// Updates active transfer progress.
    pub fn update_transfers(&self, transfers: impl IntoIterator<Item = TransferProgress>) {
        let mut inner = self.inner.write();
        inner.active_transfers.clear();
        let iter = transfers.into_iter();
        let (min, _) = iter.size_hint();
        inner.active_transfers.reserve(min);
        inner.active_transfers.extend(iter);
    }

    /// Gets active transfers as a boxed slice (frozen collection).
    #[inline]
    #[must_use]
    pub fn active_transfers(&self) -> Box<[TransferProgress]> {
        self.inner.read().active_transfers.clone().into_boxed_slice()
    }

    /// Gets active transfers count.
    #[inline]
    #[must_use]
    pub fn active_transfers_count(&self) -> usize {
        self.inner.read().active_transfers.len()
    }

    /// Takes active transfers, clearing them in one operation.
    #[must_use]
    pub fn take_active_transfers(&self) -> Vec<TransferProgress> {
        std::mem::take(&mut self.inner.write().active_transfers)
    }

    /// Returns the device ID if endpoint is initialized.
    #[inline]
    #[must_use]
    pub fn device_id(&self) -> Option<PublicKey> {
        self.inner.read().endpoint.as_ref().map(|e| e.device_id())
    }

    /// Queue files to be sent.
    pub fn queue_files_for_send(&self, files: impl IntoIterator<Item = PathBuf>) {
        let mut inner = self.inner.write();
        let iter = files.into_iter();
        let (min, _) = iter.size_hint();
        inner.pending_send_files.reserve(min);
        inner.pending_send_files.extend(iter);
        if !inner.pending_send_files.is_empty() {
            inner.portal_state = PortalState::Transferring;
        }
    }

    /// Queue a single file to be sent.
    pub fn queue_file_for_send(&self, file: PathBuf) {
        let mut inner = self.inner.write();
        inner.pending_send_files.push(file);
        inner.portal_state = PortalState::Transferring;
    }

    /// Take pending files to send (clears the queue).
    #[must_use]
    pub fn take_pending_send_files(&self) -> Box<[PathBuf]> {
        std::mem::take(&mut self.inner.write().pending_send_files).into_boxed_slice()
    }

    /// Check if there are files pending to send.
    #[inline]
    #[must_use]
    pub fn has_pending_send_files(&self) -> bool {
        !self.inner.read().pending_send_files.is_empty()
    }

    /// Gets the count of pending send files.
    #[inline]
    #[must_use]
    pub fn pending_send_files_count(&self) -> usize {
        self.inner.read().pending_send_files.len()
    }

    /// Clears pending send files without returning them.
    #[inline]
    pub fn clear_pending_send_files(&self) {
        self.inner.write().pending_send_files.clear();
    }

    /// Resets the state to idle, clearing all transient data.
    pub fn reset_to_idle(&self) {
        let mut inner = self.inner.write();
        inner.portal_state = PortalState::Idle;
        inner.transfer_progress = 0.0;
        inner.connected_peer = None;
        inner.active_transfers.clear();
        inner.pending_send_files.clear();
    }

}



impl Default for AppState {
    #[inline]
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
    /// Creates a new App with default state.
    #[inline]
    #[must_use]
    pub const fn new() -> Self {
        Self {
            portal_state: PortalState::Idle,
            transfer_progress: 0.0,
        }
    }

    /// Returns true if the app is in a transfer-capable state.
    #[inline]
    #[must_use]
    pub const fn can_transfer(&self) -> bool {
        self.portal_state.can_transfer()
    }
}

impl Default for App {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    // ===== PortalState Tests =====

    #[test]
    fn test_portal_state_default() {
        let state: PortalState = Default::default();
        assert_eq!(state, PortalState::Idle);
    }

    #[test]
    fn test_portal_state_can_transfer() {
        assert!(!PortalState::Idle.can_transfer());
        assert!(!PortalState::Searching.can_transfer());
        assert!(PortalState::Connected.can_transfer());
        assert!(PortalState::Transferring.can_transfer());
    }

    #[test]
    fn test_portal_state_is_active() {
        assert!(!PortalState::Idle.is_active());
        assert!(PortalState::Searching.is_active());
        assert!(PortalState::Connected.is_active());
        assert!(PortalState::Transferring.is_active());
    }

    #[test]
    fn test_portal_state_transitions() {
        // Test all possible state transitions
        let states = [
            PortalState::Idle,
            PortalState::Searching,
            PortalState::Connected,
            PortalState::Transferring,
        ];

        for (i, &from) in states.iter().enumerate() {
            for (j, &to) in states.iter().enumerate() {
                // All transitions should be valid Copy types
                let current = to;
                assert_eq!(current, to, "Transition from {:?} to {:?} failed", from, to);
                
                // Test equality
                if i == j {
                    assert_eq!(from, to);
                } else {
                    assert_ne!(from, to);
                }
            }
        }
    }

    // ===== PeerInfo Tests =====

    #[test]
    fn test_peer_info_new_with_name() {
        // Create a dummy public key for testing
        let key_bytes = [1u8; 32];
        let id = PublicKey::from_bytes(&key_bytes).unwrap();
        let peer = PeerInfo::new(id, Some("Test Device"));
        
        assert_eq!(peer.id, id);
        assert_eq!(peer.name.as_ref().map(|s| s.as_ref()), Some("Test Device"));
    }

    #[test]
    fn test_peer_info_new_without_name() {
        let key_bytes = [2u8; 32];
        let id = PublicKey::from_bytes(&key_bytes).unwrap();
        let peer = PeerInfo::new(id, None::<&str>);
        
        assert_eq!(peer.id, id);
        assert!(peer.name.is_none());
    }

    #[test]
    fn test_peer_info_display_name_with_name() {
        let key_bytes = [3u8; 32];
        let id = PublicKey::from_bytes(&key_bytes).unwrap();
        let peer = PeerInfo::new(id, Some("My Device"));
        
        assert_eq!(peer.display_name(), "My Device");
    }

    #[test]
    fn test_peer_info_display_name_without_name() {
        let key_bytes = [4u8; 32];
        let id = PublicKey::from_bytes(&key_bytes).unwrap();
        let peer = PeerInfo::new(id, None::<&str>);
        
        // Should return shortened ID
        let display = peer.display_name();
        assert!(display.len() <= 8);
    }

    #[test]
    fn test_peer_info_clone() {
        let key_bytes = [5u8; 32];
        let id = PublicKey::from_bytes(&key_bytes).unwrap();
        let peer = PeerInfo::new(id, Some("Clone Test"));
        let cloned = peer.clone();
        
        assert_eq!(peer.id, cloned.id);
        assert_eq!(peer.name, cloned.name);
        // Arc should point to same allocation
        if let (Some(a), Some(b)) = (&peer.name, &cloned.name) {
            assert!(Arc::ptr_eq(a, b));
        }
    }

    // ===== ReceivedFile Tests =====

    #[test]
    fn test_received_file_new() {
        let path = PathBuf::from("/tmp/test.txt");
        let file = ReceivedFile::new("test.txt", path.clone(), 1024);
        
        assert_eq!(file.name.as_ref(), "test.txt");
        assert_eq!(file.path, path);
        assert_eq!(file.size, 1024);
        // Age should be very small
        assert!(file.age().as_millis() < 100);
    }

    #[test]
    fn test_received_file_clone() {
        let path = PathBuf::from("/tmp/clone_test.txt");
        let file = ReceivedFile::new("clone_test.txt", path, 2048);
        let cloned = file.clone();
        
        assert_eq!(file.name, cloned.name);
        assert_eq!(file.path, cloned.path);
        assert_eq!(file.size, cloned.size);
        // Arc should point to same allocation
        assert!(Arc::ptr_eq(&file.name, &cloned.name));
    }

    #[test]
    fn test_received_file_empty_name() {
        let path = PathBuf::from("/tmp/empty.txt");
        let file = ReceivedFile::new("", path, 0);
        
        assert_eq!(file.name.as_ref(), "");
        assert_eq!(file.size, 0);
    }

    #[test]
    fn test_received_file_large_size() {
        let path = PathBuf::from("/tmp/large.bin");
        let large_size = u64::MAX;
        let file = ReceivedFile::new("large.bin", path, large_size);
        
        assert_eq!(file.size, u64::MAX);
    }

    // ===== AppState Tests =====

    #[test]
    fn test_app_state_new() {
        let state = AppState::new();
        assert_eq!(state.portal_state(), PortalState::Idle);
        assert_eq!(state.transfer_progress(), 0.0);
        assert!(!state.is_connected());
        assert!(!state.has_pending_send_files());
        assert_eq!(state.received_files_count(), 0);
        assert_eq!(state.active_transfers_count(), 0);
        assert_eq!(state.pending_send_files_count(), 0);
    }

    #[test]
    fn test_app_state_with_capacity() {
        let state = AppState::with_capacity(32, 16, 8);
        // Just verify it doesn't panic and creates correctly
        assert_eq!(state.portal_state(), PortalState::Idle);
    }

    #[test]
    fn test_app_state_default() {
        let state: AppState = Default::default();
        assert_eq!(state.portal_state(), PortalState::Idle);
    }

    #[test]
    fn test_app_state_portal_state() {
        let state = AppState::new();
        
        state.set_portal_state(PortalState::Searching);
        assert_eq!(state.portal_state(), PortalState::Searching);
        
        state.set_portal_state(PortalState::Connected);
        assert_eq!(state.portal_state(), PortalState::Connected);
        
        state.set_portal_state(PortalState::Transferring);
        assert_eq!(state.portal_state(), PortalState::Transferring);
        
        state.set_portal_state(PortalState::Idle);
        assert_eq!(state.portal_state(), PortalState::Idle);
    }

    #[test]
    fn test_app_state_transfer_progress() {
        let state = AppState::new();
        
        state.set_transfer_progress(0.5);
        assert_eq!(state.transfer_progress(), 0.5);
        
        state.set_transfer_progress(0.75);
        assert_eq!(state.transfer_progress(), 0.75);
        
        // Test clamping at upper bound
        state.set_transfer_progress(1.5);
        assert_eq!(state.transfer_progress(), 1.0);
        
        // Test clamping at lower bound
        state.set_transfer_progress(-0.5);
        assert_eq!(state.transfer_progress(), 0.0);
    }

    #[test]
    fn test_app_state_connected_peer() {
        let state = AppState::new();
        let key_bytes = [6u8; 32];
        let id = PublicKey::from_bytes(&key_bytes).unwrap();
        let peer = PeerInfo::new(id, Some("Test Peer"));
        
        assert!(state.connected_peer().is_none());
        assert!(!state.is_connected());
        
        state.set_connected_peer(Some(peer.clone()));
        assert!(state.is_connected());
        assert_eq!(state.portal_state(), PortalState::Connected);
        
        let retrieved = state.connected_peer().unwrap();
        assert_eq!(retrieved.id, id);
        
        // Disconnect
        state.set_connected_peer(None);
        assert!(!state.is_connected());
        assert_eq!(state.portal_state(), PortalState::Idle);
    }

    #[test]
    fn test_app_state_received_files() {
        let state = AppState::new();
        let path = PathBuf::from("/tmp/received.txt");
        let file = ReceivedFile::new("received.txt", path, 1024);
        
        assert_eq!(state.received_files_count(), 0);
        
        state.add_received_file(file.clone());
        assert_eq!(state.received_files_count(), 1);
        
        let files = state.received_files();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].name.as_ref(), "received.txt");
        
        // Test removal with NonZeroUsize (1-based index)
        let removed = state.remove_received_file(NonZeroUsize::new(1).unwrap());
        assert!(removed.is_some());
        assert_eq!(state.received_files_count(), 0);
        
        // Test removal of non-existent index
        let removed = state.remove_received_file(NonZeroUsize::new(1).unwrap());
        assert!(removed.is_none());
    }

    #[test]
    fn test_app_state_received_files_at() {
        let state = AppState::new();
        let file1 = ReceivedFile::new("file1.txt", PathBuf::from("/tmp/file1.txt"), 100);
        let file2 = ReceivedFile::new("file2.txt", PathBuf::from("/tmp/file2.txt"), 200);
        
        state.add_received_file(file1);
        state.add_received_file(file2);
        
        // Remove at index 0
        let removed = state.remove_received_file_at(0);
        assert!(removed.is_some());
        assert_eq!(state.received_files_count(), 1);
        
        // Remove at index 0 again
        let removed = state.remove_received_file_at(0);
        assert!(removed.is_some());
        assert_eq!(state.received_files_count(), 0);
        
        // Out of bounds
        let removed = state.remove_received_file_at(0);
        assert!(removed.is_none());
    }

    #[test]
    fn test_app_state_add_received_files_batch() {
        let state = AppState::new();
        let files: Vec<_> = (0..10)
            .map(|i| ReceivedFile::new(
                format!("file{}.txt", i),
                PathBuf::from(format!("/tmp/file{}.txt", i)),
                i as u64 * 100,
            ))
            .collect();
        
        state.add_received_files(files);
        assert_eq!(state.received_files_count(), 10);
    }

    #[test]
    fn test_app_state_take_received_files() {
        let state = AppState::new();
        let file = ReceivedFile::new("test.txt", PathBuf::from("/tmp/test.txt"), 100);
        state.add_received_file(file);
        
        let taken = state.take_received_files();
        assert_eq!(taken.len(), 1);
        assert_eq!(state.received_files_count(), 0);
    }

    #[test]
    fn test_app_state_clear_received_files() {
        let state = AppState::new();
        state.add_received_file(ReceivedFile::new("test.txt", PathBuf::from("/tmp/test.txt"), 100));
        
        state.clear_received_files();
        assert_eq!(state.received_files_count(), 0);
    }

    #[test]
    fn test_app_state_pending_send_files() {
        let state = AppState::new();
        
        assert!(!state.has_pending_send_files());
        assert_eq!(state.pending_send_files_count(), 0);
        
        // Queue a single file
        state.queue_file_for_send(PathBuf::from("/tmp/send1.txt"));
        assert!(state.has_pending_send_files());
        assert_eq!(state.pending_send_files_count(), 1);
        assert_eq!(state.portal_state(), PortalState::Transferring);
        
        // Take pending files
        let taken = state.take_pending_send_files();
        assert_eq!(taken.len(), 1);
        assert!(!state.has_pending_send_files());
        
        // Queue multiple files
        state.queue_files_for_send(vec![
            PathBuf::from("/tmp/a.txt"),
            PathBuf::from("/tmp/b.txt"),
        ]);
        assert_eq!(state.pending_send_files_count(), 2);
        
        // Clear without taking
        state.clear_pending_send_files();
        assert!(!state.has_pending_send_files());
    }

    #[test]
    fn test_app_state_update_transfers() {
        let state = AppState::new();
        let key_bytes = [7u8; 32];
        let peer_id = PublicKey::from_bytes(&key_bytes).unwrap();
        
        // Create some dummy transfer progress
        let transfers = vec![TransferProgress {
            transfer_id: crate::net::TransferId::from(1u64),
            direction: crate::net::TransferDirection::Send,
            peer_id,
            file_name: "test.txt".to_string(),
            total_bytes: 1000,
            transferred_bytes: 500,
            state: crate::net::TransferState::InProgress,
            started_at: Instant::now(),
            speed_bps: 100,
        }];
        
        state.update_transfers(transfers.clone());
        assert_eq!(state.active_transfers_count(), 1);
        
        let active = state.active_transfers();
        assert_eq!(active.len(), 1);
        
        // Update with empty list
        state.update_transfers(vec![]);
        assert_eq!(state.active_transfers_count(), 0);
    }

    #[test]
    fn test_app_state_take_active_transfers() {
        let state = AppState::new();
        let key_bytes = [8u8; 32];
        let peer_id = PublicKey::from_bytes(&key_bytes).unwrap();
        
        let transfers = vec![TransferProgress {
            transfer_id: crate::net::TransferId::from(2u64),
            direction: crate::net::TransferDirection::Send,
            peer_id,
            file_name: "test.txt".to_string(),
            total_bytes: 1000,
            transferred_bytes: 500,
            state: crate::net::TransferState::InProgress,
            started_at: Instant::now(),
            speed_bps: 100,
        }];
        
        state.update_transfers(transfers);
        let taken = state.take_active_transfers();
        assert_eq!(taken.len(), 1);
        assert_eq!(state.active_transfers_count(), 0);
    }

    #[test]
    fn test_app_state_reset_to_idle() {
        let state = AppState::new();
        let key_bytes = [9u8; 32];
        let id = PublicKey::from_bytes(&key_bytes).unwrap();
        let peer = PeerInfo::new(id, Some("Peer"));
        
        // Setup some state
        state.set_connected_peer(Some(peer));
        state.set_transfer_progress(0.5);
        state.queue_file_for_send(PathBuf::from("/tmp/test.txt"));
        state.add_received_file(ReceivedFile::new("recv.txt", PathBuf::from("/tmp/recv.txt"), 100));
        
        // Reset
        state.reset_to_idle();
        
        assert_eq!(state.portal_state(), PortalState::Idle);
        assert_eq!(state.transfer_progress(), 0.0);
        assert!(!state.is_connected());
        assert!(!state.has_pending_send_files());
        assert_eq!(state.active_transfers_count(), 0);
        // Note: received_files are not cleared by reset_to_idle
    }

    // ===== Concurrent Access Tests =====

    #[test]
    fn test_app_state_concurrent_reads() {
        let state = AppState::new();
        let mut handles = vec![];
        
        // Spawn multiple readers
        for _ in 0..10 {
            let state = state.clone();
            handles.push(thread::spawn(move || {
                for _ in 0..100 {
                    let _ = state.portal_state();
                    let _ = state.transfer_progress();
                    let _ = state.is_connected();
                }
            }));
        }
        
        for handle in handles {
            handle.join().unwrap();
        }
    }

    #[test]
    fn test_app_state_concurrent_writes() {
        let state = AppState::new();
        let mut handles = vec![];
        
        // Spawn multiple writers
        for i in 0..10 {
            let state = state.clone();
            handles.push(thread::spawn(move || {
                state.set_transfer_progress(i as f32 / 10.0);
            }));
        }
        
        for handle in handles {
            handle.join().unwrap();
        }
        
        // Final value should be valid (clamped)
        let final_progress = state.transfer_progress();
        assert!(final_progress >= 0.0 && final_progress <= 1.0);
    }

    #[test]
    fn test_app_state_concurrent_mixed() {
        let state = AppState::new();
        let mut handles = vec![];
        
        // Mix of readers and writers
        for i in 0..20 {
            let state = state.clone();
            handles.push(thread::spawn(move || {
                if i % 2 == 0 {
                    // Writer
                    let file = ReceivedFile::new(
                        format!("file{}.txt", i),
                        PathBuf::from(format!("/tmp/file{}.txt", i)),
                        i as u64 * 100,
                    );
                    state.add_received_file(file);
                } else {
                    // Reader
                    for _ in 0..50 {
                        let _ = state.received_files_count();
                    }
                }
            }));
        }
        
        for handle in handles {
            handle.join().unwrap();
        }
        
        // Should have 10 files
        assert_eq!(state.received_files_count(), 10);
    }

    // ===== Boundary Condition Tests =====

    #[test]
    fn test_app_state_empty_file_operations() {
        let state = AppState::new();
        
        // Operations on empty collections
        let files = state.received_files();
        assert!(files.is_empty());
        
        let pending = state.take_pending_send_files();
        assert!(pending.is_empty());
        
        let active = state.active_transfers();
        assert!(active.is_empty());
    }

    #[test]
    fn test_app_state_max_index() {
        let state = AppState::new();
        
        // Add one file
        state.add_received_file(ReceivedFile::new(
            "test.txt",
            PathBuf::from("/tmp/test.txt"),
            100,
        ));
        
        // Try to remove at max usize (should fail gracefully)
        let removed = state.remove_received_file_at(usize::MAX);
        assert!(removed.is_none());
        
        // NonZeroUsize::new(usize::MAX) is valid but out of bounds
        let max_nonzero = NonZeroUsize::new(usize::MAX).unwrap();
        let removed = state.remove_received_file(max_nonzero);
        assert!(removed.is_none());
    }

    #[test]
    fn test_app_state_zero_size_file() {
        let state = AppState::new();
        let file = ReceivedFile::new("empty.txt", PathBuf::from("/tmp/empty.txt"), 0);
        
        state.add_received_file(file);
        let files = state.received_files();
        assert_eq!(files[0].size, 0);
    }

    // ===== Legacy App Tests =====

    #[test]
    fn test_legacy_app_new() {
        let app = App::new();
        assert_eq!(app.portal_state, PortalState::Idle);
        assert_eq!(app.transfer_progress, 0.0);
    }

    #[test]
    fn test_legacy_app_default() {
        let app: App = Default::default();
        assert_eq!(app.portal_state, PortalState::Idle);
    }

    #[test]
    fn test_legacy_app_can_transfer() {
        let mut app = App::new();
        assert!(!app.can_transfer());
        
        app.portal_state = PortalState::Connected;
        assert!(app.can_transfer());
        
        app.portal_state = PortalState::Transferring;
        assert!(app.can_transfer());
    }
}
