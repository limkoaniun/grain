//! Review screen: question, optional reference line, answer after reveal,
//! grade row and a status line.

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget, Wrap};
use ratatui::Frame;
use ratatui_image::protocol::Protocol;
use ratatui_image::Image;

use crate::app::{App, Phase};
use crate::media::Media;
use crate::vault::card::Segment;
use crate::vault::frontmatter::ItemMeta;

pub const HINTS: &[(&str, &str)] = &[
    ("space", "reveal"),
    ("0-5", "grade"),
    ("u", "undo"),
    ("tab", "queue"),
    ("q", "quit"),
];

/// The same with `r` after `u`, for a card that has a sound on either side.
pub const HINTS_AUDIO: &[(&str, &str)] = &[
    ("space", "reveal"),
    ("0-5", "grade"),
    ("u", "undo"),
    ("r", "replay"),
    ("tab", "queue"),
    ("q", "quit"),
];

/// The `y/n` offer at the end of the main pass.
pub const DRILL_PROMPT_HINTS: &[(&str, &str)] =
    &[("y", "drill"), ("n", "finish"), ("tab", "queue"), ("q", "quit")];

/// The drill itself: reveal and grade, but no undo — drill grades are never written.
pub const DRILL_HINTS: &[(&str, &str)] =
    &[("space", "reveal"), ("0-5", "grade"), ("tab", "queue"), ("q", "quit")];

/// The drill with `r`. There is no `u` to follow here, so `r` takes undo's place.
pub const DRILL_HINTS_AUDIO: &[(&str, &str)] = &[
    ("space", "reveal"),
    ("0-5", "grade"),
    ("r", "replay"),
    ("tab", "queue"),
    ("q", "quit"),
];

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

    // The reference lines are the question's last lines, so they hug the text instead
    // of floating at the bottom of the question area. `↳` (the parent) comes first,
    // then `↗` (where an import came from); either may stand alone.
    let (mut question, q_images) = side_lines(&cur.card.body.question, &cur.media.question, app);
    question.extend(reference_line(&cur.card.meta));
    if let Some(url) = cur.card.meta.url.as_deref() {
        question.push(super::url_line(url, cur.card.meta.imported, q_area.width));
    }
    render_side(frame, q_area, question, q_images);
    if app.review.revealed {
        frame.render_widget(Line::from("─ ─ ─").dim(), gap_area);
        let (answer, a_images) = side_lines(&cur.card.body.answer, &cur.media.answer, app);
        render_side(frame, a_area, answer, a_images);
        frame.render_widget(grade_row(), grade_area);
    }
    if let Some(status) = &app.review.status {
        frame.render_widget(Line::from(status.as_str()).dim().right_aligned(), status_area);
    }
}

/// An image waiting to be drawn: the line its placeholder would take, the encoding
/// to draw, and the target the placeholder names.
struct PendingImage<'a> {
    /// Index in the side's text lines where `[image: <target>]` goes if it cannot
    /// be drawn — the place the embed had.
    slot: usize,
    protocol: &'a Protocol,
    target: &'a str,
}

/// The text of one side, then its pictures under it.
///
/// Each image is offered the rows left under the text and the images before it.
/// One that does not fit — too wide, or past the bottom of the side — goes back to
/// being the dim `[image: x.png]` line it was in M0, in the place its embed had, so
/// the text is never pushed off the screen by a picture that cannot be drawn.
///
/// That placeholder is itself a line, which pushes every picture one row further
/// down and can cost a picture above it its room. So the two settle first — one
/// image gives up per round, and the rows are measured again — and only the
/// standing arrangement is drawn. Text and pictures then never share a row.
fn render_side(frame: &mut Frame, area: Rect, lines: Vec<Line>, images: Vec<PendingImage>) {
    let mut given_up = vec![false; images.len()];
    let (lines, drawn) = loop {
        let lines = with_placeholders(&lines, &images, &given_up);
        let mut drawn: Vec<(Rect, &Protocol)> = Vec::with_capacity(images.len());
        let mut y = area.y.saturating_add(text_rows(&lines, area));
        let mut no_room = None;
        for (i, image) in images.iter().enumerate() {
            if given_up[i] {
                continue;
            }
            let size = image.protocol.size();
            let image_area = Rect { x: area.x, y, width: size.width, height: size.height };
            if size.width <= area.width && image_area.bottom() <= area.bottom() {
                y = y.saturating_add(size.height);
                drawn.push((image_area, image.protocol));
            } else {
                no_room = Some(i);
                break;
            }
        }
        match no_room {
            Some(i) => given_up[i] = true,
            None => break (lines, drawn),
        }
    };
    frame.render_widget(wrapped(lines), area);
    for (image_area, protocol) in drawn {
        frame.render_widget(Image::new(protocol), image_area);
    }
}

