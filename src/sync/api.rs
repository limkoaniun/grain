//! Wire layer for the SuperMemo algorithm API (`/algorithm-api`, SM-20).
//!
//! The [`Scheduler`] trait is the seam: one implementation over ureq, one fake
//! in tests. Nothing outside `sync` names ureq.

use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};

pub const DEFAULT_BASE_URL: &str = "https://api.supermemo.com/algorithm-api";
/// Environment variable holding the per-project API key. Absent means offline.
pub const API_KEY_ENV: &str = "GRAIN_SM_API_KEY";
const ALGORITHM: &str = "SM20";
const TIMEOUT: Duration = Duration::from_secs(20);

/// Client-owned ids the API keys reviews by. From `meta` (`sm_learner_id`,
/// `sm_collection_id`, `forgetting_index`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Identity {
    pub learner_id: i64,
    pub collection_id: i64,
    pub forgetting_index: Option<u8>,
}

/// Body of `POST /algorithm/review`. Field names are the wire names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReviewRequest {
    pub ext_learner_id: i64,
    pub ext_collection_id: i64,
    pub ext_item_id: i32,
    pub algorithm_type: &'static str,
    pub grade: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forgetting_index: Option<u8>,
    /// `YYYY-MM-DD`, the local date the grade was given.
    pub review_date: String,
}

impl ReviewRequest {
    /// Build a request; fails as [`ApiError::Rejected`] when `sm_id` does not fit `ext_item_id` (int32).
    pub fn new(identity: &Identity, sm_id: i64, grade: u8, review_date: &str) -> Result<Self, ApiError> {
        let ext_item_id = i32::try_from(sm_id).map_err(|_| ApiError::Rejected {
            message: format!("sm_id {sm_id} does not fit ext_item_id (int32)"),
        })?;
        Ok(ReviewRequest {
            ext_learner_id: identity.learner_id,
            ext_collection_id: identity.collection_id,
            ext_item_id,
            algorithm_type: ALGORITHM,
            grade,
            forgetting_index: identity.forgetting_index,
            review_date: review_date.to_string(),
        })
    }
}

/// `200` body of `POST /algorithm/review`: days until the next review.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct Reviewed {
    pub interval: i64,
}

/// An id the server may send as a number or a string.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum Id {
    Int(i64),
    Str(String),
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Id::Int(n) => write!(f, "{n}"),
            Id::Str(s) => f.write_str(s),
        }
    }
}

/// `200` body of `GET /auth/me`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Whoami {
    /// Not printed; optional so an unexpected shape cannot fail `--auth-check` for nothing.
    #[serde(default)]
    pub id: Option<Id>,
    #[serde(default)]
    pub name: Option<String>,
    pub project_id: Id,
    pub api_key_id: Id,
}

/// What can go wrong on the wire, classified by what the outbox should do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiError {
    /// 401 or 403. The key is wrong; sync stops for the session.
    Unauthorized { status: u16, message: String },
    /// 422 or another client-side rejection. The row is kept unsynced and skipped this session.
    Rejected { message: String },
    /// 5xx or 429: the server saw the request and could not take it. Retried with backoff.
    Transient { reason: String },
    /// Transport failure: the request never reached the server. Retried with backoff,
    /// and not counted against the daily request budget.
    Unreachable { reason: String },
}

impl ApiError {
    /// Short text for the status line: `401 unauthorized`, `connection refused`, `grade: must be ≤ 5`.
    pub fn summary(&self) -> String {
        match self {
            ApiError::Unauthorized { status, .. } => format!("{status} {}", reason_phrase(*status)),
            ApiError::Rejected { message } => message.clone(),
            ApiError::Transient { reason } | ApiError::Unreachable { reason } => reason.clone(),
        }
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApiError::Unauthorized { status, message } => write!(f, "{status} {}: {message}", reason_phrase(*status)),
            ApiError::Rejected { message } => write!(f, "rejected by API: {message}"),
            ApiError::Transient { reason } | ApiError::Unreachable { reason } => write!(f, "sync failed: {reason}"),
        }
    }
}

impl std::error::Error for ApiError {}

/// The scheduling service as grain sees it. `Send` so the worker thread can own it.
pub trait Scheduler: Send {
    fn review(&self, req: &ReviewRequest) -> Result<Reviewed, ApiError>;
    fn whoami(&self) -> Result<Whoami, ApiError>;
}

/// `{code, message, errors: [{name, code, message}]}` on 422; `{code, message}` otherwise.
#[derive(Debug, Deserialize)]
struct ErrorBody {
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    errors: Vec<FieldError>,
}

#[derive(Debug, Deserialize)]
struct FieldError {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

/// Blocking client over ureq (rustls). One agent, one bearer key.
pub struct UreqScheduler {
    agent: ureq::Agent,
    base_url: String,
    auth: String,
}

impl UreqScheduler {
    pub fn new(api_key: &str) -> Self {
        Self::with_base_url(api_key, DEFAULT_BASE_URL)
    }

