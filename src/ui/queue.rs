//! Queue screen: one table of cards and articles ordered by prio, then due.

use ratatui::layout::{Constraint, Rect};
use ratatui::style::Style;
use ratatui::widgets::{Row, Table, TableState};
use ratatui::Frame;

use crate::app::App;
use crate::db::ItemRow;
use crate::vault::frontmatter::ItemType;

pub const HINTS: &[(&str, &str)] = &[
    ("j/k", "move"),
    ("enter", "open"),
    ("tab", "review"),
    ("q", "quit"),
];

pub fn render(frame: &mut Frame, area: Rect, app: &App) {
    if app.items.is_empty() {
        super::centered_note(frame, area, "vault has no cards or articles");
        return;
    }
    let rows = app.items.iter().map(row);
    let widths = [
        Constraint::Length(7),
        Constraint::Length(4),
        Constraint::Length(10),
        Constraint::Fill(1),
    ];
    let table = Table::new(rows, widths)
        .header(Row::new(["type", "prio", "due", "title"]).style(Style::new().dim()))
        .column_spacing(2)
        .row_highlight_style(Style::new().bold().reversed())
        .highlight_symbol("▸ ");
    let mut state = TableState::new().with_selected(Some(app.queue_sel));
    frame.render_stateful_widget(table, area, &mut state);
}

fn row(item: &ItemRow) -> Row<'_> {
    let kind = item.kind.as_str();
    let due = match (item.kind, item.due) {
        (ItemType::Article, _) => "—".to_string(),
        (ItemType::Card, None) => "now".to_string(),
        (ItemType::Card, Some(d)) => d.to_string(),
    };
    let title = item.title.clone().unwrap_or_else(|| item.path.clone());
    Row::new([kind.to_string(), item.prio.to_string(), due, title])
}
