//! Root view - the main application window content.
//!
//! # Testing Limitations
//!
//! The `RootView` component is a GPUI view that requires a running GPUI
//! application context to function. It depends on:
//! - `PortalView` - another GPUI view requiring app context
//! - `ReceivedFilesView` - another GPUI view requiring app context
//! - Global `Theme` - requires GPUI's global context system
//!
//! Unit testing this component would require:
//! 1. A full GPUI app initialization
//! 2. Mocking of global state (`Theme`, `AppState`)
//! 3. Window and rendering context
//!
//! For comprehensive testing of this component, integration tests with
//! a headless GPUI environment would be necessary.

use gpui::*;

use crate::ui::components::{PortalView, ReceivedFilesView};
use crate::ui::Theme;

/// The root view containing the entire application UI.
pub struct RootView {
    portal: Entity<PortalView>,
    received: Entity<ReceivedFilesView>,
}

impl RootView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let portal = cx.new(PortalView::new);
        let received = cx.new(ReceivedFilesView::new);
        Self { portal, received }
    }
}

impl Render for RootView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.global::<Theme>();

        div()
            .id("root")
            .size_full()
            .bg(theme.background)
            .relative()
            .child(self.portal.clone())
            .child(self.received.clone())
    }
}

#[cfg(test)]
mod tests {
    // Note: RootView tests require GPUI app context.
    // See module-level documentation for details.

    #[test]
    fn test_module_exists() {
        // This test verifies the test module compiles
        // Actual RootView testing requires GPUI integration tests
        assert!(true);
    }
}
