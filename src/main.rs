//! HumanFileShare - Cross-platform file sharing made simple.
//!
//! A minimalist application that makes sharing files between devices
//! as simple as possible.
//!
//! ## Testing with multiple instances on one machine
//!
//! Run two instances with different config directories:
//! ```bash
//! # Terminal 1
//! HFS_CONFIG_DIR=/tmp/hfs1 cargo run
//!
//! # Terminal 2
//! HFS_CONFIG_DIR=/tmp/hfs2 cargo run
//! ```

use std::path::PathBuf;
use std::sync::Arc;
use std::thread;

use anyhow::Result;
use gpui::*;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

use humanfileshare::app::{AppState, PeerInfo, PortalState, ReceivedFile};
use humanfileshare::net::{DiscoveredPeer, Discovery, Endpoint, PeerEvent, TransferManager};
use humanfileshare::ui::{RootView, Theme};

// Define actions for keyboard shortcuts
actions!(humanfileshare, [Quit, Hide, Minimize, CloseWindow]);

/// Window dimensions - a compact square.
const WINDOW_SIZE: f32 = 320.0;

/// Channel capacity for file send requests.
const SEND_FILES_CHANNEL_CAPACITY: usize = 32;

fn main() -> Result<()> {
    init_logging();

    // Create app state that will be shared between UI and networking
    let app_state = AppState::new();
    let app_state_for_net = app_state.clone();

    // Spawn networking in a separate thread with its own tokio runtime
    thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");
        rt.block_on(async move {
            if let Err(e) = run_networking(app_state_for_net).await {
                error!(error = %e, "Networking error");
            }
        });
    });

    Application::new()
        .with_assets(gpui_component_assets::Assets)
        .run(move |cx: &mut App| {
        // Initialize gpui-component
        gpui_component::init(cx);

        // Set global theme
        cx.set_global(Theme::light());

        // Set global app state
        cx.set_global(app_state.clone());

        // Bind keyboard shortcuts (platform-aware)
        #[cfg(target_os = "macos")]
        cx.bind_keys([
            KeyBinding::new("cmd-q", Quit, None),
            KeyBinding::new("cmd-w", CloseWindow, None),
            KeyBinding::new("cmd-h", Hide, None),
            KeyBinding::new("cmd-m", Minimize, None),
        ]);

        #[cfg(not(target_os = "macos"))]
        cx.bind_keys([
            KeyBinding::new("ctrl-q", Quit, None),
            KeyBinding::new("alt-f4", Quit, None),
            KeyBinding::new("ctrl-w", CloseWindow, None),
        ]);

        // Register global action handlers
        cx.on_action(|_: &Quit, cx| {
            // Signal shutdown to networking thread
            if let Some(app_state) = cx.try_global::<AppState>() {
                app_state.shutdown();
            }
            cx.quit();
        });
        cx.on_action(|_: &Hide, cx| cx.hide());
        cx.on_action(|_: &Minimize, cx| {
            if let Some(window) = cx.active_window() {
                window.update(cx, |_, window, _| window.minimize_window()).ok();
            }
        });
        cx.on_action(|_: &CloseWindow, cx| {
            if let Some(window) = cx.active_window() {
                window.update(cx, |_, window, _| window.remove_window()).ok();
            }
        });

        // Configure window options
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds {
                origin: Point::default(),
                size: Size {
                    width: px(WINDOW_SIZE),
                    height: px(WINDOW_SIZE),
                },
            })),
            titlebar: Some(TitlebarOptions {
                title: Some("HumanFileShare".into()),
                appears_transparent: true,
                ..Default::default()
            }),
            is_resizable: false,
            ..Default::default()
        };

        cx.open_window(options, |window, cx| {
            let root_view = cx.new(RootView::new);
            cx.new(|cx| gpui_component::Root::new(root_view, window, cx))
        })
        .expect("Failed to open window");
    });

    Ok(())
}

/// Request to send files to the connected peer.
#[derive(Debug)]
struct SendFilesRequest {
    /// Files to send.
    files: Box<[PathBuf]>,
}

