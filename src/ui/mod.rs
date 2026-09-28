//! Rendering. Every screen is the same three regions joined by two thin rule
//! lines: status row, rule, content, rule, key hints. No bordered panels, no
//! titles; the only `Block` in the crate is the rounded box in `note_box`.
//! Screens are pure functions of `&App`.

mod queue;
mod read;
mod review;
mod stats;

use chrono::NaiveDate;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Padding, Paragraph, Widget, Wrap};
use ratatui::Frame;

use crate::app::{App, Phase, Screen, TextStage};

/// Amber for key letters in the hints row (256-color index, works without truecolor).
pub const AMBER: Color = Color::Indexed(214);
pub const COLLECTION_NAME: &str = "all";
/// The caret drawn at the end of an open text prompt (U+258F).
const CARET: char = '▏';

pub fn render(frame: &mut Frame, app: &App) {
    let [top, rule_a, content, rule_b, hints] = frame
        .area()
        .layout(&Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Fill(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ]));
    rule(frame, rule_a);
    rule(frame, rule_b);

    // The status row's middle: the open prompt, else a notice, else the selection.
    // A text prompt is being typed into, so it carries the caret; the priority
    // prompt has no insertion point and stays as it is.
    let stage = app.text_prompt_stage();
    let middle = app
        .prompt_text()
        .map(|t| if stage.is_some() { format!("{t}{CARET}") } else { t })
        .or_else(|| app.notice.clone())
        .or_else(|| app.selection_status());
    let prompt_open = app.prompt_text().is_some();
    // A text prompt takes the hints row on every screen.
    let text_hints = stage.map(|stage| match stage {
        TextStage::Question => TEXT_PROMPT_NEXT_HINTS,
        TextStage::Final => TEXT_PROMPT_SAVE_HINTS,
    });
    match app.screen {
        Screen::Queue => {
            status_row(frame, top, middle.as_deref(), &app.queue_context(), app.progress());
            queue::render(frame, content, app);
            let base = if prompt_open { PROMPT_HINTS } else { queue::HINTS };
            hints_row(frame, hints, text_hints.unwrap_or(base));
        }
        Screen::Review => {
            status_row(frame, top, middle.as_deref(), &app.review_context(), app.progress());
            review::render(frame, content, app);
            hints_row(frame, hints, text_hints.unwrap_or_else(|| review_hints(app)));
        }
        Screen::Read => {
            status_row(frame, top, middle.as_deref(), &app.read_context(), app.progress());
            read::render(frame, content, app);
            let base = if prompt_open {
                PROMPT_HINTS
            } else if app.selection_status().is_some() {
                read::SELECT_HINTS
            } else {
                read::HINTS
            };
            hints_row(frame, hints, text_hints.unwrap_or(base));
        }
        // No prompt can be open on the stats screen, so the hints never change.
        Screen::Stats => {
            status_row(frame, top, middle.as_deref(), &app.stats_context(), app.progress());
            stats::render(frame, content, app);
            hints_row(frame, hints, stats::HINTS);
        }
    }
}

/// The review hints follow the session: the drill prompt, the drill, the finish
/// line, or a card in the main pass. A card with a sound on either side gains
/// `r replay`; `r` works in the drill too, so the drill row gains it as well.
fn review_hints(app: &App) -> &'static [(&'static str, &'static str)] {
    let audio = app.card_has_audio();
    match (app.review.phase, app.review.current.is_some()) {
        (Phase::DrillPrompt, _) => review::DRILL_PROMPT_HINTS,
        (Phase::Drilling, _) if audio => review::DRILL_HINTS_AUDIO,
        (Phase::Drilling, _) => review::DRILL_HINTS,
        (Phase::Main, false) => review::DONE_HINTS,
        (Phase::Main, true) if audio => review::HINTS_AUDIO,
        (Phase::Main, true) => review::HINTS,
    }
}

/// Hints while the priority prompt is open, on any screen.
pub const PROMPT_HINTS: &[(&str, &str)] = &[("0-9", "value"), ("j/k", "nudge"), ("enter", "set"), ("esc", "cancel")];

/// Hints on a text prompt step that leads to another step (the card's question).
pub const TEXT_PROMPT_NEXT_HINTS: &[(&str, &str)] = &[("enter", "next"), ("esc", "cancel")];

/// Hints on the last step of a text prompt (the card's answer, an import).
pub const TEXT_PROMPT_SAVE_HINTS: &[(&str, &str)] = &[("enter", "save"), ("esc", "cancel")];

/// `↗ en.wikipedia.org/wiki/Pomelo · 2026-09-20` (blue, dim): where an item was
/// imported from and when. The scheme is dropped; the line is cut to `width`
/// characters with `…` as the last one when it does not fit.
pub(super) fn url_line(url: &str, imported: Option<NaiveDate>, width: u16) -> Line<'static> {
    let width = width as usize;
    if width == 0 {
        return Line::default();
    }
    let short = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    let mut text = format!("↗ {short}");
    if let Some(day) = imported {
        text.push_str(&format!(" · {day}"));
    }
    if text.chars().count() > width {
        text = text.chars().take(width - 1).collect();
        text.push('…');
    }
    Line::from(text).style(Style::new().fg(Color::Blue).dim())
}

/// One dim horizontal rule filling `area`. A line of `─`, never a `Block`.
fn rule(frame: &mut Frame, area: Rect) {
    frame.render_widget(Line::from("─".repeat(area.width as usize)).dim(), area);
}

/// Collection name and the session progress bar left, optional notice in the
/// middle, screen context right (dim).
fn status_row(
    frame: &mut Frame,
    area: Rect,
    notice: Option<&str>,
    context: &str,
    progress: Option<(usize, usize)>,
) {
    let mut left_spans = vec![Span::from(COLLECTION_NAME).bold()];
    if let Some((reached, len)) = progress.filter(|&(_, len)| len > 0) {
        // At most twenty cells; `reached` rounds to the nearest one.
        let cells = len.min(20);
        let filled = (reached * cells + len / 2) / len;
        left_spans.push(Span::from("  "));
        left_spans.push(Span::from("▮".repeat(filled)).fg(AMBER));
        left_spans.push(Span::from("▯".repeat(cells.saturating_sub(filled))).dim());
    }
    let left_line = Line::from(left_spans);
    let ctx = Line::from(context).dim().right_aligned();
    let [left, middle, right] = area.layout(&Layout::horizontal([
        Constraint::Length(left_line.width() as u16),
        Constraint::Fill(1),
        Constraint::Length(ctx.width() as u16),
    ]));
    frame.render_widget(left_line, left);
    if let Some(notice) = notice {
        let line = Line::from(vec![Span::from("  "), Span::from(notice).fg(AMBER)]);
        frame.render_widget(line, middle);
    }
    frame.render_widget(ctx, right);
}

/// Dim hints row with amber key letters: `space reveal · 0-5 grade · q quit`.
fn hints_row(frame: &mut Frame, area: Rect, hints: &[(&str, &str)]) {
    let mut spans = Vec::with_capacity(hints.len() * 3);
    for (i, (key, label)) in hints.iter().enumerate() {
        if i > 0 {
            spans.push(Span::from(" · ").dim());
        }
        spans.push(Span::from(*key).fg(AMBER));
        if !label.is_empty() {
            spans.push(Span::from(format!(" {label}")).dim());
        }
    }
    frame.render_widget(Line::from(spans), area);
}

/// A small rounded box around `lines`, centered; `status` goes on the row under it.
/// Falls back to the first line, dim and centered, when the area is too small.
pub(crate) fn note_box(frame: &mut Frame, area: Rect, lines: &[Line], status: Option<&str>) {
    let w = lines.iter().map(|l| l.width()).max().unwrap_or(0) as u16 + 8;
    let h = lines.len() as u16 + 4;
    if area.width < w || area.height < h {
        let [_, row, _] = area.layout(&Layout::vertical([
            Constraint::Fill(1),
            Constraint::Length(1),
            Constraint::Fill(1),
        ]));
        if let Some(first) = lines.first() {
            frame.render_widget(first.clone().dim().centered(), row);
        }
        return;
    }
    let boxed = area.centered(Constraint::Length(w), Constraint::Length(h));
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().dim())
        .padding(Padding::new(3, 3, 1, 1));
    let inner = block.inner(boxed);
    frame.render_widget(block, boxed);
    let body: Vec<Line> = lines.iter().cloned().map(Line::centered).collect();
    frame.render_widget(Paragraph::new(body), inner);
    if let Some(s) = status {
        if boxed.bottom() < area.bottom() {
            let row = Rect { y: boxed.bottom(), height: 1, ..area };
            frame.render_widget(Line::from(s).dim().centered(), row);
        }
    }
}

pub(super) fn wrapped<'a>(lines: Vec<Line<'a>>) -> Paragraph<'a> {
    Paragraph::new(lines).wrap(Wrap { trim: false })
}

