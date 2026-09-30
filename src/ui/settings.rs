//! Settings screen (M10): the four config keys, one row each, then one blank row,
//! the file the edits are saved to, and — while an edit was refused — the reason,
//! which cannot use the status row because the prompt is still in it. A pure
//! function of `&App`; SuperMemo's Tools : Options without the dialog.

use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::{Line, Span};
use ratatui::Frame;

use super::AMBER;
use crate::app::App;

pub const HINTS: &[(&str, &str)] =
    &[("j/k", "move"), ("enter", "edit"), ("o/esc", "back"), ("q", "quit")];

/// Cells the dim label takes before its value; the stats screen uses the same width.
const LABEL_WIDTH: usize = 13;

pub fn render(frame: &mut Frame, area: Rect, app: &App) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let bottom = area.y.saturating_add(area.height);
    let rows = app.settings_rows();
    let sel = app.settings_sel();
    for (i, (label, value, note)) in rows.iter().enumerate() {
        let y = area.y.saturating_add(i as u16);
        if y >= bottom {
            return;
        }
        let row = Rect { y, height: 1, ..area };
        frame.render_widget(setting_line(i == sel, label, value, note), row);
    }
    // One blank row, then the file the edits go to.
    let y = area.y.saturating_add(rows.len() as u16 + 1);
    if y >= bottom {
        return;
    }
    let row = Rect { y, height: 1, ..area };
    frame.render_widget(Line::from(app.settings_footer()).dim(), row);

    // A refused value keeps the prompt in the status row, so a notice cannot go there:
    // the reason sits under the footer instead, until the next key clears it.
    let Some(notice) = app.notice.as_deref() else {
        return;
    };
    let y = y.saturating_add(1);
    if y >= bottom {
        return;
    }
    let row = Rect { y, height: 1, ..area };
    frame.render_widget(Line::from(notice.to_string()).fg(AMBER), row);
}

/// `▎ vault        vault  next launch`: an amber gutter mark on the selected row, the
/// dim label padded to `LABEL_WIDTH`, the value plain, then the dim note when there is one.
fn setting_line(selected: bool, label: &str, value: &str, note: &str) -> Line<'static> {
    let marker = if selected {
        Span::from("▎ ").fg(AMBER)
    } else {
        Span::from("  ")
    };
    let mut spans = vec![
        marker,
        Span::from(format!("{label:<width$}", width = LABEL_WIDTH)).dim(),
        Span::from(value.to_string()),
    ];
    if !note.is_empty() {
        spans.push(Span::from("  "));
        spans.push(Span::from(note.to_string()).dim());
    }
    Line::from(spans)
}
