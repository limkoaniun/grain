# grain M1 — SuperMemo sync

Date: 2026-09-20
Status: draft, awaiting review

M1 makes the grades grain already journals reach the SuperMemo API, and brings the
returned interval back into the vault. After M1, grading a card in grain moves its
`due` date the way SuperMemo says it should. The app stays usable with no key, no
network, and no server, exactly as in M0.

## Goal

M0 journals every grade in `journal` with `synced = 0` and never touches `due` or
`interval`. M1 adds the step after the commit: send the grade, receive an interval,
record it, write `due` and `interval` back into the card file, and flip `synced`
to 1. Nothing about how a grade is journaled changes. The UI never waits on the
network.

## The API, as it actually is

Read from the OpenAPI document embedded in https://api.supermemo.com/documentation
on 2026-09-20. Version banner on the site: 2026.06.10.

| | |
|---|---|
| Base URL | `https://api.supermemo.com/algorithm-api` |
| Auth | `Authorization: Bearer <api-key>`; keys are per project |
| Identity | client-owned integer ids: `ext_learner_id` (int64), `ext_collection_id` (int64), `ext_item_id` (int32) |
| Item creation | none; an item exists once it has a review |
| Rate limit | 1 request/second on every tier |
| Free tier | €0, 300 repetitions/month, **50 requests/day**, 1 user |
| Regular | €4.90/month, 3,000 repetitions/month, 1,000 requests/day |
| Errors | `422` → `{code, message, errors: [{name, code, message}]}`; other → `{code, message}` |

The one endpoint M1 needs:

```
POST /algorithm/review
{
  "ext_learner_id":    1,
  "ext_collection_id": 1,
  "ext_item_id":       1042,        // sm_id
  "algorithm_type":    "SM20",
  "grade":             4,           // 0..5, same scale as grain
  "forgetting_index":  10,          // optional, 1..99, server default 10
  "review_date":       "2026-09-20" // YYYY-MM-DD
}
→ 200 { "interval": 12 }            // days until the next review
```

Endpoints M1 uses only for diagnostics or leaves for later:

| Endpoint | Use |
|---|---|
| `GET /auth/me` → `{id, name, project_id, api_key_id}` | `grain --auth-check` |
| `GET /learner/{L}/collection/{C}/next-review?reviewDate=` → `[item_id]` | later: reconcile local `due` against the server |
| `GET /learner/{L}/collection/{C}/item/{I}/review` → paginated `[{item_id, interval, used_interval, grade, review_date, is_first, created_date}]` | later: history and stats screen |
| `POST /algorithm/sm20/weights-optimize` `{ext_learner_id, ext_collection_id}` → 204 | later: per-learner tuning after enough history |
| `POST /algorithm/sm20/import` multipart CSV `itemId,grade,reviewDate` sorted by date, plus `GET .../import/status` | later: seed the server from an existing journal |

There is no endpoint to delete or amend a review. Once a grade is accepted by the
server it is permanent. That single fact drives the undo design below.

## Decisions

Settled while writing this spec; recorded so they are not relitigated. Items marked
**ask** are the ones the user should confirm before `/feature start`.

