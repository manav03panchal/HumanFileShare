//! The Received Files component - displays received files on the right side.

use gpui::*;
use gpui_component::{Icon, IconName, Sizable};
use std::path::PathBuf;
use std::process::Command;

use crate::app::AppState;
use crate::ui::Theme;

/// A GPUI view for displaying received files.
pub struct ReceivedFilesView;

impl ReceivedFilesView {
    pub fn new(_cx: &mut Context<Self>) -> Self {
        Self
    }

    /// Open folder in Finder/Explorer
    fn open_folder(path: &PathBuf) {
        #[cfg(target_os = "macos")]
        {
            let _ = Command::new("open").arg(path).spawn();
        }
        #[cfg(target_os = "linux")]
        {
            let _ = Command::new("xdg-open").arg(path).spawn();
        }
        #[cfg(target_os = "windows")]
        {
            let _ = Command::new("explorer").arg(path).spawn();
        }
    }
}

impl Render for ReceivedFilesView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.global::<Theme>();
        let app_state = cx.global::<AppState>();
        let received_files = app_state.received_files();

        // Get actual window size for responsive layout
        let bounds = window.viewport_size();
        let half_width = bounds.width * 0.5;

        if received_files.is_empty() {
            div()
                .id("received-container")
                .absolute()
                .right(px(0.0))
                .top(px(0.0))
                .w(half_width)
                .h_full()
        } else {
            let total_files = received_files.len();
            let total_size: u64 = received_files.iter().map(|f| f.size).sum();
            let downloads_dir = dirs::download_dir().unwrap_or_else(|| PathBuf::from("."));

            div()
                .id("received-container")
                .absolute()
                .right(px(0.0))
                .top(px(0.0))
                .w(half_width)
                .h_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .id("received-clickable")
                        .flex()
                        .flex_col()
                        .items_center()
                        .cursor_pointer()
                        .on_mouse_down(MouseButton::Left, move |_event, _window, _cx| {
                            Self::open_folder(&downloads_dir);
                        })
                        .child(
                            Icon::new(IconName::Folder)
                                .large()
                                .text_color(theme.foreground.opacity(0.5)),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.foreground.opacity(0.4))
                                .mt_1()
                                .child(format!("{} files · {}", total_files, format_size(total_size))),
                        ),
                )
        }
    }
}

/// Format file size in human-readable format.
fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_size_zero_bytes() {
        assert_eq!(format_size(0), "0 B");
    }

    #[test]
    fn test_format_size_bytes() {
        assert_eq!(format_size(1), "1 B");
        assert_eq!(format_size(100), "100 B");
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1023), "1023 B");
    }

    #[test]
    fn test_format_size_kilobytes() {
        assert_eq!(format_size(1024), "1.0 KB");
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(format_size(2048), "2.0 KB");
        assert_eq!(format_size(10240), "10.0 KB");
        assert_eq!(format_size(1048575), "1024.0 KB");
    }

    #[test]
    fn test_format_size_megabytes() {
        assert_eq!(format_size(1048576), "1.0 MB");
        assert_eq!(format_size(1572864), "1.5 MB");
        assert_eq!(format_size(5242880), "5.0 MB");
        assert_eq!(format_size(1073741823), "1024.0 MB");
    }

    #[test]
    fn test_format_size_gigabytes() {
        assert_eq!(format_size(1073741824), "1.0 GB");
        assert_eq!(format_size(1610612736), "1.5 GB");
        assert_eq!(format_size(5368709120), "5.0 GB");
    }
}
