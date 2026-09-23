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
    create_child, create_item, load_card, refresh, write_article_session, write_done, write_prio,
    write_read_pos, write_schedule, LoadedCard, NewItem, RefreshReport,
};
use crate::import::{fetch_title_body, html_to_markdown, Fetcher, UreqFetcher};

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

/// What the inline text prompt is collecting (M5).
#[derive(Debug, Clone)]
enum TextKind {
    /// `a`, first step: the question of a new card.
    CardQuestion,
    /// `a`, second step: its answer, holding the question already typed.
    CardAnswer { question: String },
    /// `i`: the url or path of an article to import.
    Import,
}

/// The inline free-text prompt in the status row (M5): `a` and `i`.
#[derive(Debug, Clone)]
struct TextPrompt {
    kind: TextKind,
    buf: String,
}

/// Which step of a text prompt is open, for the hints row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextStage {
    /// `enter` moves on to another prompt.
    Question,
    /// `enter` commits.
    Final,
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
    sm_id: i64,
    grade: u8,
    /// Position in `Review::due` to return to on undo.
    pos: usize,
}

/// Where the session stands: the main pass, the `y/n` prompt at its end, or the drill.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Phase {
    #[default]
    Main,
    DrillPrompt,
    Drilling,
}

/// Learn session over the cards and articles due today.
#[derive(Debug, Default)]
pub struct Review {
    /// Due sm_ids of both types in queue order, fixed at startup (plus any item opened early).
    pub due: Vec<i64>,
    /// Index into `due`; equal to `due.len()` when the session is finished.
    pub pos: usize,
    pub done: usize,
    /// Articles rescheduled or marked done this session.
    pub read_done: usize,
    /// Cards failed in the main pass, in memory only, front first. Never journaled.
    pub drill: Vec<i64>,
    pub phase: Phase,
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
    text_prompt: Option<TextPrompt>,
    sync: Option<Sync>,
    last_grade: Option<LastGrade>,
    /// The clock as of the last `tick`; `grade` uses it for the grace deadline.
    now: Instant,
    /// The local date as of open or the last `tick`.
    today: NaiveDate,
    /// How `i` fetches a URL. Replaced in tests.
    fetcher: Box<dyn Fetcher>,
    /// A URL `i` accepted and has not fetched yet.
    pending_import: Option<PendingImport>,
}

/// A URL waiting for the frame that says `fetching …`.
///
/// The event loop runs `draw`, then the key, then `tick`, so the `tick` that follows
/// `enter` happens before the next `draw`: fetching there would block on the frame that
/// still shows the open prompt and `fetching …` would never reach the screen. Arming on
/// that first `tick` and fetching on the second puts a `draw` in between.
struct PendingImport {
    url: String,
    /// False until the first `tick` after `enter`.
    armed: bool,
}

