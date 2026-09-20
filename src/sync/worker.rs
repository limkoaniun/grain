//! The sync worker: one `std::thread` that owns the [`Scheduler`] and does HTTP,
//! and nothing else. Requests come in on a channel, results go back on another.
//! No database, no filesystem.

use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use anyhow::{Context, Result};

use crate::sync::api::{Identity, ReviewRequest, Scheduler};
use crate::sync::{SyncRequest, SyncResult};

/// The UI thread's end of the worker. Dropping it ends the thread.
pub struct Worker {
    tx: Sender<SyncRequest>,
    rx: Receiver<SyncResult>,
}

impl Worker {
    /// Hand a request to the thread. `false` if the thread has gone away.
    pub fn send(&self, req: SyncRequest) -> bool {
        self.tx.send(req).is_ok()
    }

    /// A finished result, if one is waiting.
    pub fn try_recv(&self) -> Option<SyncResult> {
        self.rx.try_recv().ok()
    }

    /// Wait up to `timeout` for a result.
    pub fn recv_timeout(&self, timeout: Duration) -> Option<SyncResult> {
        self.rx.recv_timeout(timeout).ok()
    }
}

/// Start the worker thread. It exits when the returned [`Worker`] is dropped.
pub fn spawn(scheduler: Box<dyn Scheduler>, identity: Identity) -> Result<Worker> {
    let (req_tx, req_rx) = mpsc::channel::<SyncRequest>();
    let (res_tx, res_rx) = mpsc::channel::<SyncResult>();
    std::thread::Builder::new()
        .name("grain-sync".to_string())
        .spawn(move || {
            for req in req_rx {
                let outcome = ReviewRequest::new(&identity, req.sm_id, req.grade, &req.review_date.to_string())
                    .and_then(|r| scheduler.review(&r));
                let result = SyncResult {
                    journal_id: req.journal_id,
                    outcome,
                };
                if res_tx.send(result).is_err() {
                    break;
                }
            }
        })
        .context("starting the sync worker thread")?;
    Ok(Worker { tx: req_tx, rx: res_rx })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::sync::api::{ApiError, Identity, ReviewRequest, Reviewed, Whoami};
    use crate::sync::SyncRequest;
    use chrono::NaiveDate;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    struct Fake {
        seen: Arc<Mutex<Vec<ReviewRequest>>>,
    }

    impl Scheduler for Fake {
        fn review(&self, req: &ReviewRequest) -> Result<Reviewed, ApiError> {
            self.seen.lock().unwrap().push(req.clone());
            Ok(Reviewed {
                interval: i64::from(req.grade) * 3,
            })
        }

        fn whoami(&self) -> Result<Whoami, ApiError> {
            unreachable!("the worker never calls whoami")
        }
    }

    fn identity() -> Identity {
        Identity {
            learner_id: 2,
            collection_id: 3,
            forgetting_index: None,
        }
    }

    #[test]
    fn worker_builds_the_request_from_identity_and_replies_on_the_channel() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let worker = spawn(Box::new(Fake { seen: seen.clone() }), identity()).unwrap();
        let req = SyncRequest {
            journal_id: 9,
            sm_id: 1042,
            grade: 4,
            review_date: NaiveDate::from_ymd_opt(2026, 9, 20).unwrap(),
        };
        assert!(worker.send(req));
        let result = worker.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(result.journal_id, 9);
        assert_eq!(result.outcome, Ok(Reviewed { interval: 12 }));
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].ext_learner_id, 2);
        assert_eq!(seen[0].ext_collection_id, 3);
        assert_eq!(seen[0].ext_item_id, 1042);
        assert_eq!(seen[0].review_date, "2026-09-20");
        assert_eq!(seen[0].algorithm_type, "SM20");
    }

    #[test]
    fn unsendable_sm_id_comes_back_rejected_without_a_call() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let worker = spawn(Box::new(Fake { seen: seen.clone() }), identity()).unwrap();
        worker.send(SyncRequest {
            journal_id: 1,
            sm_id: i64::from(i32::MAX) + 1,
            grade: 4,
            review_date: NaiveDate::from_ymd_opt(2026, 9, 20).unwrap(),
        });
        let result = worker.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(matches!(result.outcome, Err(ApiError::Rejected { .. })), "{:?}", result.outcome);
        assert!(seen.lock().unwrap().is_empty());
        assert!(worker.try_recv().is_none());
    }
}
