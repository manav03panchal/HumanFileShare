//! Title/branding component.
//!
//! # Testing Limitations
//!
//! The `Title` component relies on GPUI's `Context` and `IntoElement` traits
//! which require a running GPUI application context. Unit testing the `render`
//! method directly is not feasible without a full GPUI app initialization.
//!
//! Tests in this module focus on the testable logic:
//! - Struct instantiation and default implementations
//! - Basic property verification

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_title_new() {
        let title = Title::new();
        // Title is a unit struct, so we can only verify it was created
        // The actual rendering requires GPUI context
        let _ = title;
    }

    #[test]
    fn test_title_default() {
        let title_default: Title = Default::default();
        let title_new = Title::new();

        // Both should create equivalent instances (unit struct)
        let _ = (title_default, title_new);
    }

    #[test]
    fn test_title_multiple_instances() {
        // Title should be freely cloneable (as it's a unit struct)
        let title1 = Title::new();
        let title2 = Title::new();
        let title_default: Title = Default::default();

        // All instances are equivalent
        let _ = (title1, title2, title_default);
    }
}