/// `lines` with the dim `[image: x.png]` line back in the place of every image that
/// gave up. Slots rise with embed order, so inserting in that order keeps two
/// placeholders that share a slot in the order their embeds had.
fn with_placeholders<'a>(
    lines: &[Line<'a>],
    images: &[PendingImage<'a>],
    given_up: &[bool],
) -> Vec<Line<'a>> {
    let mut lines = lines.to_vec();
    let mut inserted = 0usize;
    for (image, gave_up) in images.iter().zip(given_up) {
        if *gave_up {
            let line = Line::from(format!("[image: {}]", image.target)).dim();
            lines.insert(image.slot + inserted, line);
            inserted += 1;
        }
    }
    lines
}

fn wrapped<'a>(lines: Vec<Line<'a>>) -> Paragraph<'a> {
    Paragraph::new(lines).wrap(Wrap { trim: false })
}

/// The rows `lines` take inside `area`, wrapped exactly as they will be drawn.
///
/// `Paragraph::line_count` is behind ratatui's unstable `rendered-line-info`
/// feature, so the widget answers instead of a second wrapping of our own: the same
/// `Paragraph` is drawn into a scratch buffer the size of the side, and the last row
/// carrying a character is the last row the text needs. Rows that stay blank are
/// free for a picture.
fn text_rows(lines: &[Line], area: Rect) -> u16 {
    if area.width == 0 || area.height == 0 {
        return 0;
    }
    let scratch_area = Rect::new(0, 0, area.width, area.height);
    let mut scratch = Buffer::empty(scratch_area);
    wrapped(lines.to_vec()).render(scratch_area, &mut scratch);
    (0..area.height)
        .rev()
        .find(|&y| {
            (0..area.width).any(|x| scratch.cell((x, y)).is_some_and(|c| c.symbol() != " "))
        })
        .map_or(0, |y| y + 1)
}

/// The lines of one side, and the images to draw under them.
///
/// Embeds pair with `media` in order; a side whose media is not loaded yet (the
/// answer before the reveal) falls back to the M0 placeholders. An image that was
/// encoded leaves no line — it is drawn as a picture — but remembers the line index
/// its placeholder takes if [`render_side`] finds no room for it.
fn side_lines<'a>(
    segments: &'a [Segment],
    media: &'a [Media],
    app: &App,
) -> (Vec<Line<'a>>, Vec<PendingImage<'a>>) {
    let mut lines: Vec<Line> = Vec::with_capacity(segments.len());
    let mut images = Vec::new();
    let mut embed = 0usize;
    for segment in segments {
        let e = match segment {
            Segment::Text(t) => {
                lines.push(Line::from(t.as_str()));
                continue;
            }
            Segment::Embed(e) => e,
        };
        let entry = media.get(embed);
        embed += 1;
        match entry {
            Some(Media::Image { target, protocol: Some(protocol), .. }) => {
                images.push(PendingImage { slot: lines.len(), protocol, target });
            }
            Some(Media::Image { target, note: Some(note), .. }) => {
                lines.push(Line::from(format!("[image: {target} · {note}]")).dim());
            }
            Some(Media::Image { target, .. }) => {
                lines.push(Line::from(format!("[image: {target}]")).dim());
            }
            Some(Media::Audio { target, path, failed }) => {
                lines.push(audio_line(app, target, path.as_deref(), *failed));
            }
            Some(Media::Other { .. }) | None => lines.push(Line::from(e.placeholder()).dim()),
        }
    }
    (lines, images)
}

/// `♪ pomelo.mp3`, dim, with what the sound is doing: amber ` · playing` while it
/// runs, and a dim reason when there is nothing to play.
fn audio_line(app: &App, target: &str, path: Option<&str>, failed: bool) -> Line<'static> {
    let mut spans = vec![Span::from(format!("♪ {target}")).dim()];
    if app.audio_playing(target) {
        spans.push(Span::from(" · playing").fg(super::AMBER));
    } else if path.is_none() {
        spans.push(Span::from(" · not found").dim());
    } else if failed {
        spans.push(Span::from(" · audio unavailable").dim());
    }
    Line::from(spans)
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