| Decision | Choice |
|---|---|
| Journal first | Unchanged from M0: insert + commit, then the UI advances. Sync is a separate step over rows with `synced = 0`. |
| Schema | **No change.** `journal.synced` and `journal.interval_returned` already exist. New `meta` keys only. |
| File format | **No change.** `due` and `interval` are already defined keys; M1 starts writing them. |
| Who writes | All SQLite writes and all vault-file writes stay on the UI thread. The worker thread only does HTTP. |
| HTTP client | **ask** — `ureq` 3 with rustls (blocking, no tokio, small) rather than `reqwest`. reqwest's blocking mode spins up a tokio runtime internally, which the M0 brief listed as deliberately absent. |
| Undo vs. server | A grade can be undone only while it is unsent. Sending is delayed by a grace period so undo has its window. Once sent, undo is refused with a reason. |
| Grace period | 5 seconds after grading, or immediately on quit. `meta.sync_grace_secs` overrides. |
| Identity | `ext_learner_id` and `ext_collection_id` come from `meta` (`sm_learner_id`, `sm_collection_id`), both default `1`. `ext_item_id` is `sm_id`. |
| API key | **ask** — environment variable `GRAIN_SM_API_KEY`. Not in the vault, not in `.grain/`, not in a config file (config is a later milestone). Absent key = offline mode, said plainly in the status row. |
| Due date | `due = review_date + interval` days, where `review_date` is the local date of `graded_at`. Written to `items.due`, `items.interval`, and the card's frontmatter. |
| Daily cap | Track requests per local day in `meta.sync_requests_today`/`meta.sync_requests_day`. Stop sending at `meta.sync_daily_cap` (default 50, the Free tier). Pending grades wait for tomorrow. |
| Rate limit | One request in flight at a time, at least 1,100 ms apart. |
| Ordering | Pending rows are sent oldest first (`journal.id ASC`). Two grades of one card on one day both go, in order; the server sees two reviews. |
| Startup | Pending rows from earlier sessions are sent after the grace period, subject to the daily cap. |
| Failure | Network errors and 5xx: exponential backoff (2 s, 4 s, … 60 s), row stays pending. 401/403: stop syncing for the session, show it. 422: the row is marked failed in memory, skipped for the session, logged; it stays `synced = 0` on disk so a fix can resend it. |

## Architecture

```
UI thread (main)                          sync worker thread
────────────────                          ──────────────────
grade → journal insert, commit
      → status "graded 4 · journaled"
      → schedule: due_at = now + grace
tick (100 ms):
  drain results ◄──────────────────────── SyncResult { journal_id, Ok(interval) | Err }
  apply: journal.synced=1, interval_returned
         items.due/interval
         frontmatter due/interval → file
         items.mtime = new mtime
         status "graded 4 · synced · due 2026-10-02"
  dispatch: rows past grace, none in flight,
            under daily cap
       ────────────────────────────────►  SyncRequest { journal_id, sm_id, grade, review_date }
                                          sleep to honour 1 req/s
                                          POST /algorithm/review
                                          reply
```

- `src/sync/mod.rs` — `Outbox` on the UI thread: knows pending rows, grace
  deadlines, in-flight id, backoff state, daily counter. Pure logic over a clock
  it is given, so it is unit-testable without threads.
- `src/sync/api.rs` — the wire layer. `trait Scheduler { fn review(&self,
  ReviewRequest) -> Result<Reviewed, ApiError>; fn whoami(&self) -> Result<Whoami,
  ApiError>; }`. One impl over ureq; one fake for tests. Request and response
  structs derive serde with the exact field names above.
- `src/sync/worker.rs` — spawns a `std::thread`, owns a `Box<dyn Scheduler>`,
  loops on an `mpsc::Receiver<SyncRequest>`, replies on
  `mpsc::Sender<SyncResult>`. No database, no filesystem.
- `src/app.rs` — gains `tick()`, called by the event loop every 100 ms. Applies
  results, asks the outbox what to dispatch, updates status strings.
- `src/main.rs` — event loop switches from blocking `event::read` to
  `event::poll(100 ms)` then `read`, with `app.tick()` on every iteration.
  `--auth-check` calls `/auth/me` and prints the result without opening the TUI.
- `src/db.rs` — new queries: `pending_grades()`, `mark_synced(journal_id,
  interval)`, `set_schedule(sm_id, due, interval, mtime)`, `meta` helpers for the
  new keys. No schema change; `schema_version` stays 1.
- `src/vault/index.rs` — gains `write_schedule(root, path, due, interval)`,
  the second sanctioned vault write, through `Document::serialize` so unknown
  keys survive. Returns the new mtime.

