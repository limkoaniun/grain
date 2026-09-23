//! Read screen: the article body as wrapped raw markdown, an amber gutter bar on
//! the current paragraph and a dim dot on harvested ones, other paragraphs dim,
//! `#` lines bold, harvested spans dim, the selection reversed, the cursor word
//! underlined. Scrolls by whole paragraphs to keep the cursor visible.

use ratatui::layout::Rect;
// `Style`'s modifier helpers (`dim`, `reversed`, `underlined`) are inherent, so
// `Stylize` is no longer needed here.
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::Frame;

use crate::app::{App, Read};
use crate::vault::article::Span as Offsets;

pub const HINTS: &[(&str, &str)] = &[
    ("j/k", "¶"),
    ("w/b", "word"),
    ("v", "mark"),
    ("^x", "extract"),
    ("^z", "cloze"),
    ("enter", "next"),
    ("d", "done"),
    // `p` opens the priority prompt here too, but the row is full at 80 columns
    // and the queue already shows `p prio`.
    ("u", "undo"),
];

pub const SELECT_HINTS: &[(&str, &str)] = &[
    ("w/b", ""),
    ("j/k", "extend"),
    ("^x", "extract"),
    ("^z", "cloze"),
    ("esc", "cancel"),
];

const GUTTER: u16 = 2;

pub fn render(frame: &mut Frame, area: Rect, app: &App) {
    let Some(read) = &app.read else {
        return;
    };
    if read.paragraphs.is_empty() {
        super::note_box(frame, area, &[Line::from("article is empty · tab back to queue")], None);
        return;
    }
    if area.height == 0 {
        return;
    }
    // An imported article says where it came from on the first row; the body starts
    // one row lower. A path import has no `url` and no line.
    let mut area = area;
    if let Some(url) = read.item.url.as_deref() {
        let row = Rect { height: 1, ..area };
        frame.render_widget(super::url_line(url, read.item.imported, area.width), row);
        area = Rect { y: area.y + 1, height: area.height - 1, ..area };
    }
    if area.width <= GUTTER || area.height == 0 {
        return;
    }
    let width = area.width - GUTTER;
    let marks = Marks {
        selection: read.selected_span(),
        cursor_word: read.current_word(),
        children: &read.children,
    };

    // Scroll by whole paragraphs: start at the cursor and take as much context above as fits.
    let rows_of = |i: usize| -> usize {
        paragraph_lines(read, i, &marks)
            .iter()
            .map(|l| wrapped_rows(l.width(), width))
            .sum()
    };
    let height = area.height as usize;
    let mut start = read.cursor;
    let mut used = rows_of(start);
    while start > 0 {
        let above = rows_of(start - 1) + 1;
        if used + above > height {
            break;
        }
        used += above;
        start -= 1;
    }

    let mut lines: Vec<Line> = Vec::new();
    for i in start..read.paragraphs.len() {
        if i > start {
            lines.push(Line::default());
        }
        lines.extend(paragraph_lines(read, i, &marks));
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

struct Marks<'a> {
    selection: Option<Offsets>,
    cursor_word: Option<Offsets>,
    children: &'a [Offsets],
}

/// Rows a line of `width_chars` takes when word-wrapped into `cols`, with one row of
/// slack for the space word wrapping wastes.
fn wrapped_rows(width_chars: usize, cols: u16) -> usize {
    let cols = usize::from(cols.max(1));
    if width_chars <= cols {
        1
    } else {
        width_chars.div_ceil(cols) + 1
    }
}

/// One `Line` per source line of paragraph `i`, styled per character offset.
fn paragraph_lines<'a>(read: &'a Read, i: usize, marks: &Marks) -> Vec<Line<'a>> {
    let Some(p) = read.paragraphs.get(i) else {
        return Vec::new();
    };
    let current = i == read.cursor;
    let harvested = marks.children.iter().any(|c| c.start <= p.start && p.start < c.end);
    let text = p.text(&read.body);
    let mut out = Vec::new();
    let mut off = p.start;
    for (n, line) in text.split('\n').enumerate() {
        let mut spans: Vec<Span> = Vec::with_capacity(4);
        // Two cells on every line, so `wrapped_rows` keeps its estimate.
        let (gutter, gutter_style) = if current {
            ("▎ ", Style::new().fg(super::AMBER))
        } else if n == 0 && harvested {
            ("• ", Style::new().dim())
        } else {
            ("  ", Style::new())
        };
        spans.push(Span::styled(gutter, gutter_style));
        let heading = line.trim_start().starts_with('#');
        spans.extend(styled_runs(line, off, marks).into_iter().map(|mut span| {
            if !current {
                span.style = span.style.add_modifier(Modifier::DIM);
            }
            if heading {
                span.style = span.style.add_modifier(Modifier::BOLD);
            }
            span
        }));
        out.push(Line::from(spans));
        off += line.chars().count() + 1;
    }
    out
}

/// Split `line` (starting at character offset `start`) into runs of equal style.
fn styled_runs<'a>(line: &'a str, start: usize, marks: &Marks) -> Vec<Span<'a>> {
    let style_at = |off: usize| -> Style {
        let inside = |s: &Offsets| s.start <= off && off < s.end;
        let mut style = Style::new();
        if marks.children.iter().any(inside) {
            style = style.dim();
        }
        if marks.selection.as_ref().is_some_and(inside) {
            style = style.reversed();
        } else if marks.cursor_word.as_ref().is_some_and(inside) {
            style = style.underlined();
        }
        style
    };
    let mut runs = Vec::new();
    let mut run_start_byte = 0usize;
    let mut run_style: Option<Style> = None;
    for (k, (byte, _)) in line.char_indices().enumerate() {
        let style = style_at(start + k);
        match run_style {
            Some(s) if s == style => {}
            Some(s) => {
                runs.push(Span::styled(&line[run_start_byte..byte], s));
                run_start_byte = byte;
                run_style = Some(style);
            }
            None => run_style = Some(style),
        }
    }
    if let Some(s) = run_style {
        runs.push(Span::styled(&line[run_start_byte..], s));
    }
    runs
}
