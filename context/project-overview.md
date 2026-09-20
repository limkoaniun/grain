# grain — Project Overview

> **Review what I know from the terminal, with the notes staying plain markdown in my Obsidian vault.**

| | |
|---|---|
| **Type** | Local single-user terminal app. No server, no accounts, no deployment. |
| **Stack** | Rust (edition 2021, MSRV 1.88), ratatui 0.30 + crossterm 0.29, rusqlite (bundled), serde_yaml, walkdir, chrono, anyhow |
| **Data in** | Obsidian-compatible vault of markdown files |
| **Data out** | `sm_id` written into frontmatter once; `<vault>/.grain/grain.db` sidecar |
| **Scheduling** | SuperMemo API (SM-20) from M1. M0 is fully offline. |
| **Repo** | `github.com/limkoaniun/grain`, public, MIT |

---

## 1. Problem

Spaced-repetition tools own your data in their own format. Notes in an Obsidian
vault are the format I already write in. grain is the review engine on top of
those files: the vault stays the source of truth, grain keeps a rebuildable index
and a grade journal beside it, and scheduling is delegated to SuperMemo rather
than reimplemented.

---

## 2. Two systems, one contract

```
<vault>/*.md               cards and articles, YAML frontmatter, Q:/A: markers
        │
        ▼
grain (this repo)          scan → SQLite index → queue → review → journal
        │
        ▼  (M1)
SuperMemo API (SM-20)      takes grades, returns intervals; grain writes due/interval back
```

**The vault owns content. grain owns the queue, the review loop and the journal.
SuperMemo owns scheduling.** Two things are contracts with later milestones and
must not change without asking: the file format and the SQLite schema.

---

## 3. File format

A card:

```markdown
---
type: card
sm_id: 1042        # may be absent on first scan; grain allocates and writes it back
due: 2026-09-25    # absent means due now
interval: 6        # may be absent
prio: 28           # absent means 50
source: "[[citrus-vocab]]"
range: 2210-2380
---
Q: Large citrus fruit with a thick rind and a mild grapefruit-like taste?
![[pomelo.png]]

A: pomelo /ˈpɒmɪloʊ/
![[pomelo.mp3]]
```

Rules, all implemented and tested in `src/vault/`:

- Frontmatter is YAML between `---` fences at the top of the file.
- `Q:` and `A:` count only at line start; the first `Q:` and the first `A:` after it
  win. Question = between them, answer = `A:` to end of file. Both may span lines.
- `![[...]]` and `![](...)` embed lines are kept in the data model on the side they
  appear. M0 renders them as a dim placeholder such as `[image: pomelo.png]`.
- Markers are never shown on screen.
- An article is `type: article` with optional `prio` and `read_pos`. Body is
  ordinary markdown, not parsed in M0.
- Unknown frontmatter keys are preserved when grain rewrites a file. The raw YAML
  mapping is kept and re-serialized; the typed view is derived from it.
- `sm_id` is the item's identity. A rename is a known `sm_id` at a new path and
  updates the index row. Missing `sm_id` gets the next integer from
  `meta.next_sm_id` (starting at 1, skipping ids already used by files), written
  back immediately.

---

## 4. Sidecar and data model

`<vault>/.grain/grain.db`, `PRAGMA journal_mode = WAL`, all writes on one thread.
Rebuildable except for unsynced journal rows.

```sql
CREATE TABLE items (
  sm_id    INTEGER PRIMARY KEY,
  path     TEXT NOT NULL UNIQUE,
  type     TEXT NOT NULL CHECK (type IN ('card','article')),
  due      TEXT,
  interval INTEGER,
  prio     INTEGER NOT NULL DEFAULT 50,
  read_pos INTEGER,
  tags     TEXT NOT NULL DEFAULT '',
  mtime    INTEGER NOT NULL,
  title    TEXT
);
CREATE INDEX idx_due  ON items(due) WHERE type = 'card';
CREATE INDEX idx_prio ON items(prio);

CREATE TABLE journal (
  id        INTEGER PRIMARY KEY AUTOINCREMENT,
  sm_id     INTEGER NOT NULL,
  grade     INTEGER NOT NULL CHECK (grade BETWEEN 0 AND 5),
  graded_at TEXT NOT NULL,          -- ISO 8601 UTC
  synced    INTEGER NOT NULL DEFAULT 0,
  interval_returned INTEGER
);
CREATE INDEX idx_pending ON journal(synced) WHERE synced = 0;

CREATE TABLE media (hash TEXT PRIMARY KEY, path TEXT NOT NULL);
CREATE TABLE meta  (key TEXT PRIMARY KEY, value TEXT NOT NULL);
-- meta keys used in M0: schema_version, next_sm_id
```