The `Scheduler` trait is the seam for M2+ (weights optimize, history, next-review)
and for tests. Nothing outside `src/sync/` names ureq.

## Behaviour

### Grading
Unchanged: `0`–`5` after reveal inserts the journal row and advances. The status
line reads `graded 4 · journaled` and, when a key is configured,
`· sync in 5s` counting down.

### Sync states, as shown in the review status line

| State | Status line |
|---|---|
| no key | `graded 4 · journaled (offline)` |
| waiting for grace | `graded 4 · journaled · sync in 3s` |
| in flight | `graded 4 · syncing…` |
| synced | `graded 4 · synced · interval 12 · due 2026-10-02` |
| retrying | `graded 4 · sync failed (connection refused) · retry in 8s` |
| daily cap | `graded 4 · journaled · daily cap reached, resumes tomorrow` |
| auth failure | `sync stopped · 401 unauthorized · check GRAIN_SM_API_KEY` |
| rejected | `graded 4 · rejected by API (grade: must be ≤ 5) · kept unsynced` |

The status row (top right) gains a pending count when non-zero:
`card · prio 28 · due 12 · done 3/12 · 2 unsynced`. The queue screen shows the
same suffix: `queue · sort prio · 2 unsynced`.

### Undo
`u` on a grade that is still waiting (grace not elapsed, not in flight) works as
in M0 and cancels its dispatch. `u` on a grade that is in flight or synced is
refused: `cannot undo · already sent to SuperMemo`. The review history stack is
unchanged; the refusal leaves it in place so a later `u` targets the same row and
gives the same answer.

### After a sync lands
The card's `due` moves forward, so it stops being due. The review session's list
was fixed at startup and the card is already behind `pos`, so nothing on screen
jumps. On the next startup the queue reflects the new dates.

### Quit
`q` with rows in flight: wait up to 2 seconds for the reply, apply it, then exit.
Rows still waiting for grace are sent immediately before exit if under the cap,
one at a time, up to a 5-second total budget. Anything left stays pending for the
next session. The terminal is restored before any of this waits, and a line on
stderr says what was left: `grain: 2 grades pending sync`.

### `grain --auth-check`
Reads the key, calls `/auth/me`, prints `project <project_id> · key <api_key_id> ·
learner <sm_learner_id> · collection <sm_collection_id>` and exits 0, or the
error and exits 1. No TUI. This is how the user proves the key works before
trusting the status line.

### Offline mode
No `GRAIN_SM_API_KEY`: no worker thread is spawned, every status line says
`(offline)`, nothing else differs from M0. Setting the key later and restarting
sends the backlog.

## Data

### Journal row lifecycle
```
insert  synced=0, interval_returned=NULL        (M0, unchanged)
sent    in-memory only: in_flight = Some(id)
landed  synced=1, interval_returned=<interval>  one UPDATE, committed
```
`synced = 1` rows are never deleted (M0 guard, now load-bearing).

### New `meta` keys
| key | default | meaning |
|---|---|---|
| `sm_learner_id` | `1` | `ext_learner_id` |
| `sm_collection_id` | `1` | `ext_collection_id` (the `all` collection) |
| `sync_grace_secs` | `5` | delay before a grade is dispatched |
| `sync_daily_cap` | `50` | requests per local day before pausing |
| `sync_requests_day` | — | local date the counter belongs to |
| `sync_requests_today` | `0` | requests made on that day |
| `forgetting_index` | unset | sent as `forgetting_index` when set; otherwise omitted |

### Card file after a sync
```markdown
---
type: card
sm_id: 1042
due: 2026-10-02
interval: 12
prio: 28
source: "[[citrus-vocab]]"
range: 2210-2380
---
```
Keys keep their existing position; `due` and `interval` are inserted after
`sm_id` when absent. Unknown keys untouched. The index row's `mtime` is set to
the file's new mtime in the same transaction as `items.due`, so the next refresh
skips the file.

