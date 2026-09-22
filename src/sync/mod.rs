//! SuperMemo sync: the outbox of unsent grades and the rules for sending them.
//!
//! [`Outbox`] is pure logic over a clock it is handed, so every timing rule
//! (grace, spacing, backoff, daily cap) is unit-tested without threads. The
//! UI thread owns it; [`worker`] does the HTTP.

pub mod api;
pub mod worker;

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use chrono::NaiveDate;

use self::api::{ApiError, Reviewed};

/// Minimum gap between two requests: the API allows 1 request/second on every tier.
pub const RATE_SPACING: Duration = Duration::from_millis(1100);
/// Defaults for the `meta` keys that tune the outbox.
pub const DEFAULT_GRACE_SECS: i64 = 5;
pub const DEFAULT_DAILY_CAP: i64 = 50;
const BACKOFF_BASE: Duration = Duration::from_secs(2);
const BACKOFF_MAX: Duration = Duration::from_secs(60);

/// One grade to send. Built from a journal row; `review_date` is the local date of `graded_at`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncRequest {
    pub journal_id: i64,
    pub sm_id: i64,
    pub grade: u8,
    pub review_date: NaiveDate,
}

/// What the worker sends back for one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncResult {
    pub journal_id: i64,
    pub outcome: Result<Reviewed, ApiError>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxConfig {
    /// Delay between grading and sending, so undo has its window.
    pub grace: Duration,
    /// Minimum gap between dispatches.
    pub spacing: Duration,
    /// Requests allowed per local day before pausing.
    pub daily_cap: i64,
    /// Counter carried over from `meta`: the day it belongs to and the count so far.
    pub requests_day: Option<NaiveDate>,
    pub requests_today: i64,
}

/// What happened to a row when its result came back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Landed {
    Synced { req: SyncRequest, interval: i64 },
    Retry { after: Duration, reason: String },
    Stopped { reason: String },
    Rejected { reason: String },
    /// Not the row in flight; ignored.
    Stale,
}

