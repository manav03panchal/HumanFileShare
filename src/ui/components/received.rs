//! The Received Files component - displays received files on the right side.

use gpui::*;

use crate::app::AppState;
use crate::ui::Theme;

/// Window size (square).
const WINDOW_SIZE: f32 = 320.0;

/// A GPUI view for displaying received files.
pub struct ReceivedFilesView;

impl ReceivedFilesView {
    pub fn new(_cx: &mut Context<Self>) -> Self {
        Self
    }
}

impl Render for ReceivedFilesView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.global::<Theme>();
        let app_state = cx.global::<AppState>();
        let received_files = app_state.received_files();

        let half_width = WINDOW_SIZE / 2.0;

        div()
            .id("received-container")
            .absolute()
            .right(px(0.0))
            .top(px(0.0))
            .w(px(half_width))
            .h(px(WINDOW_SIZE))
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_2()
            .children(received_files.iter().enumerate().map(|(idx, file)| {
                let file_name = file.name.clone();
                let file_size = format_size(file.size);

                div()
                    .id(ElementId::Integer(idx as u64))
                    .px_3()
                    .py_2()
                    .bg(theme.foreground.opacity(0.1))
                    .rounded_md()
                    .cursor_pointer()
                    .hover(|style| style.bg(theme.foreground.opacity(0.2)))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(theme.foreground)
                                    .child(truncate_filename(&file_name, 20)),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.foreground.opacity(0.6))
                                    .child(file_size),
                            ),
                    )
            }))
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

/// Truncate filename if too long.
fn truncate_filename(name: &str, max_len: usize) -> String {
    if name.len() <= max_len {
        name.to_string()
    } else {
        let half = (max_len - 3) / 2;
        format!("{}...{}", &name[..half], &name[name.len() - half..])
    }
}