Invariants:

- Grading a card is one small transaction: insert the journal row, commit. The UI
  advances only after that commit. In M1 the API call happens after this, async.
- Undo deletes the newest journal row with `synced = 0` from this session and
  returns to that card. Rows with `synced = 1` are never deleted; the guard exists
  now even though nothing sets `synced = 1` yet.
- A card is due when `due IS NULL OR due <= today` (local date).
- Index refresh: skip files whose path and mtime match; upsert the rest; delete
  rows for missing files. Journal rows are always kept.

---

## 5. Features (M0)

### 5.1 Queue screen
One table of every item, cards and articles mixed, ordered `prio ASC, due ASC`
(NULL due first, meaning due now). `enter` on a card opens review. `enter` on an
article shows "reading arrives in M2" in the status row.

### 5.2 Review screen
Question, then reference line `↳ citrus-vocab.md › 2210-2380` when `source` is
set. `space` reveals the answer and the grade row
`[0 null] [1 bad] [2 fail]   [3 pass] [4 good] [5 bright]`. `0`–`5` journals the
grade and advances to the next due card; grades are ignored until reveal. `u`
undoes the newest grade of the session and returns to that card, revealed. After
grading, the status line reads `graded 4 · journaled (offline)`.

The review session is the list of due cards fixed at startup. Opening a card that
is not yet due inserts it into the session at the current position.

### 5.3 Chrome
Every screen is the same three rows: status (collection name `all` left, screen
context right, dim), content, key hints (dim, amber key letters). No borders, no
tabs, no titles. Unknown keys are ignored.

---

## 6. Architecture

```
src/main.rs           args (--vault, default ./vault), ratatui::run, event loop
src/app.rs            App: Screen enum, key dispatch, Review session, undo stack
src/ui/mod.rs         three-row layout, status row, hints row
src/ui/queue.rs       Table of items
src/ui/review.rs      question / reference / answer / grade row / status
src/vault/frontmatter.rs   Document {front: Mapping, body}, ItemMeta typed view
src/vault/card.rs     parse_card_body → CardBody {question, answer} of Segments
src/vault/scan.rs     walkdir *.md, skips dot-directories, mtime in seconds
src/vault/index.rs    refresh(): mtime-diff, allocation + write-back, rename, delete
src/db.rs             schema, migrate via meta.schema_version, queries, journal
fixtures/vault/       6 cards + 2 articles used by tests and for a first look
```

Boundaries that matter:

- `ui/` is pure render functions of `&App`. No state, no I/O.
- `app.rs` owns state transitions and calls `db` and `vault`; it never renders.
- `db.rs` is the only module that touches SQLite. `vault/index.rs` is the only
  module that writes to a vault file, and only to add `sm_id`.
- Tests render through ratatui's `TestBackend`; the state machine is tested with
  a temp copy of `fixtures/vault` so the checked-in fixtures never change.

---

## 7. Build order

### M0 — offline core · **Done**, 2026-09-20
Open a vault, index it, queue, review end to end, journal, undo, quit. 53 tests,
clippy clean, acceptance checklist verified against the real binary in a pty.

### M1 — SuperMemo API client · Not started
After each journal commit, post the grade to SM-20, store `interval_returned`,
mark `synced = 1`, write `due`/`interval` back to the file. Async, never blocking
the UI. Needs reqwest and probably tokio; both deliberately absent today.

### M2 — incremental reading · Not started
Read screen for articles, `read_pos`, extract to card, priority modal.

### Later
Images (ratatui-image), audio (rodio), SuperMemo collection import, cloze
rendering, stats screen, config file.

---

## 8. Accepted limitations

- Grading never changes `due` or `interval` in M0. A graded card stays due until
  M1 exists. This is by design; scheduling is not reimplemented locally.
- `mtime` is whole seconds. An edit within the same second as the last index is
  missed until the next change.
- Running against `fixtures/vault` writes `sm_id` into three checked-in files.
  Restore them with `git checkout -- fixtures` and delete `fixtures/vault/.grain`.
- `ratatui::run` panics if the terminal cannot be initialized. That is the
  library's contract and the one place a panic is accepted.
- The reference line shows the `range` value. The original spec's example said
  `extract 7`, but no extract number exists in the file format.

---

## 9. Open questions

- Should grading be allowed before reveal? M0 ignores it. Revisit with M1.
- Where does the collection name (`all`) come from once there is more than one?
- `tags` is stored comma-joined. Fine until something needs to query by tag.

---

## Related

- `coding-standards.md` for how the code is written
- `ai-interaction.md` for the workflow
- `current-feature.md` for what is in flight
- `features/m0-offline-core-spec.md` for the M0 contract as given
