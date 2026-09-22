//! App state machine: which screen is showing, what the keys do, and the
//! review session. Rendering lives in `ui`; persistence lives in `db`;
//! HTTP lives on the sync worker thread and reaches here only through `tick`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::{DateTime, Local, NaiveDate, SecondsFormat, Utc};
use crossterm::event::{KeyCode, KeyModifiers};

use crate::db::{Db, ItemRow, Schedule};
use crate::vault::article::{self, Paragraph, Span};
use crate::vault::frontmatter::Document;
use crate::sync::api::{Identity, Scheduler, API_KEY_ENV};
use crate::sync::worker::{self, Worker};
use crate::sync::{
    Landed, Outbox, OutboxConfig, RowState, SyncRequest, DEFAULT_DAILY_CAP, DEFAULT_GRACE_SECS,
    RATE_SPACING,
};
use crate::vault::frontmatter::ItemType;
use crate::vault::index::{
    create_child, load_card, refresh, write_article_session, write_done, write_prio, write_read_pos,
    write_schedule, LoadedCard, RefreshReport,
};

/// How long `finish` waits for a reply already in flight.
const QUIT_INFLIGHT_WAIT: Duration = Duration::from_secs(2);
/// Total budget `finish` spends sending rows that were still waiting for grace.
const QUIT_DRAIN_BUDGET: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Queue,
    Review,
    Read,
}

/// Default `a_factor` when the article has none, and the clamp applied on read.
pub const DEFAULT_A_FACTOR: f64 = 1.5;
const A_FACTOR_RANGE: (f64, f64) = (1.01, 5.0);
/// Step of a `j`/`k` nudge in the priority prompt.
const PRIO_NUDGE: i64 = 5;

/// The next reading interval in days (M2, the SuperMemo topic rule computed locally):
/// the first interval scales with priority, 1 day at `prio` 0 to 30 at 100; every later
/// review multiplies the interval by `a_factor`, rounded, never below one day.
pub fn next_article_interval(prio: i64, interval: Option<i64>, a_factor: Option<f64>) -> i64 {
    let prio = prio.clamp(0, 100) as f64;
    let a = a_factor.unwrap_or(DEFAULT_A_FACTOR).clamp(A_FACTOR_RANGE.0, A_FACTOR_RANGE.1);
    match interval {
        None => 1 + (prio / 100.0 * 29.0).round() as i64,
        Some(i) => ((i.max(0) as f64) * a).round().max(1.0) as i64,
    }
}

/// A vim-style selection on the read screen, anchored where `v` was pressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Selection {
    /// Words of the cursor paragraph from `anchor` to the cursor word.
    Words { anchor: usize },
    /// Whole paragraphs from `anchor` to the cursor paragraph.
    Paragraphs { anchor: usize },
}

/// The article open on the read screen.
#[derive(Debug, Clone)]
pub struct Read {
    pub item: ItemRow,
    pub body: String,
    pub paragraphs: Vec<Paragraph>,
    /// Current paragraph index.
    pub cursor: usize,
    /// Current word index within the current paragraph.
    pub word: usize,
    /// Words of the current paragraph.
    pub words: Vec<Span>,
    pub selection: Option<Selection>,
    /// Spans of the body already harvested by children, for dim marks.
    pub children: Vec<Span>,
}

impl Read {
    pub fn paragraph(&self) -> Option<Paragraph> {
        self.paragraphs.get(self.cursor).copied()
    }

    pub fn current_word(&self) -> Option<Span> {
        self.words.get(self.word).copied()
    }

    /// The body span the selection covers, if any.
    pub fn selected_span(&self) -> Option<Span> {
        match self.selection? {
            Selection::Words { anchor } => {
                let (a, b) = (anchor.min(self.word), anchor.max(self.word));
                Some(Span {
                    start: self.words.get(a)?.start,
                    end: self.words.get(b)?.end,
                })
            }
            Selection::Paragraphs { anchor } => {
                let (a, b) = (anchor.min(self.cursor), anchor.max(self.cursor));
                Some(Span {
                    start: self.paragraphs.get(a)?.start,
                    end: self.paragraphs.get(b)?.end,
                })
            }
        }
    }

    /// The offset written as `read_pos`: the current paragraph's first character.
    fn read_pos(&self) -> Option<i64> {
        self.paragraph().map(|p| p.start as i64)
    }

    fn move_paragraph(&mut self, delta: isize) {
        if self.paragraphs.is_empty() {
            return;
        }
        let last = self.paragraphs.len() - 1;
        let from = self.cursor;
        let next = (from as isize + delta).clamp(0, last as isize) as usize;
        if next != from {
            self.cursor = next;
            self.word = 0;
            self.words = self.paragraph().map(|p| article::words(&self.body, &p)).unwrap_or_default();
        }
        if matches!(self.selection, Some(Selection::Words { .. })) {
            // A word selection becomes a paragraph selection on the first j/k, anchored
            // at the paragraph where `v` was pressed, whether or not the cursor moved.
            self.selection = Some(Selection::Paragraphs { anchor: from });
        }
    }

    fn move_word(&mut self, delta: isize) {
        if matches!(self.selection, Some(Selection::Paragraphs { .. })) || self.words.is_empty() {
            return;
        }
        let last = self.words.len() - 1;
        self.word = (self.word as isize + delta).clamp(0, last as isize) as usize;
    }
}