/// Run the networking stack (endpoint, discovery, transfer manager).
async fn run_networking(app_state: AppState) -> Result<()> {
    info!("Initializing networking...");

    // Create the endpoint (with built-in mDNS discovery via discovery_local_network())
    let endpoint = Arc::new(Endpoint::new().await?);
    let device_id = endpoint.device_id();
    info!(device_id = %device_id, "Device ID initialized");

    // Create the transfer manager
    let transfer_manager = Arc::new(TransferManager::new(endpoint.clone()).await?);
    info!("Transfer manager initialized");

    // Store in app state
    app_state.set_endpoint(endpoint.clone());
    app_state.set_transfer_manager(transfer_manager.clone());

    // Create discovery wrapper around the endpoint
    // Note: mDNS is already running via Iroh's discovery_local_network()
    let iroh_endpoint = endpoint.iroh_endpoint()?;
    let discovery = Arc::new(Discovery::new(iroh_endpoint));
    info!("Discovery initialized (using Iroh's built-in mDNS)");

    info!("Networking initialized successfully");

    // Cancellation token for graceful shutdown
    let cancel_token = CancellationToken::new();

    // Task to monitor UI shutdown signal
    let shutdown_monitor = {
        let app_state = app_state.clone();
        let cancel = cancel_token.clone();

        tokio::spawn(async move {
            loop {
                if app_state.is_shutdown() {
                    info!("Shutdown signal received from UI");
                    cancel.cancel();
                    break;
                }
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
            }
        })
    };

    // Channel for file send requests (replaces polling)
    let (send_files_tx, mut send_files_rx) = mpsc::channel::<SendFilesRequest>(SEND_FILES_CHANNEL_CAPACITY);
    
    // Store the sender in app state for UI to use
    // We create a wrapper that can be called from non-async contexts
    let send_files_sender = Arc::new(send_files_tx);

    // Task to handle discovered peers (event-driven via peer_stream)
    let peer_task = {
        let app_state = app_state.clone();
        let transfer_manager = transfer_manager.clone();
        let discovery = discovery.clone();
        let cancel = cancel_token.clone();

        tokio::spawn(async move {
            let mut peer_stream = discovery.peer_stream();

            info!("Peer discovery task started (event-driven)");

            loop {
                tokio::select! {
                    biased;

                    // Check for cancellation first
                    _ = cancel.cancelled() => {
                        info!("Peer discovery task shutting down");
                        break;
                    }

                    // Handle peer events
                    Some(event) = peer_stream.recv() => {
                        handle_peer_event(&event, &app_state, &transfer_manager).await;
                    }
                }
            }
        })
    };

    // Task to handle file send requests (event-driven via channel)
    let transfer_task = {
        let app_state = app_state.clone();
        let transfer_manager = transfer_manager.clone();
        let cancel = cancel_token.clone();

        tokio::spawn(async move {
            info!("Transfer task started (event-driven)");

            loop {
                tokio::select! {
                    biased;

                    // Check for cancellation first
                    _ = cancel.cancelled() => {
                        info!("Transfer task shutting down");
                        break;
                    }

                    // Wait for file send requests
                    Some(request) = send_files_rx.recv() => {
                        handle_send_request(request, &app_state, &transfer_manager).await;
                    }
                }
            }
        })
    };

    // Task to poll for pending files from the UI and send them via channel
    // This bridges the sync UI world with the async networking world
    let file_poller_task = {
        let app_state = app_state.clone();
        let send_files_sender = send_files_sender.clone();
        let cancel = cancel_token.clone();

        tokio::spawn(async move {
            info!("File poller task started (bridging UI to async)");

            loop {
                tokio::select! {
                    biased;

                    _ = cancel.cancelled() => {
                        info!("File poller task shutting down");
                        break;
                    }

                    // Poll at a reasonable rate for UI responsiveness
                    _ = tokio::time::sleep(tokio::time::Duration::from_millis(50)) => {
                        if app_state.has_pending_send_files() {
                            let files = app_state.take_pending_send_files();
                            if !files.is_empty() {
                                let request = SendFilesRequest { files };
                                if send_files_sender.send(request).await.is_err() {
                                    error!("Failed to send file request - channel closed");
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        })
    };

    // Task to receive incoming files
    let receive_task = {
        let app_state = app_state.clone();
        let transfer_manager = transfer_manager.clone();
        let cancel = cancel_token.clone();

        tokio::spawn(async move {
            info!("Receive task started");

            // Create callback for received files
            let callback = Arc::new(move |name: Arc<str>, path: PathBuf, size: u64| {
                info!(name = %name, size = size, "Received file");
                app_state.add_received_file(ReceivedFile::new(name, path, size));
            });

            // Run receive loop with cancellation support
            tokio::select! {
                _ = cancel.cancelled() => {
                    info!("Receive task shutting down");
                }
                _ = transfer_manager.receive_loop(callback) => {
                    warn!("Receive loop exited unexpectedly");
                }
            }
        })
    };

    // Wait for all tasks with graceful shutdown on any task completion
    tokio::select! {
        _ = shutdown_monitor => {
            info!("Shutdown monitor triggered, initiating shutdown");
        }
        _ = peer_task => {
            info!("Peer task completed, initiating shutdown");
        }
        _ = transfer_task => {
            info!("Transfer task completed, initiating shutdown");
        }
        _ = file_poller_task => {
            info!("File poller task completed, initiating shutdown");
        }
        _ = receive_task => {
            info!("Receive task completed, initiating shutdown");
        }
    }

    // Signal all tasks to shut down gracefully
    cancel_token.cancel();

    // Give tasks a moment to clean up
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    info!("Networking shutdown complete");
    Ok(())
}

/// Handle a peer discovery event.
async fn handle_peer_event(
    event: &PeerEvent,
    app_state: &AppState,
    transfer_manager: &Arc<TransferManager>,
) {
    match event {
        PeerEvent::Discovered(peer) => {
            debug!(
                peer_id = %peer.device_id,
                addresses = ?peer.addresses(),
                "Peer discovered"
            );

            // Auto-connect to the first discovered peer if not already connected
            if app_state.connected_peer().is_none() {
                connect_to_peer(peer, app_state, transfer_manager).await;
            }
        }
        PeerEvent::Lost(node_id) => {
            debug!(node_id = %node_id, "Peer lost");

            // Check if we lost the connected peer
            if let Some(connected) = app_state.connected_peer() {
                if connected.id == *node_id {
                    info!(peer_id = %node_id, "Connected peer lost, disconnecting");
                    app_state.set_connected_peer(None);
                    transfer_manager.set_connected_peer(None);
                    app_state.set_portal_state(PortalState::Idle);
                }
            }
        }
    }
}

/// Connect to a discovered peer.
async fn connect_to_peer(
    peer: &DiscoveredPeer,
    app_state: &AppState,
    transfer_manager: &Arc<TransferManager>,
) {
    info!(
        peer_id = %peer.device_id,
        addresses = ?peer.addresses(),
        "Auto-connecting to discovered peer"
    );

    // Set as connected peer
    app_state.set_connected_peer(Some(PeerInfo {
        id: peer.device_id,
        name: peer.device_name.as_deref().map(Arc::from),
    }));

    // Set the peer in transfer manager
    transfer_manager.set_connected_peer(Some(peer.to_node_addr()));

    app_state.set_portal_state(PortalState::Connected);
}

/// Handle a file send request.
async fn handle_send_request(
    request: SendFilesRequest,
    app_state: &AppState,
    transfer_manager: &Arc<TransferManager>,
) {
    let files: Vec<PathBuf> = request.files.into_vec();

    if files.is_empty() {
        return;
    }

    if app_state.connected_peer().is_none() {
        warn!("No peer connected, cannot send files");
        app_state.set_portal_state(PortalState::Idle);
        return;
    }

    info!(file_count = files.len(), "Sending files to peer");
    app_state.set_portal_state(PortalState::Transferring);

    match transfer_manager.send_files(files).await {
        Ok(handles) => {
            info!(count = handles.len(), "Started transfers");

            // Process progress updates with batching
            let mut completed_count = 0u32;
            let mut failed_count = 0u32;

            for handle in handles {
                // Collect progress updates for this transfer
                while let Some(progress) = handle.next_progress().await {
                    if progress.is_complete() {
                        info!(transfer_id = %progress.transfer_id, "Transfer completed");
                        completed_count += 1;
                        break;
                    } else if progress.is_failed() {
                        warn!(transfer_id = %progress.transfer_id, "Transfer failed");
                        failed_count += 1;
                        break;
                    }

                    // Batch progress updates to reduce lock contention
                    // Only update every N progress messages or on completion
                }
            }

            info!(
                completed = completed_count,
                failed = failed_count,
                "All transfers finished"
            );

            // Return to connected state
            app_state.set_portal_state(PortalState::Connected);
        }
        Err(e) => {
            error!(error = %e, "Failed to send files");
            app_state.set_portal_state(PortalState::Idle);
        }
    }
}

/// Initialize logging with tracing.
fn init_logging() {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("humanfileshare=info"));

    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer())
        .init();
}