    pub fn with_base_url(api_key: &str, base_url: &str) -> Self {
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(TIMEOUT))
            .user_agent(concat!("grain/", env!("CARGO_PKG_VERSION")))
            .build();
        UreqScheduler {
            agent: ureq::Agent::new_with_config(config),
            base_url: base_url.trim_end_matches('/').to_string(),
            auth: format!("Bearer {api_key}"),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    /// Run one request and turn the response into `T` or an [`ApiError`].
    fn handle<T: serde::de::DeserializeOwned>(
        &self,
        result: Result<ureq::http::Response<ureq::Body>, ureq::Error>,
    ) -> Result<T, ApiError> {
        let mut response = result.map_err(transport_error)?;
        let status = response.status().as_u16();
        if response.status().is_success() {
            return response.body_mut().read_json::<T>().map_err(|e| ApiError::Rejected {
                message: format!("unreadable {status} response: {e}"),
            });
        }
        let body: Option<ErrorBody> = response.body_mut().read_json().ok();
        let message = body
            .as_ref()
            .and_then(|b| b.message.clone())
            .unwrap_or_else(|| reason_phrase(status));
        Err(match status {
            401 | 403 => ApiError::Unauthorized { status, message },
            422 => ApiError::Rejected {
                message: body
                    .as_ref()
                    .and_then(|b| b.errors.first())
                    .map(|e| match (&e.name, &e.message) {
                        (Some(n), Some(m)) => format!("{n}: {m}"),
                        (Some(n), None) => n.clone(),
                        (None, Some(m)) => m.clone(),
                        (None, None) => message.clone(),
                    })
                    .unwrap_or(message),
            },
            429 | 500..=599 => ApiError::Transient {
                reason: format!("{status} {}", reason_phrase(status)),
            },
            _ => ApiError::Rejected {
                message: format!("{status} {message}"),
            },
        })
    }
}

impl Scheduler for UreqScheduler {
    fn review(&self, req: &ReviewRequest) -> Result<Reviewed, ApiError> {
        let result = self
            .agent
            .post(self.url("/algorithm/review"))
            .header("Authorization", &self.auth)
            .header("Accept", "application/json")
            .send_json(req);
        self.handle(result)
    }

    fn whoami(&self) -> Result<Whoami, ApiError> {
        let result = self
            .agent
            .get(self.url("/auth/me"))
            .header("Authorization", &self.auth)
            .header("Accept", "application/json")
            .call();
        self.handle(result)
    }
}

/// Short, one-line reason for a transport failure. Shared by `sync` and `import`
/// so both status lines read the same for the same underlying error.
pub fn transport_reason(e: &ureq::Error) -> String {
    match e {
        ureq::Error::Io(io) => match io.kind() {
            std::io::ErrorKind::ConnectionRefused => "connection refused".to_string(),
            std::io::ErrorKind::ConnectionReset => "connection reset".to_string(),
            std::io::ErrorKind::TimedOut => "timed out".to_string(),
            _ => {
                let message = io.to_string();
                // ureq 3.4.2's resolver turns a getaddrinfo failure into an
                // uncategorised `io::Error` (resolver.rs `addr.to_socket_addrs()?`),
                // with a stable message prefix and a platform-specific tail
                // (macOS: "nodename nor servname provided, or not known";
                // glibc: "Name or service not known"). Match the prefix only.
                if message.starts_with("failed to lookup address information") {
                    "host not found".to_string()
                } else {
                    message
                }
            }
        },
        ureq::Error::Timeout(_) => "timed out".to_string(),
        ureq::Error::HostNotFound => "host not found".to_string(),
        ureq::Error::ConnectionFailed => "connection failed".to_string(),
        other => other.to_string(),
    }
}

fn transport_error(e: ureq::Error) -> ApiError {
    ApiError::Unreachable {
        reason: transport_reason(&e),
    }
}

fn reason_phrase(status: u16) -> String {
    ureq::http::StatusCode::from_u16(status)
        .ok()
        .and_then(|s| s.canonical_reason())
        .map(str::to_ascii_lowercase)
        .unwrap_or_else(|| "error".to_string())
}

/// A scripted stand-in for the API, shared by the app and ui tests.
/// Outcomes are consumed in order; when the script runs out, every review returns `interval 12`.
/// `gate` blocks each review until released, to hold a request in flight.
#[cfg(test)]
pub struct FakeScheduler {
    pub script: std::sync::Mutex<std::collections::VecDeque<Result<Reviewed, ApiError>>>,
    pub gate: Option<std::sync::mpsc::Receiver<()>>,
    pub seen: std::sync::Arc<std::sync::Mutex<Vec<ReviewRequest>>>,
    pub panic: bool,
}

#[cfg(test)]
impl FakeScheduler {
    pub fn new(script: Vec<Result<Reviewed, ApiError>>) -> Self {
        FakeScheduler {
            script: std::sync::Mutex::new(script.into()),
            gate: None,
            seen: std::sync::Arc::default(),
            panic: false,
        }
    }

