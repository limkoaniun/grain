//! App state machine: which screen is showing, what the keys do, and the
//! review session. Rendering lives in `ui`; persistence lives in `db`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{NaiveDate, SecondsFormat, Utc};
use crossterm::event::KeyCode;

use crate::db::{Db, ItemRow};
use crate::vault::frontmatter::ItemType;
use crate::vault::index::{load_card, refresh, LoadedCard, RefreshReport};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Queue,
    Review,
}

/// The card on screen in review.
#[derive(Debug, Clone)]
pub struct CurrentCard {
    pub item: ItemRow,
    pub card: LoadedCard,
}

/// One grade that can be undone this session.
#[derive(Debug, Clone, Copy)]
struct Graded {
    journal_id: i64,
    grade: u8,
    /// Position in `Review::due` to return to on undo.
    pos: usize,
}

/// Review session over the cards due today.
#[derive(Debug, Default)]
pub struct Review {
    /// Due card sm_ids in queue order, fixed at startup (plus any card opened early).
    pub due: Vec<i64>,
    /// Index into `due`; equal to `due.len()` when the session is finished.
    pub pos: usize,
    pub done: usize,
    pub revealed: bool,
    pub current: Option<CurrentCard>,
    /// Status line under the grade row, e.g. `graded 4 · journaled (offline)`.
    pub status: Option<String>,
    history: Vec<Graded>,
}

pub struct App {
    pub root: PathBuf,
    pub db: Db,
    pub items: Vec<ItemRow>,
    pub screen: Screen,
    pub queue_sel: usize,
    pub review: Review,
    /// One-line notice for the queue status area (e.g. `reading arrives in M2`).
    pub notice: Option<String>,
    pub should_quit: bool,
    pub refresh: RefreshReport,
}

impl App {
    /// Open the vault, refresh the index and build the queue and review session.
    pub fn open(root: &Path, today: NaiveDate) -> Result<Self> {
        let db = Db::open(&root.join(".grain").join("grain.db"))?;
        let refresh = refresh(root, &db).context("refreshing index")?;
        let items = db.queue()?;
        let due = db.due_cards(today)?.into_iter().map(|i| i.sm_id).collect();
        Ok(App {
            root: root.to_path_buf(),
            db,
            items,
            screen: Screen::Queue,
            queue_sel: 0,
            review: Review {
                due,
                ..Review::default()
            },
            notice: None,
            should_quit: false,
            refresh,
        })
    }

    pub fn queue_context(&self) -> String {
        "queue · sort prio".to_string()
    }

    pub fn review_context(&self) -> String {
        let due = self.review.due.len();
        let done = self.review.done;
        match &self.review.current {
            Some(cur) => format!("card · prio {} · due {due} · done {done}/{due}", cur.item.prio),
            None => format!("card · due {due} · done {done}/{due}"),
        }
    }

    /// Dispatch one key press. Unknown keys are ignored.
    pub fn handle_key(&mut self, key: KeyCode) -> Result<()> {
        self.notice = None;
        match key {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Tab | KeyCode::BackTab => self.cycle_screen()?,
            _ => match self.screen {
                Screen::Queue => self.handle_queue_key(key)?,
                Screen::Review => self.handle_review_key(key)?,
            },
        }
        Ok(())
    }

