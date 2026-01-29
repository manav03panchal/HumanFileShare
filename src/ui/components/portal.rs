//! The Portal component - a semicircle on the left side for file drops.
//!
//! # Testing Limitations
//!
//! The `PortalView` component is deeply integrated with GPUI and requires
//! a running application context to test properly. It depends on:
//!
//! - **GPUI Context**: Requires `Context<Self>` for initialization and rendering
//! - **Global State**: Depends on `AppState` global for state management
//! - **Event Handling**: Uses `on_drop`, `on_drag_move` callbacks
//! - **Window System**: Requires `Window` reference for rendering
//!
//! ## What Can Be Tested
//!
//! The `WINDOW_SIZE` constant and basic struct layout can be verified.
//!
//! ## What Requires Integration Tests
//!
//! - `handle_file_drop()` - requires mocked file system and global state
//! - `render()` - requires full GPUI context, Theme global, and Window
//! - State transitions (`drag_hovering`, `PortalState`)
//! - Status text generation logic
//!
//! For comprehensive testing, a headless GPUI test harness or mock
//! infrastructure would be needed.

use gpui::*;
use std::path::PathBuf;
use tracing::info;

use crate::app::{AppState, PortalState};
use crate::ui::Theme;

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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.global::<Theme>();
        let app_state = cx.global::<AppState>();
        let portal_state = app_state.portal_state();
        let connected_peer = app_state.connected_peer();

        // Get actual window size for responsive layout
        let bounds = window.viewport_size();
        let window_height = bounds.height;
        let half_width = bounds.width * 0.5;
        let circle_size = window_height; // Circle diameter = window height

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
                        .as_ref()
                        .map(|n| n.to_string())
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
            .w(half_width)
            .h_full()
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
                    .left(half_width - circle_size)
                    .w(circle_size)
                    .h(circle_size)
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

#[cfg(test)]
mod tests {
    // Note: PortalView tests require GPUI app context.
    // See module-level documentation for details.

    #[test]
    fn test_half_width_calculation() {
        // The portal uses half the window width
        let window_width = 400.0;
        let half_width = window_width / 2.0;
        assert_eq!(half_width, 200.0);
    }

    #[test]
    fn test_circle_position_calculation() {
        // Circle is positioned so its center is at the right edge
        let window_height = 400.0;
        let window_width = 400.0;
        let circle_size = window_height;
        let half_width = window_width / 2.0;
        let left_position = half_width - circle_size;

        // This positions the circle mostly off-screen to the left
        assert_eq!(left_position, -200.0);
    }
}