    pub fn always_ok() -> Self {
        Self::new(Vec::new())
    }

    /// Panics on the first review, taking the worker thread down with it.
    pub fn panicking() -> Self {
        let mut fake = Self::always_ok();
        fake.panic = true;
        fake
    }

    /// Hold every review until a `()` arrives on the returned sender.
    pub fn gated() -> (Self, std::sync::mpsc::Sender<()>) {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut fake = Self::always_ok();
        fake.gate = Some(rx);
        (fake, tx)
    }
}

#[cfg(test)]
impl Scheduler for FakeScheduler {
    fn review(&self, req: &ReviewRequest) -> Result<Reviewed, ApiError> {
        assert!(!self.panic, "FakeScheduler::panicking: simulated worker crash");
        if let Some(gate) = &self.gate {
            let _ = gate.recv();
        }
        if let Ok(mut seen) = self.seen.lock() {
            seen.push(req.clone());
        }
        self.script
            .lock()
            .ok()
            .and_then(|mut s| s.pop_front())
            .unwrap_or(Ok(Reviewed { interval: 12 }))
    }

    fn whoami(&self) -> Result<Whoami, ApiError> {
        Ok(Whoami {
            id: Some(Id::Int(1)),
            name: Some("fake".to_string()),
            project_id: Id::Int(9),
            api_key_id: Id::Int(77),
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;

    /// One-shot HTTP/1.1 stub: accepts one connection, hands the raw request back, replies `response`.
    fn stub(response: &'static str) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut raw = Vec::new();
            let mut buf = [0u8; 1024];
            loop {
                let n = sock.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                raw.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&raw).to_string();
                if let Some(head_end) = text.find("\r\n\r\n") {
                    let head = &text[..head_end].to_ascii_lowercase();
                    let len: usize = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .map(|v| v.trim().parse().unwrap())
                        .unwrap_or(0);
                    if raw.len() >= head_end + 4 + len {
                        break;
                    }
                }
            }
            tx.send(String::from_utf8_lossy(&raw).to_string()).unwrap();
            sock.write_all(response.as_bytes()).unwrap();
            sock.flush().unwrap();
        });
        // Same shape as production: the base URL carries the `/algorithm-api` prefix.
        (format!("http://{addr}/algorithm-api"), rx)
    }

    /// The request body with ureq's pretty-printing whitespace removed (no value here contains a space).
    fn body_of(raw: &str) -> String {
        raw[raw.find("\r\n\r\n").unwrap() + 4..]
            .split_whitespace()
            .collect()
    }

    fn http(status: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn ids() -> Identity {
        Identity {
            learner_id: 1,
            collection_id: 1,
            forgetting_index: None,
        }
    }

    fn review_req() -> ReviewRequest {
        ReviewRequest::new(&ids(), 1042, 4, "2026-09-20").unwrap()
    }

    #[test]
    fn review_posts_exact_json_with_bearer_key_and_reads_interval() {
        let response: &'static str = Box::leak(http("200 OK", r#"{"interval":12}"#).into_boxed_str());
        let (base, seen) = stub(response);
        let client = UreqScheduler::with_base_url("secret-key", &base);
        let reviewed = client.review(&review_req()).unwrap();
        assert_eq!(reviewed, Reviewed { interval: 12 });

        let raw = seen.recv().unwrap();
        assert!(raw.starts_with("POST /algorithm-api/algorithm/review HTTP/1.1\r\n"), "{raw}");
        let lower = raw.to_ascii_lowercase();
        assert!(lower.contains("\r\nauthorization: bearer secret-key\r\n"), "{raw}");
        assert!(lower.contains("\r\ncontent-type: application/json"), "{raw}");
        assert_eq!(
            body_of(&raw),
            r#"{"ext_learner_id":1,"ext_collection_id":1,"ext_item_id":1042,"algorithm_type":"SM20","grade":4,"review_date":"2026-09-20"}"#
        );
    }

    #[test]
    fn forgetting_index_is_sent_only_when_set() {
        let response: &'static str = Box::leak(http("200 OK", r#"{"interval":1}"#).into_boxed_str());
        let (base, seen) = stub(response);
        let mut identity = ids();
        identity.forgetting_index = Some(15);
        let req = ReviewRequest::new(&identity, 7, 5, "2026-09-20").unwrap();
        UreqScheduler::with_base_url("k", &base).review(&req).unwrap();
        let raw = seen.recv().unwrap();
        assert_eq!(
            body_of(&raw),
            r#"{"ext_learner_id":1,"ext_collection_id":1,"ext_item_id":7,"algorithm_type":"SM20","grade":5,"forgetting_index":15,"review_date":"2026-09-20"}"#
        );
    }

    #[test]
    fn sm_id_beyond_int32_is_rejected_before_sending() {
        let err = ReviewRequest::new(&ids(), i64::from(i32::MAX) + 1, 4, "2026-09-20").unwrap_err();
        assert!(matches!(err, ApiError::Rejected { .. }), "{err:?}");
        assert!(err.summary().contains("int32"), "{err}");
    }

    #[test]
    fn validation_error_surfaces_the_field_name() {
        let response: &'static str = Box::leak(
            http(
                "422 Unprocessable Entity",
                r#"{"code":422,"message":"Validation failed","errors":[{"name":"grade","code":"max","message":"must be ≤ 5"}]}"#,
            )
            .into_boxed_str(),
        );
        let (base, _seen) = stub(response);
        let client = UreqScheduler::with_base_url("k", &base);
        let err = client.review(&review_req()).unwrap_err();
        assert_eq!(
            err,
            ApiError::Rejected {
                message: "grade: must be ≤ 5".to_string()
            }
        );
        assert_eq!(err.summary(), "grade: must be ≤ 5");
    }

