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
        let keys = Line::from(vec![
            Span::from("y").fg(super::AMBER),
            Span::from(" drill").dim(),
            Span::from("        "),
            Span::from("n").fg(super::AMBER),
            Span::from(" finish").dim(),
        ]);
        super::note_box(
            frame,
            area,
            &[Line::from(app.drill_prompt()), Line::default(), keys],
            None,
        );
        return;
    }
    let Some(cur) = &app.review.current else {
        // `nothing more to learn · 4 graded · 2 read` becomes a bold title over dim counts.
        let finish = app.finish_line();
        let (title, counts) = match finish.split_once(" · ") {
            Some((title, counts)) => (title.to_string(), counts.to_string()),
            None => (finish, String::new()),
        };
        super::note_box(
            frame,
            area,
            &[Line::from(title).bold(), Line::from(counts).dim()],
            app.review.status.as_deref(),
        );
        return;
    };

    // Two columns of breathing room on the left; the card is the only screen that indents.
    let area = Rect { x: area.x + 2, width: area.width.saturating_sub(2), ..area };
    let [q_area, gap_area, a_area, grade_area, status_area] = area.layout(&Layout::vertical([
        Constraint::Fill(3),
        Constraint::Length(1),
        Constraint::Fill(2),
        Constraint::Length(1),
        Constraint::Length(1),
    ]));

    // The reference line is the question's last line, so it hugs the text instead of
    // floating at the bottom of the question area.
    let mut question = lines(&cur.card.body.question);
    question.extend(reference_line(&cur.card.meta));
    frame.render_widget(Paragraph::new(question).wrap(Wrap { trim: false }), q_area);
    if app.review.revealed {
        frame.render_widget(Line::from("─ ─ ─").dim(), gap_area);
        frame.render_widget(paragraph(&cur.card.body.answer), a_area);
        frame.render_widget(grade_row(), grade_area);
    }
    if let Some(status) = &app.review.status {
        frame.render_widget(Line::from(status.as_str()).dim().right_aligned(), status_area);
    }
}

/// Bare text lines; embeds become dim placeholders such as `[image: pomelo.png]`.
fn lines(segments: &[Segment]) -> Vec<Line<'_>> {
    segments
        .iter()
        .map(|s| match s {
            Segment::Text(t) => Line::from(t.as_str()),
            Segment::Embed(e) => Line::from(e.placeholder()).dim(),
        })
        .collect()
}

fn paragraph(segments: &[Segment]) -> Paragraph<'_> {
    Paragraph::new(lines(segments)).wrap(Wrap { trim: false })
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

/// `0 null   1 bad   2 fail   │   3 pass   4 good   5 bright`: amber digits, the
/// failing half red-ish and the passing half green-ish, split by a dim bar.
fn grade_row() -> Line<'static> {
    let mut spans = Vec::with_capacity(17);
    for (i, (key, label)) in
        [("0", " null"), ("1", " bad"), ("2", " fail"), ("3", " pass"), ("4", " good"), ("5", " bright")]
            .into_iter()
            .enumerate()
    {
        match i {
            0 => {}
            3 => spans.push(Span::from("   │   ").dim()),
            _ => spans.push(Span::from("   ")),
        }
        spans.push(Span::from(key).fg(super::AMBER));
        let half = if i < 3 { Color::LightRed } else { Color::LightGreen };
        spans.push(Span::from(label).fg(half));
    }
    Line::from(spans)
}
