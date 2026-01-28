//! The Portal component - a semicircle on the left side for file drops.

use gpui::*;
use std::path::PathBuf;
use tracing::info;

use crate::app::{AppState, PortalState};
use crate::ui::Theme;

/// Window size (square).
const WINDOW_SIZE: f32 = 320.0;

/// A GPUI view for the Portal - left semicircle drop zone.
pub struct PortalView {
    /// Whether a drag is currently hovering over the portal
    drag_hovering: bool,
}

impl PortalView {
    pub fn new(_cx: &mut Context<Self>) -> Self {
        Self {
            drag_hovering: false,
        }
    }

    /// Handle files being dropped onto the portal
    fn handle_file_drop(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        if paths.is_empty() {
            return;
        }

        info!("Files dropped onto portal: {:?}", paths);

        // Queue files for sending via the global app state
        let app_state = cx.global::<AppState>();
        app_state.queue_files_for_send(paths);

        self.drag_hovering = false;
        cx.notify();
    }
}

impl Render for PortalView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.global::<Theme>();
        let app_state = cx.global::<AppState>();
        let portal_state = app_state.portal_state();
        let connected_peer = app_state.connected_peer();

        // Left half of window, containing a circle positioned so only right half shows
        let circle_size = WINDOW_SIZE;
        let half_width = WINDOW_SIZE / 2.0;

        // Adjust color based on state
        let bg_color = match (self.drag_hovering, portal_state) {
            (true, _) => theme.foreground.opacity(0.7), // Lighter when hovering
            (_, PortalState::Transferring) => theme.foreground.opacity(0.5), // Dimmed during transfer
            (_, PortalState::Connected) => theme.foreground, // Full color when connected
            (_, PortalState::Searching) => theme.foreground.opacity(0.8), // Slightly dimmed when searching
            _ => theme.foreground.opacity(0.6), // Dimmed when idle/not connected
        };

        // Status text at bottom
        let status_text = match portal_state {
            PortalState::Idle => {
                if connected_peer.is_some() {
                    "Ready".to_string()
                } else {
                    "Searching...".to_string()
                }
            }
            PortalState::Searching => "Searching...".to_string(),
            PortalState::Connected => {
                if let Some(peer) = &connected_peer {
                    peer.name
                        .clone()
                        .unwrap_or_else(|| format!("{}...", &peer.id.to_string()[..8]))
                } else {
                    "Connected".to_string()
                }
            }
            PortalState::Transferring => "Sending...".to_string(),
        };

        div()
            .id("portal-container")
            .absolute()
            .left(px(0.0))
            .top(px(0.0))
            .w(px(half_width))
            .h(px(WINDOW_SIZE))
            .overflow_hidden()
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _window, cx| {
                let file_paths: Vec<PathBuf> = paths.paths().to_vec();
                this.handle_file_drop(file_paths, cx);
            }))
            .on_drag_move(cx.listener(|this, _: &DragMoveEvent<()>, _window, cx| {
                if !this.drag_hovering {
                    this.drag_hovering = true;
                    cx.notify();
                }
            }))
            .child(
                // Circle positioned so its center is at the right edge of this container
                div()
                    .id("portal-circle")
                    .absolute()
                    .top(px(0.0))
                    .left(px(half_width - circle_size))
                    .w(px(circle_size))
                    .h(px(circle_size))
                    .rounded_full()
                    .bg(bg_color)
                    .cursor_pointer(),
            )
            .child(
                // Status indicator at bottom left
                div()
                    .absolute()
                    .bottom(px(12.0))
                    .left(px(12.0))
                    .text_xs()
                    .text_color(theme.background)
                    .font_weight(FontWeight::MEDIUM)
                    .child(status_text),
            )
    }
}
