//! Review screen: question, optional reference line, answer after reveal,
//! grade row and a status line.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::Frame;

use crate::app::{App, Phase};
use crate::vault::card::Segment;
use crate::vault::frontmatter::ItemMeta;

pub const HINTS: &[(&str, &str)] = &[
    ("space", "reveal"),
    ("0-5", "grade"),
    ("u", "undo"),
    ("tab", "queue"),
    ("q", "quit"),
];

/// The `y/n` offer at the end of the main pass.
pub const DRILL_PROMPT_HINTS: &[(&str, &str)] =
    &[("y", "drill"), ("n", "finish"), ("tab", "queue"), ("q", "quit")];

/// The drill itself: reveal and grade, but no undo — drill grades are never written.
pub const DRILL_HINTS: &[(&str, &str)] =
    &[("space", "reveal"), ("0-5", "grade"), ("tab", "queue"), ("q", "quit")];

/// The finish line: nothing left to reveal, grade or undo.
pub const DONE_HINTS: &[(&str, &str)] = &[("tab", "queue"), ("q", "quit")];

pub fn render(frame: &mut Frame, area: Rect, app: &App) {
    if app.review.phase == Phase::DrillPrompt {
        super::centered_note(frame, area, &app.drill_prompt());
        return;
    }
    let Some(cur) = &app.review.current else {
        let msg = match &app.review.status {
            Some(status) => format!("{} · {status}", app.finish_line()),
            None => app.finish_line(),
        };
        super::centered_note(frame, area, &msg);
        return;
    };

    let [q_area, ref_area, _gap, a_area, grade_area, status_area] = area.layout(&Layout::vertical([
        Constraint::Fill(3),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Fill(2),
        Constraint::Length(1),
        Constraint::Length(1),
    ]));

    frame.render_widget(paragraph(&cur.card.body.question), q_area);
    if let Some(line) = reference_line(&cur.card.meta) {
        frame.render_widget(line, ref_area);
    }
    if app.review.revealed {
        frame.render_widget(paragraph(&cur.card.body.answer), a_area);
        frame.render_widget(grade_row(), grade_area);
    }
    if let Some(status) = &app.review.status {
        frame.render_widget(Line::from(status.as_str()).dim(), status_area);
    }
}

/// Bare text lines; embeds become dim placeholders such as `[image: pomelo.png]`.
fn paragraph(segments: &[Segment]) -> Paragraph<'_> {
    let lines: Vec<Line> = segments
        .iter()
        .map(|s| match s {
            Segment::Text(t) => Line::from(t.as_str()),
            Segment::Embed(e) => Line::from(e.placeholder()).dim(),
        })
        .collect();
    Paragraph::new(lines).wrap(Wrap { trim: false })
}

/// `↳ citrus-vocab.md › 2210-2380` (blue, dim) when the card has a `source`.
fn reference_line(meta: &ItemMeta) -> Option<Line<'static>> {
    let source = meta.source.as_deref()?;
    let name = source
        .trim()
        .trim_start_matches("[[")
        .trim_end_matches("]]")
        .split('|')
        .next()
        .unwrap_or_default()
        .trim();
    if name.is_empty() {
        return None;
    }
    let file = if name.contains('.') {
        name.to_string()
    } else {
        format!("{name}.md")
    };
    let mut text = format!("↳ {file}");
    if let Some(range) = meta.range.as_deref().map(str::trim).filter(|r| !r.is_empty()) {
        text.push_str(&format!(" › {range}"));
    }
    Some(Line::from(text).style(Style::new().fg(Color::Blue).dim()))
}

/// `[0 null] [1 bad] [2 fail]   [3 pass] [4 good] [5 bright]`, red-ish then green-ish.
fn grade_row() -> Line<'static> {
    Line::from(vec![
        Span::from("[0 null] [1 bad] [2 fail]").fg(Color::LightRed),
        Span::from("   "),
        Span::from("[3 pass] [4 good] [5 bright]").fg(Color::LightGreen),
    ])
}
