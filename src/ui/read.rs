//! Read screen: the article body as wrapped raw markdown, an amber gutter bar on
//! the current paragraph and a dim dot on harvested ones, other paragraphs dim,
//! `#` lines bold, harvested spans dim, the selection reversed, the cursor word
//! underlined. Scrolls by whole paragraphs to keep the cursor visible.
//!
//! A paragraph that is nothing but one embed line is drawn as its media instead
//! of as text (M7): a picture in the flow with the same gutter mark, or one dim
//! placeholder row when there is no picture to draw.

use ratatui::layout::Rect;
// `Style`'s modifier helpers (`dim`, `reversed`, `underlined`) are inherent, so
// `Stylize` is no longer needed here.
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::Frame;
use ratatui_image::protocol::Protocol;
use ratatui_image::Image;

use crate::app::{App, Read};
use crate::media::Media;
use crate::vault::article::Span as Offsets;
use crate::vault::card::Embed;

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
    if area.height == 0 {
        return;
    }
    // An imported article says where it came from on the first row; the body (or,
    // for an empty article, the note box) starts one row lower. A path import has
    // no `url` and no line.
    let mut area = area;
    if let Some(url) = read.item.url.as_deref() {
        let row = Rect { height: 1, ..area };
        frame.render_widget(super::url_line(url, read.item.imported, area.width), row);
        area = Rect { y: area.y + 1, height: area.height - 1, ..area };
    }
    if read.paragraphs.is_empty() {
        super::note_box(frame, area, &[Line::from("article is empty · tab back to queue")], None);
        return;
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

    let chunks: Vec<Chunk> =
        (0..read.paragraphs.len()).map(|i| chunk_for(read, i, &marks, width)).collect();
    let heights: Vec<usize> = chunks.iter().map(|c| usize::from(rows_of(c, area))).collect();

    // Scroll by whole paragraphs: start at the cursor and take as much context above as fits.
    let height = area.height as usize;
    let mut start = read.cursor.min(chunks.len().saturating_sub(1));
    let mut used = heights.get(start).copied().unwrap_or(0);
    while start > 0 {
        let above = heights[start - 1] + 1;
        if used + above > height {
            break;
        }
        used += above;
        start -= 1;
    }

    // One blank row between paragraphs; stop as soon as the area is full.
    let mut y = area.y;
    for (i, chunk) in chunks.into_iter().enumerate().skip(start) {
        if y >= area.bottom() {
            break;
        }
        if i > start {
            y += 1;
            if y >= area.bottom() {
                break;
            }
        }
        let rows = heights[i] as u16;
        let left = area.bottom() - y;
        match chunk {
            Chunk::Text(lines) => {
                let rows = rows.min(left);
                frame.render_widget(super::wrapped(lines), Rect { y, height: rows, ..area });
                y += rows;
            }
            Chunk::Placeholder(line) => {
                let rows = rows.min(left);
                frame.render_widget(super::wrapped(vec![line]), Rect { y, height: rows, ..area });
                y += rows;
            }
            Chunk::Picture { protocol, mark, target } => {
                let size = protocol.size();
                if size.height > left {
                    // No room for the picture: back to the M0 placeholder line, and
                    // nothing after it would fit anyway.
                    let line = placeholder_line(mark, format!("[image: {target}]"));
                    frame.render_widget(super::wrapped(vec![line]), Rect { y, height: 1, ..area });
                    break;
                }
                let (text, style) = mark;
                let gutter = Rect { x: area.x, y, width: GUTTER, height: 1 };
                frame.render_widget(Line::from(Span::styled(text, style)), gutter);
                let picture = Rect {
                    x: area.x + GUTTER,
                    y,
                    width: size.width.min(width),
                    height: size.height,
                };
                frame.render_widget(Image::new(protocol), picture);
                y += size.height;
            }
        }
    }
}

/// What one paragraph draws as: its styled text, a picture, or a single dim row.
enum Chunk<'a> {
    Text(Vec<Line<'a>>),
    Picture {
        protocol: &'a Protocol,
        /// The gutter cells and their style, as a text paragraph would get them.
        mark: (&'static str, Style),
        target: &'a str,
    },
    Placeholder(Line<'a>),
}

/// Paragraph `i` as it will be drawn.
///
/// Only a paragraph that is one embed line and nothing else has media; everything
/// else is text, exactly as before. An image becomes a picture when it was encoded
/// and still fits the text width — the encoding follows the viewport, so a terminal
/// narrowed since the last encode falls back to the placeholder rather than
/// silently drawing nothing.
fn chunk_for<'a>(read: &'a Read, i: usize, marks: &Marks, text_width: u16) -> Chunk<'a> {
    let Some(media) = read.media.get(&i) else {
        return Chunk::Text(paragraph_lines(read, i, marks));
    };
    let mark = gutter_mark(read, i, marks);
    match media {
        Media::Image { target, protocol: Some(p), .. } if p.size().width <= text_width => {
            Chunk::Picture { protocol: p, mark, target }
        }
        Media::Image { target, note: Some(note), .. } => {
            Chunk::Placeholder(placeholder_line(mark, format!("[image: {target} · {note}]")))
        }
        Media::Image { target, .. } => {
            Chunk::Placeholder(placeholder_line(mark, format!("[image: {target}]")))
        }
        Media::Audio { target, .. } => {
            Chunk::Placeholder(placeholder_line(mark, format!("♪ {target}")))
        }
        Media::Other { target } => {
            let text = Embed { target: target.clone() }.placeholder();
            Chunk::Placeholder(placeholder_line(mark, text))
        }
    }
}

/// The rows `chunk` takes inside `area`, wrapped exactly as it will be drawn.
fn rows_of(chunk: &Chunk, area: Rect) -> u16 {
    match chunk {
        Chunk::Text(lines) => super::text_rows(lines, area),
        Chunk::Picture { protocol, .. } => protocol.size().height,
        Chunk::Placeholder(line) => super::text_rows(std::slice::from_ref(line), area),
    }
}

/// The gutter of a media paragraph: the same amber bar, dim dot or two blanks
/// [`paragraph_lines`] puts on a text paragraph's first line.
fn gutter_mark(read: &Read, i: usize, marks: &Marks) -> (&'static str, Style) {
    if i == read.cursor {
        return ("▎ ", Style::new().fg(super::AMBER));
    }
    let harvested = read
        .paragraphs
        .get(i)
        .is_some_and(|p| marks.children.iter().any(|c| c.start <= p.start && p.start < c.end));
    if harvested {
        ("• ", Style::new().dim())
    } else {
        ("  ", Style::new())
    }
}

/// A dim one-row stand-in for media, behind the paragraph's gutter.
fn placeholder_line(mark: (&'static str, Style), text: String) -> Line<'static> {
    Line::from(vec![Span::styled(mark.0, mark.1), Span::styled(text, Style::new().dim())])
}

struct Marks<'a> {
    selection: Option<Offsets>,
    cursor_word: Option<Offsets>,
    children: &'a [Offsets],
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
