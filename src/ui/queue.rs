//! Queue screen: one table of cards and articles ordered by prio, then due.

use chrono::NaiveDate;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::Line;
use ratatui::widgets::{Cell, Row, Table, TableState};
use ratatui::Frame;

use super::AMBER;
use crate::app::App;
use crate::db::ItemRow;
use crate::vault::frontmatter::ItemType;

pub const HINTS: &[(&str, &str)] = &[
    ("j/k", "move"),
    ("enter", "open"),
    ("p", "prio"),
    ("tab", "learn"),
    ("q", "quit"),
];

pub fn render(frame: &mut Frame, area: Rect, app: &App) {
    if app.items.is_empty() {
        super::note_box(frame, area, &[Line::from("vault has no cards or articles")], None);
        return;
    }
    // Two for the gutter bar, then glyph, prio and due with their spacing.
    let title_width = (area.width as usize).saturating_sub(2 + 1 + 2 + 4 + 2 + 10 + 2);
    let today = app.today();
    let rows = app.items.iter().map(|item| row(item, today, title_width));
    let widths = [
        Constraint::Length(1),
        Constraint::Length(4),
        Constraint::Length(10),
        Constraint::Fill(1),
    ];
    let table = Table::new(rows, widths)
        .header(
            Row::new(["", "prio", "due", "title"])
                .style(Style::new().dim())
                .bottom_margin(1),
        )
        .column_spacing(2)
        .row_highlight_style(Style::new().bold())
        .highlight_symbol(Line::from("▎ ").fg(AMBER));
    let mut state = TableState::new().with_selected(Some(app.queue_sel));
    frame.render_stateful_widget(table, area, &mut state);

    // The header's bottom margin is left blank by the table; a rule goes there.
    if area.height > 1 {
        let under = Rect { y: area.y + 1, height: 1, ..area };
        frame.render_widget(Line::from("─".repeat(area.width as usize)).dim(), under);
    }
}

/// One item: kind glyph, priority, due coloured by how it sits against `today`,
/// and the title cut to `title_width`.
fn row(item: &ItemRow, today: NaiveDate, title_width: usize) -> Row<'static> {
    let glyph = Cell::from(match item.kind {
        ItemType::Card => "▪",
        ItemType::Article => "≡",
    })
    .style(Style::new().dim());
    let due = match item.due {
        None => Cell::from("now").style(Style::new().fg(AMBER)),
        Some(d) if d < today => Cell::from(d.to_string()).style(Style::new().fg(Color::LightRed)),
        Some(d) => Cell::from(d.to_string()).style(Style::new().dim()),
    };
    let t = item.title.clone().unwrap_or_else(|| item.path.clone());
    let title = if t.chars().count() > title_width {
        t.chars().take(title_width.saturating_sub(1)).collect::<String>() + "…"
    } else {
        t
    };
    Row::new([glyph, Cell::from(item.prio.to_string()), due, Cell::from(title)])
}