    fn handle_queue_key(&mut self, key: KeyCode) -> Result<()> {
        match key {
            KeyCode::Char('j') | KeyCode::Down => {
                if self.queue_sel + 1 < self.items.len() {
                    self.queue_sel += 1;
                }
            }
            KeyCode::Char('k') | KeyCode::Up => self.queue_sel = self.queue_sel.saturating_sub(1),
            KeyCode::Enter => {
                let Some(item) = self.items.get(self.queue_sel) else {
                    return Ok(());
                };
                match item.kind {
                    ItemType::Card => self.open_review_at(item.sm_id)?,
                    ItemType::Article => self.notice = Some("reading arrives in M2".to_string()),
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_review_key(&mut self, key: KeyCode) -> Result<()> {
        match key {
            KeyCode::Char(' ') => {
                if self.review.current.is_some() {
                    self.review.revealed = true;
                }
            }
            KeyCode::Char(c @ '0'..='5') => {
                if self.review.revealed {
                    let grade = c as u8 - b'0';
                    self.grade(grade)?;
                }
            }
            KeyCode::Char('u') => self.undo()?,
            _ => {}
        }
        Ok(())
    }

    fn cycle_screen(&mut self) -> Result<()> {
        self.screen = match self.screen {
            Screen::Queue => {
                self.load_current()?;
                Screen::Review
            }
            Screen::Review => Screen::Queue,
        };
        Ok(())
    }

    /// Jump the review session to `sm_id`, inserting it if it is not due yet.
    fn open_review_at(&mut self, sm_id: i64) -> Result<()> {
        match self.review.due.iter().position(|&id| id == sm_id) {
            Some(idx) => self.review.pos = idx,
            None => {
                let at = self.review.pos.min(self.review.due.len());
                self.review.due.insert(at, sm_id);
                self.review.pos = at;
            }
        }
        self.review.status = None;
        self.load_current()?;
        self.screen = Screen::Review;
        Ok(())
    }

    /// Load the card at `review.pos` from disk, or clear when the session is finished.
    fn load_current(&mut self) -> Result<()> {
        self.review.revealed = false;
        let Some(&sm_id) = self.review.due.get(self.review.pos) else {
            self.review.current = None;
            return Ok(());
        };
        let item = self
            .db
            .item(sm_id)?
            .with_context(|| format!("sm_id {sm_id} vanished from the index"))?;
        let card = load_card(&self.root, &item.path)?;
        self.review.current = Some(CurrentCard { item, card });
        Ok(())
    }

    /// Journal one grade (committed before the UI moves on), then advance.
    fn grade(&mut self, grade: u8) -> Result<()> {
        let Some(cur) = &self.review.current else {
            return Ok(());
        };
        let graded_at = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
        let journal_id = self.db.insert_grade(cur.item.sm_id, grade, &graded_at)?;
        self.review.history.push(Graded {
            journal_id,
            grade,
            pos: self.review.pos,
        });
        self.review.done += 1;
        self.review.status = Some(format!("graded {grade} · journaled (offline)"));
        self.review.pos += 1;
        self.load_current()
    }

    /// Remove the newest unsynced grade of this session and go back to that card.
    fn undo(&mut self) -> Result<()> {
        let Some(last) = self.review.history.pop() else {
            self.review.status = Some("nothing to undo".to_string());
            return Ok(());
        };
        if !self.db.delete_unsynced_grade(last.journal_id)? {
            self.review.status = Some(format!("cannot undo grade {} · already synced", last.grade));
            return Ok(());
        }
        self.review.done = self.review.done.saturating_sub(1);
        self.review.pos = last.pos;
        self.load_current()?;
        self.review.revealed = true;
        self.review.status = Some(format!("undid grade {}", last.grade));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use chrono::NaiveDate;
    use std::path::Path;

    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 9, 20).unwrap()
    }

    /// Copy fixtures/vault into a temp dir so tests never mutate the checked-in files.
    fn fixture_app() -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/vault");
        for entry in std::fs::read_dir(&src).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_file() {
                std::fs::copy(entry.path(), dir.path().join(entry.file_name())).unwrap();
            }
        }
        let app = App::open(dir.path(), today()).unwrap();
        (dir, app)
    }

    fn press(app: &mut App, c: char) {
        app.handle_key(KeyCode::Char(c)).unwrap();
    }

    #[test]
    fn opens_queue_with_all_eight_items_sorted_by_prio() {
        let (_d, app) = fixture_app();
        assert_eq!(app.screen, Screen::Queue);
        assert_eq!(app.items.len(), 8);
        let prios: Vec<i64> = app.items.iter().map(|i| i.prio).collect();
        assert!(prios.windows(2).all(|w| w[0] <= w[1]));
        assert_eq!(app.items[0].title.as_deref(), Some("Japanese citrus, fragrant, used in ponzu?"));
        assert_eq!(app.queue_context(), "queue · sort prio");
    }

    #[test]
    fn due_cards_are_those_due_today_or_earlier_or_undated() {
        let (_d, app) = fixture_app();
        // kumquat (09-10), yuzu (none), bergamot (09-01), finger-lime (none); pomelo 09-25 and
        // buddhas-hand 09-30 are not due.
        assert_eq!(app.review.due.len(), 4);
    }

    #[test]
    fn j_and_k_move_selection_within_bounds() {
        let (_d, mut app) = fixture_app();
        press(&mut app, 'k');
        assert_eq!(app.queue_sel, 0);
        for _ in 0..20 {
            press(&mut app, 'j');
        }
        assert_eq!(app.queue_sel, 7);
        press(&mut app, 'k');
        assert_eq!(app.queue_sel, 6);
    }

    #[test]
    fn enter_on_article_shows_notice_and_stays_on_queue() {
        let (_d, mut app) = fixture_app();
        let idx = app.items.iter().position(|i| i.kind == ItemType::Article).unwrap();
        app.queue_sel = idx;
        app.handle_key(KeyCode::Enter).unwrap();
        assert_eq!(app.screen, Screen::Queue);
        assert_eq!(app.notice.as_deref(), Some("reading arrives in M2"));
        press(&mut app, 'j');
        assert_eq!(app.notice, None, "notice clears on next key");
    }

    #[test]
    fn enter_on_card_opens_review_of_that_card() {
        let (_d, mut app) = fixture_app();
        let idx = app.items.iter().position(|i| i.path == "kumquat.md").unwrap();
        app.queue_sel = idx;
        app.handle_key(KeyCode::Enter).unwrap();
        assert_eq!(app.screen, Screen::Review);
        let cur = app.review.current.as_ref().unwrap();
        assert_eq!(cur.item.path, "kumquat.md");
        assert!(!app.review.revealed);
        assert_eq!(app.review_context(), "card · prio 35 · due 4 · done 0/4");
    }

    #[test]
    fn enter_on_not_yet_due_card_reviews_it_anyway() {
        let (_d, mut app) = fixture_app();
        let idx = app.items.iter().position(|i| i.path == "pomelo.md").unwrap();
        app.queue_sel = idx;
        app.handle_key(KeyCode::Enter).unwrap();
        assert_eq!(app.review.current.as_ref().unwrap().item.sm_id, 1042);
        assert_eq!(app.review.due.len(), 5);
    }

    #[test]
    fn grade_requires_reveal_then_journals_and_advances() {
        let (_d, mut app) = fixture_app();
        app.handle_key(KeyCode::Tab).unwrap();
        assert_eq!(app.screen, Screen::Review);
        let first = app.review.current.as_ref().unwrap().item.sm_id;
        press(&mut app, '4');
        assert_eq!(app.db.journal_count(first).unwrap(), 0, "grade before reveal is ignored");
        press(&mut app, ' ');
        assert!(app.review.revealed);
        press(&mut app, '4');
        assert_eq!(app.db.journal_count(first).unwrap(), 1);
        assert_eq!(app.review.status.as_deref(), Some("graded 4 · journaled (offline)"));
        assert_eq!(app.review.done, 1);
        let second = app.review.current.as_ref().unwrap().item.sm_id;
        assert_ne!(second, first);
        assert!(!app.review.revealed);
    }

    #[test]
    fn undo_removes_newest_grade_and_returns_to_that_card() {
        let (_d, mut app) = fixture_app();
        app.handle_key(KeyCode::Tab).unwrap();
        let first = app.review.current.as_ref().unwrap().item.sm_id;
        press(&mut app, ' ');
        press(&mut app, '3');
        press(&mut app, ' ');
        press(&mut app, '5');
        let third = app.review.current.as_ref().unwrap().item.sm_id;
        press(&mut app, 'u');
        let back = app.review.current.as_ref().unwrap().item.sm_id;
        assert_ne!(back, third);
        assert_eq!(app.review.done, 1);
        assert_eq!(app.db.journal_count(back).unwrap(), 0);
        assert_eq!(app.db.journal_count(first).unwrap(), 1);
        press(&mut app, 'u');
        assert_eq!(app.review.current.as_ref().unwrap().item.sm_id, first);
        assert_eq!(app.db.journal_count(first).unwrap(), 0);
        assert_eq!(app.review.done, 0);
        press(&mut app, 'u');
        assert_eq!(app.review.status.as_deref(), Some("nothing to undo"));
    }

    #[test]
    fn finishing_all_due_cards_leaves_review_empty() {
        let (_d, mut app) = fixture_app();
        app.handle_key(KeyCode::Tab).unwrap();
        for _ in 0..4 {
            press(&mut app, ' ');
            press(&mut app, '3');
        }
        assert!(app.review.current.is_none());
        assert_eq!(app.review_context(), "card · due 4 · done 4/4");
        press(&mut app, ' ');
        press(&mut app, '3');
        assert_eq!(app.review.done, 4, "grading with no card is a no-op");
        press(&mut app, 'u');
        assert!(app.review.current.is_some());
    }

    #[test]
    fn tab_cycles_screens_and_q_quits_and_unknown_keys_are_ignored() {
        let (_d, mut app) = fixture_app();
        app.handle_key(KeyCode::Tab).unwrap();
        assert_eq!(app.screen, Screen::Review);
        app.handle_key(KeyCode::Tab).unwrap();
        assert_eq!(app.screen, Screen::Queue);
        press(&mut app, 'x');
        app.handle_key(KeyCode::F(5)).unwrap();
        assert!(!app.should_quit);
        press(&mut app, 'q');
        assert!(app.should_quit);
    }

    #[test]
    fn second_open_allocates_nothing_and_ids_persist() {
        let (dir, app) = fixture_app();
        assert_eq!(app.refresh.allocated, 3, "bergamot, finger-lime, earl-grey");
        drop(app);
        let app = App::open(dir.path(), today()).unwrap();
        assert_eq!(app.refresh.allocated, 0);
        assert_eq!(app.refresh.unchanged, 8);
    }
}
