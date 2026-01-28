//! Title/branding component.

use gpui::*;

use crate::ui::Theme;

/// The application title displayed in the top-left corner.
pub struct Title;

impl Title {
    pub fn new() -> Self {
        Self
    }

    pub fn render<V: 'static>(&self, cx: &Context<V>) -> impl IntoElement {
        let theme = cx.global::<Theme>();

        div()
            .font_family("Helvetica")
            .font_weight(FontWeight::EXTRA_BOLD)
            .text_size(px(16.0))
            .text_color(theme.foreground)
            .child("HumanFileShare")
    }
}

impl Default for Title {
    fn default() -> Self {
        Self::new()
    }
}