impl App {
    /// Open the vault, refresh the index and build the queue and learn session, landing
    /// on the first due item. Offline.
    pub fn open(root: &Path, today: NaiveDate) -> Result<Self> {
        let db = Db::open(&root.join(".grain").join("grain.db"))?;
        let refresh = refresh(root, &db).context("refreshing index")?;
        let items = db.queue()?;
        let due = db.due_items(today)?.into_iter().map(|i| i.sm_id).collect();
        let mut app = App {
            root: root.to_path_buf(),
            db,
            items,
            screen: Screen::Review,
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
            text_prompt: None,
            sync: None,
            last_grade: None,
            now: Instant::now(),
            today,
            fetcher: Box::new(UreqFetcher::new()),
            pending_import: None,
        };
        app.load_current()?;
        Ok(app)
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

    /// `card · prio 12 · 1/6`: where the session stands, `drill · prio 12 · 2 left`
    /// during the final drill, or `card · 6/6` once the main pass is over.
    pub fn review_context(&self) -> String {
        let len = self.review.due.len();
        let suffix = self.unsynced_suffix();
        match &self.review.current {
            Some(cur) if self.review.phase == Phase::Drilling => {
                format!("drill · prio {} · {} left{suffix}", cur.item.prio, self.review.drill.len())
            }
            Some(cur) => format!("card · prio {} · {}/{len}{suffix}", cur.item.prio, self.review.pos + 1),
            None => format!("card · {len}/{len}{suffix}"),
        }
    }

    /// `final drill · 2 cards`: the offer made when the main pass ends with failures.
    /// The `y`/`n` keys are drawn inside the note box, not in this string.
    pub fn drill_prompt(&self) -> String {
        let n = self.review.drill.len();
        format!("final drill · {n} card{}", if n == 1 { "" } else { "s" })
    }

    /// `nothing more to learn · 4 graded · 2 read`: the session is over. Drill grades
    /// are not counted; undone grades are already off `done`.
    pub fn finish_line(&self) -> String {
        format!(
            "nothing more to learn · {} graded · {} read",
            self.review.done, self.review.read_done
        )
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

    /// `(reached, len)` for the status-row progress bar during the main pass; `None` elsewhere.
    pub fn progress(&self) -> Option<(usize, usize)> {
        let len = self.review.due.len();
        if len == 0 || self.review.phase != Phase::Main || self.screen == Screen::Queue {
            return None;
        }
        Some(((self.review.pos + 1).min(len), len))
    }

    /// The local date this session works against, fixed at open and moved on by `tick`.
    pub fn today(&self) -> NaiveDate {
        self.today
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

    /// The open prompt as one line: `prio 28 › 30` for the priority prompt, else the
    /// text prompt's label followed by what has been typed. The caret is the renderer's.
    pub fn prompt_text(&self) -> Option<String> {
        if let Some(p) = self.prompt.as_ref() {
            return Some(format!("prio {} › {}", p.current, p.value));
        }
        let t = self.text_prompt.as_ref()?;
        let label = match t.kind {
            TextKind::CardQuestion => "add card · Q: ",
            TextKind::CardAnswer { .. } => "add card · A: ",
            TextKind::Import => "import · url or path: ",
        };
        Some(format!("{label}{}", t.buf))
    }

    /// Which step of the text prompt is open, for the hints row. `None` when none is.
    pub fn text_prompt_stage(&self) -> Option<TextStage> {
        Some(match self.text_prompt.as_ref()?.kind {
            TextKind::CardQuestion => TextStage::Question,
            TextKind::CardAnswer { .. } | TextKind::Import => TextStage::Final,
        })
    }

    /// Swap the fetcher `i` pulls a URL through (tests).
    #[cfg(test)]
    pub fn set_fetcher(&mut self, f: Box<dyn Fetcher>) {
        self.fetcher = f;
    }

    /// A bracketed paste: appended to the open text prompt as one line. Ignored otherwise.
    pub fn paste(&mut self, text: &str) {
        if let Some(t) = self.text_prompt.as_mut() {
            t.buf.extend(text.chars().filter(|c| *c != '\n' && *c != '\r'));
        }
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
        if self.text_prompt.is_some() {
            return self.handle_text_key(key, mods);
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
        match self.pending_import.take() {
            None => {}
            // First tick after `enter`: only arm it, so the `fetching …` frame is drawn.
            Some(p) if !p.armed => {
                self.pending_import = Some(PendingImport { url: p.url, armed: true });
            }
            Some(p) => self.run_import(&p.url)?,
        }
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
                let Some(sm_id) = self.items.get(self.queue_sel).map(|i| i.sm_id) else {
                    return Ok(());
                };
                self.jump_to(sm_id)?;
            }
            KeyCode::Char('p') => {
                if let Some(item) = self.items.get(self.queue_sel).cloned() {
                    self.open_prompt(&item);
                }
            }
            KeyCode::Char('a') => self.open_text_prompt(TextKind::CardQuestion),
            KeyCode::Char('i') => self.open_text_prompt(TextKind::Import),
            _ => {}
        }
        Ok(())
    }

    // ---- text prompt (M5) ----

    fn open_text_prompt(&mut self, kind: TextKind) {
        self.text_prompt = Some(TextPrompt {
            kind,
            buf: String::new(),
        });
    }

    /// Every key while a text prompt is open: printable characters type, `backspace`
    /// deletes, `esc` abandons, `enter` submits. Ctrl chords are ignored.
    fn handle_text_key(&mut self, key: KeyCode, mods: KeyModifiers) -> Result<()> {
        let Some(t) = self.text_prompt.as_mut() else {
            return Ok(());
        };
        match key {
            KeyCode::Char(_) if mods.contains(KeyModifiers::CONTROL) => {}
            KeyCode::Char(c) => t.buf.push(c),
            KeyCode::Backspace => {
                t.buf.pop();
            }
            KeyCode::Esc => self.text_prompt = None,
            KeyCode::Enter => return self.submit_text(),
            _ => {}
        }
        Ok(())
    }

    /// `enter`: an empty buffer is ignored, the question step moves on to the answer,
    /// and the last step writes. The prompt closes only once the write has returned.
    fn submit_text(&mut self) -> Result<()> {
        let Some(t) = self.text_prompt.as_ref() else {
            return Ok(());
        };
        let text = t.buf.trim().to_string();
        if text.is_empty() {
            return Ok(());
        }
        match t.kind.clone() {
            TextKind::CardQuestion => {
                self.text_prompt = Some(TextPrompt {
                    kind: TextKind::CardAnswer { question: text },
                    buf: String::new(),
                });
            }
            TextKind::CardAnswer { question } => {
                self.add_card(&question, &text)?;
                self.text_prompt = None;
            }
            TextKind::Import => {
                self.text_prompt = None;
                self.start_import(&text)?;
            }
        }
        Ok(())
    }

    // ---- `i` import (M5) ----

    /// `i` submitted. An `http(s)` URL is checked against the index and then queued for
    /// the second `tick` from here, so a frame showing `fetching …` is drawn before the
    /// fetch blocks the thread. Anything else is a path and is read right here.
    fn start_import(&mut self, input: &str) -> Result<()> {
        if !is_url(input) {
            return self.import_path(input);
        }
        if let Some(row) = self.db.item_by_url(input)? {
            self.queue_sel = self.items.iter().position(|i| i.sm_id == row.sm_id).unwrap_or(0);
            self.notice = Some(format!("already imported · {}", row.path));
            return Ok(());
        }
        self.pending_import = Some(PendingImport { url: input.to_string(), armed: false });
        self.notice = Some("fetching …".to_string());
        Ok(())
    }

    /// The deferred half of a URL import: fetch, strip and write. A fetch failure is a
    /// notice and nothing is written; only a failure to write the file propagates.
    fn run_import(&mut self, url: &str) -> Result<()> {
        let fetched = fetch_title_body(self.fetcher.as_ref(), url);
        let (title, body) = match fetched {
            Ok(pair) => pair,
            Err(e) => {
                self.notice = Some(format!("import failed · {}", e.root_cause()));
                return Ok(());
            }
        };
        let title = non_empty(title).unwrap_or_else(|| url_host(url).to_string());
        self.write_import(&title, &body, Some(url))
    }

    /// `i` with a path: read the file, strip it when it is HTML and take it verbatim
    /// otherwise. A read failure is a notice; nothing is written.
    fn import_path(&mut self, input: &str) -> Result<()> {
        let path = Path::new(input);
        let read = std::fs::read_to_string(path).with_context(|| format!("reading {input}"));
        let text = match read {
            Ok(t) => t,
            Err(e) => {
                self.notice = Some(format!("import failed · {}", e.root_cause()));
                return Ok(());
            }
        };
        let stem = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let is_html = path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("html") || e.eq_ignore_ascii_case("htm"));
        let (title, body) = if is_html {
            let (title, body) = html_to_markdown(&text);
            (non_empty(title).unwrap_or(stem), body)
        } else {
            (first_heading(&text).unwrap_or(stem), text)
        };
        self.write_import(&title, &body, None)
    }

    /// Write one imported article, re-read the queue and select it. The body keeps its
    /// own leading `# ` heading; without one the title becomes the heading.
    fn write_import(&mut self, title: &str, body: &str, url: Option<&str>) -> Result<()> {
        let body = if body.starts_with("# ") {
            body.to_string()
        } else {
            format!("# {title}\n\n{body}")
        };
        let row = create_item(
            &self.root,
            &self.db,
            NewItem {
                kind: ItemType::Article,
                slug_base: title,
                body: &body,
                url,
                imported: Some(self.today),
            },
        )?;
        self.reload_items()?;
        self.queue_sel = self.items.iter().position(|i| i.sm_id == row.sm_id).unwrap_or(0);
        self.notice = Some(format!("imported {}", row.path));
        Ok(())
    }

    /// Write a new card file, re-read the queue and select it. The learn session is
    /// left alone: a card added today joins the queue, not the pass already running.
    fn add_card(&mut self, question: &str, answer: &str) -> Result<()> {
        let body = format!("Q: {question}\n\nA: {answer}\n");
        let row = create_item(
            &self.root,
            &self.db,
            NewItem {
                kind: ItemType::Card,
                slug_base: question,
                body: &body,
                url: None,
                imported: None,
            },
        )?;
        self.reload_items()?;
        self.queue_sel = self.items.iter().position(|i| i.sm_id == row.sm_id).unwrap_or(0);
        self.notice = Some(format!("added {}", row.path));
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
            (KeyCode::Char('w' | 'l') | KeyCode::Right, false) => self.with_read(|r| r.move_word(1)),
            (KeyCode::Char('b' | 'h') | KeyCode::Left, false) => self.with_read(|r| r.move_word(-1)),
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
            (KeyCode::Char('u'), false) => {
                // Undo reaches back to the card graded before this article. On success
                // `undo` loads that card and leaves the read screen; on refusal it only
                // sets a status line, which this screen does not render, so it is shown
                // as a notice and the article stays up.
                self.save_read_pos()?;
                self.undo()?;
                if self.screen == Screen::Review {
                    self.read = None;
                } else {
                    self.notice = self.review.status.clone();
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

    /// `enter`/`space`: a review. Save the read-point, schedule the next reading, next item.
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
        self.notice = Some(format!("{} · next in {interval} days", r.item.path));
        self.advance_read()
    }

    /// `d`: the article is finished. It leaves the queue; its children stay.
    fn mark_done(&mut self) -> Result<()> {
        let Some(r) = self.read.take() else {
            return Ok(());
        };
        let mtime = write_done(&self.root, &r.item.path, self.today)?;
        self.db.set_done(r.item.sm_id, self.today, mtime)?;
        self.reload_items()?;
        self.notice = Some(format!("done · {}", r.item.path));
        self.advance_read()
    }

    /// The article is dealt with: count it and move the session on to the next item.
    /// The status line goes with it; a grade two items back is not this card's news.
    fn advance_read(&mut self) -> Result<()> {
        self.set_status(None);
        self.review.read_done += 1;
        self.review.pos += 1;
        self.load_current()
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

    /// The phase decides what a key means: the prompt swallows grades, and a drill
    /// grade never reaches the journaling `grade`.
    fn handle_review_key(&mut self, key: KeyCode) -> Result<()> {
        match self.review.phase {
            Phase::DrillPrompt => return self.handle_drill_prompt_key(key),
            Phase::Drilling => return self.handle_drill_key(key),
            Phase::Main => {}
        }
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

    /// `y` starts the final drill, `n` drops it. Every other key is ignored;
    /// `tab` and `q` are handled before the screen sees the key.
    fn handle_drill_prompt_key(&mut self, key: KeyCode) -> Result<()> {
        match key {
            KeyCode::Char('y') => {
                // The main pass is over: its last grade's status (and sync countdown)
                // must not sit under a drill card.
                self.set_status(None);
                self.review.phase = Phase::Drilling;
                self.load_drill_card()?;
            }
            KeyCode::Char('n') => {
                self.review.drill.clear();
                self.review.phase = Phase::Main;
                self.set_status(None);
            }
            _ => {}
        }
        Ok(())
    }

    /// Reveal and grade in the drill. There is no undo: the grades were never written.
    fn handle_drill_key(&mut self, key: KeyCode) -> Result<()> {
        match key {
            KeyCode::Char(' ') => {
                if self.review.current.is_some() {
                    self.review.revealed = true;
                }
            }
            KeyCode::Char(c @ '0'..='5') => {
                if self.review.revealed {
                    self.drill_grade(c as u8 - b'0')?;
                }
            }
            KeyCode::Char('u') => self.set_status(Some("no undo in drill".to_string())),
            _ => {}
        }
        Ok(())
    }

    /// A drill grade: 4 or 5 clears the card, 0–3 sends it to the back of the list.
    /// Held in memory only — no journal row, no outbox entry, no file write, no `last_grade`.
    fn drill_grade(&mut self, grade: u8) -> Result<()> {
        if self.review.drill.is_empty() {
            return Ok(());
        }
        let sm_id = self.review.drill.remove(0);
        if grade >= 4 {
            self.set_status(Some(format!("drill {grade} · cleared")));
        } else {
            self.review.drill.push(sm_id);
            self.set_status(Some(format!("drill {grade} · stays")));
        }
        self.load_drill_card()
    }

    /// `tab`: the table from the session, the session from the table. Coming back from the
    /// table reopens whatever the session is on, which may be either screen.
    fn cycle_screen(&mut self) -> Result<()> {
        match self.screen {
            Screen::Queue => self.load_current()?,
            Screen::Review | Screen::Read => self.screen = Screen::Queue,
        }
        Ok(())
    }

    /// Jump the session to `sm_id`, inserting it at the current position if it is not due yet.
    /// A jump always shows the chosen item, so it leaves the drill prompt and the drill
    /// itself; the drill list survives and the prompt returns when the pass runs out again.
    fn jump_to(&mut self, sm_id: i64) -> Result<()> {
        self.review.phase = Phase::Main;
        match self.review.due.iter().position(|&id| id == sm_id) {
            Some(idx) => self.review.pos = idx,
            None => {
                let at = self.review.pos.min(self.review.due.len());
                self.review.due.insert(at, sm_id);
                self.review.pos = at;
            }
        }
        self.set_status(None);
        self.load_current()
    }

    /// Show the item at `review.pos`: a card in review, an article on the read screen.
    /// Past the end of the main pass there is nothing to show, the screen stays review
    /// and a non-empty drill list turns into the prompt. While drilling the list, not
    /// `review.pos`, says what to show, so `tab` back from the table resumes the drill.
    fn load_current(&mut self) -> Result<()> {
        if self.review.phase == Phase::Drilling {
            return self.load_drill_card();
        }
        self.review.revealed = false;
        let Some(&sm_id) = self.review.due.get(self.review.pos) else {
            self.review.current = None;
            self.screen = Screen::Review;
            if self.review.phase == Phase::Main && !self.review.drill.is_empty() {
                self.review.phase = Phase::DrillPrompt;
            }
            return Ok(());
        };
        let item = self
            .db
            .item(sm_id)?
            .with_context(|| format!("sm_id {sm_id} vanished from the index"))?;
        match item.kind {
            ItemType::Card => {
                let card = load_card(&self.root, &item.path)?;
                self.review.current = Some(CurrentCard { item, card });
                self.screen = Screen::Review;
            }
            ItemType::Article => {
                self.review.current = None;
                self.open_read(sm_id)?;
            }
        }
        Ok(())
    }

    /// Show the card at the front of the drill list; an empty list ends the session.
    fn load_drill_card(&mut self) -> Result<()> {
        self.review.revealed = false;
        let Some(&sm_id) = self.review.drill.first() else {
            self.review.phase = Phase::Main;
            self.review.current = None;
            self.screen = Screen::Review;
            return Ok(());
        };
        let item = self
            .db
            .item(sm_id)?
            .with_context(|| format!("sm_id {sm_id} vanished from the index"))?;
        let card = load_card(&self.root, &item.path)?;
        self.review.current = Some(CurrentCard { item, card });
        self.screen = Screen::Review;
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
            sm_id,
            grade,
            pos: self.review.pos,
        });
        // Anything below Good joins the final drill, once, in the order first failed.
        if grade <= 3 && !self.review.drill.contains(&sm_id) {
            self.review.drill.push(sm_id);
        }
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
        // The card leaves the drill list with the grade that put it there, unless another
        // failing grade of this session still stands for it (`history` has already popped).
        if !self.review.history.iter().any(|g| g.sm_id == last.sm_id && g.grade <= 3) {
            self.review.drill.retain(|&id| id != last.sm_id);
        }
        // By identity, since an item inserted from the table may have shifted the session
        // since this grade; `pos` is the fallback for an item no longer in the list.
        self.review.pos = self.review.due.iter().position(|&id| id == last.sm_id).unwrap_or(last.pos);
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

/// Whether `i` should fetch `input` rather than read it off disk.
fn is_url(input: &str) -> bool {
    input.starts_with("http://") || input.starts_with("https://")
}

/// The host of `url`: between `://` and the next `/`, or the whole remainder.
/// The title of last resort for a page with no usable `<title>`.
fn url_host(url: &str) -> &str {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    rest.split('/').next().unwrap_or(rest)
}

/// `Some` only when the title has visible text, trimmed.
fn non_empty(title: Option<String>) -> Option<String> {
    title.map(|t| t.trim().to_string()).filter(|t| !t.is_empty())
}

/// The text of a leading `# ` heading, when `text` starts with one.
fn first_heading(text: &str) -> Option<String> {
    let rest = text.strip_prefix("# ")?;
    let line = rest.split('\n').next().unwrap_or(rest);
    non_empty(Some(line.to_string()))
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
    fn the_queue_table_holds_all_eight_items_sorted_by_prio() {
        let (_d, app) = fixture_app();
        assert_eq!(app.screen, Screen::Review, "the session, not the table");
        assert_eq!(app.items.len(), 8);
        let prios: Vec<i64> = app.items.iter().map(|i| i.prio).collect();
        assert!(prios.windows(2).all(|w| w[0] <= w[1]));
        assert_eq!(app.items[0].title.as_deref(), Some("Japanese citrus, fragrant, used in ponzu?"));
        assert_eq!(app.queue_context(), "queue · sort prio");
    }

    #[test]
    fn j_and_k_move_selection_within_bounds() {
        let (_d, mut app) = fixture_app();
        app.handle_key(KeyCode::Tab).unwrap();
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
        app.handle_key(KeyCode::Tab).unwrap();
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
        open_from_table(&mut app, "kumquat.md");
        assert_eq!(app.screen, Screen::Review);
        let cur = app.review.current.as_ref().unwrap();
        assert_eq!(cur.item.path, "kumquat.md");
        assert!(!app.review.revealed);
        assert_eq!(app.review_context(), "card · prio 35 · 3/6");
    }

    #[test]
    fn enter_on_not_yet_due_card_reviews_it_anyway() {
        let (_d, mut app) = fixture_app();
        open_from_table(&mut app, "pomelo.md");
        assert_eq!(app.review.current.as_ref().unwrap().item.sm_id, 1042);
        assert_eq!(app.review.due.len(), 7);
    }

    #[test]
    fn grade_requires_reveal_then_journals_and_advances() {
        let (_d, mut app) = fixture_app();
        // Start at kumquat: finger-lime follows it, so the next item is another card.
        open_from_table(&mut app, "kumquat.md");
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
        // Two not-yet-due cards inserted at the front, so three cards run back to back.
        open_from_table(&mut app, "pomelo.md");
        open_from_table(&mut app, "buddhas-hand.md");
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
    fn grading_past_the_end_is_a_no_op_and_undo_brings_the_last_card_back() {
        let (_d, mut app) = fixture_app();
        // Grade 4 throughout: this is about the end of the main pass, and a failing
        // grade would end it on the drill prompt instead, where `u` does not apply.
        for _ in 0..6 {
            if app.screen == Screen::Read {
                app.handle_key(KeyCode::Enter).unwrap();
            } else {
                press(&mut app, ' ');
                press(&mut app, '4');
            }
        }
        assert!(app.review.current.is_none());
        assert_eq!(app.review.phase, Phase::Main);
        press(&mut app, ' ');
        press(&mut app, '4');
        assert_eq!(app.review.done, 4, "grading with no card is a no-op");
        press(&mut app, 'u');
        assert_eq!(app.review.current.as_ref().unwrap().item.path, "bergamot.md");
        assert_eq!(app.review.done, 3);
    }

    #[test]
    fn tab_cycles_screens_and_q_quits_and_unknown_keys_are_ignored() {
        let (_d, mut app) = fixture_app();
        app.handle_key(KeyCode::Tab).unwrap();
        assert_eq!(app.screen, Screen::Queue);
        app.handle_key(KeyCode::Tab).unwrap();
        assert_eq!(app.screen, Screen::Review);
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

    /// Grade the card the session is on, ending the reading of any article it stops at
    /// on the way there: these tests are about cards and sync only.
    fn grade_current(app: &mut App, g: char) -> (i64, String) {
        while app.screen == Screen::Read {
            app.handle_key(KeyCode::Enter).unwrap();
        }
        let cur = app.review.current.as_ref().unwrap();
        let (sm_id, path) = (cur.item.sm_id, cur.item.path.clone());
        press(app, ' ');
        press(app, g);
        (sm_id, path)
    }

    #[test]
    fn offline_app_has_no_sync_and_tick_is_a_no_op() {
        let (_d, mut app) = fixture_app();
        grade_current(&mut app, '4');
        tick(&mut app, Duration::from_secs(60));
        assert_eq!(status(&app), "graded 4 · journaled (offline)");
        assert_eq!(app.queue_context(), "queue · sort prio");
        assert_eq!(app.finish().unwrap(), 0);
    }

    #[test]
    fn grace_countdown_then_undo_cancels_the_send() {
        let (_d, mut app) = synced_app(FakeScheduler::always_ok(), &[]);
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
        let (sm_id, path) = grade_current(&mut app, '4');
        // The session moved on to the article that follows yuzu; the sync must not move it.
        let shown = app.read.as_ref().unwrap().item.sm_id;
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
        assert_eq!(app.read.as_ref().unwrap().item.sm_id, shown, "screen did not jump");
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
        grade_current(&mut app, '4');
        assert_eq!(status(&app), "graded 4 · journaled · sync in 5s");
        assert_eq!(app.finish().unwrap(), 0);
        assert!(app.db.pending_grades().unwrap().is_empty());
    }

    #[test]
    fn a_card_deleted_before_its_sync_lands_is_still_marked_synced() {
        let (fake, release) = FakeScheduler::gated();
        let (dir, mut app) = synced_app(fake, &[("sync_grace_secs", "0")]);
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

    /// Show the table, whatever screen we are on, and open `path` from it.
    fn open_from_table(app: &mut App, path: &str) {
        if app.screen != Screen::Queue {
            app.handle_key(KeyCode::Tab).unwrap();
        }
        let idx = app.items.iter().position(|i| i.path == path).unwrap();
        app.queue_sel = idx;
        app.handle_key(KeyCode::Enter).unwrap();
    }

    fn open_article(app: &mut App, path: &str) {
        open_from_table(app, path);
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
    fn h_l_and_the_side_arrows_move_the_word_cursor_like_b_and_w() {
        let (_d, mut app) = fixture_app();
        open_article(&mut app, "citrus-vocab.md");
        assert_eq!((read(&app).cursor, read(&app).word), (2, 0));
        press(&mut app, 'l');
        press(&mut app, 'l');
        assert_eq!(read(&app).word, 2, "l moves to the next word like w");
        press(&mut app, 'h');
        assert_eq!(read(&app).word, 1, "h moves to the previous word like b");
        app.handle_key(KeyCode::Right).unwrap();
        assert_eq!(read(&app).word, 2, "right arrow like w");
        app.handle_key(KeyCode::Left).unwrap();
        app.handle_key(KeyCode::Left).unwrap();
        app.handle_key(KeyCode::Left).unwrap();
        assert_eq!(read(&app).word, 0, "left arrow like b, clamped at the first word");
        assert_eq!(read(&app).cursor, 2, "h/l never change the paragraph");
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
        assert_eq!(app.screen, Screen::Review, "on to the next item, not the table");
        assert_eq!(app.review.current.as_ref().unwrap().item.path, "kumquat.md");
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
        assert_eq!(app.screen, Screen::Review, "on to the next item, not the table");
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
        app.handle_key(KeyCode::Tab).unwrap();
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

    // ---- text prompt: `a` add card (M5) ----

    /// How many `.md` files sit directly in the vault root.
    fn root_md_count(dir: &tempfile::TempDir) -> usize {
        std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
            .filter(|e| e.file_name().to_string_lossy().ends_with(".md"))
            .count()
    }

    fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            press(app, c);
        }
    }

    #[test]
    fn a_opens_question_prompt_then_answer_then_writes_card() {
        let (dir, mut app) = fixture_app();
        app.handle_key(KeyCode::Tab).unwrap();
        press(&mut app, 'a');
        assert_eq!(app.prompt_text().as_deref(), Some("add card · Q: "));
        type_text(&mut app, "Big citrus?");
        assert_eq!(app.prompt_text().as_deref(), Some("add card · Q: Big citrus?"));
        app.handle_key(KeyCode::Enter).unwrap();
        assert_eq!(app.prompt_text().as_deref(), Some("add card · A: "));
        type_text(&mut app, "pomelo");
        assert_eq!(app.prompt_text().as_deref(), Some("add card · A: pomelo"));
        app.handle_key(KeyCode::Enter).unwrap();

        assert_eq!(app.prompt_text(), None);
        assert_eq!(app.notice.as_deref(), Some("added big-citrus.md"));
        assert_eq!(app.screen, Screen::Queue);
        let row = app.items.iter().find(|i| i.path == "big-citrus.md").unwrap().clone();
        assert_eq!(row.kind, ItemType::Card);
        assert_eq!(
            file(&dir, "big-citrus.md"),
            format!("---\ntype: card\nsm_id: {}\n---\nQ: Big citrus?\n\nA: pomelo\n", row.sm_id)
        );
        assert_eq!(app.items[app.queue_sel].path, "big-citrus.md", "the table selects the new card");
    }

    #[test]
    fn a_esc_writes_nothing() {
        let (dir, mut app) = fixture_app();
        app.handle_key(KeyCode::Tab).unwrap();
        let before = root_md_count(&dir);

        press(&mut app, 'a');
        type_text(&mut app, "Big citrus?");
        app.handle_key(KeyCode::Esc).unwrap();
        assert_eq!(app.prompt_text(), None);
        assert_eq!(root_md_count(&dir), before, "esc at the question writes nothing");

        press(&mut app, 'a');
        type_text(&mut app, "Big citrus?");
        app.handle_key(KeyCode::Enter).unwrap();
        type_text(&mut app, "pomelo");
        app.handle_key(KeyCode::Esc).unwrap();
        assert_eq!(app.prompt_text(), None);
        assert_eq!(root_md_count(&dir), before, "esc at the answer writes nothing");
    }

    #[test]
    fn a_enter_on_empty_is_ignored() {
        let (dir, mut app) = fixture_app();
        app.handle_key(KeyCode::Tab).unwrap();
        let before = root_md_count(&dir);
        press(&mut app, 'a');
        app.handle_key(KeyCode::Enter).unwrap();
        assert_eq!(app.prompt_text().as_deref(), Some("add card · Q: "));
        press(&mut app, ' ');
        app.handle_key(KeyCode::Enter).unwrap();
        assert_eq!(app.prompt_text().as_deref(), Some("add card · Q:  "), "blank is empty too");
        type_text(&mut app, "Q");
        app.handle_key(KeyCode::Enter).unwrap();
        assert_eq!(app.prompt_text().as_deref(), Some("add card · A: "));
        app.handle_key(KeyCode::Enter).unwrap();
        assert_eq!(app.prompt_text().as_deref(), Some("add card · A: "), "empty answer is ignored");
        assert_eq!(root_md_count(&dir), before);
    }

    #[test]
    fn a_backspace_edits() {
        let (_d, mut app) = fixture_app();
        app.handle_key(KeyCode::Tab).unwrap();
        press(&mut app, 'a');
        press(&mut app, 'x');
        press(&mut app, 'y');
        app.handle_key(KeyCode::Backspace).unwrap();
        assert_eq!(app.prompt_text().as_deref(), Some("add card · Q: x"));
        app.handle_key(KeyCode::Backspace).unwrap();
        app.handle_key(KeyCode::Backspace).unwrap();
        assert_eq!(app.prompt_text().as_deref(), Some("add card · Q: "), "backspace on empty is a no-op");

        // The prompt takes every key: `q` types, `tab` and ctrl chords do nothing.
        press(&mut app, 'q');
        assert!(!app.should_quit, "q types instead of quitting while the prompt is open");
        app.handle_key(KeyCode::Tab).unwrap();
        assert_eq!(app.screen, Screen::Queue, "tab does not switch screens while the prompt is open");
        app.handle_key_with(KeyCode::Char('x'), KeyModifiers::CONTROL).unwrap();
        app.handle_key_with(KeyCode::Char('Q'), KeyModifiers::SHIFT).unwrap();
        assert_eq!(app.prompt_text().as_deref(), Some("add card · Q: qQ"), "shift types, ctrl does not");
    }

    #[test]
    fn a_is_ignored_off_the_queue_screen() {
        let (_d, mut app) = fixture_app();
        open_from_table(&mut app, "kumquat.md");
        let sm_id = app.review.current.as_ref().unwrap().item.sm_id;
        press(&mut app, 'a');
        assert_eq!(app.prompt_text(), None);
        press(&mut app, ' ');
        press(&mut app, '4');
        assert_eq!(app.db.journal_count(sm_id).unwrap(), 1, "grades still work");

        open_article(&mut app, "citrus-vocab.md");
        press(&mut app, 'a');
        assert_eq!(app.prompt_text(), None, "the read screen has no add-card key");
        assert_eq!(app.screen, Screen::Read);
    }

    #[test]
    fn text_prompt_hints_kind() {
        let (_d, mut app) = fixture_app();
        app.handle_key(KeyCode::Tab).unwrap();
        assert_eq!(app.text_prompt_stage(), None);
        press(&mut app, 'a');
        assert_eq!(app.text_prompt_stage(), Some(TextStage::Question));
        type_text(&mut app, "Q");
        app.handle_key(KeyCode::Enter).unwrap();
        assert_eq!(app.text_prompt_stage(), Some(TextStage::Final));
        app.handle_key(KeyCode::Esc).unwrap();
        assert_eq!(app.text_prompt_stage(), None);
        press(&mut app, 'i');
        assert_eq!(app.text_prompt_stage(), Some(TextStage::Final));
    }

    #[test]
    fn paste_appends_to_prompt() {
        let (_d, mut app) = fixture_app();
        app.handle_key(KeyCode::Tab).unwrap();
        press(&mut app, 'i');
        app.paste("https://x.org/a\n");
        assert_eq!(app.prompt_text().as_deref(), Some("import · url or path: https://x.org/a"));
        app.handle_key(KeyCode::Esc).unwrap();
        app.paste("ignored");
        assert_eq!(app.prompt_text(), None, "paste with no prompt open is ignored");
    }

    // ---- text prompt: `i` import article (M5) ----

    /// The same HTML as `import::tests::html_scopes_to_main_and_drops_chrome`, so the
    /// expected body here and the stripper's own expectation stay one string.
    const WIKI_HTML: &str = "<html><head><title>Pomelo - Wikipedia</title><style>x</style></head><body><nav>Menu</nav><main><h1>Pomelo</h1><p>The pomelo is a <b>citrus</b> fruit &amp; large.</p><script>bad()</script><ul><li>one</li><li>two</li></ul><h2>Uses</h2><p>Juice.</p></main><footer>foot</footer></body></html>";
    const WIKI_URL: &str = "https://en.wikipedia.org/wiki/Pomelo";
    const WIKI_BODY: &str =
        "# Pomelo\n\nThe pomelo is a citrus fruit & large.\n\n- one\n\n- two\n\n## Uses\n\nJuice.\n";

    /// A [`Fetcher`] over a fixed map: url -> body, or url -> the error to report.
    struct FakeFetcher(std::collections::HashMap<String, std::result::Result<String, String>>);

    impl FakeFetcher {
        fn ok(url: &str, body: &str) -> Self {
            let mut m = std::collections::HashMap::new();
            m.insert(url.to_string(), Ok(body.to_string()));
            FakeFetcher(m)
        }

        fn err(url: &str, reason: &str) -> Self {
            let mut m = std::collections::HashMap::new();
            m.insert(url.to_string(), Err(reason.to_string()));
            FakeFetcher(m)
        }
    }

    impl Fetcher for FakeFetcher {
        fn get(&self, url: &str) -> Result<String> {
            match self.0.get(url) {
                Some(Ok(body)) => Ok(body.clone()),
                Some(Err(reason)) => Err(anyhow::anyhow!("{reason}")),
                None => Err(anyhow::anyhow!("no route to {url}")),
            }
        }
    }

    /// `i`, type `input`, `enter`.
    fn import(app: &mut App, input: &str) {
        press(app, 'i');
        type_text(app, input);
        app.handle_key(KeyCode::Enter).unwrap();
    }

    /// One event-loop tick.
    fn tick_once(app: &mut App) {
        app.tick(Instant::now(), today()).unwrap();
    }

    /// The two ticks a URL import takes: the first arms it and leaves `fetching …` on
    /// screen for the draw in between, the second fetches and writes.
    fn tick_twice(app: &mut App) {
        tick_once(app);
        tick_once(app);
    }

    #[test]
    fn i_url_fetches_on_the_second_tick_so_fetching_is_drawn() {
        let (dir, mut app) = fixture_app();
        app.handle_key(KeyCode::Tab).unwrap();
        let before = root_md_count(&dir);
        app.set_fetcher(Box::new(FakeFetcher::ok(WIKI_URL, WIKI_HTML)));
        import(&mut app, WIKI_URL);
        assert_eq!(app.prompt_text(), None, "the prompt closes on enter");

        // The loop draws between two ticks, so this is the frame the user sees.
        tick_once(&mut app);
        assert_eq!(app.notice.as_deref(), Some("fetching …"), "the fetch waits one frame");
        assert_eq!(root_md_count(&dir), before, "nothing is written yet");
        assert!(app.db.item_by_url(WIKI_URL).unwrap().is_none(), "nothing indexed yet");

        tick_once(&mut app);
        assert_eq!(app.notice.as_deref(), Some("imported pomelo-wikipedia.md"));
        assert_eq!(root_md_count(&dir), before + 1);
        assert!(dir.path().join("pomelo-wikipedia.md").exists());
    }

    #[test]
    fn i_url_imports_article_with_reference() {
        let (dir, mut app) = fixture_app();
        app.handle_key(KeyCode::Tab).unwrap();
        app.set_fetcher(Box::new(FakeFetcher::ok(WIKI_URL, WIKI_HTML)));
        import(&mut app, WIKI_URL);
        tick_twice(&mut app);

        assert_eq!(app.notice.as_deref(), Some("imported pomelo-wikipedia.md"));
        let row = app.items.iter().find(|i| i.path == "pomelo-wikipedia.md").unwrap().clone();
        assert_eq!(row.kind, ItemType::Article);
        assert_eq!(row.url.as_deref(), Some(WIKI_URL));
        assert_eq!(row.imported, Some(today()));
        assert_eq!(
            file(&dir, "pomelo-wikipedia.md"),
            format!(
                "---\ntype: article\nsm_id: {}\nurl: {WIKI_URL}\nimported: 2026-09-20\n---\n{WIKI_BODY}",
                row.sm_id
            )
        );
        assert_eq!(app.items[app.queue_sel].path, "pomelo-wikipedia.md", "the table selects it");
    }

    #[test]
    fn i_duplicate_url_is_refused() {
        let (dir, mut app) = fixture_app();
        app.handle_key(KeyCode::Tab).unwrap();
        app.set_fetcher(Box::new(FakeFetcher::ok(WIKI_URL, WIKI_HTML)));
        import(&mut app, WIKI_URL);
        tick_twice(&mut app);
        let after_first = root_md_count(&dir);

        app.queue_sel = 0;
        import(&mut app, WIKI_URL);
        assert_eq!(app.notice.as_deref(), Some("already imported · pomelo-wikipedia.md"));
        tick_once(&mut app);
        assert_eq!(
            app.notice.as_deref(),
            Some("already imported · pomelo-wikipedia.md"),
            "the refusal is final; the first tick arms nothing"
        );
        tick_once(&mut app);
        assert_eq!(
            app.notice.as_deref(),
            Some("already imported · pomelo-wikipedia.md"),
            "and the second tick fetches nothing"
        );
        assert_eq!(root_md_count(&dir), after_first, "no second file");
        assert_eq!(app.items[app.queue_sel].path, "pomelo-wikipedia.md", "the table selects the original");
    }

    #[test]
    fn i_fetch_error_reports_and_writes_nothing() {
        let (dir, mut app) = fixture_app();
        app.handle_key(KeyCode::Tab).unwrap();
        let before = root_md_count(&dir);
        app.set_fetcher(Box::new(FakeFetcher::err(WIKI_URL, "connection refused")));
        import(&mut app, WIKI_URL);
        tick_once(&mut app);
        assert_eq!(app.notice.as_deref(), Some("fetching …"), "the failure waits for the fetch");
        tick_once(&mut app);

        assert_eq!(app.notice.as_deref(), Some("import failed · connection refused"));
        assert_eq!(root_md_count(&dir), before);
        assert!(app.db.item_by_url(WIKI_URL).unwrap().is_none(), "nothing indexed");
    }

    #[test]
    fn i_path_imports_markdown_verbatim() {
        let (dir, mut app) = fixture_app();
        app.handle_key(KeyCode::Tab).unwrap();
        let src = tempfile::tempdir().unwrap();
        let path = src.path().join("notes.md");
        std::fs::write(&path, "# Tea\n\nLeaves.\n").unwrap();

        import(&mut app, &path.to_string_lossy());
        assert_eq!(app.notice.as_deref(), Some("imported tea.md"), "a path import needs no tick");
        let row = app.items.iter().find(|i| i.path == "tea.md").unwrap().clone();
        assert_eq!(row.url, None, "a path import has no url");
        assert_eq!(row.imported, Some(today()));
        assert_eq!(
            file(&dir, "tea.md"),
            format!("---\ntype: article\nsm_id: {}\nimported: 2026-09-20\n---\n# Tea\n\nLeaves.\n", row.sm_id)
        );
        assert_eq!(app.items[app.queue_sel].path, "tea.md");
    }

    #[test]
    fn i_path_without_heading_uses_stem() {
        let (dir, mut app) = fixture_app();
        app.handle_key(KeyCode::Tab).unwrap();
        let src = tempfile::tempdir().unwrap();
        let path = src.path().join("plain.txt");
        std::fs::write(&path, "Just text.\n").unwrap();

        import(&mut app, &path.to_string_lossy());
        assert_eq!(app.notice.as_deref(), Some("imported plain.md"));
        let row = app.items.iter().find(|i| i.path == "plain.md").unwrap().clone();
        assert_eq!(
            file(&dir, "plain.md"),
            format!("---\ntype: article\nsm_id: {}\nimported: 2026-09-20\n---\n# plain\n\nJust text.\n", row.sm_id)
        );
    }

    #[test]
    fn i_path_html_is_stripped() {
        let (dir, mut app) = fixture_app();
        app.handle_key(KeyCode::Tab).unwrap();
        let src = tempfile::tempdir().unwrap();
        let path = src.path().join("page.html");
        std::fs::write(&path, "<title>Page</title><main><p>Hi</p></main>").unwrap();

        import(&mut app, &path.to_string_lossy());
        assert_eq!(app.notice.as_deref(), Some("imported page.md"));
        let row = app.items.iter().find(|i| i.path == "page.md").unwrap().clone();
        assert_eq!(
            file(&dir, "page.md"),
            format!("---\ntype: article\nsm_id: {}\nimported: 2026-09-20\n---\n# Page\n\nHi\n", row.sm_id)
        );
    }

    #[test]
    fn i_missing_path_reports() {
        let (dir, mut app) = fixture_app();
        app.handle_key(KeyCode::Tab).unwrap();
        let before = root_md_count(&dir);
        let src = tempfile::tempdir().unwrap();
        let path = src.path().join("gone.md");

        import(&mut app, &path.to_string_lossy());
        let notice = app.notice.clone().unwrap();
        assert!(notice.starts_with("import failed · "), "got {notice:?}");
        assert_eq!(root_md_count(&dir), before, "nothing written");
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
             DROP INDEX IF EXISTS idx_url;
             ALTER TABLE items DROP COLUMN a_factor;
             ALTER TABLE items DROP COLUMN done;
             ALTER TABLE items DROP COLUMN source;
             ALTER TABLE items DROP COLUMN range_start;
             ALTER TABLE items DROP COLUMN range_end;
             ALTER TABLE items DROP COLUMN url;
             ALTER TABLE items DROP COLUMN imported;
             UPDATE meta SET value = '1' WHERE key = 'schema_version';",
        )
        .unwrap();
        drop(conn);
        let app = App::open(dir.path(), today()).unwrap();
        assert_eq!(app.db.meta("schema_version").unwrap().as_deref(), Some("3"));
        assert_eq!(app.refresh.indexed, 8, "one full re-index after the migration");
        assert_eq!(app.refresh.allocated, 0);
        assert_eq!(app.items, expected);
        assert_eq!(app.db.children_of("citrus-vocab").unwrap().len(), 3);
    }

    // ---- M3 learn session ----

    #[test]
    fn open_lands_on_the_first_due_item() {
        let (_d, app) = fixture_app();
        assert_eq!(app.screen, Screen::Review);
        assert_eq!(app.review.current.as_ref().unwrap().item.path, "yuzu.md");
        assert_eq!(app.review_context(), "card · prio 12 · 1/6");
    }

    #[test]
    fn due_items_are_cards_and_articles_due_today_or_earlier_or_undated() {
        let (_d, app) = fixture_app();
        // yuzu (none), citrus-vocab (none), kumquat (09-10), finger-lime (none),
        // earl-grey (none), bergamot (09-01); pomelo 09-25 and buddhas-hand 09-30 are not due.
        assert_eq!(app.review.due.len(), 6);
    }

    #[test]
    fn grading_a_card_moves_to_the_next_item_even_an_article() {
        let (_d, mut app) = fixture_app();
        press(&mut app, ' ');
        press(&mut app, '4');
        assert_eq!(app.screen, Screen::Read);
        assert_eq!(read(&app).item.path, "citrus-vocab.md");
        assert_eq!(app.review.pos, 1);
        assert!(app.review.current.is_none(), "the article is not a card in review");
    }

    #[test]
    fn article_enter_advances_to_the_next_item_not_the_table() {
        let (dir, mut app) = fixture_app();
        press(&mut app, ' ');
        press(&mut app, '4');
        app.handle_key(KeyCode::Enter).unwrap();
        assert_eq!(app.screen, Screen::Review);
        assert_eq!(app.review.current.as_ref().unwrap().item.path, "kumquat.md");
        assert_eq!(app.review.read_done, 1);
        let text = file(&dir, "citrus-vocab.md");
        assert!(text.contains("\ndue: 2026-09-27\ninterval: 7\n"), "M2 scheduling unchanged: {text}");
    }

    #[test]
    fn article_d_advances_too() {
        let (_d, mut app) = fixture_app();
        press(&mut app, ' ');
        press(&mut app, '4'); // yuzu
        app.handle_key(KeyCode::Enter).unwrap(); // citrus-vocab
        press(&mut app, ' ');
        press(&mut app, '4'); // kumquat
        press(&mut app, ' ');
        press(&mut app, '4'); // finger-lime
        assert_eq!(read(&app).item.path, "earl-grey.md");
        press(&mut app, 'd');
        assert_eq!(app.screen, Screen::Review);
        assert_eq!(app.review.current.as_ref().unwrap().item.path, "bergamot.md");
        // citrus-vocab was read, earl-grey was marked done: both count as read.
        assert_eq!(app.review.read_done, 2);
    }

    #[test]
    fn tab_from_read_saves_and_tab_back_reopens_the_same_article() {
        let (dir, mut app) = fixture_app();
        press(&mut app, ' ');
        press(&mut app, '4');
        assert_eq!(read(&app).cursor, 2, "opens at read_pos 83");
        press(&mut app, 'j');
        press(&mut app, 'j');
        assert_eq!(read(&app).cursor, 3, "clamped at the last of four paragraphs");
        app.handle_key(KeyCode::Tab).unwrap();
        assert_eq!(app.screen, Screen::Queue);
        assert!(app.read.is_none());
        assert!(file(&dir, "citrus-vocab.md").contains("\nread_pos: 207\n"));
        app.handle_key(KeyCode::Tab).unwrap();
        assert_eq!(app.screen, Screen::Read);
        assert_eq!(read(&app).item.path, "citrus-vocab.md");
        assert_eq!(read(&app).cursor, 3, "back at the saved read-point");
        assert_eq!(app.review.pos, 1, "still the same place in the session");
    }

    #[test]
    fn u_on_the_read_screen_undoes_the_grade_before_it() {
        let (_d, mut app) = fixture_app();
        press(&mut app, ' ');
        press(&mut app, '4');
        assert_eq!(read(&app).item.path, "citrus-vocab.md");
        let yuzu = 1044;
        assert_eq!(app.db.journal_count(yuzu).unwrap(), 1);
        press(&mut app, 'u');
        assert_eq!(app.screen, Screen::Review);
        assert_eq!(app.review.current.as_ref().unwrap().item.path, "yuzu.md");
        assert!(app.review.revealed);
        assert_eq!(app.review.status.as_deref(), Some("undid grade 4"));
        assert!(app.read.is_none());
        assert_eq!(app.db.journal_count(yuzu).unwrap(), 0);
    }

    #[test]
    fn u_on_the_read_screen_with_nothing_to_undo_says_so_and_stays() {
        let (_d, mut app) = fixture_app();
        open_article(&mut app, "citrus-vocab.md");
        press(&mut app, 'u');
        assert_eq!(app.screen, Screen::Read);
        assert!(app.read.is_some());
        // The read screen shows no status line, so the refusal arrives as a notice.
        assert_eq!(app.notice.as_deref(), Some("nothing to undo"));
    }

    #[test]
    fn the_grade_status_line_does_not_survive_an_article() {
        let (_d, mut app) = fixture_app();
        press(&mut app, ' ');
        press(&mut app, '4');
        assert_eq!(app.review.status.as_deref(), Some("graded 4 · journaled (offline)"));
        app.handle_key(KeyCode::Enter).unwrap();
        assert_eq!(app.review.current.as_ref().unwrap().item.path, "kumquat.md");
        assert!(app.review.status.is_none(), "{:?} belongs to the card two items back", app.review.status);
    }

    #[test]
    fn enter_on_a_table_row_inserts_an_undue_item_at_the_current_position() {
        let (_d, mut app) = fixture_app();
        open_from_table(&mut app, "pomelo.md");
        assert_eq!(app.screen, Screen::Review);
        assert_eq!(app.review.due.len(), 7);
        assert_eq!(app.review.pos, 0);
        assert_eq!(app.review.current.as_ref().unwrap().item.path, "pomelo.md");
        assert_eq!(app.review_context(), "card · prio 28 · 1/7");
        press(&mut app, ' ');
        press(&mut app, '4');
        assert_eq!(app.review.current.as_ref().unwrap().item.path, "yuzu.md");
    }

    #[test]
    fn enter_on_a_table_article_jumps_the_session_there() {
        let (_d, mut app) = fixture_app();
        open_from_table(&mut app, "earl-grey.md");
        assert_eq!(app.screen, Screen::Read);
        assert_eq!(app.review.pos, 4);
        app.handle_key(KeyCode::Enter).unwrap();
        assert_eq!(app.review.current.as_ref().unwrap().item.path, "bergamot.md");
    }

    #[test]
    fn finishing_the_session_shows_no_current_item() {
        let (_d, mut app) = fixture_app();
        for _ in 0..6 {
            if app.screen == Screen::Read {
                app.handle_key(KeyCode::Enter).unwrap();
            } else {
                press(&mut app, ' ');
                press(&mut app, '4');
            }
        }
        assert!(app.review.current.is_none());
        assert_eq!(app.screen, Screen::Review);
        assert_eq!(app.review_context(), "card · 6/6");
        assert_eq!(app.review.done, 4);
        assert_eq!(app.review.read_done, 2);
    }

    // ---- M3 final drill ----

    fn id_of(app: &App, path: &str) -> i64 {
        app.items.iter().find(|i| i.path == path).unwrap().sm_id
    }

    /// Walk the whole main pass: yuzu 2, kumquat 3, finger-lime and bergamot 4,
    /// both articles `enter`. Leaves the session on the drill prompt.
    fn walk_to_prompt(app: &mut App) {
        walk_to_prompt_with(app, ['2', '3', '4', '4']);
    }

    /// The same walk with one grade per card, in session order: yuzu, kumquat,
    /// finger-lime, bergamot. The two articles are ended with `enter`.
    fn walk_to_prompt_with(app: &mut App, grades: [char; 4]) {
        let mut grades = grades.into_iter();
        for _ in 0..6 {
            if app.screen == Screen::Read {
                app.handle_key(KeyCode::Enter).unwrap();
            } else {
                press(app, ' ');
                press(app, grades.next().unwrap());
            }
        }
    }

    /// Modification times of every markdown file in the vault, by name.
    fn mtimes(dir: &tempfile::TempDir) -> Vec<(String, std::time::SystemTime)> {
        let mut out: Vec<(String, std::time::SystemTime)> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap())
            .filter(|e| e.path().extension().is_some_and(|x| x == "md"))
            .map(|e| {
                (
                    e.file_name().to_string_lossy().into_owned(),
                    e.metadata().unwrap().modified().unwrap(),
                )
            })
            .collect();
        out.sort();
        out
    }

    /// A vault holding only the named fixture files.
    fn vault_with(names: &[&str]) -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/vault");
        for name in names {
            std::fs::copy(src.join(name), dir.path().join(name)).unwrap();
        }
        let app = App::open(dir.path(), today()).unwrap();
        (dir, app)
    }

    #[test]
    fn failing_grades_join_the_drill_list_once() {
        let (_d, mut app) = fixture_app();
        let yuzu = id_of(&app, "yuzu.md");
        press(&mut app, ' ');
        press(&mut app, '2');
        assert_eq!(app.review.drill, vec![yuzu]);
        open_from_table(&mut app, "yuzu.md");
        press(&mut app, ' ');
        press(&mut app, '1');
        assert_eq!(app.review.drill, vec![yuzu], "a second failure adds nothing");
    }

    #[test]
    fn passing_grades_do_not_join_the_drill_list() {
        let (_d, mut app) = fixture_app();
        press(&mut app, ' ');
        press(&mut app, '4');
        assert!(app.review.drill.is_empty());
    }

    #[test]
    fn undo_removes_the_card_from_the_drill_list() {
        let (_d, mut app) = fixture_app();
        press(&mut app, ' ');
        press(&mut app, '2');
        assert_eq!(app.review.drill.len(), 1);
        press(&mut app, 'u');
        assert_eq!(app.review.current.as_ref().unwrap().item.path, "yuzu.md");
        assert!(app.review.drill.is_empty());
    }

    #[test]
    fn undo_keeps_a_card_whose_earlier_failing_grade_still_stands() {
        let (_d, mut app) = fixture_app();
        let yuzu = id_of(&app, "yuzu.md");
        press(&mut app, ' ');
        press(&mut app, '2');
        assert_eq!(app.review.drill, vec![yuzu]);
        open_from_table(&mut app, "yuzu.md");
        press(&mut app, ' ');
        press(&mut app, '4');
        assert_eq!(app.review.drill, vec![yuzu], "a pass does not clear the drill list");
        press(&mut app, 'u');
        assert_eq!(app.review.drill, vec![yuzu], "undoing the pass leaves the failure standing");
    }

    #[test]
    fn end_of_session_with_failures_shows_the_drill_prompt() {
        let (_d, mut app) = fixture_app();
        walk_to_prompt(&mut app);
        assert_eq!(app.review.phase, Phase::DrillPrompt);
        assert_eq!(app.drill_prompt(), "final drill · 2 cards");
        assert_eq!(app.review_context(), "card · 6/6");
    }

    #[test]
    fn n_finishes_and_empties_the_drill() {
        let (_d, mut app) = fixture_app();
        walk_to_prompt(&mut app);
        press(&mut app, 'n');
        assert_eq!(app.review.phase, Phase::Main);
        assert!(app.review.drill.is_empty());
        assert_eq!(app.finish_line(), "nothing more to learn · 4 graded · 2 read");
    }

    #[test]
    fn y_clears_the_last_real_grade_status_before_the_first_drill_card() {
        let (_d, mut app) = fixture_app();
        walk_to_prompt(&mut app);
        assert_eq!(app.review.status.as_deref(), Some("graded 4 · journaled (offline)"), "bergamot's grade");
        press(&mut app, 'y');
        assert_eq!(app.review.phase, Phase::Drilling);
        assert!(app.review.status.is_none(), "the main-pass grade must not sit under a drill card");
        press(&mut app, ' ');
        press(&mut app, '4');
        assert_eq!(app.review.status.as_deref(), Some("drill 4 · cleared"));
    }

    #[test]
    fn y_drills_until_every_card_passes() {
        let (_d, mut app) = fixture_app();
        let (yuzu, kumquat) = (id_of(&app, "yuzu.md"), id_of(&app, "kumquat.md"));
        walk_to_prompt(&mut app);
        press(&mut app, 'y');
        assert_eq!(app.review.phase, Phase::Drilling);
        assert_eq!(app.review.current.as_ref().unwrap().item.path, "yuzu.md");
        assert_eq!(app.review_context(), "drill · prio 12 · 2 left");

        press(&mut app, ' ');
        press(&mut app, '3');
        assert_eq!(app.review.current.as_ref().unwrap().item.path, "kumquat.md");
        assert_eq!(app.review.drill, vec![kumquat, yuzu], "a failed card goes to the back");
        assert_eq!(app.review.status.as_deref(), Some("drill 3 · stays"));

        press(&mut app, ' ');
        press(&mut app, '4');
        assert_eq!(app.review.current.as_ref().unwrap().item.path, "yuzu.md");
        assert_eq!(app.review.drill, vec![yuzu]);
        assert_eq!(app.review_context(), "drill · prio 12 · 1 left");
        assert_eq!(app.review.status.as_deref(), Some("drill 4 · cleared"));

        press(&mut app, ' ');
        press(&mut app, '5');
        assert_eq!(app.review.phase, Phase::Main);
        assert!(app.review.current.is_none());
        assert!(app.review.drill.is_empty());
        assert_eq!(app.finish_line(), "nothing more to learn · 4 graded · 2 read");
    }

    #[test]
    fn opening_a_card_from_the_table_at_the_prompt_shows_the_card() {
        let (_d, mut app) = fixture_app();
        let yuzu = id_of(&app, "yuzu.md");
        walk_to_prompt_with(&mut app, ['2', '4', '4', '4']);
        open_from_table(&mut app, "pomelo.md");
        assert_eq!(app.review.phase, Phase::Main, "the table jump leaves the prompt");
        assert_eq!(app.review.current.as_ref().unwrap().item.path, "pomelo.md");
        assert_eq!(app.review_context(), "card · prio 28 · 7/7");
        assert_eq!(app.review.drill, vec![yuzu], "the drill list is kept");
        press(&mut app, ' ');
        press(&mut app, '4');
        assert_eq!(app.review.phase, Phase::DrillPrompt, "the prompt comes back at the end");
        assert_eq!(app.drill_prompt(), "final drill · 1 card");
    }

    #[test]
    fn opening_an_item_from_the_table_while_drilling_leaves_the_drill() {
        let (_d, mut app) = fixture_app();
        let yuzu = id_of(&app, "yuzu.md");
        walk_to_prompt_with(&mut app, ['2', '4', '4', '4']);
        press(&mut app, 'y');
        assert_eq!(app.review.phase, Phase::Drilling);
        open_from_table(&mut app, "pomelo.md");
        assert_eq!(app.review.phase, Phase::Main);
        assert_eq!(app.review.current.as_ref().unwrap().item.path, "pomelo.md");
        assert_eq!(app.review.drill, vec![yuzu], "the drill list is kept");
        press(&mut app, ' ');
        press(&mut app, '4');
        assert_eq!(app.review.phase, Phase::DrillPrompt);
        assert_eq!(app.drill_prompt(), "final drill · 1 card");
    }

    #[test]
    fn drill_grades_write_nothing() {
        let (dir, mut app) = fixture_app();
        walk_to_prompt(&mut app);
        let cards = ["yuzu.md", "kumquat.md", "finger-lime.md", "bergamot.md"];
        let journalled: Vec<i64> =
            cards.iter().map(|p| app.db.journal_count(id_of(&app, p)).unwrap()).collect();
        let pending = app.db.pending_grades().unwrap().len();
        let files = mtimes(&dir);
        let done = app.review.done;

        press(&mut app, 'y');
        for g in ['3', '4', '5'] {
            press(&mut app, ' ');
            press(&mut app, g);
        }
        assert!(app.review.drill.is_empty(), "the drill ran to the end");

        let after: Vec<i64> = cards.iter().map(|p| app.db.journal_count(id_of(&app, p)).unwrap()).collect();
        assert_eq!(after, journalled, "no journal row");
        assert_eq!(app.db.pending_grades().unwrap().len(), pending, "no outbox work");
        assert_eq!(mtimes(&dir), files, "no file write");
        assert_eq!(app.review.done, done, "drill grades are not counted");
    }

    #[test]
    fn u_in_drill_says_so_and_changes_nothing() {
        let (_d, mut app) = fixture_app();
        walk_to_prompt(&mut app);
        press(&mut app, 'y');
        let drill = app.review.drill.clone();
        let current = app.review.current.as_ref().unwrap().item.sm_id;
        press(&mut app, 'u');
        assert_eq!(app.review.status.as_deref(), Some("no undo in drill"));
        assert_eq!(app.review.drill, drill);
        assert_eq!(app.review.current.as_ref().unwrap().item.sm_id, current);
    }

    #[test]
    fn tab_during_drill_and_back_resumes_it() {
        let (_d, mut app) = fixture_app();
        walk_to_prompt(&mut app);
        press(&mut app, 'y');
        let current = app.review.current.as_ref().unwrap().item.sm_id;
        app.handle_key(KeyCode::Tab).unwrap();
        assert_eq!(app.screen, Screen::Queue);
        app.handle_key(KeyCode::Tab).unwrap();
        assert_eq!(app.screen, Screen::Review);
        assert_eq!(app.review.phase, Phase::Drilling);
        assert_eq!(app.review.current.as_ref().unwrap().item.sm_id, current);
    }

    #[test]
    fn empty_session_opens_on_the_finish_line() {
        let (_d, app) = vault_with(&["pomelo.md", "buddhas-hand.md"]);
        assert_eq!(app.screen, Screen::Review);
        assert!(app.review.current.is_none());
        assert_eq!(app.review.phase, Phase::Main);
        assert_eq!(app.finish_line(), "nothing more to learn · 0 graded · 0 read");
    }

    #[test]
    fn progress_counts_reached_items_in_the_main_pass_only() {
        let (_d, mut app) = fixture_app();
        assert_eq!(app.progress(), Some((1, 6)), "the first item is already reached");
        press(&mut app, ' ');
        press(&mut app, '4'); // yuzu graded; citrus-vocab (an article) is next
        assert_eq!(app.progress(), Some((2, 6)), "the bar follows onto the read screen");
        app.handle_key(KeyCode::Enter).unwrap(); // end the article
        assert_eq!(app.progress(), Some((3, 6)));
        app.handle_key(KeyCode::Tab).unwrap();
        assert_eq!(app.screen, Screen::Queue);
        assert_eq!(app.progress(), None, "no bar on the queue");
        app.handle_key(KeyCode::Tab).unwrap();
        assert_eq!(app.progress(), Some((3, 6)), "back on the session, back to the bar");

        let (_d, mut app) = fixture_app();
        walk_to_prompt(&mut app);
        assert_eq!(app.review.phase, Phase::DrillPrompt);
        assert_eq!(app.progress(), None, "no bar at the drill prompt");
        press(&mut app, 'y');
        assert_eq!(app.review.phase, Phase::Drilling);
        assert_eq!(app.progress(), None, "no bar while drilling");

        let (_d, app) = vault_with(&["pomelo.md", "buddhas-hand.md"]);
        assert_eq!(app.progress(), None, "no bar when nothing is due");

        let (_d, mut app) = fixture_app();
        walk_to_prompt_with(&mut app, ['4', '4', '4', '4']);
        assert_eq!(app.review.phase, Phase::Main, "no failure, no drill");
        assert_eq!(app.progress(), Some((6, 6)), "a finished pass reads full");
    }

    #[test]
    fn today_is_the_open_date() {
        let (_d, app) = fixture_app();
        assert_eq!(app.today(), NaiveDate::from_ymd_opt(2026, 9, 20).unwrap());
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
