//! Rendering. Every screen is the same chromeless three-row layout:
//! status row, content, key hints. Screens are pure functions of `&App`.

mod queue;
mod read;
mod review;

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::app::{App, Screen};

/// Amber for key letters in the hints row (256-color index, works without truecolor).
pub const AMBER: Color = Color::Indexed(214);
pub const COLLECTION_NAME: &str = "all";

pub fn render(frame: &mut Frame, app: &App) {
    let [top, content, hints] = frame
        .area()
        .layout(&Layout::vertical([
            Constraint::Length(1),
            Constraint::Fill(1),
            Constraint::Length(1),
        ]));

    // The status row's middle: the priority prompt, else a notice, else the selection.
    let middle = app
        .prompt_text()
        .or_else(|| app.notice.clone())
        .or_else(|| app.selection_status());
    let prompt_open = app.prompt_text().is_some();
    match app.screen {
        Screen::Queue => {
            status_row(frame, top, middle.as_deref(), &app.queue_context());
            queue::render(frame, content, app);
            hints_row(frame, hints, if prompt_open { PROMPT_HINTS } else { queue::HINTS });
        }
        Screen::Review => {
            status_row(frame, top, middle.as_deref(), &app.review_context());
            review::render(frame, content, app);
            hints_row(frame, hints, review::HINTS);
        }
        Screen::Read => {
            status_row(frame, top, middle.as_deref(), &app.read_context());
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

/// Hints while the priority prompt is open, on any screen.
pub const PROMPT_HINTS: &[(&str, &str)] = &[("0-9", "value"), ("j/k", "nudge"), ("enter", "set"), ("esc", "cancel")];

/// Collection name left, optional notice in the middle, screen context right (dim).
fn status_row(frame: &mut Frame, area: Rect, notice: Option<&str>, context: &str) {
    let name = Span::from(COLLECTION_NAME).bold();
    let ctx = Line::from(context).dim().right_aligned();
    let [left, middle, right] = area.layout(&Layout::horizontal([
        Constraint::Length(name.width() as u16),
        Constraint::Fill(1),
        Constraint::Length(ctx.width() as u16),
    ]));
    frame.render_widget(Line::from(name), left);
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

/// A dim centered one-liner, for empty states.
fn centered_note(frame: &mut Frame, area: Rect, text: &str) {
    let [_, row, _] = area.layout(&Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(1),
        Constraint::Fill(1),
    ]));
    frame.render_widget(
        Paragraph::new(text)
            .style(Style::new().dim())
            .centered(),
        row,
    );
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

    #[test]
    fn queue_screen_has_status_row_items_and_hints() {
        let (_d, app) = fixture_app();
        let r = rows(&app);
        assert!(r[0].starts_with("all"), "{:?}", r[0]);
        assert!(r[0].ends_with("queue · sort prio"), "{:?}", r[0]);
        let body = r[1..23].join("\n");
        assert!(body.contains("Japanese citrus, fragrant, used in ponzu?"), "{body}");
        assert!(body.contains("Citrus vocabulary"), "{body}");
        assert!(body.contains("article"), "{body}");
        assert!(body.contains("card"), "{body}");
        assert!(r[23].contains("enter open"), "{:?}", r[23]);
        assert!(r[23].contains("q quit"), "{:?}", r[23]);
        assert_eq!(r[23], "j/k move · enter open · p prio · tab review · q quit", "p is a queue key (M2)");
        assert!(!r.iter().any(|l| l.contains('│') || l.contains('┌')), "no borders");
    }

    #[test]
    fn queue_notice_appears_in_status_area() {
        let (_d, mut app) = fixture_app();
        app.notice = Some("reading arrives in M2".to_string());
        let r = rows(&app);
        assert!(r[0].contains("reading arrives in M2"), "{:?}", r[0]);
    }

    #[test]
    fn review_hides_answer_until_revealed_and_shows_grade_row_after() {
        let (_d, mut app) = fixture_app();
        let idx = app.items.iter().position(|i| i.path == "pomelo.md").unwrap();
        app.queue_sel = idx;
        app.handle_key(KeyCode::Enter).unwrap();
        let r = rows(&app);
        let all = r.join("\n");
        assert!(r[0].ends_with("card · prio 28 · due 5 · done 0/5"), "{:?}", r[0]);
        assert!(all.contains("Large citrus fruit with a thick rind"), "{all}");
        assert!(all.contains("[image: pomelo.png]"), "{all}");
        assert!(!all.contains("Q:"), "markers never shown");
        assert!(!all.contains("pomelo /"), "answer hidden: {all}");
        assert!(all.contains("↳ citrus-vocab.md"), "{all}");
        assert!(all.contains("space reveal"), "{all}");
        assert!(!all.contains("[5 bright]"), "grade row hidden before reveal");

        app.handle_key(KeyCode::Char(' ')).unwrap();
        let all = rows(&app).join("\n");
        assert!(all.contains("pomelo /ˈpɒmɪloʊ/"), "{all}");
        assert!(all.contains("[audio: pomelo.mp3]"), "{all}");
        assert!(!all.contains("A:"), "markers never shown");
        assert!(all.contains("[0 null] [1 bad] [2 fail]   [3 pass] [4 good] [5 bright]"), "{all}");
    }

    #[test]
    fn review_status_line_after_grade_and_empty_state() {
        let (_d, mut app) = fixture_app();
        app.handle_key(KeyCode::Tab).unwrap();
        app.handle_key(KeyCode::Char(' ')).unwrap();
        app.handle_key(KeyCode::Char('4')).unwrap();
        let all = rows(&app).join("\n");
        assert!(all.contains("graded 4 · journaled (offline)"), "{all}");
        for _ in 0..3 {
            app.handle_key(KeyCode::Char(' ')).unwrap();
            app.handle_key(KeyCode::Char('3')).unwrap();
        }
        let all = rows(&app).join("\n");
        assert!(all.contains("no cards due"), "{all}");
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
        app.handle_key(KeyCode::Tab).unwrap();
        app.handle_key(KeyCode::Char(' ')).unwrap();
        app.handle_key(KeyCode::Char('4')).unwrap();
        let r = rows(&app);
        assert!(r[0].ends_with("card · prio 35 · due 4 · done 1/4 · 1 unsynced"), "{:?}", r[0]);
        assert!(r.join("\n").contains("graded 4 · journaled · sync in 5s"), "{r:?}");
        app.handle_key(KeyCode::Tab).unwrap();
        let r = rows(&app);
        assert!(r[0].ends_with("queue · sort prio · 1 unsynced"), "{:?}", r[0]);
        app.handle_key(KeyCode::Tab).unwrap();
        app.handle_key(KeyCode::Char('u')).unwrap();
        let r = rows(&app);
        assert!(r[0].ends_with("card · prio 12 · due 4 · done 0/4"), "{:?}", r[0]);
    }

    #[test]
    fn every_sync_status_line_variant_renders_in_full() {
        let (_d, mut app) = fixture_app();
        app.handle_key(KeyCode::Tab).unwrap();
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
        assert!(all.contains("▸ Pomelo is the largest"), "gutter mark on the current paragraph: {all}");
        assert!(!all.contains("▸ # Citrus"), "{all}");
        assert_eq!(r[23], "j/k ¶ · w/b word · v mark · ^x extract · ^z cloze · enter next · d done · p prio");
        assert!(cell_mods(&app, "Pomelo is").contains(Modifier::DIM), "harvested span is dim");
        assert!(!cell_mods(&app, "A long-form").contains(Modifier::DIM), "unharvested text is not");
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
        let idx = app.items.iter().position(|i| i.path == "long.md").unwrap();
        app.queue_sel = idx;
        app.handle_key(KeyCode::Enter).unwrap();
        for _ in 0..29 {
            app.handle_key(KeyCode::Char('j')).unwrap();
        }
        let r = rows(&app);
        assert!(r.iter().any(|l| l.starts_with("▸ Paragraph 30")), "last paragraph on screen: {r:?}");
        assert!(r.iter().all(|l| l.chars().count() <= 80), "wrapped");
        assert!(r.iter().filter(|l| l.contains("word word")).count() > 3, "several wrapped rows: {r:?}");
        drop(app);
    }

    #[test]
    fn queue_shows_article_due_like_cards() {
        let (_d, app) = fixture_app();
        let r = rows(&app);
        let earl = r.iter().find(|l| l.contains("Earl Grey")).unwrap();
        assert!(earl.contains("now"), "{earl}");
        assert!(!earl.contains('—'), "{earl}");
    }

    #[test]
    fn renders_at_narrow_sizes_without_panicking() {
        let (_d, mut app) = fixture_app();
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
        app.handle_key(KeyCode::Tab).unwrap();
        open_article(&mut app);
        app.handle_key(KeyCode::Char('v')).unwrap();
        app.handle_key(KeyCode::Char('p')).unwrap();
        for (w, h) in [(20u16, 3u16), (1, 1), (0, 0), (40, 2), (3, 30)] {
            let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
            terminal.draw(|f| render(f, &app)).unwrap();
        }
    }
}