/// The rows `lines` take inside `area`, wrapped exactly as they will be drawn.
///
/// `Paragraph::line_count` is behind ratatui's unstable `rendered-line-info`
/// feature, so the widget answers instead of a second wrapping of our own: the same
/// `Paragraph` is drawn into a scratch buffer the size of the side, and the last row
/// carrying a character is the last row the text needs. Rows that stay blank are
/// free for a picture.
pub(super) fn text_rows(lines: &[Line<'_>], area: Rect) -> u16 {
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::app::App;
    use crate::media::{test_picker, AudioLog, NullAudio};
    use chrono::NaiveDate;
    use crossterm::event::{KeyCode, KeyModifiers};
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::style::Modifier;
    use ratatui::Terminal;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    /// The fixture vault's top-level markdown files, copied into `dest`.
    fn copy_fixture_notes(dest: &Path) {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/vault");
        for entry in std::fs::read_dir(&src).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_file() {
                std::fs::copy(entry.path(), dest.join(entry.file_name())).unwrap();
            }
        }
    }

    /// The fixture vault's `media/` folder, so `![[pomelo.png]]` resolves to a real
    /// file. Without it every embed would render as `· not found`.
    fn copy_fixture_media(dest: &Path) {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/vault/media");
        std::fs::create_dir_all(dest.join("media")).unwrap();
        for entry in std::fs::read_dir(&src).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_file() {
                std::fs::copy(entry.path(), dest.join("media").join(entry.file_name())).unwrap();
            }
        }
    }

    fn fixture_app() -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        copy_fixture_notes(dir.path());
        copy_fixture_media(dir.path());
        let today = NaiveDate::from_ymd_opt(2026, 9, 20).unwrap();
        let app = App::open(dir.path(), today).unwrap();
        (dir, app)
    }

    /// The fixture vault with the halfblocks picker, an 80x24 viewport and an audio
    /// backend that opens no device: what a card's media needs to be drawn at all.
    /// `extra` files are written over the vault before it is indexed.
    fn media_app(extra: &[(&str, &str)]) -> (tempfile::TempDir, App, Arc<Mutex<AudioLog>>) {
        let dir = tempfile::tempdir().unwrap();
        copy_fixture_notes(dir.path());
        copy_fixture_media(dir.path());
        open_media_app(dir, extra)
    }

    /// `media/` and `extra`, nothing else: one due card, so the app opens on it.
    fn solo_media_app(extra: &[(&str, &str)]) -> (tempfile::TempDir, App, Arc<Mutex<AudioLog>>) {
        let dir = tempfile::tempdir().unwrap();
        copy_fixture_media(dir.path());
        open_media_app(dir, extra)
    }

    fn open_media_app(
        dir: tempfile::TempDir,
        extra: &[(&str, &str)],
    ) -> (tempfile::TempDir, App, Arc<Mutex<AudioLog>>) {
        for (name, body) in extra {
            std::fs::write(dir.path().join(name), body).unwrap();
        }
        let today = NaiveDate::from_ymd_opt(2026, 9, 20).unwrap();
        let mut app = App::open(dir.path(), today).unwrap();
        app.set_picker(test_picker());
        let (audio, log) = NullAudio::new();
        app.set_audio(Box::new(audio));
        app.set_viewport(80, 24);
        (dir, app, log)
    }

    /// Open `path` from the table, which also inserts it into the session.
    fn open_card(app: &mut App, path: &str) {
        if app.screen != Screen::Queue {
            app.handle_key(KeyCode::Tab).unwrap();
        }
        let idx = app.items.iter().position(|i| i.path == path).unwrap();
        app.queue_sel = idx;
        app.handle_key(KeyCode::Enter).unwrap();
    }

    /// A one-file vault written before `App::open`, so the frontmatter is indexed.
    /// With a single due item the app opens straight on that item's screen.
    fn app_with_file(name: &str, content: &str) -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(name), content).unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 9, 20).unwrap();
        let app = App::open(dir.path(), today).unwrap();
        (dir, app)
    }

    /// A card carrying `url` and `imported` (M5).
    const URL_CARD: &str = "---\ntype: card\nsm_id: 91\nprio: 5\nurl: https://en.wikipedia.org/wiki/Pomelo\nimported: 2026-09-20\n---\nQ: Largest citrus?\n\nA: pomelo\n";

    /// An article carrying `url` but no `imported`.
    const URL_ARTICLE: &str = "---\ntype: article\nsm_id: 93\nprio: 5\nurl: https://en.wikipedia.org/wiki/Pomelo\n---\nPomelo is a citrus.\n";

    fn vault_with(names: &[&str]) -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/vault");
        for name in names {
            std::fs::copy(src.join(name), dir.path().join(name)).unwrap();
        }
        let today = NaiveDate::from_ymd_opt(2026, 9, 20).unwrap();
        let app = App::open(dir.path(), today).unwrap();
        (dir, app)
    }

    /// Walk the whole main pass: one grade per card in session order (yuzu,
    /// kumquat, finger-lime, bergamot), `enter` for the two articles. Leaves the
    /// session on the drill prompt when at least one grade failed.
    fn walk_to_prompt(app: &mut App, grades: [char; 4]) {
        let mut grades = grades.into_iter();
        for _ in 0..6 {
            if app.screen == Screen::Read {
                app.handle_key(KeyCode::Enter).unwrap();
            } else {
                app.handle_key(KeyCode::Char(' ')).unwrap();
                app.handle_key(KeyCode::Char(grades.next().unwrap())).unwrap();
            }
        }
    }

    /// Draw into the two degenerate sizes; the test is that nothing panics.
    fn draw_tiny(app: &App) {
        for (w, h) in [(1u16, 1u16), (0, 0)] {
            let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
            terminal.draw(|f| render(f, app)).unwrap();
        }
    }

    /// Render into an 80x24 test terminal and return the rows as trimmed strings.
    fn rows(app: &App) -> Vec<String> {
        rows_of(&buffer_at(app, 80, 24))
    }

    /// The rows of a rendered buffer as trimmed strings.
    fn rows_of(buf: &Buffer) -> Vec<String> {
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "))
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    /// Cells a halfblocks image wrote on rows `y0..=y1`: its glyph is `▀`, and every
    /// cell it touches carries a background colour. Text never sets one.
    fn image_cells(buf: &Buffer, y0: u16, y1: u16) -> usize {
        (y0..=y1.min(buf.area.height.saturating_sub(1)))
            .flat_map(|y| (0..buf.area.width).map(move |x| (x, y)))
            .filter(|&(x, y)| {
                buf.cell((x, y))
                    .is_some_and(|c| c.symbol() == "▀" || c.bg != Color::Reset)
            })
            .count()
    }

    /// Render into a `w`x`h` test terminal and return the whole screen as one string.
    fn at(app: &App, w: u16, h: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|f| render(f, app)).unwrap();
        let buf = terminal.backend().buffer();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "))
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Render into an 80x24 test terminal and return the raw buffer, for style assertions.
    fn buffer(app: &App) -> Buffer {
        buffer_at(app, 80, 24)
    }

    /// The same at an arbitrary size.
    fn buffer_at(app: &App, w: u16, h: u16) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|f| render(f, app)).unwrap();
        terminal.backend().buffer().clone()
    }

    #[test]
    fn text_rows_counts_wrapped_rows_exactly() {
        let long = Line::from("a".repeat(100));
        assert_eq!(text_rows(&[long], Rect::new(0, 0, 40, 10)), 3, "100 chars at width 40 wrap to 3 rows");

        let two = [Line::from("0123456789"), Line::from("0123456789")];
        assert_eq!(text_rows(&two, Rect::new(0, 0, 40, 10)), 2, "two short lines take one row each");

        assert_eq!(text_rows(&[], Rect::new(0, 0, 40, 10)), 0, "no lines, no rows");
        assert_eq!(text_rows(&[Line::from("x")], Rect::new(0, 0, 0, 10)), 0, "zero width");

        let over = Line::from("a".repeat(1000));
        assert_eq!(
            text_rows(&[over], Rect::new(0, 0, 10, 5)),
            5,
            "a line longer than the area clamps to its height"
        );
    }

    #[test]
    fn rule_rows_frame_the_content_on_every_screen() {
        let (_d, mut app) = fixture_app();
        let rule = "─".repeat(80);

        let r = rows(&app);
        assert_eq!(r[1], rule, "review: rule under the status row");
        assert_eq!(r[22], rule, "review: rule over the hints row");
        assert!(r[23].contains("q quit"), "hints stay on the last row: {:?}", r[23]);

        app.handle_key(KeyCode::Tab).unwrap();
        let r = rows(&app);
        assert_eq!(r[1], rule, "queue: rule under the status row");
        assert_eq!(r[22], rule, "queue: rule over the hints row");
        assert!(r[23].contains("q quit"), "{:?}", r[23]);

        open_article(&mut app);
        let r = rows(&app);
        assert_eq!(r[1], rule, "read: rule under the status row");
        assert_eq!(r[22], rule, "read: rule over the hints row");
        assert!(r[23].contains("enter next"), "{:?}", r[23]);
    }

    #[test]
    fn status_row_shows_the_progress_bar_only_in_the_main_pass() {
        use ratatui::style::Modifier;
        let (_d, mut app) = fixture_app();
        let r = rows(&app);
        assert!(r[0].starts_with("all  ▮▯▯▯▯▯"), "{:?}", r[0]);

        // Grade yuzu, then end citrus-vocab: the third of six items is reached.
        app.handle_key(KeyCode::Char(' ')).unwrap();
        app.handle_key(KeyCode::Char('4')).unwrap();
        app.handle_key(KeyCode::Enter).unwrap();
        let r = rows(&app);
        assert!(r[0].starts_with("all  ▮▮▮▯▯▯"), "{:?}", r[0]);
        let buf = buffer(&app);
        assert_eq!(buf[(5, 0)].style().fg, Some(AMBER), "filled cells are amber");
        assert_eq!(buf[(7, 0)].style().fg, Some(AMBER));
        assert!(buf[(8, 0)].style().add_modifier.contains(Modifier::DIM), "empty cells are dim");

        app.handle_key(KeyCode::Tab).unwrap();
        let r = rows(&app);
        assert!(!r[0].contains('▮') && !r[0].contains('▯'), "no bar on the queue: {:?}", r[0]);

        let (_d, mut app) = fixture_app();
        walk_to_prompt(&mut app, ['2', '4', '4', '4']);
        let r = rows(&app);
        assert!(!r[0].contains('▮') && !r[0].contains('▯'), "no bar at the prompt: {:?}", r[0]);
        app.handle_key(KeyCode::Char('y')).unwrap();
        let r = rows(&app);
        assert!(!r[0].contains('▮') && !r[0].contains('▯'), "no bar while drilling: {:?}", r[0]);
    }

    #[test]
    fn note_box_draws_a_rounded_frame_and_falls_back_when_small() {
        let (_d, app) = vault_with(&["pomelo.md", "buddhas-hand.md"]);
        let all = rows(&app).join("\n");
        for corner in ['╭', '╮', '╰', '╯'] {
            assert!(all.contains(corner), "rounded corner {corner} missing:\n{all}");
        }
        assert!(all.contains("nothing more to learn"), "{all}");
        assert!(all.contains("0 graded · 0 read"), "{all}");

        // Too short for the box but wide enough for the line: the first line survives.
        let short = at(&app, 60, 6);
        assert!(!short.contains('╭'), "no box when the area is too small:\n{short}");
        assert!(
            short.contains("nothing more to learn"),
            "falls back to a plain line:\n{short}"
        );

        // Too narrow as well: still a plain line, cut to the width (centered, so
        // ratatui drops characters from both ends).
        let small = at(&app, 30, 6);
        assert!(!small.contains('╭'), "no box when the area is too small:\n{small}");
        assert!(small.contains("more to learn"), "falls back to a plain line:\n{small}");
    }

    #[test]
    fn queue_screen_has_status_row_items_and_hints() {
        let (_d, mut app) = fixture_app();
        app.handle_key(KeyCode::Tab).unwrap(); // the app opens on the session; tab shows the table
        let r = rows(&app);
        assert!(r[0].starts_with("all"), "{:?}", r[0]);
        assert!(r[0].ends_with("queue · sort prio"), "{:?}", r[0]);
        let body = r[1..23].join("\n");
        assert!(body.contains("Japanese citrus, fragrant, used in ponzu?"), "{body}");
        assert!(body.contains("Citrus vocabulary"), "{body}");
        // The type column is a glyph now: `≡` for an article, `▪` for a card.
        assert!(body.contains('≡'), "{body}");
        assert!(body.contains('▪'), "{body}");
        assert!(r[23].contains("enter open"), "{:?}", r[23]);
        assert!(r[23].contains("q quit"), "{:?}", r[23]);
        assert_eq!(
            r[23],
            "j/k move · enter open · a add · i import · s stats · p prio · tab learn · q quit",
            "p is a queue key (M2), a and i are M5, s is M8"
        );
        assert!(!r.iter().any(|l| l.contains('│') || l.contains('┌')), "no borders");
    }

    /// Column of the first char of `needle` in `row`. Every glyph left of a queue
    /// cell is one column wide, so the char count is the column.
    fn col_of(row: &str, needle: &str) -> u16 {
        let byte = row.find(needle).unwrap_or_else(|| panic!("{needle:?} not in {row:?}"));
        row[..byte].chars().count() as u16
    }

    #[test]
    fn queue_rows_show_glyphs_colours_and_a_gutter_bar() {
        use ratatui::style::Modifier;
        let (_d, mut app) = fixture_app();
        app.handle_key(KeyCode::Tab).unwrap();
        let r = rows(&app);
        let buf = buffer(&app);

        // The header is dim and a rule sits on the row under it.
        assert!(r[2].contains("prio") && r[2].contains("title"), "{:?}", r[2]);
        assert_eq!(r[3], "─".repeat(80), "a rule under the header: {:?}", r[3]);
        assert!(!r[4].starts_with("  card"), "the type column is a glyph, not a word: {:?}", r[4]);

        // yuzu is the selected row: amber gutter bar, card glyph, `now` due.
        let y_yuzu = r.iter().position(|l| l.contains("Japanese citrus")).unwrap();
        assert!(r[y_yuzu].starts_with("▎ ▪"), "{:?}", r[y_yuzu]);
        assert_eq!(
            buf[(0, y_yuzu as u16)].style().fg,
            Some(AMBER),
            "the gutter bar on the selected row is amber"
        );
        let x = col_of(&r[y_yuzu], "now");
        assert_eq!(buf[(x, y_yuzu as u16)].style().fg, Some(AMBER), "`now` is amber");

        // The article carries its own glyph.
        let y_art = r.iter().position(|l| l.contains("Citrus vocabulary")).unwrap();
        assert!(r[y_art].contains('≡'), "article glyph: {:?}", r[y_art]);

        // bergamot is overdue, pomelo is not.
        let y_over = r.iter().position(|l| l.contains("2026-09-01")).unwrap();
        let x = col_of(&r[y_over], "2026-09-01");
        assert_eq!(
            buf[(x, y_over as u16)].style().fg,
            Some(Color::LightRed),
            "a due date before today is red-ish"
        );
        let y_future = r.iter().position(|l| l.contains("2026-09-25")).unwrap();
        let x = col_of(&r[y_future], "2026-09-25");
        assert!(
            buf[(x, y_future as u16)].style().add_modifier.contains(Modifier::DIM),
            "a due date from today on is dim"
        );
    }

    #[test]
    fn long_titles_are_cut_with_an_ellipsis() {
        let (dir, _first) = fixture_app();
        let long = "citrus ".repeat(20); // 140 chars, well past the 57-column title
        std::fs::write(
            dir.path().join("long.md"),
            format!("---\ntype: card\nsm_id: 88\nprio: 99\n---\nQ: {long}\n\nA: yes\n"),
        )
        .unwrap();
        let mut app = App::open(dir.path(), NaiveDate::from_ymd_opt(2026, 9, 20).unwrap()).unwrap();
        app.handle_key(KeyCode::Tab).unwrap();
        let r = rows(&app);
        let line = r.iter().find(|l| l.contains("citrus citrus")).unwrap();
        assert!(line.ends_with('…'), "the cut title ends with an ellipsis: {line:?}");
        assert!(line.chars().count() <= 80, "the row still fits: {line:?}");
        drop(app);
    }

    #[test]
    fn queue_notice_appears_in_status_area() {
        let (_d, mut app) = fixture_app();
        app.notice = Some("reading arrives in M2".to_string());
        let r = rows(&app);
        assert!(r[0].contains("reading arrives in M2"), "{:?}", r[0]);
    }

    #[test]
    fn reference_line_sits_right_under_the_question() {
        let (_d, mut app) = fixture_app();
        app.handle_key(KeyCode::Tab).unwrap();
        let idx = app.items.iter().position(|i| i.path == "pomelo.md").unwrap();
        app.queue_sel = idx;
        app.handle_key(KeyCode::Enter).unwrap();
        let r = rows(&app);
        let last_q = r.iter().position(|l| l.contains("[image: pomelo.png]")).unwrap();
        let reference = r.iter().position(|l| l.contains("↳ citrus-vocab.md")).unwrap();
        assert_eq!(reference, last_q + 1, "reference hugs the question: {r:?}");
        app.handle_key(KeyCode::Char(' ')).unwrap();
        let r = rows(&app);
        let reference = r.iter().position(|l| l.contains("↳ citrus-vocab.md")).unwrap();
        let dash = r.iter().position(|l| l.contains("─ ─ ─")).unwrap();
        assert!(reference < dash, "reference stays with the question after reveal: {r:?}");
    }

    #[test]
    fn review_hides_answer_until_revealed_and_shows_grade_row_after() {
        let (_d, mut app) = fixture_app();
        app.handle_key(KeyCode::Tab).unwrap();
        let idx = app.items.iter().position(|i| i.path == "pomelo.md").unwrap();
        app.queue_sel = idx;
        app.handle_key(KeyCode::Enter).unwrap();
        let r = rows(&app);
        let all = r.join("\n");
        assert!(r[0].ends_with("card · prio 28 · 1/7"), "{:?}", r[0]);
        assert!(all.contains("Large citrus fruit with a thick rind"), "{all}");
        assert!(all.contains("[image: pomelo.png]"), "{all}");
        assert!(!all.contains("Q:"), "markers never shown");
        assert!(!all.contains("pomelo /"), "answer hidden: {all}");
        assert!(all.contains("↳ citrus-vocab.md"), "{all}");
        assert!(all.contains("space reveal"), "{all}");
        assert!(!all.contains("5 bright"), "grade row hidden before reveal");

        app.handle_key(KeyCode::Char(' ')).unwrap();
        let all = rows(&app).join("\n");
        assert!(all.contains("pomelo /ˈpɒmɪloʊ/"), "{all}");
        assert!(all.contains("♪ pomelo.mp3"), "{all}");
        assert!(!all.contains("A:"), "markers never shown");
        assert!(all.contains("0 null   1 bad   2 fail   │   3 pass   4 good   5 bright"), "{all}");
    }

    /// A card whose question image is missing and whose answer sound is missing.
    const MISSING_MEDIA: (&str, &str) = (
        "missing.md",
        "---\ntype: card\nsm_id: 300\nprio: 1\n---\nQ: gone?\n![[nope.png]]\n\nA: yes\n![[ghost.mp3]]\n",
    );

    /// A card with two images on the question side: at 80x24 there is room for the
    /// first under the text and none for the second.
    const TWO_IMAGES: (&str, &str) = (
        "two-images.md",
        "---\ntype: card\nsm_id: 302\nprio: 1\n---\nQ: two\n![[pomelo.png]]\n![[buddhas-hand.jpg]]\n\nA: ok\n",
    );

    /// A card with a sound and no image, due today and first in the queue.
    const SOLO_AUDIO: (&str, &str) = (
        "solo.md",
        "---\ntype: card\nsm_id: 301\nprio: 1\n---\nQ: hear?\n![[pomelo.mp3]]\n\nA: a pomelo\n",
    );

    #[test]
    fn review_renders_the_question_image_under_the_text() {
        let (_d, mut app, _log) = media_app(&[]);
        open_card(&mut app, "pomelo.md");
        let r = rows(&app);
        let all = r.join("\n");
        assert!(all.contains("Large citrus fruit with a thick rind"), "{all}");
        assert!(all.contains("↳ citrus-vocab.md"), "the reference line stays: {all}");
        assert!(
            !all.contains("[image: pomelo.png"),
            "the image is drawn, not named: {all}"
        );

        // The question area is rows 2..=11; the picture sits under its two text lines.
        let buf = buffer(&app);
        assert!(image_cells(&buf, 2, 11) > 0, "no image cells in the question area: {r:?}");
        assert_eq!(image_cells(&buf, 0, 3), 0, "the text rows are untouched: {r:?}");
    }

    /// The placeholder of a second image is itself a line, so it moves the first
    /// image down. Settling the two is what keeps the picture off the text.
    #[test]
    fn a_second_image_that_does_not_fit_keeps_its_placeholder() {
        let (_d, app, _log) = solo_media_app(&[TWO_IMAGES]);
        let r = rows(&app);
        let buf = buffer(&app);

        let y = r
            .iter()
            .position(|l| l.contains("[image: buddhas-hand.jpg]"))
            .unwrap_or_else(|| panic!("the second image keeps its placeholder: {r:?}"))
            as u16;
        assert_eq!(r[y as usize].trim(), "[image: buddhas-hand.jpg]", "in full: {r:?}");
        assert_eq!(
            image_cells(&buf, y, y),
            0,
            "no picture sits on the placeholder's row: {r:?}"
        );
        assert!(
            image_cells(&buf, y + 1, 21) > 0,
            "the first image is still drawn, under the text: {r:?}"
        );
        assert!(
            !r.iter().any(|l| l.contains("[image: pomelo.png")),
            "the first image is a picture, not a line: {r:?}"
        );
    }

    #[test]
    fn review_shows_audio_line_after_reveal() {
        let (_d, mut app, log) = media_app(&[]);
        open_card(&mut app, "pomelo.md");
        let all = rows(&app).join("\n");
        assert!(!all.contains('♪'), "the answer's sound is hidden before the reveal: {all}");

        app.handle_key(KeyCode::Char(' ')).unwrap();
        let all = rows(&app).join("\n");
        assert!(all.contains("♪ pomelo.mp3"), "{all}");
        assert!(!all.contains("· playing"), "nothing plays until the log says so: {all}");

        log.lock().unwrap().playing = true;
        let r = rows(&app);
        let all = r.join("\n");
        assert!(all.contains("♪ pomelo.mp3 · playing"), "{all}");
        let y = r.iter().position(|l| l.contains("♪ pomelo.mp3")).unwrap() as u16;
        let col = r[y as usize].find("playing").unwrap();
        let x = r[y as usize][..col].chars().count() as u16;
        let buf = buffer(&app);
        assert_eq!(buf[(x, y)].style().fg, Some(AMBER), "`playing` is amber");
    }

    #[test]
    fn review_placeholder_when_no_room() {
        let (_d, mut app, _log) = media_app(&[]);
        open_card(&mut app, "pomelo.md");
        app.set_viewport(80, 10);

        let buf = buffer_at(&app, 80, 10);
        let r = rows_of(&buf);
        assert!(
            r.iter().any(|l| l.contains("[image: pomelo.png]")),
            "no room for the picture, so the placeholder comes back: {r:?}"
        );
        assert_eq!(image_cells(&buf, 0, 9), 0, "nothing is drawn: {r:?}");
    }

    #[test]
    fn review_hints_show_replay_only_with_audio() {
        let (_d, mut app, _log) = media_app(&[]);
        open_card(&mut app, "pomelo.md");
        assert_eq!(
            rows(&app)[23],
            "space reveal · 0-5 grade · u undo · r replay · tab queue · s stats · q quit"
        );

        // yuzu has no embeds at all.
        open_card(&mut app, "yuzu.md");
        assert_eq!(rows(&app)[23], "space reveal · 0-5 grade · u undo · tab queue · s stats · q quit");
    }

    #[test]
    fn drill_hints_show_replay_with_audio() {
        let (_d, mut app, _log) = solo_media_app(&[SOLO_AUDIO]);
        // One due card: fail it, take the drill offer, and the drilled card still has
        // its sound — but no undo, so `r` takes undo's place.
        app.handle_key(KeyCode::Char(' ')).unwrap();
        app.handle_key(KeyCode::Char('2')).unwrap();
        app.handle_key(KeyCode::Char('y')).unwrap();
        assert_eq!(app.review.phase, Phase::Drilling);
        assert_eq!(rows(&app)[23], "space reveal · 0-5 grade · r replay · tab queue · q quit");
    }

    #[test]
    fn review_missing_media_notes() {
        let (_d, mut app, _log) = solo_media_app(&[MISSING_MEDIA]);
        let all = rows(&app).join("\n");
        assert!(all.contains("[image: nope.png · not found]"), "{all}");

        app.handle_key(KeyCode::Char(' ')).unwrap();
        let all = rows(&app).join("\n");
        assert!(all.contains("♪ ghost.mp3 · not found"), "{all}");
    }

    /// A cloze child as `Ctrl+z` writes it: one `[...]` in the question, the
    /// hidden words as the answer.
    const CLOZE: (&str, &str) =
        ("cloze.md", "---\ntype: card\n---\nQ: Pomelo is the [...] citrus fruit.\n\nA: largest\n");

    /// Two blanks and a two-line answer: only the first of each is used.
    const CLOZE_TWO: (&str, &str) =
        ("cloze-two.md", "---\ntype: card\n---\nQ: [...] and [...]\n\nA: one\ntwo\n");

    /// Is the cell at `(x, y)` a filled-in blank: amber and bold?
    fn amber_bold(buf: &Buffer, x: u16, y: u16) -> bool {
        buf.cell((x, y))
            .is_some_and(|c| c.fg == AMBER && c.modifier.contains(Modifier::BOLD))
    }

    #[test]
    fn cloze_question_keeps_the_blank_before_reveal() {
        let (_d, mut app, _log) = media_app(&[CLOZE]);
        open_card(&mut app, "cloze.md");
        let r = rows(&app);
        let y = r
            .iter()
            .position(|l| l.contains("Pomelo is the [...] citrus fruit."))
            .unwrap_or_else(|| panic!("the blank stays before the reveal: {r:?}"))
            as u16;
        let buf = buffer(&app);
        assert!(
            (0..buf.area.width).all(|x| !amber_bold(&buf, x, y)),
            "nothing is filled in before the reveal: {r:?}"
        );
    }

    #[test]
    fn cloze_question_fills_the_blank_on_reveal() {
        let (_d, mut app, _log) = media_app(&[CLOZE]);
        open_card(&mut app, "cloze.md");
        app.handle_key(KeyCode::Char(' ')).unwrap();
        let r = rows(&app);
        let y = r
            .iter()
            .position(|l| l.contains("Pomelo is the largest citrus fruit."))
            .unwrap_or_else(|| panic!("the blank is filled at the reveal: {r:?}"));
        // The row is ASCII, so the byte offset is the column.
        let fill = r[y].find("largest").unwrap() as u16;
        let text = r[y].find("Pomelo").unwrap() as u16;
        let buf = buffer(&app);
        let y = y as u16;
        for i in 0.."largest".len() as u16 {
            assert!(amber_bold(&buf, fill + i, y), "the fill is amber bold: {r:?}");
        }
        assert!(!amber_bold(&buf, text, y), "the rest of the line is untouched: {r:?}");
        assert!(
            !amber_bold(&buf, fill + "largest".len() as u16, y),
            "only the answer is styled: {r:?}"
        );

        let dash = r.iter().position(|l| l.contains("─ ─ ─")).unwrap();
        assert!(
            r.iter().skip(dash + 1).any(|l| l.trim() == "largest"),
            "the answer area still shows the answer: {r:?}"
        );
    }

    #[test]
    fn cloze_fill_uses_first_blank_and_first_answer_line() {
        let (_d, mut app, _log) = media_app(&[CLOZE_TWO]);
        open_card(&mut app, "cloze-two.md");
        app.handle_key(KeyCode::Char(' ')).unwrap();
        let r = rows(&app);
        let y = r
            .iter()
            .position(|l| l.trim() == "one and [...]")
            .unwrap_or_else(|| panic!("the first blank takes the first answer line: {r:?}"));
        let fill = r[y].find("one").unwrap() as u16;
        let rest = r[y].find("and").unwrap() as u16;
        let second = r[y].find("[...]").unwrap() as u16;
        let buf = buffer(&app);
        let y = y as u16;
        for i in 0..3 {
            assert!(amber_bold(&buf, fill + i, y), "`one` is amber bold: {r:?}");
        }
        assert!(!amber_bold(&buf, rest, y), "`and` is untouched: {r:?}");
        assert!(!amber_bold(&buf, second, y), "the second blank stays: {r:?}");
    }

    #[test]
    fn non_cloze_card_is_untouched_on_reveal() {
        let (_d, mut app, _log) = media_app(&[]);
        open_card(&mut app, "pomelo.md");
        app.handle_key(KeyCode::Char(' ')).unwrap();
        let r = rows(&app);
        let y = r.iter().position(|l| l.contains("Large citrus fruit")).unwrap() as u16;
        let buf = buffer(&app);
        assert!(
            (0..buf.area.width).all(|x| !amber_bold(&buf, x, y)),
            "a card without a blank is unchanged: {r:?}"
        );
        assert!(r.join("\n").contains("pomelo /ˈpɒmɪloʊ/"), "the answer still shows: {r:?}");
    }

    #[test]
    fn grade_row_colours_digits_amber_and_halves_red_green() {
        let (_d, mut app) = fixture_app();
        app.handle_key(KeyCode::Char(' ')).unwrap();
        let r = rows(&app);
        let y = r.iter().position(|l| l.contains("0 null")).unwrap() as u16;
        // The grade row and everything left of it is ASCII, so the byte offset is the column.
        let zero = r[y as usize].find("0 null").unwrap() as u16;
        let three = r[y as usize].find("3 pass").unwrap() as u16;
        let buf = buffer(&app);
        assert_eq!(buf[(zero, y)].style().fg, Some(AMBER), "the digit is amber");
        assert_eq!(
            buf[(zero + 2, y)].style().fg,
            Some(Color::LightRed),
            "the `n` of `null` is red-ish"
        );
        assert_eq!(
            buf[(three + 2, y)].style().fg,
            Some(Color::LightGreen),
            "the `p` of `pass` is green-ish"
        );
    }

    #[test]
    fn question_is_indented_and_the_answer_follows_a_dashed_rule() {
        let (_d, mut app) = fixture_app();
        let r = rows(&app);
        assert!(r[2].starts_with("  Japanese"), "the question is indented by two: {:?}", r[2]);
        assert!(!r.iter().any(|l| l.contains("─ ─ ─")), "no dashed rule before reveal: {r:?}");

        app.handle_key(KeyCode::Char(' ')).unwrap();
        let r = rows(&app);
        let dash = r.iter().position(|l| l.contains("─ ─ ─")).unwrap();
        // `rows()` pads wide glyphs, so match the ASCII half of `yuzu 柚子 (ゆず)`.
        let answer = r.iter().position(|l| l.contains("yuzu")).unwrap();
        assert!(dash < answer, "the dashed rule comes before the answer: {dash} then {answer}");
    }

    #[test]
    fn review_status_line_after_grade_and_empty_state() {
        let (_d, mut app) = fixture_app();
        // yuzu, then the article, then kumquat: the status line shows on the next card.
        // kumquat's digit differs from yuzu's, so a status left over from yuzu would fail.
        app.handle_key(KeyCode::Char(' ')).unwrap();
        app.handle_key(KeyCode::Char('4')).unwrap();
        app.handle_key(KeyCode::Enter).unwrap();
        app.handle_key(KeyCode::Char(' ')).unwrap();
        app.handle_key(KeyCode::Char('3')).unwrap();
        let all = rows(&app).join("\n");
        assert!(all.contains("graded 3 · journaled (offline)"), "{all}");
        assert!(!all.contains("graded 4"), "yuzu's status did not survive the article: {all}");
        // finger-lime, earl-grey (an article), bergamot: the rest of the session.
        app.handle_key(KeyCode::Char(' ')).unwrap();
        app.handle_key(KeyCode::Char('3')).unwrap();
        app.handle_key(KeyCode::Enter).unwrap();
        app.handle_key(KeyCode::Char(' ')).unwrap();
        app.handle_key(KeyCode::Char('3')).unwrap();
        // Three of the four grades were failures, so the main pass ends on the drill
        // prompt; `n` drops the drill and shows the finish line.
        let all = rows(&app).join("\n");
        assert!(all.contains("final drill · 3 cards"), "{all}");
        assert!(all.contains("y drill        n finish"), "the box carries its own keys: {all}");
        app.handle_key(KeyCode::Char('n')).unwrap();
        let r = rows(&app);
        let all = r.join("\n");
        assert!(r.iter().any(|l| l.contains("nothing more to learn")), "{all}");
        assert!(r.iter().any(|l| l.contains("4 graded · 2 read")), "{all}");
        // `n` clears the status on its way out of the prompt (M2, `handle_drill_prompt_key`),
        // so this finish line has none: the box stands alone.
        assert!(!all.contains("journaled (offline)"), "the prompt cleared the status: {all}");

        // A pass with no failure ends on the finish line with the last grade's status
        // still set. That is the one path that renders a status under the box.
        let (_d, mut app) = fixture_app();
        walk_to_prompt(&mut app, ['4', '4', '4', '5']);
        assert_eq!(app.review.phase, Phase::Main, "no failure, no drill prompt");
        assert!(app.review.current.is_none(), "the session is over");
        let r = rows(&app);
        let all = r.join("\n");
        assert!(r.iter().any(|l| l.contains("nothing more to learn")), "{all}");
        assert!(r.iter().any(|l| l.contains("4 graded · 2 read")), "{all}");
        assert!(all.contains("graded 5 · journaled (offline)"), "status under the box: {all}");
        let note = r.iter().position(|l| l.contains("nothing more to learn")).unwrap();
        let status = r.iter().position(|l| l.contains("graded 5 · journaled (offline)")).unwrap();
        let bottom = r.iter().rposition(|l| l.contains('╰')).unwrap();
        assert!(note < bottom, "the note is inside the box: note {note}, bottom {bottom}");
        assert!(status > bottom, "the status sits under the box: status {status}, bottom {bottom}");
    }

    #[test]
    fn status_row_shows_unsynced_count_on_both_screens_until_undone() {
        let (_d, mut app) = {
            let dir = tempfile::tempdir().unwrap();
            let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/vault");
            for entry in std::fs::read_dir(&src).unwrap() {
                let entry = entry.unwrap();
                if entry.file_type().unwrap().is_file() {
                    std::fs::copy(entry.path(), dir.path().join(entry.file_name())).unwrap();
                }
            }
            let today = NaiveDate::from_ymd_opt(2026, 9, 20).unwrap();
            let fake = crate::sync::api::FakeScheduler::always_ok();
            let app = App::open_with_scheduler(dir.path(), today, Box::new(fake)).unwrap();
            (dir, app)
        };
        // Start at kumquat: a card follows it, so the review screen is still up after the grade.
        app.handle_key(KeyCode::Tab).unwrap();
        let idx = app.items.iter().position(|i| i.path == "kumquat.md").unwrap();
        app.queue_sel = idx;
        app.handle_key(KeyCode::Enter).unwrap();
        app.handle_key(KeyCode::Char(' ')).unwrap();
        app.handle_key(KeyCode::Char('4')).unwrap();
        let r = rows(&app);
        assert!(r[0].ends_with("card · prio 40 · 4/6 · 1 unsynced"), "{:?}", r[0]);
        assert!(r.join("\n").contains("graded 4 · journaled · sync in 5s"), "{r:?}");
        app.handle_key(KeyCode::Tab).unwrap();
        let r = rows(&app);
        assert!(r[0].ends_with("queue · sort prio · 1 unsynced"), "{:?}", r[0]);
        app.handle_key(KeyCode::Tab).unwrap();
        app.handle_key(KeyCode::Char('u')).unwrap();
        let r = rows(&app);
        assert!(r[0].ends_with("card · prio 35 · 3/6"), "{:?}", r[0]);
    }

    #[test]
    fn every_sync_status_line_variant_renders_in_full() {
        let (_d, mut app) = fixture_app();
        app.handle_key(KeyCode::Char(' ')).unwrap();
        for line in [
            "graded 4 · journaled (offline)",
            "graded 4 · journaled · sync in 3s",
            "graded 4 · syncing…",
            "graded 4 · synced · interval 12 · due 2026-10-02",
            "graded 4 · sync failed (connection refused) · retry in 8s",
            "graded 4 · journaled · daily cap reached, resumes tomorrow",
            "sync stopped · 401 unauthorized · check GRAIN_SM_API_KEY",
            "graded 4 · rejected by API (grade: must be ≤ 5) · kept unsynced",
            "cannot undo · already sent to SuperMemo",
        ] {
            app.review.status = Some(line.to_string());
            let all = rows(&app).join("\n");
            assert!(all.contains(line), "missing {line:?} in\n{all}");
        }
    }

    fn open_article(app: &mut App) {
        if app.screen != Screen::Queue {
            app.handle_key(KeyCode::Tab).unwrap();
        }
        let idx = app.items.iter().position(|i| i.path == "citrus-vocab.md").unwrap();
        app.queue_sel = idx;
        app.handle_key(KeyCode::Enter).unwrap();
    }

    /// Modifiers of the cell that holds the first character of `needle` on screen.
    fn cell_mods(app: &App, needle: &str) -> ratatui::style::Modifier {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| render(f, app)).unwrap();
        let buf = terminal.backend().buffer();
        for y in 0..buf.area.height {
            let row: String = (0..buf.area.width).map(|x| buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" ")).collect();
            if let Some(col) = row.find(needle) {
                let x = row[..col].chars().count() as u16;
                return buf.cell((x, y)).unwrap().modifier;
            }
        }
        panic!("{needle:?} not on screen");
    }

    #[test]
    fn read_screen_shows_paragraphs_with_gutter_dim_marks_selection_and_hints() {
        use ratatui::style::Modifier;
        let (_d, mut app) = fixture_app();
        open_article(&mut app);
        let r = rows(&app);
        assert!(r[0].ends_with("read · prio 20 · ¶ 3/4 · 3 harvested"), "{:?}", r[0]);
        let all = r.join("\n");
        assert!(all.contains("# Citrus vocabulary"), "raw markdown: {all}");
        assert!(all.contains("▎ Pomelo is the largest"), "gutter mark on the current paragraph: {all}");
        assert!(!all.contains("▎ # Citrus"), "{all}");
        assert_eq!(r[23], "j/k ¶ · w/b word · v mark · ^x extract · ^z cloze · enter next · d done · u undo");
        assert!(cell_mods(&app, "Pomelo is").contains(Modifier::DIM), "harvested span is dim");
        // Unharvested text is dim here only because its paragraph is out of focus;
        // `read_screen_dims_other_paragraphs_bolds_headings_and_dots_harvested` pins
        // that it loses the dim once the cursor moves onto it.
        assert!(cell_mods(&app, "A long-form").contains(Modifier::DIM), "out-of-focus paragraph is dim");
        assert!(cell_mods(&app, "Pomelo is").contains(Modifier::UNDERLINED), "word under the cursor");

        app.handle_key(KeyCode::Char('v')).unwrap();
        app.handle_key(KeyCode::Char('w')).unwrap();
        let r = rows(&app);
        assert!(r[0].contains("selecting 2 words"), "{:?}", r[0]);
        assert_eq!(r[23], "w/b · j/k extend · ^x extract · ^z cloze · esc cancel");
        assert!(cell_mods(&app, "Pomelo is").contains(Modifier::REVERSED));
        assert!(!cell_mods(&app, "the largest").contains(Modifier::REVERSED));

        app.handle_key(KeyCode::Esc).unwrap();
        app.handle_key(KeyCode::Char('p')).unwrap();
        let r = rows(&app);
        assert!(r[0].contains("prio 20 › 20"), "{:?}", r[0]);
        assert_eq!(r[23], "0-9 value · j/k nudge · enter set · esc cancel");
        assert!(!r.iter().any(|l| l.contains('│') || l.contains('┌')), "no borders");
    }

    #[test]
    fn read_screen_keeps_the_cursor_paragraph_visible_and_wraps_long_lines() {
        let (dir, _first) = fixture_app();
        let long = (1..=30).map(|i| format!("Paragraph {i} {}", "word ".repeat(30))).collect::<Vec<_>>().join("\n\n");
        std::fs::write(dir.path().join("long.md"), format!("---\ntype: article\nsm_id: 77\nprio: 1\n---\n{long}\n")).unwrap();
        let mut app = App::open(dir.path(), NaiveDate::from_ymd_opt(2026, 9, 20).unwrap()).unwrap();
        // prio 1 puts it first in the session, so the app opens straight on its read screen.
        assert_eq!(app.read.as_ref().unwrap().item.path, "long.md");
        for _ in 0..29 {
            app.handle_key(KeyCode::Char('j')).unwrap();
        }
        let r = rows(&app);
        assert!(r.iter().any(|l| l.starts_with("▎ Paragraph 30")), "last paragraph on screen: {r:?}");
        assert!(r.iter().all(|l| l.chars().count() <= 80), "wrapped");
        assert!(r.iter().filter(|l| l.contains("word word")).count() > 3, "several wrapped rows: {r:?}");
        drop(app);
    }

    #[test]
    fn read_screen_dims_other_paragraphs_bolds_headings_and_dots_harvested() {
        use ratatui::style::Modifier;
        let (_d, mut app) = fixture_app();
        open_article(&mut app);
        let r = rows(&app);
        let buf = buffer(&app);

        // `# Citrus vocabulary` is a non-current paragraph: dim from the focus pass,
        // bold because its first non-space character is `#`.
        let y = r.iter().position(|l| l.contains("# Citrus vocabulary")).unwrap() as u16;
        let head = buf[(2, y)].style().add_modifier;
        assert!(head.contains(Modifier::BOLD), "heading is bold: {head:?}");
        assert!(head.contains(Modifier::DIM), "out-of-focus paragraph is dim: {head:?}");

        // Every line of the current paragraph carries the amber gutter bar.
        let y = r.iter().position(|l| l.contains("Pomelo is the largest")).unwrap();
        assert!(r[y].starts_with("▎ Pomelo"), "{:?}", r[y]);
        assert!(r[y + 1].starts_with("▎ Kumquat"), "second line of the same paragraph: {:?}", r[y + 1]);
        assert_eq!(buf[(0, y as u16)].style().fg, Some(AMBER), "gutter bar is amber");

        // A non-current paragraph whose start lies inside a harvested span gets a dim dot
        // on its first line only. Here that is `Buddha's hand …` at offset 207.
        let y = r.iter().position(|l| l.starts_with("• ")).unwrap();
        assert!(r[y].contains("Buddha's hand"), "{:?}", r[y]);
        assert!(buf[(0, y as u16)].style().add_modifier.contains(Modifier::DIM), "the dot is dim");
        assert_eq!(r.iter().filter(|l| l.starts_with("• ")).count(), 1, "one dot per harvested paragraph: {r:?}");

        // The focus pass never dims the current paragraph. `Pomelo …` is itself inside a
        // harvested span and stays dim from `styled_runs`, so step up to `A long-form …`,
        // which is not harvested, to see the focus dim absent.
        app.handle_key(KeyCode::Char('k')).unwrap();
        let r = rows(&app);
        let buf = buffer(&app);
        let y = r.iter().position(|l| l.contains("A long-form")).unwrap();
        assert!(r[y].starts_with("▎ A long-form"), "{:?}", r[y]);
        assert!(!buf[(2, y as u16)].style().add_modifier.contains(Modifier::DIM), "the current paragraph is not dimmed");
    }

    #[test]
    fn queue_shows_article_due_like_cards() {
        let (_d, mut app) = fixture_app();
        app.handle_key(KeyCode::Tab).unwrap();
        let r = rows(&app);
        let earl = r.iter().find(|l| l.contains("Earl Grey")).unwrap();
        assert!(earl.contains("now"), "{earl}");
        assert!(!earl.contains('—'), "{earl}");
    }

    #[test]
    fn open_renders_the_first_due_card_not_the_table() {
        let (_d, app) = fixture_app();
        let r = rows(&app);
        assert!(r[0].ends_with("card · prio 12 · 1/6"), "{:?}", r[0]);
        let all = r.join("\n");
        assert!(all.contains("Japanese citrus, fragrant, used in ponzu?"), "{all}");
        assert!(!r[1..23].iter().any(|l| l.starts_with("type")), "no table header: {r:?}");
        assert_eq!(r[23], "space reveal · 0-5 grade · u undo · tab queue · s stats · q quit");
    }

    #[test]
    fn finish_line_renders_when_nothing_is_due() {
        let (_d, app) = vault_with(&["pomelo.md", "buddhas-hand.md"]);
        let r = rows(&app);
        let title = r.iter().position(|l| l.contains("nothing more to learn")).unwrap();
        let counts = r.iter().position(|l| l.contains("0 graded · 0 read")).unwrap();
        assert_eq!(counts, title + 1, "the counts sit on their own row under the title: {r:?}");
        assert_eq!(r[23], "tab queue · s stats · q quit");
    }

    #[test]
    fn drill_prompt_renders_with_its_hints() {
        let (_d, mut app) = fixture_app();
        walk_to_prompt(&mut app, ['2', '4', '4', '4']);
        let r = rows(&app);
        let all = r.join("\n");
        assert!(all.contains("final drill · 1 card"), "{r:?}");
        assert!(all.contains('╭'), "the prompt sits in the rounded box: {r:?}");
        assert!(all.contains("y drill        n finish"), "{r:?}");
        assert_eq!(r[23], "y drill · n finish · tab queue · q quit");
    }

    #[test]
    fn drilling_renders_the_card_and_drill_hints() {
        let (_d, mut app) = fixture_app();
        walk_to_prompt(&mut app, ['2', '4', '4', '4']);
        app.handle_key(KeyCode::Char('y')).unwrap();
        let r = rows(&app);
        assert!(r[0].ends_with("drill · prio 12 · 1 left"), "{:?}", r[0]);
        assert!(r.join("\n").contains("Japanese citrus, fragrant, used in ponzu?"), "{r:?}");
        assert_eq!(r[23], "space reveal · 0-5 grade · tab queue · q quit");

        app.handle_key(KeyCode::Char(' ')).unwrap();
        app.handle_key(KeyCode::Char('3')).unwrap();
        assert!(rows(&app).join("\n").contains("drill 3 · stays"), "{:?}", rows(&app));
    }

    #[test]
    fn new_states_do_not_panic_when_tiny() {
        let (_d, mut app) = fixture_app();
        walk_to_prompt(&mut app, ['2', '4', '4', '4']);
        draw_tiny(&app); // the drill prompt
        app.handle_key(KeyCode::Char('y')).unwrap();
        draw_tiny(&app); // drilling
        app.handle_key(KeyCode::Char(' ')).unwrap();
        app.handle_key(KeyCode::Char('4')).unwrap();
        draw_tiny(&app); // finished
    }

    #[test]
    fn queue_hints_show_add_and_import() {
        let (_d, mut app) = fixture_app();
        app.handle_key(KeyCode::Tab).unwrap();
        let r = rows(&app);
        assert_eq!(
            r[23],
            "j/k move · enter open · a add · i import · s stats · p prio · tab learn · q quit"
        );
        let add = r[23].find("a add").unwrap();
        let import = r[23].find("i import").unwrap();
        let prio = r[23].find("p prio").unwrap();
        assert!(add < import && import < prio, "add and import come before prio: {:?}", r[23]);
    }

    #[test]
    fn text_prompt_renders_in_status_row_with_hints() {
        let (_d, mut app) = fixture_app();
        app.handle_key(KeyCode::Tab).unwrap();

        app.handle_key(KeyCode::Char('a')).unwrap();
        let r = rows(&app);
        assert!(r[0].contains("add card · Q: ▏"), "{:?}", r[0]);
        assert!(r[23].contains("enter next"), "{:?}", r[23]);
        assert!(r[23].contains("esc cancel"), "{:?}", r[23]);

        for c in "why".chars() {
            app.handle_key(KeyCode::Char(c)).unwrap();
        }
        let r = rows(&app);
        assert!(r[0].contains("add card · Q: why▏"), "the caret follows the text: {:?}", r[0]);

        app.handle_key(KeyCode::Enter).unwrap();
        let r = rows(&app);
        assert!(r[0].contains("add card · A: ▏"), "{:?}", r[0]);
        assert!(r[23].contains("enter save"), "{:?}", r[23]);
        assert!(r[23].contains("esc cancel"), "{:?}", r[23]);

        app.handle_key(KeyCode::Esc).unwrap();
        app.handle_key(KeyCode::Char('i')).unwrap();
        let r = rows(&app);
        assert!(r[0].contains("import · url or path: ▏"), "{:?}", r[0]);
        assert!(r[23].contains("enter save"), "{:?}", r[23]);

        // The priority prompt is unchanged: no caret, its own hints.
        app.handle_key(KeyCode::Esc).unwrap();
        app.handle_key(KeyCode::Char('p')).unwrap();
        let r = rows(&app);
        assert!(r[0].contains("prio 12 › 12"), "{:?}", r[0]);
        assert!(!r[0].contains('▏'), "no caret on the priority prompt: {:?}", r[0]);
        assert_eq!(r[23], "0-9 value · j/k nudge · enter set · esc cancel");
    }

    #[test]
    fn review_shows_reference_line_for_url() {
        let (_d, app) = app_with_file("pom.md", URL_CARD);
        let r = rows(&app);
        assert!(
            r.iter().any(|l| l.trim() == "↗ en.wikipedia.org/wiki/Pomelo · 2026-09-20"),
            "{r:?}"
        );

        // With a `source` as well, the `↳` line comes first and the `↗` line follows it.
        let with_source = "---\ntype: card\nsm_id: 92\nprio: 5\nsource: \"[[citrus-vocab]]\"\nrange: 1-2\nurl: https://en.wikipedia.org/wiki/Pomelo\nimported: 2026-09-20\n---\nQ: Largest citrus?\n\nA: pomelo\n";
        let (_d2, app) = app_with_file("pom2.md", with_source);
        let r = rows(&app);
        let down = r.iter().position(|l| l.contains("↳ citrus-vocab.md › 1-2")).unwrap();
        let up = r.iter().position(|l| l.contains("↗ en.wikipedia.org/wiki/Pomelo")).unwrap();
        assert_eq!(up, down + 1, "the url line sits under the source line: {r:?}");
    }

    #[test]
    fn read_shows_reference_line_first() {
        let (_d, app) = app_with_file("art.md", URL_ARTICLE);
        assert_eq!(app.read.as_ref().unwrap().item.path, "art.md");
        let r = rows(&app);
        assert_eq!(r[2].trim(), "↗ en.wikipedia.org/wiki/Pomelo", "{r:?}");
        assert!(r[3].contains("Pomelo is a citrus."), "the body starts one row lower: {r:?}");
    }

    #[test]
    fn read_shows_url_line_above_the_empty_note() {
        let (_d, app) = app_with_file(
            "empty-import.md",
            "---\ntype: article\nurl: https://example.com/\nimported: 2026-09-20\n---\n",
        );
        let r = rows(&app);
        assert_eq!(r[2].trim(), "↗ example.com/ · 2026-09-20", "{r:?}");
        assert!(
            r.iter().skip(3).any(|l| l.contains("article is empty")),
            "note box below the url line: {r:?}"
        );
    }

    #[test]
    fn read_empty_article_without_url_unchanged() {
        let (_d, app) = app_with_file("empty.md", "---\ntype: article\n---\n");
        let r = rows(&app);
        assert!(!r.iter().any(|l| l.contains('↗')), "no url line: {r:?}");
        assert!(r.iter().any(|l| l.contains("article is empty")), "{r:?}");
    }

    #[test]
    fn reference_line_truncates_long_url() {
        let long = format!("https://example.org/{}", "a".repeat(200));
        let (_d, app) = app_with_file(
            "art.md",
            &format!("---\ntype: article\nsm_id: 94\nprio: 5\nurl: {long}\n---\nBody here.\n"),
        );
        let r = rows(&app);
        assert!(r[2].starts_with("↗ example.org/aaa"), "{:?}", r[2]);
        assert!(r[2].ends_with('…'), "the cut url ends with an ellipsis: {:?}", r[2]);
        assert_eq!(r[2].chars().count(), 80, "cut to the width exactly: {:?}", r[2]);
    }

    #[test]
    fn renders_at_narrow_sizes_without_panicking() {
        let (_d, mut app) = fixture_app();
        app.handle_key(KeyCode::Tab).unwrap();
        for (w, h) in [(20u16, 3u16), (1, 1), (0, 0), (40, 2)] {
            let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
            terminal.draw(|f| render(f, &app)).unwrap();
        }
        app.handle_key(KeyCode::Tab).unwrap();
        app.handle_key(KeyCode::Char(' ')).unwrap();
        for (w, h) in [(20u16, 3u16), (1, 1), (0, 0), (40, 2)] {
            let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
            terminal.draw(|f| render(f, &app)).unwrap();
        }
        open_article(&mut app);
        app.handle_key(KeyCode::Char('v')).unwrap();
        app.handle_key(KeyCode::Char('p')).unwrap();
        for (w, h) in [(20u16, 3u16), (1, 1), (0, 0), (40, 2), (3, 30)] {
            let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
            terminal.draw(|f| render(f, &app)).unwrap();
        }

        // M5: an open text prompt and the two screens that carry a url reference line.
        let (_prompt_dir, mut prompt_app) = fixture_app();
        prompt_app.handle_key(KeyCode::Tab).unwrap();
        prompt_app.handle_key(KeyCode::Char('a')).unwrap();
        draw_tiny(&prompt_app);
        let (_card_dir, card_app) = app_with_file("pom.md", URL_CARD);
        draw_tiny(&card_app);
        let (_art_dir, art_app) = app_with_file("art.md", URL_ARTICLE);
        draw_tiny(&art_app);
        let (_empty_dir, empty_app) = app_with_file(
            "empty-import.md",
            "---\ntype: article\nurl: https://example.com/\nimported: 2026-09-20\n---\n",
        );
        draw_tiny(&empty_app);

        // M6: a card carrying an encoded image and a sound, before and after the reveal.
        let (_media_dir, mut media, _log) = media_app(&[]);
        open_card(&mut media, "pomelo.md");
        draw_tiny(&media);
        for (w, h) in [(20u16, 3u16), (40, 2), (3, 30)] {
            let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
            terminal.draw(|f| render(f, &media)).unwrap();
        }
        media.handle_key(KeyCode::Char(' ')).unwrap();
        draw_tiny(&media);

        // M7: an article whose last paragraph is a picture, with the cursor on it.
        let (_read_dir, mut read_media, _read_log) = media_app(&[]);
        open_card(&mut read_media, "earl-grey.md");
        read_media.handle_key(KeyCode::Char('j')).unwrap();
        read_media.handle_key(KeyCode::Char('j')).unwrap();
        draw_tiny(&read_media);
        let mut terminal = Terminal::new(TestBackend::new(80, 3)).unwrap();
        terminal.draw(|f| render(f, &read_media)).unwrap();
    }

    // ---- M7: pictures on the read screen ----

    /// The first row of the rendered buffer that carries image cells.
    fn first_image_row(buf: &Buffer) -> Option<u16> {
        (0..buf.area.height).find(|&y| image_cells(buf, y, y) > 0)
    }

    /// An article whose paragraphs are `# T`, then `extra`, as a one-file vault.
    fn embed_article(extra: &str) -> String {
        format!("---\ntype: article\nsm_id: 80\nprio: 5\n---\n# T\n\n{extra}")
    }

    #[test]
    fn read_draws_a_picture_paragraph_in_the_flow() {
        let (_d, mut app, _log) = media_app(&[]);
        open_card(&mut app, "earl-grey.md");
        let buf = buffer_at(&app, 80, 24);
        let r = rows_of(&buf);
        assert!(r[2].contains("# Earl Grey"), "the heading is the first body row: {r:?}");
        assert!(r[4].contains("Earl Grey is a tea blend"), "the sentence follows it: {r:?}");
        assert!(
            !r.iter().any(|l| l.contains("![[buddhas-hand.jpg]]")),
            "the embed line is never shown as text: {r:?}"
        );
        assert!(image_cells(&buf, 4, 14) >= 1, "the picture is drawn under the text: {r:?}");
        assert_eq!(image_cells(&buf, 0, 4), 0, "and never over it: {r:?}");
    }

    #[test]
    fn read_picture_paragraph_gets_the_gutter_mark() {
        let (_d, mut app, _log) = media_app(&[]);
        open_card(&mut app, "earl-grey.md");

        // The cursor starts on the heading, so the picture's gutter is blank.
        let buf = buffer_at(&app, 80, 24);
        let r = rows_of(&buf);
        let heading = r.iter().position(|l| l.contains("# Earl Grey")).unwrap();
        assert!(r[heading].starts_with("▎ "), "the bar is on the heading: {r:?}");
        let y = first_image_row(&buf).unwrap();
        assert_eq!(buf[(0, y)].symbol(), " ", "no mark on a picture the cursor is not on");

        app.handle_key(KeyCode::Char('j')).unwrap();
        app.handle_key(KeyCode::Char('j')).unwrap();
        assert_eq!(app.read.as_ref().unwrap().cursor, 2, "the cursor is on the embed paragraph");
        let buf = buffer_at(&app, 80, 24);
        let y = first_image_row(&buf).unwrap();
        assert_eq!(buf[(0, y)].symbol(), "▎", "the bar moves onto the picture");
        assert_eq!(buf[(0, y)].style().fg, Some(AMBER), "and it is amber");
    }

    #[test]
    fn read_harvested_picture_shows_a_dot() {
        let (_d, mut app, _log) = media_app(&[]);
        open_card(&mut app, "earl-grey.md");
        app.handle_key(KeyCode::Char('j')).unwrap();
        app.handle_key(KeyCode::Char('j')).unwrap();
        app.handle_key_with(KeyCode::Char('x'), KeyModifiers::CONTROL).unwrap();
        app.handle_key(KeyCode::Char('k')).unwrap();

        let buf = buffer_at(&app, 80, 24);
        let y = first_image_row(&buf).unwrap();
        assert_eq!(buf[(0, y)].symbol(), "•", "a harvested picture gets the dot");
        assert!(
            buf[(0, y)].style().add_modifier.contains(Modifier::DIM),
            "the dot is dim"
        );
    }

    #[test]
    fn read_missing_picture_shows_placeholder_in_flow() {
        let (_d, app, _log) = solo_media_app(&[("t.md", &embed_article("text\n\n![[nope.png]]\n"))]);
        assert_eq!(app.read.as_ref().unwrap().item.path, "t.md");
        let buf = buffer_at(&app, 80, 24);
        let r = rows_of(&buf);
        let y = r
            .iter()
            .position(|l| l.contains("[image: nope.png · not found]"))
            .unwrap_or_else(|| panic!("no placeholder row: {r:?}"));
        assert_eq!(r[y], "  [image: nope.png · not found]", "gutter-prefixed: {r:?}");
        assert!(
            buf[(2, y as u16)].style().add_modifier.contains(Modifier::DIM),
            "the placeholder is dim"
        );
        assert_eq!(image_cells(&buf, 0, 23), 0, "nothing is drawn: {r:?}");
    }

    #[test]
    fn read_audio_and_other_embeds_are_one_dim_row() {
        let (_d, mut app, log) =
            solo_media_app(&[("t.md", &embed_article("![[pomelo.mp3]]\n\n![[x.svg]]\n"))]);
        let buf = buffer_at(&app, 80, 24);
        let r = rows_of(&buf);
        let sound = r.iter().position(|l| l == "  ♪ pomelo.mp3").unwrap_or_else(|| panic!("{r:?}"));
        let other = r.iter().position(|l| l == "  [embed: x.svg]").unwrap_or_else(|| panic!("{r:?}"));
        assert!(
            buf[(2, sound as u16)].style().add_modifier.contains(Modifier::DIM),
            "the audio row is dim"
        );
        assert!(
            buf[(2, other as u16)].style().add_modifier.contains(Modifier::DIM),
            "the other-embed row is dim"
        );
        assert!(log.lock().unwrap().plays.is_empty(), "the read screen never plays a sound");

        app.handle_key(KeyCode::Char('j')).unwrap();
        app.handle_key(KeyCode::Char('k')).unwrap();
        assert!(log.lock().unwrap().plays.is_empty(), "moving the cursor plays nothing either");
    }

    #[test]
    fn read_picture_without_room_falls_back_to_placeholder() {
        let (_d, mut app, _log) = media_app(&[]);
        open_card(&mut app, "earl-grey.md");
        app.handle_key(KeyCode::Char('j')).unwrap();
        app.handle_key(KeyCode::Char('j')).unwrap();
        app.set_viewport(80, 6);

        let buf = buffer_at(&app, 80, 6);
        let r = rows_of(&buf);
        assert!(
            r.iter().any(|l| l.contains("[image: buddhas-hand.jpg]")),
            "a picture with no room is the M0 placeholder again: {r:?}"
        );
        assert_eq!(image_cells(&buf, 0, 5), 0, "and nothing is drawn: {r:?}");
    }

    #[test]
    fn read_scroll_keeps_the_cursor_picture_visible() {
        let text: String = (1..=8).map(|i| format!("Paragraph {i}\n\n")).collect();
        let body = format!(
            "---\ntype: article\nsm_id: 82\nprio: 5\n---\n{text}![[buddhas-hand.jpg]]\n"
        );
        let (_d, mut app, _log) = solo_media_app(&[("long.md", &body)]);
        assert_eq!(app.read.as_ref().unwrap().item.path, "long.md");
        for _ in 0..8 {
            app.handle_key(KeyCode::Char('j')).unwrap();
        }
        assert_eq!(app.read.as_ref().unwrap().cursor, 8, "the cursor is on the picture");

        let buf = buffer_at(&app, 80, 24);
        let r = rows_of(&buf);
        let y = first_image_row(&buf).unwrap_or_else(|| panic!("no picture on screen: {r:?}"));
        let last_text = r
            .iter()
            .position(|l| l.contains("Paragraph 8"))
            .unwrap_or_else(|| panic!("no context above the picture: {r:?}"));
        assert!(last_text < y as usize, "the last text paragraph sits above it: {r:?}");
    }

    // ---- M8: the stats screen ----

    /// The fixture vault with the stats screen open, reached from the table.
    fn stats_app() -> (tempfile::TempDir, App) {
        let (dir, mut app) = fixture_app();
        app.handle_key(KeyCode::Tab).unwrap();
        app.handle_key(KeyCode::Char('s')).unwrap();
        assert_eq!(app.screen, Screen::Stats);
        (dir, app)
    }

    /// The rendered rows that belong to the calendar: the whole window is September
    /// 2026, so a date is how a calendar row starts.
    fn calendar_rows(r: &[String]) -> Vec<String> {
        r.iter()
            .filter(|l| l.trim_start_matches(['▎', ' ']).starts_with("09-"))
            .cloned()
            .collect()
    }

    #[test]
    fn stats_screen_shows_the_fixture_numbers_and_calendar() {
        let (_d, app) = stats_app();
        let screen = at(&app, 80, 24);
        for field in [
            "first day    —",
            "memorized    4 · pending 4 · dismissed 0 burden",
            "repetitions  — · 0 total",
            "lapses       — · 0 today",
            "outstanding  4+2",
            "burden       1.57 + 0.00 /day",
            "measured FI  —",
            "interval     6.0 d (I) · — (T)",
        ] {
            assert!(screen.contains(field), "missing {field:?} in\n{screen}");
        }

        let r = rows(&app);
        let buf = buffer(&app);
        assert!(r[0].ends_with("stats · 8 items"), "{:?}", r[0]);
        assert_eq!(r[23], "s/esc back · q quit");
        assert_eq!(buf[(0, 23)].style().fg, Some(AMBER), "the hint key is amber");

        // Four number rows, one blank, then the calendar. The left column's widest
        // value fills its half exactly, so the right column carries a one-cell pad.
        assert_eq!(
            r[3],
            "memorized    4 · pending 4 · dismissed 0 burden       1.57 + 0.00 /day",
            "the columns keep a gap at 80: {r:?}"
        );
        assert_eq!(r[6], "", "one blank row under the numbers: {r:?}");
        let cal = calendar_rows(&r);
        assert_eq!(cal.len(), 15, "15 calendar rows at 80x24: {r:?}");
        assert!(cal[0].contains("09-13 Sun"), "first calendar row: {:?}", cal[0]);
        assert!(cal[14].contains("09-27 Sun"), "last calendar row: {:?}", cal[14]);
        assert!(!screen.contains("09-30"), "09-30 is outside the window:\n{screen}");

        // Today: amber gutter bar, amber date, one `▮` per due item and the count.
        let y = r.iter().position(|l| l.contains("09-20 Sun")).unwrap() as u16;
        assert_eq!(r[y as usize], "▎ 09-20 Sun  ▮▮▮▮▮▮ 6", "{r:?}");
        assert_eq!(cal[7], r[y as usize], "today sits in the middle of the window");
        assert_eq!(buf[(0, y)].symbol(), "▎");
        assert_eq!(buf[(0, y)].style().fg, Some(AMBER), "the gutter bar is amber");
        assert_eq!(buf[(2, y)].style().fg, Some(AMBER), "today's date is amber");

        // A future day with one due item, and a zero day with no bar at all.
        let y = r.iter().position(|l| l.contains("09-25 Fri")).unwrap() as u16;
        assert_eq!(r[y as usize], "  09-25 Fri  ▮ 1", "{r:?}");
        assert!(
            !buf[(2, y)].style().add_modifier.contains(Modifier::DIM),
            "a future date is plain"
        );
        let y = r.iter().position(|l| l.contains("09-21 Mon")).unwrap() as u16;
        assert_eq!(r[y as usize], "  09-21 Mon  0", "a zero day has no bar: {r:?}");
        assert!(
            buf[(13, y)].style().add_modifier.contains(Modifier::DIM),
            "the zero is dim"
        );

        // A past day is dim and carries no gutter bar.
        let y = r.iter().position(|l| l.contains("09-13 Sun")).unwrap() as u16;
        assert_eq!(buf[(0, y)].symbol(), " ", "no gutter bar off today");
        assert!(
            buf[(2, y)].style().add_modifier.contains(Modifier::DIM),
            "a past date is dim"
        );
    }

    #[test]
    fn stats_screen_survives_tiny_sizes() {
        let (_d, app) = stats_app();
        draw_tiny(&app);

        // Two content rows: the first two number rows of each column, no calendar.
        let r = rows_of(&buffer_at(&app, 80, 6));
        assert!(r[2].contains("first day") && r[2].contains("outstanding"), "{r:?}");
        assert!(r[3].contains("memorized") && r[3].contains("burden"), "{r:?}");
        assert!(!r[2].contains("repetitions"), "{r:?}");
        assert!(calendar_rows(&r).is_empty(), "no room for the calendar: {r:?}");

        // Six content rows: four number rows, the blank, and today alone.
        let r = rows_of(&buffer_at(&app, 80, 10));
        assert!(r[5].contains("lapses") && r[5].contains("interval"), "{r:?}");
        assert_eq!(r[6], "", "{r:?}");
        assert_eq!(calendar_rows(&r), vec!["▎ 09-20 Sun  ▮▮▮▮▮▮ 6".to_string()], "{r:?}");

        // Narrow but not degenerate: the stacked block and a cut-down calendar row.
        for (w, h) in [(20u16, 3u16), (40, 2), (3, 30), (12, 24)] {
            let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
            terminal.draw(|f| render(f, &app)).unwrap();
        }
    }

    #[test]
    fn stats_screen_stacks_the_numbers_below_eighty_columns() {
        let (_d, app) = stats_app();
        let buf = buffer_at(&app, 70, 24);
        let r = rows_of(&buf);

        // Under 80 columns two halves would clip the widest value, so the eight
        // fields take one row each in `fields()` order and nothing is cut.
        assert_eq!(r[2], "first day    —", "{r:?}");
        assert_eq!(r[3], "memorized    4 · pending 4 · dismissed 0", "{r:?}");
        assert_eq!(r[4], "repetitions  — · 0 total", "{r:?}");
        assert_eq!(r[5], "lapses       — · 0 today", "{r:?}");
        assert_eq!(r[6], "outstanding  4+2", "{r:?}");
        assert_eq!(r[7], "burden       1.57 + 0.00 /day", "{r:?}");
        assert_eq!(r[8], "measured FI  —", "{r:?}");
        assert_eq!(r[9], "interval     6.0 d (I) · — (T)", "{r:?}");
        assert_eq!(r[10], "", "one blank row under the numbers: {r:?}");

        // 20 content rows less the eight numbers and the blank: 5 past, today, 5 future.
        let cal = calendar_rows(&r);
        assert_eq!(cal.len(), 11, "{r:?}");
        assert!(cal[0].contains("09-15 Tue"), "{:?}", cal[0]);
        assert_eq!(cal[5], "▎ 09-20 Sun  ▮▮▮▮▮▮ 6", "today in the middle: {r:?}");
        assert_eq!(cal[10], "  09-25 Fri  ▮ 1", "{r:?}");
        let y = r.iter().position(|l| l.contains("09-20 Sun")).unwrap() as u16;
        assert_eq!(buf[(0, y)].style().fg, Some(AMBER), "the gutter bar is still amber");
        assert_eq!(buf[(2, y)].style().fg, Some(AMBER), "today's date is still amber");
        assert!(r.iter().all(|l| l.chars().count() <= 70), "nothing overflows: {r:?}");
    }

    #[test]
    fn queue_and_review_hints_offer_stats() {
        let (_d, mut app) = fixture_app();
        assert_eq!(
            rows(&app)[23],
            "space reveal · 0-5 grade · u undo · tab queue · s stats · q quit",
            "the session offers stats before quit"
        );

        app.handle_key(KeyCode::Tab).unwrap();
        let r = rows(&app);
        assert_eq!(
            r[23],
            "j/k move · enter open · a add · i import · s stats · p prio · tab learn · q quit"
        );
        let import = r[23].find("i import").unwrap();
        let stats = r[23].find("s stats").unwrap();
        let prio = r[23].find("p prio").unwrap();
        assert!(import < stats && stats < prio, "s stats follows i import: {:?}", r[23]);

        // The drill prompt keeps its own keys; the finish line behind it offers stats.
        let (_d2, mut app) = fixture_app();
        walk_to_prompt(&mut app, ['2', '4', '4', '4']);
        assert!(!rows(&app)[23].contains("s stats"), "{:?}", rows(&app)[23]);
        app.handle_key(KeyCode::Char('n')).unwrap();
        assert_eq!(rows(&app)[23], "tab queue · s stats · q quit");
    }
}
