//! Rendering. Every screen is the same three regions joined by two thin rule
//! lines: status row, rule, content, rule, key hints. No bordered panels, no
//! titles; the only `Block` in the crate is the rounded box in `note_box`.
//! Screens are pure functions of `&App`.

mod queue;
mod read;
mod review;

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Padding, Paragraph};
use ratatui::Frame;

use crate::app::{App, Phase, Screen};

/// Amber for key letters in the hints row (256-color index, works without truecolor).
pub const AMBER: Color = Color::Indexed(214);
pub const COLLECTION_NAME: &str = "all";

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

    // The status row's middle: the priority prompt, else a notice, else the selection.
    let middle = app
        .prompt_text()
        .or_else(|| app.notice.clone())
        .or_else(|| app.selection_status());
    let prompt_open = app.prompt_text().is_some();
    match app.screen {
        Screen::Queue => {
            status_row(frame, top, middle.as_deref(), &app.queue_context(), app.progress());
            queue::render(frame, content, app);
            hints_row(frame, hints, if prompt_open { PROMPT_HINTS } else { queue::HINTS });
        }
        Screen::Review => {
            status_row(frame, top, middle.as_deref(), &app.review_context(), app.progress());
            review::render(frame, content, app);
            hints_row(frame, hints, review_hints(app));
        }
        Screen::Read => {
            status_row(frame, top, middle.as_deref(), &app.read_context(), app.progress());
            read::render(frame, content, app);
            let hints_for = if prompt_open {
                PROMPT_HINTS
            } else if app.selection_status().is_some() {
                read::SELECT_HINTS
            } else {
                read::HINTS
            };
            hints_row(frame, hints, hints_for);
        }
    }
}

/// The review hints follow the session: the drill prompt, the drill, the finish
/// line, or a card in the main pass.
fn review_hints(app: &App) -> &'static [(&'static str, &'static str)] {
    match (app.review.phase, app.review.current.is_some()) {
        (Phase::DrillPrompt, _) => review::DRILL_PROMPT_HINTS,
        (Phase::Drilling, _) => review::DRILL_HINTS,
        (Phase::Main, false) => review::DONE_HINTS,
        (Phase::Main, true) => review::HINTS,
    }
}

/// Hints while the priority prompt is open, on any screen.
pub const PROMPT_HINTS: &[(&str, &str)] = &[("0-9", "value"), ("j/k", "nudge"), ("enter", "set"), ("esc", "cancel")];

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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::app::App;
    use chrono::NaiveDate;
    use crossterm::event::KeyCode;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::path::Path;

    fn fixture_app() -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/vault");
        for entry in std::fs::read_dir(&src).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_file() {
                std::fs::copy(entry.path(), dir.path().join(entry.file_name())).unwrap();
            }
        }
        let today = NaiveDate::from_ymd_opt(2026, 9, 20).unwrap();
        let app = App::open(dir.path(), today).unwrap();
        (dir, app)
    }

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
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| render(f, app)).unwrap();
        let buf = terminal.backend().buffer();
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
    fn buffer(app: &App) -> ratatui::buffer::Buffer {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| render(f, app)).unwrap();
        terminal.backend().buffer().clone()
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
        assert_eq!(r[23], "j/k move · enter open · p prio · tab learn · q quit", "p is a queue key (M2)");
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
        assert!(all.contains("[audio: pomelo.mp3]"), "{all}");
        assert!(!all.contains("A:"), "markers never shown");
        assert!(all.contains("0 null   1 bad   2 fail   │   3 pass   4 good   5 bright"), "{all}");
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
        assert_eq!(r[23], "space reveal · 0-5 grade · u undo · tab queue · q quit");
    }

    #[test]
    fn finish_line_renders_when_nothing_is_due() {
        let (_d, app) = vault_with(&["pomelo.md", "buddhas-hand.md"]);
        let r = rows(&app);
        let title = r.iter().position(|l| l.contains("nothing more to learn")).unwrap();
        let counts = r.iter().position(|l| l.contains("0 graded · 0 read")).unwrap();
        assert_eq!(counts, title + 1, "the counts sit on their own row under the title: {r:?}");
        assert_eq!(r[23], "tab queue · q quit");
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
    }
}
