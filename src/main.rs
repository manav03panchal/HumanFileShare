//! HumanFileShare - Cross-platform file sharing made simple.
//!
//! A minimalist application that makes sharing files between devices
//! as simple as possible.

use std::sync::Arc;
use std::thread;

use anyhow::Result;
use gpui::*;
use tracing::{error, info};
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

use humanfileshare::app::AppState;
use humanfileshare::net::{Endpoint, TransferManager};
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
            if let Err(e) = init_networking(app_state_for_net).await {
                error!("Failed to initialize networking: {}", e);
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

/// Initialize the networking stack (endpoint + transfer manager).
async fn init_networking(app_state: AppState) -> Result<()> {
    info!("Initializing networking...");

    // Create the endpoint
    let endpoint = Arc::new(Endpoint::new().await?);
    info!("Device ID: {}", endpoint.device_id());

    // Create the transfer manager
    let transfer_manager = Arc::new(TransferManager::new(endpoint.clone()).await?);
    info!("Transfer manager initialized");

    // Store in app state
    app_state.set_endpoint(endpoint);
    app_state.set_transfer_manager(transfer_manager);

    info!("Networking initialized successfully");

    // Keep the runtime alive
    loop {
        tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
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