## Testing

- `Outbox` unit tests with a fake clock: grace timing, one in flight at a time,
  1.1 s spacing, backoff sequence, daily cap rollover at local midnight, oldest
  first, cancel on undo, refuse undo when in flight.
- `Scheduler` fake in `app.rs` tests: grade → tick past grace → result applied →
  journal `synced = 1` with interval, `items.due` = review_date + interval, file
  rewritten with `due`/`interval` and unknown keys preserved, `mtime` updated,
  refresh afterwards reports the file unchanged.
- Undo tests: before dispatch works and removes the row; after dispatch refused
  and the row survives; status strings exact.
- Wire test: the ureq client against a `std::net::TcpListener` stub in the test
  that asserts the request line, the `Authorization` header, the exact JSON body
  field names, and replies `{"interval": 12}`; a second case replies 422 with a
  `ValidationErrorResponse` and checks the error surfaces the field name. No
  extra test dependencies.
- Quit test: pending rows and an in-flight row, `q`, assert the reply is applied
  and the rest stay pending.
- UI tests: every status line variant renders; the `unsynced` suffix appears and
  disappears.
- Manual: `grain --auth-check` against the real API with a Free key; grade one
  fixture card; watch the countdown, the `synced` line, and the rewritten file;
  restore fixtures afterwards.

## Out of scope for M1

- Reconciling local `due` with `next-review`. The local computation is the
  truth until a later milestone adds a reconcile command.
- `weights-optimize`, history views, CSV import of the journal.
- A config file. Key by environment variable, ids by `meta`.
- Retrying 422 rows automatically. They wait for a human.
- Multiple learners or collections. One of each, ids default to 1.
- Any change to what is due today: M1 only moves cards *out* of today after a
  sync; nothing pulls cards in.

## Accepted limitations

- A crash between "sent" and "landed" resends the row on next start, and the
  server records the review twice. The grace period and the 2-second quit wait
  make this rare; it cannot be made impossible without an idempotency key the
  API does not offer.
- The Free tier's 50 requests a day means at most 50 grades sync per day. The
  rest queue. The status row's `unsynced` count is the signal to upgrade or wait.
- `review_date` is a date, not a time. Two grades of one card on the same day
  are two reviews on that date; SM-20 handles that server-side.
- `ext_item_id` is int32. `sm_id` values above 2,147,483,647 cannot sync; the
  allocator never produces them, but an imported vault could.

## Open questions

1. **ureq or reqwest?** The spec assumes ureq. Say the word if reqwest is
   preferred; the trait boundary makes it a one-file swap.
2. **Key location.** Environment variable is the assumption. A keychain or a
   file under `~/.config/grain/` are the alternatives; both fit later.
3. **Grace period length.** 5 s is a guess. Long enough to catch a slip of the
   finger, short enough that quitting rarely waits.
4. **Should `--auth-check` also print the server's `next-review` list for
   today?** Cheap to add, useful for trust, one more request against the cap.

## Acceptance checklist

- `cargo run -- --vault fixtures/vault` with no key behaves exactly as M0 and says
  `(offline)`.
- `grain --auth-check` with a valid key prints the project and exits 0; with a
  bad key prints the 401 and exits 1.
- Grade a card with a key set: status counts down, flips to `syncing…`, then
  `synced · interval N · due D`; `sqlite3` shows `synced = 1` and
  `interval_returned = N`; the card file has `due: D` and `interval: N` with every
  other key intact; a rerun reports the file unchanged.
- `u` within the grace period removes the row and cancels the send; `u` after
  the send is refused with the reason.
- Kill the network: grades stay `synced = 0`, status shows the retry countdown,
  restoring the network drains them oldest first with ≥1 s spacing.
- Set `sync_daily_cap` to 1: the second grade waits with the cap message.
- `cargo test` passes; `cargo clippy --all-targets` is clean; no new
  dependency besides ureq (and its rustls feature).