/// Where one row stands, for the review status line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowState {
    /// Queued; seconds until the grace period ends (0 when only waiting for spacing).
    Waiting { secs: u64 },
    CapReached,
    InFlight,
    Retrying { reason: String, secs: u64 },
    Stopped { reason: String },
    Rejected { reason: String },
    /// Synced, cancelled or never tracked.
    Gone,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Queued {
    req: SyncRequest,
    ready_at: Instant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Backoff {
    attempt: u32,
    until: Instant,
    reason: String,
}

/// Unsent grades and the state needed to send them politely. Owned by the UI thread.
#[derive(Debug)]
pub struct Outbox {
    cfg: OutboxConfig,
    /// Ascending by `journal_id`, so the oldest grade goes first.
    queue: VecDeque<Queued>,
    in_flight: Option<SyncRequest>,
    /// 422-rejected this session: kept `synced = 0` on disk, not retried.
    rejected: Vec<(SyncRequest, String)>,
    last_dispatch: Option<Instant>,
    backoff: Option<Backoff>,
    stopped: Option<String>,
    requests_day: Option<NaiveDate>,
    requests_today: i64,
}

impl Outbox {
    pub fn new(cfg: OutboxConfig) -> Self {
        Outbox {
            requests_day: cfg.requests_day,
            requests_today: cfg.requests_today,
            cfg,
            queue: VecDeque::new(),
            in_flight: None,
            rejected: Vec::new(),
            last_dispatch: None,
            backoff: None,
            stopped: None,
        }
    }

    /// Queue a grade; it becomes sendable once the grace period has passed.
    pub fn enqueue(&mut self, req: SyncRequest, now: Instant) {
        let ready_at = now + self.cfg.grace;
        let at = self
            .queue
            .iter()
            .position(|q| q.req.journal_id > req.journal_id)
            .unwrap_or(self.queue.len());
        self.queue.insert(at, Queued { req, ready_at });
    }

    /// Make every queued row ready now (used on quit).
    pub fn flush(&mut self, now: Instant) {
        for q in &mut self.queue {
            q.ready_at = now;
        }
    }

    /// Drop a row that has not been sent. Returns `false` when it is in flight or already gone.
    pub fn cancel(&mut self, journal_id: i64) -> bool {
        if let Some(i) = self.queue.iter().position(|q| q.req.journal_id == journal_id) {
            self.queue.remove(i);
            return true;
        }
        if let Some(i) = self.rejected.iter().position(|(r, _)| r.journal_id == journal_id) {
            self.rejected.remove(i);
            return true;
        }
        false
    }

    /// The next request to hand to the worker, if the rules allow one right now.
    pub fn poll(&mut self, now: Instant, today: NaiveDate) -> Option<SyncRequest> {
        if self.stopped.is_some() || self.in_flight.is_some() {
            return None;
        }
        if self.backoff.as_ref().is_some_and(|b| b.until > now) {
            return None;
        }
        self.roll_over(today);
        if self.requests_today >= self.cfg.daily_cap {
            return None;
        }
        if self.last_dispatch.is_some_and(|t| t + self.cfg.spacing > now) {
            return None;
        }
        if self.queue.front().is_none_or(|q| q.ready_at > now) {
            return None;
        }
        let queued = self.queue.pop_front()?;
        self.in_flight = Some(queued.req.clone());
        self.last_dispatch = Some(now);
        self.requests_today += 1;
        Some(queued.req)
    }

    /// Record the worker's reply for the row in flight.
    pub fn complete(&mut self, journal_id: i64, outcome: Result<Reviewed, ApiError>, now: Instant) -> Landed {
        let Some(req) = self.in_flight.take_if(|r| r.journal_id == journal_id) else {
            return Landed::Stale;
        };
        match outcome {
            Ok(reviewed) => {
                self.backoff = None;
                Landed::Synced {
                    req,
                    interval: reviewed.interval,
                }
            }
            Err(e @ ApiError::Transient { .. }) => {
                let attempt = self.backoff.as_ref().map_or(1, |b| b.attempt + 1);
                let after = BACKOFF_BASE
                    .saturating_mul(1u32 << (attempt - 1).min(5))
                    .min(BACKOFF_MAX);
                let reason = e.summary();
                self.backoff = Some(Backoff {
                    attempt,
                    until: now + after,
                    reason: reason.clone(),
                });
                self.queue.push_front(Queued { req, ready_at: now });
                Landed::Retry { after, reason }
            }
            Err(e @ ApiError::Unauthorized { .. }) => {
                let reason = e.summary();
                self.stopped = Some(reason.clone());
                self.queue.push_front(Queued { req, ready_at: now });
                Landed::Stopped { reason }
            }
            Err(e @ ApiError::Rejected { .. }) => {
                let reason = e.summary();
                self.rejected.push((req, reason.clone()));
                Landed::Rejected { reason }
            }
        }
    }

    /// Where a row stands, for the status line.
    pub fn state(&self, journal_id: i64, now: Instant) -> RowState {
        if self.in_flight.as_ref().is_some_and(|r| r.journal_id == journal_id) {
            return RowState::InFlight;
        }
        if let Some((_, reason)) = self.rejected.iter().find(|(r, _)| r.journal_id == journal_id) {
            return RowState::Rejected {
                reason: reason.clone(),
            };
        }
        let Some(q) = self.queue.iter().find(|q| q.req.journal_id == journal_id) else {
            return RowState::Gone;
        };
        if let Some(reason) = &self.stopped {
            return RowState::Stopped {
                reason: reason.clone(),
            };
        }
        if q.ready_at > now {
            return RowState::Waiting {
                secs: secs_until(now, q.ready_at),
            };
        }
        if let Some(b) = self.backoff.as_ref().filter(|b| b.until > now) {
            return RowState::Retrying {
                reason: b.reason.clone(),
                secs: secs_until(now, b.until),
            };
        }
        if self.requests_today >= self.cfg.daily_cap {
            return RowState::CapReached;
        }
        RowState::Waiting { secs: 0 }
    }

    /// Rows still `synced = 0` that this outbox knows about: queued, in flight, or rejected.
    pub fn pending_count(&self) -> usize {
        self.queue.len() + usize::from(self.in_flight.is_some()) + self.rejected.len()
    }

    pub fn is_stopped(&self) -> bool {
        self.stopped.is_some()
    }

    /// Nothing queued and nothing in flight (rejected rows do not count; they are not sent again).
    pub fn is_drained(&self) -> bool {
        self.queue.is_empty() && self.in_flight.is_none()
    }

    /// Whether the day's request budget is used up (as of the last `poll`).
    pub fn cap_reached(&self) -> bool {
        self.requests_today >= self.cfg.daily_cap
    }

    pub fn in_flight(&self) -> Option<&SyncRequest> {
        self.in_flight.as_ref()
    }

    /// The daily counter, for writing back to `meta` after a dispatch.
    pub fn counter(&self) -> (Option<NaiveDate>, i64) {
        (self.requests_day, self.requests_today)
    }

    fn roll_over(&mut self, today: NaiveDate) {
        if self.requests_day != Some(today) {
            self.requests_day = Some(today);
            self.requests_today = 0;
        }
    }
}

/// Whole seconds until `until`, rounded up, for countdowns.
fn secs_until(now: Instant, until: Instant) -> u64 {
    let d = until.saturating_duration_since(now);
    d.as_secs() + u64::from(d.subsec_nanos() > 0)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::api::{ApiError, Reviewed};
    use super::*;
    use chrono::NaiveDate;
    use std::time::{Duration, Instant};

    fn day(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    fn req(journal_id: i64) -> SyncRequest {
        SyncRequest {
            journal_id,
            sm_id: 1000 + journal_id,
            grade: 4,
            review_date: day("2026-09-20"),
        }
    }

    fn outbox(cap: i64) -> Outbox {
        Outbox::new(OutboxConfig {
            grace: Duration::from_secs(5),
            spacing: Duration::from_millis(1100),
            daily_cap: cap,
            requests_day: None,
            requests_today: 0,
        })
    }

    fn ok() -> Result<Reviewed, ApiError> {
        Ok(Reviewed { interval: 12 })
    }

    fn transient() -> Result<Reviewed, ApiError> {
        Err(ApiError::Transient {
            reason: "connection refused".to_string(),
        })
    }

    #[test]
    fn grace_period_holds_a_row_until_it_elapses() {
        let t0 = Instant::now();
        let today = day("2026-09-20");
        let mut ob = outbox(50);
        ob.enqueue(req(1), t0);
        assert_eq!(ob.poll(t0, today), None);
        assert_eq!(ob.poll(t0 + Duration::from_millis(4900), today), None);
        assert_eq!(ob.state(1, t0 + Duration::from_millis(2200)), RowState::Waiting { secs: 3 });
        assert_eq!(ob.poll(t0 + Duration::from_secs(5), today), Some(req(1)));
        assert_eq!(ob.state(1, t0 + Duration::from_secs(5)), RowState::InFlight);
    }

    #[test]
    fn one_in_flight_at_a_time_and_at_least_spacing_apart() {
        let t0 = Instant::now();
        let today = day("2026-09-20");
        let mut ob = outbox(50);
        ob.enqueue(req(1), t0);
        ob.enqueue(req(2), t0);
        let t1 = t0 + Duration::from_secs(5);
        assert_eq!(ob.poll(t1, today), Some(req(1)));
        assert_eq!(ob.poll(t1, today), None, "one in flight");
        assert!(matches!(ob.complete(1, ok(), t1), Landed::Synced { interval: 12, .. }));
        assert_eq!(ob.poll(t1 + Duration::from_millis(1000), today), None, "spacing");
        assert_eq!(ob.poll(t1 + Duration::from_millis(1100), today), Some(req(2)));
    }

    #[test]
    fn oldest_journal_row_goes_first_regardless_of_enqueue_order() {
        let t0 = Instant::now();
        let today = day("2026-09-20");
        let mut ob = outbox(50);
        ob.enqueue(req(9), t0);
        ob.enqueue(req(3), t0);
        ob.enqueue(req(5), t0);
        let t1 = t0 + Duration::from_secs(5);
        assert_eq!(ob.poll(t1, today).map(|r| r.journal_id), Some(3));
        ob.complete(3, ok(), t1);
        assert_eq!(ob.poll(t1 + Duration::from_secs(2), today).map(|r| r.journal_id), Some(5));
    }

    #[test]
    fn transient_failure_backs_off_exponentially_and_caps_at_sixty() {
        let t0 = Instant::now();
        let today = day("2026-09-20");
        let mut ob = outbox(50);
        ob.enqueue(req(1), t0);
        let mut t = t0 + Duration::from_secs(5);
        assert_eq!(ob.poll(t, today), Some(req(1)));
        let mut delays = Vec::new();
        for _ in 0..7 {
            match ob.complete(1, transient(), t) {
                Landed::Retry { after, reason } => {
                    assert_eq!(reason, "connection refused");
                    delays.push(after.as_secs());
                    assert_eq!(ob.poll(t + after - Duration::from_millis(1), today), None);
                    assert_eq!(
                        ob.state(1, t + Duration::from_secs(1)),
                        RowState::Retrying { reason: "connection refused".to_string(), secs: after.as_secs() - 1 }
                    );
                    t += after;
                    assert_eq!(ob.poll(t, today), Some(req(1)), "same row is retried");
                }
                other => panic!("expected retry, got {other:?}"),
            }
        }
        assert_eq!(delays, [2, 4, 8, 16, 32, 60, 60]);
        assert!(matches!(ob.complete(1, ok(), t), Landed::Synced { .. }));
        assert_eq!(ob.pending_count(), 0);
        assert_eq!(ob.state(1, t), RowState::Gone);
    }

    #[test]
    fn daily_cap_pauses_sending_until_the_local_date_changes() {
        let t0 = Instant::now();
        let mut ob = outbox(1);
        ob.enqueue(req(1), t0);
        ob.enqueue(req(2), t0);
        let t1 = t0 + Duration::from_secs(5);
        assert_eq!(ob.poll(t1, day("2026-09-20")), Some(req(1)));
        ob.complete(1, ok(), t1);
        let t2 = t1 + Duration::from_secs(2);
        assert_eq!(ob.poll(t2, day("2026-09-20")), None);
        assert_eq!(ob.state(2, t2), RowState::CapReached);
        assert_eq!(ob.counter(), (Some(day("2026-09-20")), 1));
        assert_eq!(ob.poll(t2, day("2026-09-21")), Some(req(2)));
        assert_eq!(ob.counter(), (Some(day("2026-09-21")), 1));
    }

    #[test]
    fn counter_from_a_previous_session_is_honoured_on_the_same_day_only() {
        let t0 = Instant::now();
        let mut ob = Outbox::new(OutboxConfig {
            grace: Duration::ZERO,
            spacing: Duration::ZERO,
            daily_cap: 50,
            requests_day: Some(day("2026-09-20")),
            requests_today: 50,
        });
        ob.enqueue(req(1), t0);
        assert_eq!(ob.poll(t0, day("2026-09-20")), None);
        assert_eq!(ob.poll(t0, day("2026-09-21")), Some(req(1)));
    }

    #[test]
    fn cancel_works_while_queued_and_is_refused_once_sent() {
        let t0 = Instant::now();
        let today = day("2026-09-20");
        let mut ob = outbox(50);
        ob.enqueue(req(1), t0);
        ob.enqueue(req(2), t0);
        assert_eq!(ob.pending_count(), 2);
        assert!(ob.cancel(2));
        assert_eq!(ob.pending_count(), 1);
        assert_eq!(ob.state(2, t0), RowState::Gone);
        let t1 = t0 + Duration::from_secs(5);
        ob.poll(t1, today);
        assert!(!ob.cancel(1), "in flight");
        ob.complete(1, ok(), t1);
        assert!(!ob.cancel(1), "synced");
    }

    #[test]
    fn auth_failure_stops_the_session_and_keeps_the_row() {
        let t0 = Instant::now();
        let today = day("2026-09-20");
        let mut ob = outbox(50);
        ob.enqueue(req(1), t0);
        ob.enqueue(req(2), t0);
        let t1 = t0 + Duration::from_secs(5);
        ob.poll(t1, today);
        let landed = ob.complete(
            1,
            Err(ApiError::Unauthorized {
                status: 401,
                message: "Invalid API key".to_string(),
            }),
            t1,
        );
        assert_eq!(landed, Landed::Stopped { reason: "401 unauthorized".to_string() });
        assert!(ob.is_stopped());
        assert_eq!(ob.poll(t1 + Duration::from_secs(100), today), None);
        assert_eq!(ob.pending_count(), 2);
        assert_eq!(ob.state(1, t1), RowState::Stopped { reason: "401 unauthorized".to_string() });
        assert_eq!(ob.state(2, t1), RowState::Stopped { reason: "401 unauthorized".to_string() });
    }

    #[test]
    fn rejected_row_is_skipped_for_the_session_but_still_counts_as_unsynced() {
        let t0 = Instant::now();
        let today = day("2026-09-20");
        let mut ob = outbox(50);
        ob.enqueue(req(1), t0);
        ob.enqueue(req(2), t0);
        let t1 = t0 + Duration::from_secs(5);
        ob.poll(t1, today);
        let landed = ob.complete(
            1,
            Err(ApiError::Rejected {
                message: "grade: must be ≤ 5".to_string(),
            }),
            t1,
        );
        assert_eq!(landed, Landed::Rejected { reason: "grade: must be ≤ 5".to_string() });
        assert_eq!(ob.pending_count(), 2);
        assert_eq!(ob.state(1, t1), RowState::Rejected { reason: "grade: must be ≤ 5".to_string() });
        assert_eq!(ob.poll(t1 + Duration::from_secs(2), today), Some(req(2)), "moves on");
        assert!(ob.cancel(1), "a rejected row was never recorded server-side, so undo may remove it");
        assert_eq!(ob.pending_count(), 1);
    }

    #[test]
    fn flush_makes_every_queued_row_ready_now() {
        let t0 = Instant::now();
        let today = day("2026-09-20");
        let mut ob = outbox(50);
        ob.enqueue(req(1), t0);
        assert_eq!(ob.poll(t0, today), None);
        ob.flush(t0);
        assert_eq!(ob.poll(t0, today), Some(req(1)));
    }

    #[test]
    fn stale_result_for_an_unknown_row_is_ignored() {
        let t0 = Instant::now();
        let mut ob = outbox(50);
        assert_eq!(ob.complete(42, ok(), t0), Landed::Stale);
    }
}
