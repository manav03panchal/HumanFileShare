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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_theme_light_background() {
        let theme = Theme::light();
        // White: h=0.0, s=0.0, l=1.0, a=1.0
        assert_eq!(theme.background.h, 0.0);
        assert_eq!(theme.background.s, 0.0);
        assert_eq!(theme.background.l, 1.0);
        assert_eq!(theme.background.a, 1.0);
    }

    #[test]
    fn test_theme_light_foreground() {
        let theme = Theme::light();
        // Black: h=0.0, s=0.0, l=0.0, a=1.0
        assert_eq!(theme.foreground.h, 0.0);
        assert_eq!(theme.foreground.s, 0.0);
        assert_eq!(theme.foreground.l, 0.0);
        assert_eq!(theme.foreground.a, 1.0);
    }

    #[test]
    fn test_theme_light_muted() {
        let theme = Theme::light();
        // Gray: h=0.0, s=0.0, l=0.5, a=1.0
        assert_eq!(theme.muted.h, 0.0);
        assert_eq!(theme.muted.s, 0.0);
        assert_eq!(theme.muted.l, 0.5);
        assert_eq!(theme.muted.a, 1.0);
    }

    #[test]
    fn test_theme_default() {
        let default_theme = Theme::default();
        let light_theme = Theme::light();

        // Default should be same as light theme
        assert_eq!(default_theme.background.h, light_theme.background.h);
        assert_eq!(default_theme.background.s, light_theme.background.s);
        assert_eq!(default_theme.background.l, light_theme.background.l);
        assert_eq!(default_theme.background.a, light_theme.background.a);

        assert_eq!(default_theme.foreground.h, light_theme.foreground.h);
        assert_eq!(default_theme.foreground.s, light_theme.foreground.s);
        assert_eq!(default_theme.foreground.l, light_theme.foreground.l);
        assert_eq!(default_theme.foreground.a, light_theme.foreground.a);

        assert_eq!(default_theme.muted.h, light_theme.muted.h);
        assert_eq!(default_theme.muted.s, light_theme.muted.s);
        assert_eq!(default_theme.muted.l, light_theme.muted.l);
        assert_eq!(default_theme.muted.a, light_theme.muted.a);
    }

    #[test]
    fn test_theme_opacity_method() {
        let theme = Theme::light();

        // Test opacity method on colors
        let semi_transparent = theme.foreground.opacity(0.5);
        assert_eq!(semi_transparent.h, theme.foreground.h);
        assert_eq!(semi_transparent.s, theme.foreground.s);
        assert_eq!(semi_transparent.l, theme.foreground.l);
        assert_eq!(semi_transparent.a, 0.5);

        let fully_transparent = theme.background.opacity(0.0);
        assert_eq!(fully_transparent.a, 0.0);

        let fully_opaque = theme.muted.opacity(1.0);
        assert_eq!(fully_opaque.a, 1.0);
    }
}
