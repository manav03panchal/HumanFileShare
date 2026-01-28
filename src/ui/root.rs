//! Root view - the main application window content.

use gpui::*;

use crate::ui::Theme;
use crate::ui::components::{PortalView, ReceivedFilesView};

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
