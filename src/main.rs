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

use std::sync::Arc;
use std::thread;

use anyhow::Result;
use gpui::*;
use parking_lot::RwLock;
use tracing::{error, info, warn};
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

use humanfileshare::app::{AppState, PeerInfo, PortalState, ReceivedFile};
use humanfileshare::net::{Discovery, Endpoint, TransferManager};
use humanfileshare::ui::{RootView, Theme};

/// Window dimensions - a compact square.
const WINDOW_SIZE: f32 = 320.0;

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
                error!("Networking error: {}", e);
            }
        });
    });

    Application::new().run(move |cx: &mut App| {
        // Initialize gpui-component
        gpui_component::init(cx);

        // Set global theme
        cx.set_global(Theme::light());

        // Set global app state
        cx.set_global(app_state.clone());

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

/// Run the networking stack (endpoint, discovery, transfer manager).
async fn run_networking(app_state: AppState) -> Result<()> {
    info!("Initializing networking...");

    // Create the endpoint
    let endpoint = Arc::new(Endpoint::new().await?);
    let device_id = endpoint.device_id();
    let bound_port = endpoint.bound_port().unwrap_or(0);
    info!("Device ID: {}", device_id);
    info!("Bound port: {}", bound_port);

    // Create the transfer manager
    let transfer_manager = Arc::new(TransferManager::new(endpoint.clone()).await?);
    info!("Transfer manager initialized");

    // Store in app state
    app_state.set_endpoint(endpoint.clone());
    app_state.set_transfer_manager(transfer_manager.clone());

    // Start mDNS discovery
    let discovery = Arc::new(RwLock::new(Discovery::new(device_id, bound_port)));
    if let Err(e) = discovery.write().start() {
        warn!("Failed to start mDNS discovery: {}", e);
    } else {
        info!("mDNS discovery started");
    }

    info!("Networking initialized successfully");

    // Main networking loop
    let app_state_clone = app_state.clone();
    let transfer_manager_clone = transfer_manager.clone();
    let discovery_clone = discovery.clone();

    // Task to handle discovered peers
    let peer_task = tokio::spawn(async move {
        loop {
            // Check for discovered peers
            let peers = discovery_clone.read().known_peers();
            if !peers.is_empty() {
                // Auto-connect to the first discovered peer if not already connected
                if app_state_clone.connected_peer().is_none() {
                    let peer = &peers[0];
                    info!(
                        peer_id = %peer.device_id,
                        name = ?peer.device_name,
                        "Auto-connecting to discovered peer"
                    );

                    // Set as connected peer
                    app_state_clone.set_connected_peer(Some(PeerInfo {
                        id: peer.device_id,
                        name: peer.device_name.clone(),
                    }));

                    // Set the peer in transfer manager
                    transfer_manager_clone.set_connected_peer(Some(peer.to_node_addr()));

                    app_state_clone.set_portal_state(PortalState::Connected);
                }
            }

            tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
        }
    });

    // Task to handle pending file transfers
    let app_state_for_transfer = app_state.clone();
    let transfer_manager_for_transfer = transfer_manager.clone();

    let transfer_task = tokio::spawn(async move {
        loop {
            // Check if there are files to send
            if app_state_for_transfer.has_pending_send_files() {
                let files = app_state_for_transfer.take_pending_send_files();
                if !files.is_empty() {
                    if app_state_for_transfer.connected_peer().is_some() {
                        info!("Sending {} files to peer", files.len());
                        match transfer_manager_for_transfer.send_files(files).await {
                            Ok(handles) => {
                                info!("Started {} transfers", handles.len());
                                // Wait for transfers to complete
                                for handle in handles {
                                    if let Some(progress) = handle.next_progress().await {
                                        if progress.is_complete() {
                                            info!("Transfer {} completed", progress.transfer_id);
                                        } else if progress.is_failed() {
                                            warn!("Transfer {} failed", progress.transfer_id);
                                        }
                                    }
                                }
                                app_state_for_transfer.set_portal_state(PortalState::Connected);
                            }
                            Err(e) => {
                                error!("Failed to send files: {}", e);
                                app_state_for_transfer.set_portal_state(PortalState::Idle);
                            }
                        }
                    } else {
                        warn!("No peer connected, cannot send files");
                        app_state_for_transfer.set_portal_state(PortalState::Idle);
                    }
                }
            }

            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }
    });

    // Task to receive incoming files
    let app_state_for_receive = app_state.clone();
    let transfer_manager_for_receive = transfer_manager.clone();

    let receive_task = tokio::spawn(async move {
        let callback = Arc::new(move |name: String, path: std::path::PathBuf, size: u64| {
            info!(name = %name, size = size, "Received file");
            app_state_for_receive.add_received_file(ReceivedFile {
                name,
                path,
                size,
                received_at: std::time::Instant::now(),
            });
        });

        transfer_manager_for_receive.receive_loop(callback).await;
    });

    // Wait for all tasks
    tokio::select! {
        _ = peer_task => {},
        _ = transfer_task => {},
        _ = receive_task => {},
    }

    Ok(())
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
