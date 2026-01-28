//! Application theme - minimalist black and white.

use gpui::*;

/// The application's minimalist theme.
pub struct Theme {
    /// Primary background color (white).
    pub background: Hsla,
    /// Primary foreground/accent color (black).
    pub foreground: Hsla,
    /// Subtle gray for secondary elements.
    pub muted: Hsla,
}

impl Theme {
    pub fn light() -> Self {
        Self {
            background: hsla(0.0, 0.0, 1.0, 1.0), // Pure white
            foreground: hsla(0.0, 0.0, 0.0, 1.0), // Pure black
            muted: hsla(0.0, 0.0, 0.5, 1.0),      // Gray
        }
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::light()
    }
}

impl Global for Theme {}