    #[test]
    fn unauthorized_is_terminal_and_names_the_status() {
        let response: &'static str =
            Box::leak(http("401 Unauthorized", r#"{"code":401,"message":"Invalid API key"}"#).into_boxed_str());
        let (base, _seen) = stub(response);
        let client = UreqScheduler::with_base_url("bad", &base);
        let err = client.review(&review_req()).unwrap_err();
        assert!(matches!(err, ApiError::Unauthorized { status: 401, .. }), "{err:?}");
        assert_eq!(err.summary(), "401 unauthorized");
    }

    #[test]
    fn server_errors_and_connection_failures_are_retryable() {
        let response: &'static str =
            Box::leak(http("503 Service Unavailable", r#"{"code":503,"message":"try later"}"#).into_boxed_str());
        let (base, _seen) = stub(response);
        let client = UreqScheduler::with_base_url("k", &base);
        let err = client.review(&review_req()).unwrap_err();
        assert!(matches!(err, ApiError::Transient { .. }), "{err:?}");
        assert_eq!(err.summary(), "503 service unavailable");

        // Bind then drop, so the port is closed and the connection is refused.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let client = UreqScheduler::with_base_url("k", &base);
        let err = client.review(&review_req()).unwrap_err();
        assert!(matches!(err, ApiError::Unreachable { .. }), "never reached the server: {err:?}");
        assert_eq!(err.summary(), "connection refused");
    }

    #[test]
    fn transport_reason_is_short_for_known_variants() {
        assert_eq!(transport_reason(&ureq::Error::HostNotFound), "host not found");
        assert_eq!(transport_reason(&ureq::Error::ConnectionFailed), "connection failed");
        assert_eq!(
            transport_reason(&ureq::Error::Io(std::io::Error::from(std::io::ErrorKind::ConnectionRefused))),
            "connection refused"
        );
        assert_eq!(
            transport_reason(&ureq::Error::Io(std::io::Error::from(std::io::ErrorKind::TimedOut))),
            "timed out"
        );
        let other = transport_reason(&ureq::Error::Io(std::io::Error::other("boom")));
        assert!(other.contains("boom"), "{other}");
    }

    #[test]
    fn transport_reason_recognizes_dns_lookup_failures_as_host_not_found() {
        // macOS tail.
        assert_eq!(
            transport_reason(&ureq::Error::Io(std::io::Error::other(
                "failed to lookup address information: nodename nor servname provided, or not known"
            ))),
            "host not found"
        );
        // glibc tail.
        assert_eq!(
            transport_reason(&ureq::Error::Io(std::io::Error::other(
                "failed to lookup address information: Name or service not known"
            ))),
            "host not found"
        );
    }

    #[test]
    fn whoami_gets_auth_me_and_accepts_numeric_or_string_ids() {
        let response: &'static str = Box::leak(
            http(
                "200 OK",
                r#"{"id":5,"name":"Koan","project_id":"proj-9","api_key_id":77}"#,
            )
            .into_boxed_str(),
        );
        let (base, seen) = stub(response);
        let client = UreqScheduler::with_base_url("k", &base);
        let me = client.whoami().unwrap();
        assert_eq!(me.project_id.to_string(), "proj-9");
        assert_eq!(me.api_key_id.to_string(), "77");
        assert_eq!(me.name.as_deref(), Some("Koan"));
        let raw = seen.recv().unwrap();
        assert!(raw.starts_with("GET /algorithm-api/auth/me HTTP/1.1\r\n"), "{raw}");
    }
}