/// The inline priority prompt in the status row.
#[derive(Debug, Clone)]
struct Prompt {
    sm_id: i64,
    path: String,
    current: i64,
    value: i64,
    /// True until the first digit: the first digit replaces the value instead of appending.
    fresh: bool,
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

/// Sync machinery, present only when an API key is configured.
struct Sync {
    outbox: Outbox,
    worker: Worker,
}

/// The newest grade of the session, whose sync progress the status line follows.
#[derive(Debug, Clone, Copy)]
struct LastGrade {
    journal_id: i64,
    grade: u8,
    /// `(interval, due)` once the server has answered.
    synced: Option<(i64, NaiveDate)>,
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
    /// Things worth telling the user once the terminal is back (rejected rows, unwritable files).
    pub sync_log: Vec<String>,
    /// The article open on the read screen (M2).
    pub read: Option<Read>,
    prompt: Option<Prompt>,
    sync: Option<Sync>,
    last_grade: Option<LastGrade>,
    /// The clock as of the last `tick`; `grade` uses it for the grace deadline.
    now: Instant,
    /// The local date as of open or the last `tick`.
    today: NaiveDate,
}

impl App {
    /// Open the vault, refresh the index and build the queue and review session. Offline.
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
            sync_log: Vec::new(),
            read: None,
            prompt: None,
            sync: None,
            last_grade: None,
            now: Instant::now(),
            today,
        })
    }

    /// Like [`App::open`], then start syncing grades through `scheduler`: the outbox is
    /// loaded with every unsynced journal row and the worker thread is spawned.
    pub fn open_with_scheduler(root: &Path, today: NaiveDate, scheduler: Box<dyn Scheduler>) -> Result<Self> {
        let mut app = Self::open(root, today)?;
        app.enable_sync(scheduler)?;
        Ok(app)
    }

    fn enable_sync(&mut self, scheduler: Box<dyn Scheduler>) -> Result<()> {
        let db = &self.db;
        let forgetting_index = match db.meta_i64("forgetting_index", 0)? {
            0 => None,
            n @ 1..=99 => Some(n as u8),
            n => anyhow::bail!("meta.forgetting_index {n} is outside 1..=99"),
        };
        let identity = Identity {
            learner_id: db.meta_i64("sm_learner_id", 1)?,
            collection_id: db.meta_i64("sm_collection_id", 1)?,
            forgetting_index,
        };
        let requests_day = match db.meta("sync_requests_day")? {
            None => None,
            Some(s) => Some(
                NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d")
                    .with_context(|| format!("meta.sync_requests_day `{s}` is not a date"))?,
            ),
        };
        let mut outbox = Outbox::new(OutboxConfig {
            grace: Duration::from_secs(db.meta_i64("sync_grace_secs", DEFAULT_GRACE_SECS)?.max(0) as u64),
            spacing: RATE_SPACING,
            daily_cap: db.meta_i64("sync_daily_cap", DEFAULT_DAILY_CAP)?,
            requests_day,
            requests_today: db.meta_i64("sync_requests_today", 0)?,
        });
        for row in db.pending_grades()? {
            let review_date = local_date(&row.graded_at)
                .with_context(|| format!("journal row {}", row.journal_id))?;
            outbox.enqueue(
                SyncRequest {
                    journal_id: row.journal_id,
                    sm_id: row.sm_id,
                    grade: row.grade,
                    review_date,
                },
                self.now,
            );
        }
        let worker = worker::spawn(scheduler, identity)?;
        self.sync = Some(Sync { outbox, worker });
        Ok(())
    }

    pub fn queue_context(&self) -> String {
        format!("queue · sort prio{}", self.unsynced_suffix())
    }

    pub fn review_context(&self) -> String {
        let due = self.review.due.len();
        let done = self.review.done;
        let suffix = self.unsynced_suffix();
        match &self.review.current {
            Some(cur) => format!("card · prio {} · due {due} · done {done}/{due}{suffix}", cur.item.prio),
            None => format!("card · due {due} · done {done}/{due}{suffix}"),
        }
    }

    /// `read · prio 20 · ¶ 3/12 · 2 harvested` for the status row on the read screen.
    pub fn read_context(&self) -> String {
        match &self.read {
            Some(r) => format!(
                "read · prio {} · ¶ {}/{} · {} harvested",
                r.item.prio,
                if r.paragraphs.is_empty() { 0 } else { r.cursor + 1 },
                r.paragraphs.len(),
                r.children.len()
            ),
            None => "read".to_string(),
        }
    }

    /// `selecting 4 words` or `selecting ¶ 3–5` while a selection is active on the read screen.
    pub fn selection_status(&self) -> Option<String> {
        let r = self.read.as_ref()?;
        Some(match r.selection? {
            Selection::Words { anchor } => {
                let n = anchor.abs_diff(r.word) + 1;
                format!("selecting {n} {}", if n == 1 { "word" } else { "words" })
            }
            Selection::Paragraphs { anchor } => {
                let (a, b) = (anchor.min(r.cursor) + 1, anchor.max(r.cursor) + 1);
                if a == b {
                    format!("selecting ¶ {a}")
                } else {
                    format!("selecting ¶ {a}–{b}")
                }
            }
        })
    }

    /// `prio 28 › 30` while the priority prompt is open.
    pub fn prompt_text(&self) -> Option<String> {
        self.prompt.as_ref().map(|p| format!("prio {} › {}", p.current, p.value))
    }

    /// ` · 2 unsynced` while grades are waiting, in flight or rejected; empty otherwise.
    fn unsynced_suffix(&self) -> String {
        match self.sync.as_ref().map(|s| s.outbox.pending_count()) {
            Some(n) if n > 0 => format!(" · {n} unsynced"),
            _ => String::new(),
        }
    }

    /// Dispatch one key press without modifiers (tests).
    #[cfg(test)]
    pub fn handle_key(&mut self, key: KeyCode) -> Result<()> {
        self.handle_key_with(key, KeyModifiers::NONE)
    }

    /// Dispatch one key press. The prompt takes every key while open; the read screen
    /// owns `q` and `tab` so it can save the read-point first.
    pub fn handle_key_with(&mut self, key: KeyCode, mods: KeyModifiers) -> Result<()> {
        self.notice = None;
        if self.prompt.is_some() {
            return self.handle_prompt_key(key);
        }
        if self.screen == Screen::Read {
            return self.handle_read_key(key, mods);
        }
        match key {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Tab | KeyCode::BackTab => self.cycle_screen()?,
            _ => match self.screen {
                Screen::Queue => self.handle_queue_key(key)?,
                Screen::Review => self.handle_review_key(key)?,
                Screen::Read => {}
            },
        }
        Ok(())
    }

    /// Called by the event loop every ~100 ms: apply results from the worker, hand it the
    /// next row the outbox allows, and refresh the status line. A no-op offline.
    pub fn tick(&mut self, now: Instant, today: NaiveDate) -> Result<()> {
        self.now = now;
        self.today = today;
        let Some(sync) = self.sync.as_mut() else {
            return Ok(());
        };
        let mut worker_gone = false;
        loop {
            match sync.worker.try_recv() {
                Ok(Some(result)) => {
                    let landed = sync.outbox.complete(result.journal_id, result.outcome, now);
                    Self::record(&self.db, &self.root, &mut self.last_grade, &mut self.sync_log, landed)?;
                }
                Ok(None) => break,
                Err(_) => {
                    worker_gone = true;
                    break;
                }
            }
        }
        if !worker_gone {
            if let Some(req) = sync.outbox.poll(now, today) {
                let journal_id = req.journal_id;
                if sync.worker.send(req) {
                    if let (Some(day), count) = sync.outbox.counter() {
                        self.db.set_sync_counter(day, count)?;
                    }
                } else {
                    sync.outbox.abort(journal_id, now);
                    worker_gone = true;
                }
            }
        }
        if worker_gone {
            // The thread is dead: hand back whatever it was holding, persist the refund, and
            // fall back to offline mode. Rows stay synced = 0 on disk for the next session.
            if let Some(req) = sync.outbox.in_flight().cloned() {
                sync.outbox.abort(req.journal_id, now);
            }
            if let (Some(day), count) = sync.outbox.counter() {
                self.db.set_sync_counter(day, count)?;
            }
            let pending = sync.outbox.pending_count();
            self.sync_log.push(format!("sync worker exited; {pending} grades stay pending until the next start"));
            self.sync = None;
        }
        if self.last_grade.is_some() {
            self.review.status = self.sync_status();
        }
        Ok(())
    }

    /// Runs after the terminal is restored on quit. Waits briefly for a reply in flight,
    /// then sends rows that were still waiting for grace, within a small budget.
    /// Returns how many rows are left unsynced for the next session.
    pub fn finish(&mut self) -> Result<usize> {
        let Some(sync) = self.sync.as_mut() else {
            return Ok(0);
        };
        let start = Instant::now();
        let today = Local::now().date_naive();
        if sync.outbox.in_flight().is_some() {
            if let Some(result) = sync.worker.recv_timeout(QUIT_INFLIGHT_WAIT) {
                let landed = sync.outbox.complete(result.journal_id, result.outcome, Instant::now());
                Self::record(&self.db, &self.root, &mut self.last_grade, &mut self.sync_log, landed)?;
            }
        }
        sync.outbox.flush(Instant::now());
        while start.elapsed() < QUIT_DRAIN_BUDGET {
            // Poll first: it rolls the daily counter over to `today` before the cap is judged.
            let Some(req) = sync.outbox.poll(Instant::now(), today) else {
                if sync.outbox.is_drained() || sync.outbox.is_stopped() || sync.outbox.cap_reached() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(25));
                continue;
            };
            let journal_id = req.journal_id;
            if !sync.worker.send(req) {
                sync.outbox.abort(journal_id, Instant::now());
                break;
            }
            if let (Some(day), count) = sync.outbox.counter() {
                self.db.set_sync_counter(day, count)?;
            }
            let left = QUIT_DRAIN_BUDGET.saturating_sub(start.elapsed());
            let Some(result) = sync.worker.recv_timeout(left) else {
                break;
            };
            let landed = sync.outbox.complete(result.journal_id, result.outcome, Instant::now());
            Self::record(&self.db, &self.root, &mut self.last_grade, &mut self.sync_log, landed)?;
        }
        Ok(sync.outbox.pending_count())
    }

    /// Persist what the outbox decided about a reply. A synced row gets its schedule written
    /// to the card file and the index; a rejected row is logged for the exit message.
    fn record(
        db: &Db,
        root: &Path,
        last_grade: &mut Option<LastGrade>,
        log: &mut Vec<String>,
        landed: Landed,
    ) -> Result<()> {
        match landed {
            Landed::Synced { req, interval } => {
                let due = req
                    .review_date
                    .checked_add_days(chrono::Days::new(interval.max(0) as u64))
                    .with_context(|| format!("interval {interval} overflows the due date"))?;
                // The server has the review either way. If the file cannot be rewritten the
                // index still takes the schedule (mtime untouched) so the card leaves today's queue.
                let schedule = match db.item(req.sm_id)? {
                    Some(item) => {
                        let mtime = match write_schedule(root, &item.path, due, interval) {
                            Ok(mtime) => mtime,
                            Err(e) => {
                                log.push(format!("{}: could not write schedule ({e:#})", item.path));
                                item.mtime
                            }
                        };
                        Some(Schedule {
                            sm_id: req.sm_id,
                            due,
                            interval,
                            mtime,
                        })
                    }
                    None => None,
                };
                db.apply_sync(req.journal_id, interval, schedule.as_ref())?;
                if let Some(last) = last_grade.as_mut().filter(|l| l.journal_id == req.journal_id) {
                    last.synced = Some((interval, due));
                }
            }
            Landed::Rejected { reason } => log.push(format!("grade rejected by API: {reason} (kept unsynced)")),
            Landed::Retry { .. } | Landed::Stopped { .. } | Landed::Stale => {}
        }
        Ok(())
    }

    /// The status line for the newest grade, from where the outbox says it stands.
    fn sync_status(&self) -> Option<String> {
        let last = self.last_grade?;
        let g = last.grade;
        let Some(sync) = &self.sync else {
            return Some(format!("graded {g} · journaled (offline)"));
        };
        if let Some((interval, due)) = last.synced {
            return Some(format!("graded {g} · synced · interval {interval} · due {due}"));
        }
        Some(match sync.outbox.state(last.journal_id, self.now) {
            RowState::Waiting { secs } => format!("graded {g} · journaled · sync in {}s", secs.max(1)),
            RowState::CapReached => format!("graded {g} · journaled · daily cap reached, resumes tomorrow"),
            RowState::InFlight => format!("graded {g} · syncing…"),
            RowState::Retrying { reason, secs } => {
                format!("graded {g} · sync failed ({reason}) · retry in {}s", secs.max(1))
            }
            RowState::Stopped { reason } => format!("sync stopped · {reason} · check {API_KEY_ENV}"),
            RowState::Rejected { reason } => format!("graded {g} · rejected by API ({reason}) · kept unsynced"),
            RowState::Gone => format!("graded {g} · journaled"),
        })
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
                    ItemType::Article => self.open_read(item.sm_id)?,
                }
            }
            KeyCode::Char('p') => {
                if let Some(item) = self.items.get(self.queue_sel).cloned() {
                    self.open_prompt(&item);
                }
            }
            _ => {}
        }
        Ok(())
    }

    // ---- read screen (M2) ----

    /// Open an article on the read screen at its read-point.
    fn open_read(&mut self, sm_id: i64) -> Result<()> {
        let item = self
            .db
            .item(sm_id)?
            .with_context(|| format!("sm_id {sm_id} vanished from the index"))?;
        let text = std::fs::read_to_string(self.root.join(&item.path))
            .with_context(|| format!("reading {}", item.path))?;
        let doc = Document::parse(&text).with_context(|| format!("parsing {}", item.path))?;
        let body = doc.body;
        let paragraphs = article::split_paragraphs(&body);
        let cursor = article::paragraph_at(&paragraphs, item.read_pos.unwrap_or(0).max(0) as usize);
        let words = paragraphs.get(cursor).map(|p| article::words(&body, p)).unwrap_or_default();
        let children = self.children_spans(&item.path)?;
        self.read = Some(Read {
            item,
            body,
            paragraphs,
            cursor,
            word: 0,
            words,
            selection: None,
            children,
        });
        self.screen = Screen::Read;
        Ok(())
    }

    /// Harvested spans of the article at `path`, from the index.
    fn children_spans(&self, path: &str) -> Result<Vec<Span>> {
        let target = path.strip_suffix(".md").unwrap_or(path);
        Ok(self
            .db
            .children_of(target)?
            .iter()
            .filter_map(|c| c.range)
            .map(|(a, b)| Span { start: a.max(0) as usize, end: b.max(0) as usize })
            .collect())
    }

    fn handle_read_key(&mut self, key: KeyCode, mods: KeyModifiers) -> Result<()> {
        let ctrl = mods.contains(KeyModifiers::CONTROL);
        match (key, ctrl) {
            (KeyCode::Char('x'), true) => self.extract()?,
            (KeyCode::Char('z'), true) => self.cloze()?,
            (KeyCode::Char('j') | KeyCode::Down, false) => self.with_read(|r| r.move_paragraph(1)),
            (KeyCode::Char('k') | KeyCode::Up, false) => self.with_read(|r| r.move_paragraph(-1)),
            (KeyCode::Char('w'), false) => self.with_read(|r| r.move_word(1)),
            (KeyCode::Char('b'), false) => self.with_read(|r| r.move_word(-1)),
            (KeyCode::Char('v'), false) => self.with_read(|r| {
                if !r.words.is_empty() {
                    r.selection = Some(Selection::Words { anchor: r.word });
                }
            }),
            (KeyCode::Esc, false) => self.with_read(|r| r.selection = None),
            (KeyCode::Enter | KeyCode::Char(' '), false) => self.end_read_session()?,
            (KeyCode::Char('d'), false) => self.mark_done()?,
            (KeyCode::Char('p'), false) => {
                if let Some(item) = self.read.as_ref().map(|r| r.item.clone()) {
                    self.open_prompt(&item);
                }
            }
            (KeyCode::Tab | KeyCode::BackTab, false) => {
                self.save_read_pos()?;
                self.read = None;
                self.screen = Screen::Queue;
            }
            (KeyCode::Char('q'), false) => {
                self.save_read_pos()?;
                self.should_quit = true;
            }
            _ => {}
        }
        Ok(())
    }

    fn with_read(&mut self, f: impl FnOnce(&mut Read)) {
        if let Some(r) = self.read.as_mut() {
            f(r);
        }
    }

    /// Write `read_pos` to the file and index when it moved.
    fn save_read_pos(&mut self) -> Result<()> {
        let Some(r) = self.read.as_mut() else {
            return Ok(());
        };
        let Some(pos) = r.read_pos() else {
            return Ok(());
        };
        if r.item.read_pos == Some(pos) {
            return Ok(());
        }
        let mtime = write_read_pos(&self.root, &r.item.path, pos)?;
        self.db.set_read_pos(r.item.sm_id, pos, mtime)?;
        r.item.read_pos = Some(pos);
        self.reload_items()
    }

    /// `enter`/`space`: a review. Save the read-point, schedule the next reading, back to the queue.
    fn end_read_session(&mut self) -> Result<()> {
        let Some(r) = self.read.take() else {
            return Ok(());
        };
        let pos = r.read_pos().unwrap_or(0);
        let interval = next_article_interval(r.item.prio, r.item.interval, r.item.a_factor);
        let due = self
            .today
            .checked_add_days(chrono::Days::new(interval as u64))
            .with_context(|| format!("interval {interval} overflows the due date"))?;
        let mtime = write_article_session(&self.root, &r.item.path, pos, due, interval)?;
        self.db.set_article_session(r.item.sm_id, due, interval, pos, mtime)?;
        self.reload_items()?;
        self.screen = Screen::Queue;
        self.notice = Some(format!("{} · next in {interval} days", r.item.path));
        Ok(())
    }

    /// `d`: the article is finished. It leaves the queue; its children stay.
    fn mark_done(&mut self) -> Result<()> {
        let Some(r) = self.read.take() else {
            return Ok(());
        };
        let mtime = write_done(&self.root, &r.item.path, self.today)?;
        self.db.set_done(r.item.sm_id, self.today, mtime)?;
        self.reload_items()?;
        self.screen = Screen::Queue;
        self.notice = Some(format!("done · {}", r.item.path));
        Ok(())
    }

    /// Ctrl+x: the selection, or the current paragraph, becomes a child article.
    fn extract(&mut self) -> Result<()> {
        let Some(r) = self.read.as_ref() else {
            return Ok(());
        };
        let Some(span) = r.selected_span().or_else(|| r.paragraph()) else {
            return Ok(());
        };
        let body = span.text(&r.body).to_string();
        let child = create_child(&self.root, &self.db, &r.item, ItemType::Article, &body, span)?;
        self.after_harvest(span, format!("extracted → {}", child.path))
    }

    /// Ctrl+z: the selected words, or the word under the cursor, become the hidden
    /// part of a cloze card made from the current paragraph.
    fn cloze(&mut self) -> Result<()> {
        let Some(r) = self.read.as_ref() else {
            return Ok(());
        };
        if matches!(r.selection, Some(Selection::Paragraphs { .. })) {
            self.notice = Some("cloze needs words inside one paragraph".to_string());
            return Ok(());
        }
        let (Some(paragraph), Some(hidden)) = (r.paragraph(), r.selected_span().or_else(|| r.current_word())) else {
            return Ok(());
        };
        let (q, a) = article::cloze_text(&r.body, &paragraph, hidden);
        let body = format!("Q: {q}\n\nA: {a}\n");
        let child = create_child(&self.root, &self.db, &r.item, ItemType::Card, &body, hidden)?;
        self.after_harvest(hidden, format!("clozed → {}", child.path))
    }

    fn after_harvest(&mut self, span: Span, notice: String) -> Result<()> {
        if let Some(r) = self.read.as_mut() {
            r.children.push(span);
            r.selection = None;
        }
        self.reload_items()?;
        self.notice = Some(notice);
        Ok(())
    }

    /// Re-read the queue after a write, keeping the selection on the same item.
    fn reload_items(&mut self) -> Result<()> {
        let keep = self.items.get(self.queue_sel).map(|i| i.sm_id);
        self.items = self.db.queue()?;
        if let Some(pos) = keep.and_then(|id| self.items.iter().position(|i| i.sm_id == id)) {
            self.queue_sel = pos;
        }
        self.queue_sel = self.queue_sel.min(self.items.len().saturating_sub(1));
        Ok(())
    }

    // ---- priority prompt (M2) ----

    fn open_prompt(&mut self, item: &ItemRow) {
        self.prompt = Some(Prompt {
            sm_id: item.sm_id,
            path: item.path.clone(),
            current: item.prio,
            value: item.prio,
            fresh: true,
        });
    }

    fn handle_prompt_key(&mut self, key: KeyCode) -> Result<()> {
        let Some(p) = self.prompt.as_mut() else {
            return Ok(());
        };
        match key {
            KeyCode::Char(c @ '0'..='9') => {
                let d = i64::from(c as u8 - b'0');
                if p.fresh {
                    p.value = d;
                    p.fresh = false;
                } else if p.value * 10 + d <= 100 {
                    p.value = p.value * 10 + d;
                }
            }
            KeyCode::Backspace => {
                p.value /= 10;
                p.fresh = false;
            }
            KeyCode::Char('j') => {
                p.value = (p.value + PRIO_NUDGE).min(100);
                p.fresh = false;
            }
            KeyCode::Char('k') => {
                p.value = (p.value - PRIO_NUDGE).max(0);
                p.fresh = false;
            }
            KeyCode::Esc => self.prompt = None,
            KeyCode::Enter => {
                let p = p.clone();
                self.prompt = None;
                if p.value != p.current {
                    let mtime = write_prio(&self.root, &p.path, p.value)?;
                    self.db.set_prio(p.sm_id, p.value, mtime)?;
                    if let Some(r) = self.read.as_mut().filter(|r| r.item.sm_id == p.sm_id) {
                        r.item.prio = p.value;
                    }
                    self.reload_items()?;
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
            Screen::Review | Screen::Read => Screen::Queue,
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
        self.set_status(None);
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

    /// A status line that is not about the newest grade; `tick` leaves it alone.
    fn set_status(&mut self, status: Option<String>) {
        self.last_grade = None;
        self.review.status = status;
    }

    /// Journal one grade (committed before the UI moves on), queue it for sync, then advance.
    fn grade(&mut self, grade: u8) -> Result<()> {
        let Some(cur) = &self.review.current else {
            return Ok(());
        };
        let sm_id = cur.item.sm_id;
        let graded_at = Utc::now();
        let journal_id = self
            .db
            .insert_grade(sm_id, grade, &graded_at.to_rfc3339_opts(SecondsFormat::Secs, true))?;
        self.review.history.push(Graded {
            journal_id,
            grade,
            pos: self.review.pos,
        });
        self.review.done += 1;
        if let Some(sync) = self.sync.as_mut() {
            sync.outbox.enqueue(
                SyncRequest {
                    journal_id,
                    sm_id,
                    grade,
                    review_date: graded_at.with_timezone(&Local).date_naive(),
                },
                self.now,
            );
        }
        self.last_grade = Some(LastGrade {
            journal_id,
            grade,
            synced: None,
        });
        self.review.status = self.sync_status();
        self.review.pos += 1;
        self.load_current()
    }

    /// Remove the newest unsynced grade of this session and go back to that card.
    /// Refused, with the entry left on the stack, once the grade has been sent to the server.
    fn undo(&mut self) -> Result<()> {
        let Some(last) = self.review.history.last().copied() else {
            self.set_status(Some("nothing to undo".to_string()));
            return Ok(());
        };
        if let Some(sync) = self.sync.as_mut() {
            if !sync.outbox.cancel(last.journal_id) {
                self.set_status(Some("cannot undo · already sent to SuperMemo".to_string()));
                return Ok(());
            }
        }
        self.review.history.pop();
        if !self.db.delete_unsynced_grade(last.journal_id)? {
            self.set_status(Some(format!("cannot undo grade {} · already synced", last.grade)));
            return Ok(());
        }
        self.review.done = self.review.done.saturating_sub(1);
        self.review.pos = last.pos;
        self.load_current()?;
        self.review.revealed = true;
        self.set_status(Some(format!("undid grade {}", last.grade)));
        Ok(())
    }
}

/// The local calendar date of an ISO 8601 UTC `graded_at`: the `review_date` the API gets.
fn local_date(graded_at: &str) -> Result<NaiveDate> {
    let utc = DateTime::parse_from_rfc3339(graded_at)
        .with_context(|| format!("graded_at `{graded_at}` is not RFC 3339"))?;
    Ok(utc.with_timezone(&Local).date_naive())
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
    fn enter_on_article_opens_the_read_screen_and_notices_clear_on_the_next_key() {
        let (_d, mut app) = fixture_app();
        let idx = app.items.iter().position(|i| i.kind == ItemType::Article).unwrap();
        app.queue_sel = idx;
        app.handle_key(KeyCode::Enter).unwrap();
        assert_eq!(app.screen, Screen::Read);
        app.handle_key(KeyCode::Tab).unwrap();
        assert_eq!(app.screen, Screen::Queue);
        app.notice = Some("something".to_string());
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

    // ---- M1 sync ----

    use crate::sync::api::{ApiError, FakeScheduler, Reviewed, API_KEY_ENV};
    use chrono::Local;
    use std::time::{Duration, Instant};

    fn copy_fixtures() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/vault");
        for entry in std::fs::read_dir(&src).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_file() {
                std::fs::copy(entry.path(), dir.path().join(entry.file_name())).unwrap();
            }
        }
        dir
    }

    /// A synced app with `meta` overrides applied before the outbox is built.
    fn synced_app(fake: FakeScheduler, meta: &[(&str, &str)]) -> (tempfile::TempDir, App) {
        let dir = copy_fixtures();
        {
            let db = Db::open(&dir.path().join(".grain/grain.db")).unwrap();
            for (k, v) in meta {
                db.set_meta(k, v).unwrap();
            }
        }
        let app = App::open_with_scheduler(dir.path(), today(), Box::new(fake)).unwrap();
        (dir, app)
    }

    fn tick(app: &mut App, skew: Duration) {
        app.tick(Instant::now() + skew, today()).unwrap();
    }

    /// Tick with real time until `pred` holds, or fail after three seconds.
    fn tick_until(app: &mut App, skew: Duration, pred: impl Fn(&App) -> bool) {
        let start = Instant::now();
        loop {
            tick(app, skew);
            if pred(app) {
                return;
            }
            assert!(start.elapsed() < Duration::from_secs(3), "timed out; status {:?}", app.review.status);
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn status(app: &App) -> &str {
        app.review.status.as_deref().unwrap_or("")
    }

    fn journal_row(app: &App, journal_id: i64) -> (i64, Option<i64>) {
        app.db.journal_row_sync(journal_id).unwrap()
    }

    fn review_date() -> NaiveDate {
        Local::now().date_naive()
    }

    fn grade_current(app: &mut App, g: char) -> (i64, String) {
        let cur = app.review.current.as_ref().unwrap();
        let (sm_id, path) = (cur.item.sm_id, cur.item.path.clone());
        press(app, ' ');
        press(app, g);
        (sm_id, path)
    }

    #[test]
    fn offline_app_has_no_sync_and_tick_is_a_no_op() {
        let (_d, mut app) = fixture_app();
        app.handle_key(KeyCode::Tab).unwrap();
        grade_current(&mut app, '4');
        tick(&mut app, Duration::from_secs(60));
        assert_eq!(status(&app), "graded 4 · journaled (offline)");
        assert_eq!(app.queue_context(), "queue · sort prio");
        assert_eq!(app.finish().unwrap(), 0);
    }

    #[test]
    fn grace_countdown_then_undo_cancels_the_send() {
        let (_d, mut app) = synced_app(FakeScheduler::always_ok(), &[]);
        app.handle_key(KeyCode::Tab).unwrap();
        let (sm_id, _) = grade_current(&mut app, '4');
        assert_eq!(status(&app), "graded 4 · journaled · sync in 5s");
        assert!(app.review_context().ends_with("· 1 unsynced"), "{}", app.review_context());
        assert_eq!(app.queue_context(), "queue · sort prio · 1 unsynced");
        tick(&mut app, Duration::from_millis(2200));
        assert_eq!(status(&app), "graded 4 · journaled · sync in 3s");
        press(&mut app, 'u');
        assert_eq!(status(&app), "undid grade 4");
        assert_eq!(app.db.journal_count(sm_id).unwrap(), 0);
        assert_eq!(app.queue_context(), "queue · sort prio");
        tick(&mut app, Duration::from_secs(10));
        assert_eq!(status(&app), "undid grade 4", "nothing was sent");
        assert_eq!(app.db.pending_grades().unwrap().len(), 0);
    }

    #[test]
    fn grade_syncs_writes_schedule_and_the_file_stays_unchanged_afterwards() {
        let (dir, mut app) = synced_app(FakeScheduler::always_ok(), &[("sync_grace_secs", "0")]);
        app.handle_key(KeyCode::Tab).unwrap();
        let (sm_id, path) = grade_current(&mut app, '4');
        let shown = app.review.current.as_ref().unwrap().item.sm_id;
        let journal_id = app.db.pending_grades().unwrap()[0].journal_id;
        tick_until(&mut app, Duration::ZERO, |a| status(a).starts_with("graded 4 · synced"));
        let due = review_date() + chrono::Days::new(12);
        assert_eq!(status(&app), format!("graded 4 · synced · interval 12 · due {due}"));
        assert_eq!(journal_row(&app, journal_id), (1, Some(12)));
        let item = app.db.item(sm_id).unwrap().unwrap();
        assert_eq!(item.due, Some(due));
        assert_eq!(item.interval, Some(12));
        let text = std::fs::read_to_string(dir.path().join(&path)).unwrap();
        assert!(text.contains(&format!("\ndue: {due}\ninterval: 12\n")), "{text}");
        assert!(
            text.starts_with(&format!("---\ntype: card\nsm_id: {sm_id}\ndue: {due}\ninterval: 12\nprio: 12\ntags:\n- citrus\n- japanese\n---\n")),
            "keys in place, list key intact: {text}"
        );
        assert!(text.ends_with("A: yuzu 柚子 (ゆず)\n"), "body intact: {text}");
        assert_eq!(app.review.current.as_ref().unwrap().item.sm_id, shown, "screen did not jump");
        assert_eq!(app.queue_context(), "queue · sort prio");
        assert_eq!(app.db.pending_grades().unwrap().len(), 0);
        assert_eq!(app.db.meta("sync_requests_today").unwrap().as_deref(), Some("1"));
        assert_eq!(app.db.meta("sync_requests_day").unwrap(), Some(today().to_string()), "keyed by the app clock");

        let report = refresh(dir.path(), &app.db).unwrap();
        assert_eq!(report.indexed, 0, "mtime recorded, file skipped: {report:?}");
        drop(app);
        let again = App::open(dir.path(), today()).unwrap();
        assert_eq!(again.refresh.indexed, 0);
        assert!(!again.review.due.contains(&sm_id), "no longer due");
    }

    #[test]
    fn undo_is_refused_once_the_grade_is_in_flight_or_synced() {
        let (fake, release) = FakeScheduler::gated();
        let (_d, mut app) = synced_app(fake, &[("sync_grace_secs", "0")]);
        app.handle_key(KeyCode::Tab).unwrap();
        let (sm_id, _) = grade_current(&mut app, '4');
        tick(&mut app, Duration::ZERO);
        assert_eq!(status(&app), "graded 4 · syncing…");
        press(&mut app, 'u');
        assert_eq!(status(&app), "cannot undo · already sent to SuperMemo");
        assert_eq!(app.review.done, 1);
        assert_eq!(app.db.journal_count(sm_id).unwrap(), 1);
        release.send(()).unwrap();
        tick_until(&mut app, Duration::ZERO, |a| a.db.pending_grades().unwrap().is_empty());
        press(&mut app, 'u');
        assert_eq!(status(&app), "cannot undo · already sent to SuperMemo");
        assert_eq!(app.db.journal_count(sm_id).unwrap(), 1);
        assert_eq!(app.review.done, 1);
    }

    #[test]
    fn transient_failure_shows_retry_countdown_then_succeeds() {
        let fake = FakeScheduler::new(vec![
            Err(ApiError::Transient { reason: "connection refused".to_string() }),
            Ok(Reviewed { interval: 3 }),
        ]);
        let (_d, mut app) = synced_app(fake, &[("sync_grace_secs", "0")]);
        app.handle_key(KeyCode::Tab).unwrap();
        grade_current(&mut app, '4');
        tick_until(&mut app, Duration::ZERO, |a| status(a).contains("sync failed"));
        assert_eq!(status(&app), "graded 4 · sync failed (connection refused) · retry in 2s");
        assert_eq!(app.db.pending_grades().unwrap().len(), 1);
        tick_until(&mut app, Duration::from_secs(3), |a| status(a).starts_with("graded 4 · synced"));
        assert_eq!(status(&app), format!("graded 4 · synced · interval 3 · due {}", review_date() + chrono::Days::new(3)));
    }

    #[test]
    fn auth_failure_stops_syncing_and_keeps_rows_pending() {
        let fake = FakeScheduler::new(vec![Err(ApiError::Unauthorized { status: 401, message: "bad key".to_string() })]);
        let (_d, mut app) = synced_app(fake, &[("sync_grace_secs", "0")]);
        app.handle_key(KeyCode::Tab).unwrap();
        grade_current(&mut app, '4');
        tick_until(&mut app, Duration::ZERO, |a| status(a).starts_with("sync stopped"));
        assert_eq!(status(&app), format!("sync stopped · 401 unauthorized · check {API_KEY_ENV}"));
        assert_eq!(app.db.pending_grades().unwrap().len(), 1);
        grade_current(&mut app, '3');
        tick(&mut app, Duration::from_secs(10));
        assert_eq!(status(&app), format!("sync stopped · 401 unauthorized · check {API_KEY_ENV}"));
        assert_eq!(app.db.pending_grades().unwrap().len(), 2);
        assert!(app.queue_context().ends_with("· 2 unsynced"));
    }

    #[test]
    fn rejected_grade_is_kept_unsynced_and_the_next_one_still_goes() {
        let fake = FakeScheduler::new(vec![Err(ApiError::Rejected { message: "grade: must be ≤ 5".to_string() })]);
        let (_d, mut app) = synced_app(fake, &[("sync_grace_secs", "0")]);
        app.handle_key(KeyCode::Tab).unwrap();
        grade_current(&mut app, '4');
        tick_until(&mut app, Duration::ZERO, |a| status(a).contains("rejected"));
        assert_eq!(status(&app), "graded 4 · rejected by API (grade: must be ≤ 5) · kept unsynced");
        assert_eq!(app.db.pending_grades().unwrap().len(), 1);
        grade_current(&mut app, '5');
        tick_until(&mut app, Duration::from_secs(2), |a| status(a).starts_with("graded 5 · synced"));
        assert_eq!(app.db.pending_grades().unwrap().len(), 1, "the rejected row stays");
        assert!(app.review_context().ends_with("· 1 unsynced"));
    }

    #[test]
    fn daily_cap_holds_the_second_grade_until_tomorrow() {
        let (_d, mut app) = synced_app(FakeScheduler::always_ok(), &[("sync_grace_secs", "0"), ("sync_daily_cap", "1")]);
        app.handle_key(KeyCode::Tab).unwrap();
        grade_current(&mut app, '4');
        tick_until(&mut app, Duration::ZERO, |a| status(a).starts_with("graded 4 · synced"));
        grade_current(&mut app, '3');
        tick(&mut app, Duration::from_secs(2));
        assert_eq!(status(&app), "graded 3 · journaled · daily cap reached, resumes tomorrow");
        assert_eq!(app.db.meta("sync_requests_today").unwrap().as_deref(), Some("1"));
        let tomorrow = today() + chrono::Days::new(1);
        let start = Instant::now();
        loop {
            app.tick(Instant::now() + Duration::from_secs(2), tomorrow).unwrap();
            if status(&app).starts_with("graded 3 · synced") {
                break;
            }
            assert!(start.elapsed() < Duration::from_secs(3), "{:?}", app.review.status);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(app.db.meta("sync_requests_day").unwrap(), Some(tomorrow.to_string()));
        assert_eq!(app.db.meta("sync_requests_today").unwrap().as_deref(), Some("1"));
    }

    #[test]
    fn backlog_from_an_earlier_session_is_sent_oldest_first() {
        let dir = copy_fixtures();
        let first;
        {
            let mut app = App::open(dir.path(), today()).unwrap();
            app.handle_key(KeyCode::Tab).unwrap();
            first = grade_current(&mut app, '3').0;
            grade_current(&mut app, '5');
        }
        let db = Db::open(&dir.path().join(".grain/grain.db")).unwrap();
        db.set_meta("sync_grace_secs", "0").unwrap();
        drop(db);
        let fake = FakeScheduler::always_ok();
        let seen = fake.seen.clone();
        let mut app = App::open_with_scheduler(dir.path(), today(), Box::new(fake)).unwrap();
        assert_eq!(app.queue_context(), "queue · sort prio · 2 unsynced");
        let start = Instant::now();
        while !app.db.pending_grades().unwrap().is_empty() {
            app.tick(Instant::now(), today()).unwrap();
            assert!(start.elapsed() < Duration::from_secs(4));
            std::thread::sleep(Duration::from_millis(5));
        }
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(i64::from(seen[0].ext_item_id), first);
        assert_eq!(seen[0].grade, 3);
        assert_eq!(seen[1].grade, 5);
        assert!(start.elapsed() >= Duration::from_millis(1100), "spacing honoured");
        assert_eq!(app.queue_context(), "queue · sort prio");
    }

    #[test]
    fn finish_waits_for_the_row_in_flight_and_leaves_capped_rows_pending() {
        let (fake, release) = FakeScheduler::gated();
        let (_d, mut app) = synced_app(fake, &[("sync_grace_secs", "0"), ("sync_daily_cap", "1")]);
        app.handle_key(KeyCode::Tab).unwrap();
        let (first, _) = grade_current(&mut app, '4');
        let (second, _) = grade_current(&mut app, '3');
        tick(&mut app, Duration::ZERO);
        assert_eq!(app.db.pending_grades().unwrap().len(), 2);
        press(&mut app, 'q');
        release.send(()).unwrap();
        let left = app.finish().unwrap();
        assert_eq!(left, 1);
        let pending = app.db.pending_grades().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].sm_id, second);
        assert_eq!(app.db.item(first).unwrap().unwrap().interval, Some(12));
    }

    #[test]
    fn finish_sends_rows_still_waiting_for_grace() {
        let (_d, mut app) = synced_app(FakeScheduler::always_ok(), &[]);
        app.handle_key(KeyCode::Tab).unwrap();
        grade_current(&mut app, '4');
        assert_eq!(status(&app), "graded 4 · journaled · sync in 5s");
        assert_eq!(app.finish().unwrap(), 0);
        assert!(app.db.pending_grades().unwrap().is_empty());
    }

    #[test]
    fn a_card_deleted_before_its_sync_lands_is_still_marked_synced() {
        let (fake, release) = FakeScheduler::gated();
        let (dir, mut app) = synced_app(fake, &[("sync_grace_secs", "0")]);
        app.handle_key(KeyCode::Tab).unwrap();
        let (_, path) = grade_current(&mut app, '4');
        tick(&mut app, Duration::ZERO);
        std::fs::remove_file(dir.path().join(&path)).unwrap();
        release.send(()).unwrap();
        tick_until(&mut app, Duration::ZERO, |a| a.db.pending_grades().unwrap().is_empty());
        assert!(status(&app).starts_with("graded 4 · synced"), "{}", status(&app));
        assert_eq!(app.sync_log.len(), 1, "{:?}", app.sync_log);
        assert!(app.sync_log[0].contains(&path), "{:?}", app.sync_log);
        // The index still takes the schedule so the card leaves today's queue; mtime is untouched.
        let item = app.db.item(1044).unwrap().unwrap();
        assert_eq!(item.interval, Some(12));
        assert_eq!(item.due, Some(review_date() + chrono::Days::new(12)));
    }

    #[test]
    fn a_dead_worker_drops_to_offline_and_keeps_the_row_pending() {
        let (_d, mut app) = synced_app(FakeScheduler::panicking(), &[("sync_grace_secs", "0")]);
        app.handle_key(KeyCode::Tab).unwrap();
        let (sm_id, _) = grade_current(&mut app, '4');
        tick(&mut app, Duration::ZERO);
        // The worker panics on this request; wait for the app to notice the thread is gone.
        tick_until(&mut app, Duration::ZERO, |a| !a.sync_log.is_empty());
        assert!(app.sync_log[0].contains("sync worker exited"), "{:?}", app.sync_log);
        assert_eq!(status(&app), "graded 4 · journaled (offline)");
        assert_eq!(app.queue_context(), "queue · sort prio", "no outbox any more");
        assert_eq!(app.db.pending_grades().unwrap().len(), 1, "row survives for the next session");
        assert_eq!(app.db.meta("sync_requests_today").unwrap().as_deref(), Some("0"), "counter refunded");
        press(&mut app, 'u');
        assert_eq!(status(&app), "undid grade 4", "not stuck as in flight");
        assert_eq!(app.db.journal_count(sm_id).unwrap(), 0);
        assert_eq!(app.finish().unwrap(), 0);
    }

    // ---- M2 reading ----

    use crate::vault::article::Span;
    use crossterm::event::KeyModifiers;

    fn ctrl(app: &mut App, c: char) {
        app.handle_key_with(KeyCode::Char(c), KeyModifiers::CONTROL).unwrap();
    }

    fn open_article(app: &mut App, path: &str) {
        let idx = app.items.iter().position(|i| i.path == path).unwrap();
        app.queue_sel = idx;
        app.handle_key(KeyCode::Enter).unwrap();
        assert_eq!(app.screen, Screen::Read, "{path}");
    }

    fn read(app: &App) -> &Read {
        app.read.as_ref().unwrap()
    }

    fn file(dir: &tempfile::TempDir, rel: &str) -> String {
        std::fs::read_to_string(dir.path().join(rel)).unwrap()
    }

    #[test]
    fn next_article_interval_scales_first_by_prio_then_multiplies_by_a_factor() {
        assert_eq!(next_article_interval(0, None, None), 1);
        assert_eq!(next_article_interval(20, None, None), 7);
        assert_eq!(next_article_interval(50, None, None), 16);
        assert_eq!(next_article_interval(100, None, None), 30);
        assert_eq!(next_article_interval(-5, None, None), 1, "clamped");
        assert_eq!(next_article_interval(20, Some(7), None), 11, "7 × 1.5 = 10.5 → 11");
        assert_eq!(next_article_interval(20, Some(7), Some(2.0)), 14);
        assert_eq!(next_article_interval(20, Some(1), Some(1.01)), 1, "never below 1");
        assert_eq!(next_article_interval(20, Some(10), Some(99.0)), 50, "a_factor clamped to 5");
        assert_eq!(next_article_interval(20, Some(0), Some(1.5)), 1);
    }

    #[test]
    fn enter_on_article_opens_read_screen_at_read_pos_and_moves_by_paragraph_and_word() {
        let (_d, mut app) = fixture_app();
        open_article(&mut app, "citrus-vocab.md");
        assert_eq!(read(&app).paragraphs.len(), 4);
        assert_eq!(read(&app).cursor, 2, "read_pos 83 is the third paragraph");
        assert_eq!(app.read_context(), "read · prio 20 · ¶ 3/4 · 3 harvested");
        assert_eq!(read(&app).children, vec![Span { start: 83, end: 152 }, Span { start: 153, end: 205 }, Span { start: 207, end: 255 }]);
        press(&mut app, 'k');
        assert_eq!(app.read_context(), "read · prio 20 · ¶ 2/4 · 3 harvested");
        press(&mut app, 'j');
        press(&mut app, 'j');
        assert_eq!(read(&app).cursor, 3);
        press(&mut app, 'j');
        assert_eq!(read(&app).cursor, 3, "clamped at the last paragraph");
        press(&mut app, 'k');
        assert_eq!((read(&app).cursor, read(&app).word), (2, 0));
        press(&mut app, 'w');
        press(&mut app, 'w');
        assert_eq!(read(&app).word, 2);
        assert_eq!(read(&app).current_word(), Some(Span { start: 93, end: 96 }), "\"the\"");
        press(&mut app, 'b');
        assert_eq!(read(&app).word, 1);
        for _ in 0..40 {
            press(&mut app, 'w');
        }
        assert_eq!(read(&app).word, 20, "clamped at the last word");
        press(&mut app, 'j');
        assert_eq!(read(&app).word, 0, "entering a paragraph resets the word");
        app.handle_key(KeyCode::Tab).unwrap();
        assert_eq!(app.screen, Screen::Queue);
        assert!(app.read.is_none());
    }

    #[test]
    fn extract_creates_a_child_article_and_marks_the_span_without_touching_the_parent() {
        let (dir, mut app) = fixture_app();
        let before = file(&dir, "citrus-vocab.md");
        open_article(&mut app, "citrus-vocab.md");
        press(&mut app, 'k'); // paragraph 2: the note line, not yet harvested
        ctrl(&mut app, 'x');
        assert_eq!(app.notice.as_deref(), Some("extracted → citrus-vocab/1.md"));
        let text = file(&dir, "citrus-vocab/1.md");
        assert!(text.starts_with("---\ntype: article\nsm_id: "), "{text}");
        assert!(text.contains("\nprio: 20\nsource: '[[citrus-vocab]]'\nrange: 21-81\n---\nA long-form note on citrus species and their culinary names.\n"), "{text}");
        assert!(!text.contains("a_factor"), "parent has no explicit a_factor");
        assert_eq!(file(&dir, "citrus-vocab.md"), before, "parent byte-identical");
        assert!(read(&app).children.contains(&Span { start: 21, end: 81 }), "{:?}", read(&app).children);
        assert_eq!(app.read_context(), "read · prio 20 · ¶ 2/4 · 4 harvested");
        assert_eq!(app.items.len(), 9);
        assert!(read(&app).selection.is_none());

        // A paragraph selection extracts several paragraphs joined by a blank line.
        press(&mut app, 'v');
        press(&mut app, 'j');
        assert_eq!(app.selection_status().as_deref(), Some("selecting ¶ 2–3"));
        ctrl(&mut app, 'x');
        let text = file(&dir, "citrus-vocab/2.md");
        assert!(text.contains("range: 21-205\n---\nA long-form note on citrus species and their culinary names.\n\nPomelo is the largest"), "{text}");
        assert!(text.ends_with("sour flesh.\n"), "{text}");
        assert_eq!(app.items.len(), 10);
    }

    #[test]
    fn cloze_from_a_word_selection_creates_a_card_that_is_due_now_and_gradable() {
        let (dir, mut app) = fixture_app();
        open_article(&mut app, "citrus-vocab.md");
        press(&mut app, 'v');
        assert_eq!(app.selection_status().as_deref(), Some("selecting 1 word"));
        press(&mut app, 'w');
        assert_eq!(app.selection_status().as_deref(), Some("selecting 2 words"));
        press(&mut app, 'b');
        ctrl(&mut app, 'z');
        assert_eq!(app.notice.as_deref(), Some("clozed → citrus-vocab/1.md"));
        let text = file(&dir, "citrus-vocab/1.md");
        assert!(text.contains("---\ntype: card\nsm_id: "), "{text}");
        assert!(text.contains("\nprio: 20\nsource: '[[citrus-vocab]]'\nrange: 83-89\n---\nQ: [...] is the largest citrus fruit and an ancestor of the grapefruit.\nKumquat, unusually, has a sweet peel and sour flesh.\n\nA: Pomelo\n"), "{text}");
        assert!(read(&app).children.contains(&Span { start: 83, end: 89 }));
        let child = app.items.iter().find(|i| i.path == "citrus-vocab/1.md").cloned().unwrap();
        assert_eq!(child.kind, ItemType::Card);
        assert_eq!(child.due, None);

        // A word selection becomes a paragraph selection on j/k, anchored where v was pressed,
        // even when the cursor cannot move.
        press(&mut app, 'j');
        assert_eq!(read(&app).cursor, 3, "last paragraph");
        press(&mut app, 'v');
        press(&mut app, 'j');
        assert_eq!(app.selection_status().as_deref(), Some("selecting ¶ 4"), "no phantom paragraph at the boundary");
        press(&mut app, 'k');
        assert_eq!(app.selection_status().as_deref(), Some("selecting ¶ 3–4"));
        app.handle_key(KeyCode::Esc).unwrap();
        press(&mut app, 'k');

        // A paragraph selection is refused for cloze.
        press(&mut app, 'v');
        press(&mut app, 'j');
        ctrl(&mut app, 'z');
        assert_eq!(app.notice.as_deref(), Some("cloze needs words inside one paragraph"));
        assert!(!dir.path().join("citrus-vocab/2.md").exists());
        press(&mut app, 'u');
        assert!(read(&app).selection.is_some(), "unknown keys are ignored on the read screen");
        app.handle_key(KeyCode::Esc).unwrap();
        assert!(read(&app).selection.is_none());

        // The new card grades in review like any other.
        app.handle_key(KeyCode::Tab).unwrap();
        let idx = app.items.iter().position(|i| i.sm_id == child.sm_id).unwrap();
        app.queue_sel = idx;
        app.handle_key(KeyCode::Enter).unwrap();
        assert_eq!(app.screen, Screen::Review);
        assert_eq!(app.review.current.as_ref().unwrap().item.sm_id, child.sm_id);
        press(&mut app, ' ');
        press(&mut app, '4');
        assert_eq!(app.db.journal_count(child.sm_id).unwrap(), 1);
    }

    #[test]
    fn enter_and_space_end_the_session_and_reschedule_by_prio_then_a_factor() {
        let (dir, mut app) = fixture_app();
        open_article(&mut app, "citrus-vocab.md");
        press(&mut app, 'j'); // paragraph 4 at 207
        app.handle_key(KeyCode::Enter).unwrap();
        assert_eq!(app.screen, Screen::Queue);
        assert_eq!(app.notice.as_deref(), Some("citrus-vocab.md · next in 7 days"));
        let due = today() + chrono::Days::new(7);
        let text = file(&dir, "citrus-vocab.md");
        assert!(text.starts_with(&format!("---\ntype: article\nsm_id: 1001\ndue: {due}\ninterval: 7\nprio: 20\nread_pos: 207\ntags:\n- citrus\n---\n")), "{text}");
        let row = app.db.item(1001).unwrap().unwrap();
        assert_eq!((row.due, row.interval, row.read_pos), (Some(due), Some(7), Some(207)));
        assert_eq!(app.items.iter().find(|i| i.sm_id == 1001).unwrap().due, Some(due), "queue reloaded");

        open_article(&mut app, "citrus-vocab.md");
        assert_eq!(read(&app).cursor, 3, "resumes at the saved read-point");
        press(&mut app, ' ');
        assert_eq!(app.notice.as_deref(), Some("citrus-vocab.md · next in 11 days"));
        let due2 = today() + chrono::Days::new(11);
        assert!(file(&dir, "citrus-vocab.md").contains(&format!("\ndue: {due2}\ninterval: 11\n")));

        drop(app);
        let again = App::open(dir.path(), today()).unwrap();
        assert_eq!(again.refresh.indexed, 0, "every write recorded its mtime: {:?}", again.refresh);
    }

    #[test]
    fn tab_and_q_save_only_the_read_point_and_d_marks_done() {
        let (dir, mut app) = fixture_app();
        let before = file(&dir, "citrus-vocab.md");
        open_article(&mut app, "citrus-vocab.md");
        press(&mut app, 'j');
        app.handle_key(KeyCode::Tab).unwrap();
        assert_eq!(app.screen, Screen::Queue);
        let text = file(&dir, "citrus-vocab.md");
        assert!(text.contains("\nread_pos: 207\n") && !text.contains("due:"), "{text}");
        assert_eq!(app.db.item(1001).unwrap().unwrap().read_pos, Some(207));

        open_article(&mut app, "citrus-vocab.md");
        let mtime = app.db.item(1001).unwrap().unwrap().mtime;
        press(&mut app, 'q');
        assert!(app.should_quit);
        assert_eq!(app.db.item(1001).unwrap().unwrap().mtime, mtime, "unchanged read-point, no write");
        app.should_quit = false;
        app.handle_key(KeyCode::Tab).unwrap(); // the real app would have exited; go back to the queue

        open_article(&mut app, "citrus-vocab.md");
        press(&mut app, 'd');
        assert_eq!(app.screen, Screen::Queue);
        assert_eq!(app.notice.as_deref(), Some("done · citrus-vocab.md"));
        assert!(file(&dir, "citrus-vocab.md").contains(&format!("\ndone: {}\n", today())));
        assert!(!app.items.iter().any(|i| i.sm_id == 1001), "left the queue");
        assert!(app.items.iter().any(|i| i.path == "pomelo.md"), "children stay");
        assert_eq!(app.items.len(), 7);
        assert_ne!(file(&dir, "citrus-vocab.md"), before);
        drop(app);
        let again = App::open(dir.path(), today()).unwrap();
        assert_eq!(again.refresh.indexed, 0);
        assert_eq!(again.items.len(), 7);
    }

    #[test]
    fn priority_prompt_takes_digits_or_nudges_on_queue_and_read_screens() {
        let (dir, mut app) = fixture_app();
        let idx = app.items.iter().position(|i| i.path == "kumquat.md").unwrap();
        app.queue_sel = idx;
        press(&mut app, 'p');
        assert_eq!(app.prompt_text().as_deref(), Some("prio 35 › 35"));
        press(&mut app, '3');
        assert_eq!(app.prompt_text().as_deref(), Some("prio 35 › 3"));
        press(&mut app, '0');
        assert_eq!(app.prompt_text().as_deref(), Some("prio 35 › 30"));
        press(&mut app, '9');
        assert_eq!(app.prompt_text().as_deref(), Some("prio 35 › 30"), "extra digits ignored");
        press(&mut app, 'q');
        assert!(!app.should_quit, "other keys are ignored while the prompt is open");
        app.handle_key(KeyCode::Enter).unwrap();
        assert!(app.prompt_text().is_none());
        assert!(file(&dir, "kumquat.md").contains("\nprio: 30\n"));
        let kum = app.items.iter().find(|i| i.path == "kumquat.md").unwrap();
        assert_eq!(kum.prio, 30);
        assert_eq!(app.items[app.queue_sel].path, "kumquat.md", "selection follows the item");
        let prios: Vec<i64> = app.items.iter().map(|i| i.prio).collect();
        assert!(prios.windows(2).all(|w| w[0] <= w[1]), "re-sorted: {prios:?}");

        let idx = app.items.iter().position(|i| i.path == "pomelo.md").unwrap();
        app.queue_sel = idx;
        press(&mut app, 'p');
        press(&mut app, 'k');
        press(&mut app, 'k');
        assert_eq!(app.prompt_text().as_deref(), Some("prio 28 › 18"));
        app.handle_key(KeyCode::Enter).unwrap();
        assert!(file(&dir, "pomelo.md").contains("\nprio: 18\n"));

        press(&mut app, 'p');
        press(&mut app, 'j');
        app.handle_key(KeyCode::Esc).unwrap();
        assert!(file(&dir, "pomelo.md").contains("\nprio: 18\n"), "esc changes nothing");

        open_article(&mut app, "citrus-vocab.md");
        press(&mut app, 'p');
        assert_eq!(app.prompt_text().as_deref(), Some("prio 20 › 20"));
        press(&mut app, '9');
        press(&mut app, '9');
        press(&mut app, 'j');
        assert_eq!(app.prompt_text().as_deref(), Some("prio 20 › 100"), "nudge clamps at 100");
        app.handle_key(KeyCode::Backspace).unwrap();
        assert_eq!(app.prompt_text().as_deref(), Some("prio 20 › 10"));
        app.handle_key(KeyCode::Enter).unwrap();
        assert_eq!(app.screen, Screen::Read);
        assert_eq!(app.read_context(), "read · prio 10 · ¶ 3/4 · 3 harvested");
        assert!(file(&dir, "citrus-vocab.md").contains("\nprio: 10\n"));
        drop(app);
        assert_eq!(App::open(dir.path(), today()).unwrap().refresh.indexed, 0);
    }

    #[test]
    fn a_version_1_database_migrates_and_matches_a_rebuild() {
        let (dir, app) = fixture_app();
        let expected = app.items.clone();
        drop(app);
        // Downgrade the sidecar to what M1 left behind.
        let conn = rusqlite::Connection::open(dir.path().join(".grain/grain.db")).unwrap();
        conn.execute_batch(
            "DROP INDEX IF EXISTS idx_source;
             ALTER TABLE items DROP COLUMN a_factor;
             ALTER TABLE items DROP COLUMN done;
             ALTER TABLE items DROP COLUMN source;
             ALTER TABLE items DROP COLUMN range_start;
             ALTER TABLE items DROP COLUMN range_end;
             UPDATE meta SET value = '1' WHERE key = 'schema_version';",
        )
        .unwrap();
        drop(conn);
        let app = App::open(dir.path(), today()).unwrap();
        assert_eq!(app.db.meta("schema_version").unwrap().as_deref(), Some("2"));
        assert_eq!(app.refresh.indexed, 8, "one full re-index after the migration");
        assert_eq!(app.refresh.allocated, 0);
        assert_eq!(app.items, expected);
        assert_eq!(app.db.children_of("citrus-vocab").unwrap().len(), 3);
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
