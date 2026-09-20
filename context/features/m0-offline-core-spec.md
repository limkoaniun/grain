# grain M0 — offline core

Date: 2026-09-20
Status: done (commit `3513164` on `main`)

The brief for M0 as given, kept as the record of the file-format and schema
contracts. Later milestones add the API client, an incremental-reading screen,
images (ratatui-image), audio (rodio), and SuperMemo collection import. Nothing
in M0 may paint those into a corner.

## Stack

- Rust stable, edition 2021 or 2024.
- ratatui 0.30.x (pin the minor version) + crossterm for the event loop.
- rusqlite (feature `bundled`), serde + serde_yaml, walkdir, anyhow, chrono.
- Do NOT add yet: reqwest, rodio, ratatui-image, tokio.

ratatui 0.30 restructured its API (`ratatui::run` closure entry point,
`frame.area().layout(&Layout::vertical([...]))`, widgets re-exported from
ratatui-widgets). Most online example code is pre-0.30 and will not compile.
Consult docs.rs for the pinned version; do not trust memory for ratatui APIs.

## Scope

Open a vault, index it into SQLite, show a queue, review cards end to end,
journal every grade, quit.

1. `grain --vault <path>` (default `./vault`).
2. Startup: scan the vault (walkdir, `*.md`), parse frontmatter, build/refresh the
   SQLite index by mtime-diffing.
3. Queue screen: one table of items ordered by `prio ASC, due ASC`, mixing cards
   and articles. Enter on a card opens review; enter on an article shows a
   one-line "reading arrives in M2" notice in the status area.
4. Review screen: question shown; `space` reveals answer; `0-5` grades, which
   journals the grade and advances to the next due card; `u` undoes the most
   recent unsynced grade and steps back.
5. Quit with `q`. No save step: state is already on disk.

Out of scope: SuperMemo API client, images, audio, read screen, priority modal,
cloze rendering, import, stats screen, config file.

## File format (exact contract)

```markdown
---
type: card
sm_id: 1042        # may be absent on first scan
due: 2026-09-25    # may be absent; absent means due now
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

- Frontmatter is YAML between `---` fences at the top of the file.
- `Q:` and `A:` are recognized only at line start; first occurrence of each wins.
  Question = everything between them; answer = everything after `A:` to end of
  file. Both may span multiple lines.
- `![[...]]` and `![](...)` embed lines: parse and retain their position
  (question side vs answer side) in the data model, but render in M0 as a dim
  placeholder line like `[image: pomelo.png]`.
- `Q:`/`A:` markers are never displayed on screen.
- An article is `type: article`, with optional `prio` and `read_pos`; body is
  ordinary markdown, not parsed further in M0.
- Unknown frontmatter keys are preserved, never dropped, when grain rewrites a file.

sm_id allocation: any indexed file missing `sm_id` gets the next integer from
`meta.next_sm_id` (starting at 1), written back into the file's frontmatter
immediately. sm_id is the item's identity; the path is mutable (a rename is
detected as a known sm_id at a new path and updates the index row).

## SQLite sidecar

`<vault>/.grain/grain.db`. Rebuildable cache except for unsynced journal rows.
`PRAGMA journal_mode = WAL`. All writes on a single thread. Schema as in
`project-overview.md` §4.

Invariants:

- Grading a card is one small transaction: insert the journal row, commit. UI
  advances only after that commit. (In M1 the API call happens after this,
  async; nothing in M0 may assume otherwise.)
- Undo deletes the newest journal row with `synced = 0` for the session and
  returns to that card. Rows with `synced = 1` are never deleted.
- A card is due when `due IS NULL OR due <= today` (local date).
- Deleting grain.db and restarting must reproduce the same queue (minus journal
  history). Index refresh: skip files whose path+mtime match; upsert the rest;
  delete rows for missing files (journal rows are kept).

## UI

Deliberately chromeless. Every screen is the same three-row vertical layout:

```
top status row: collection name "all" left · screen context right (dim)
content (fill)
key hints row (dim, amber key letters)
```

No borders, no tabs, no title bars. Review context: `card · prio 28 · due 12 ·
done 3/12`. Queue context: `queue · sort prio`. After grading, a status line
under the grade row: `graded 4 · journaled (offline)`.

Grade row: `[0 null] [1 bad] [2 fail]   [3 pass] [4 good] [5 bright]`, 0-2
red-ish and 3-5 green-ish. Reference line, when `source` exists, at the bottom of
the question area: `↳ citrus-vocab.md › extract 7` (blue, dim).

Keys: `space` reveal · `0-5` grade · `u` undo · `tab` cycle queue/review ·
`enter` open item from queue · `j/k` move in queue · `q` quit. Unknown keys
are ignored silently.

## Project shape

```
src/main.rs        args, terminal setup, ratatui::run
src/app.rs         App state machine (Screen enum, key dispatch)
src/ui/            one module per screen, pure render fns
src/vault/         frontmatter parse/serialize, scan, sm_id allocation
src/db.rs          schema, migrations via meta.schema_version, queries
fixtures/vault/    6 cards (incl. multi-line answer, unicode, missing sm_id,
                   missing due, an embed line) + 2 articles
```

Tests: frontmatter round-trip preserving unknown keys, Q/A extraction incl.
multi-line and `Q:`-inside-answer-text, mtime-diff rescan, rename detection,
journal insert/undo ordering. No unwrap/expect outside tests.

## Acceptance checklist

- [x] `cargo run -- --vault fixtures/vault` opens the queue listing 8 items.
- [x] Reviewing a card end to end writes a journal row; `u` removes it and
      returns to the card.
- [x] Files missing sm_id get one allocated and written back on first run;
      second run allocates nothing.
- [x] `rm -rf fixtures/vault/.grain` then rerun rebuilds the index and the queue
      looks identical.
- [x] Renaming a card file and rerunning keeps its sm_id and journal history.
- [x] `cargo test` passes; `cargo clippy` is clean.

## Decisions made while building

| Question | Choice |
|---|---|
| Grade before reveal | Ignored. Spec was silent; blind grading seemed wrong. |
| Reference line suffix | `› <range>`; no extract number exists in the format. |
| sm_id collisions | Allocation skips ids already used by files; counter still starts at 1. |
| Amber | `Color::Indexed(214)` so it renders without truecolor. |
| Duplicate sm_id in two files | Second file skipped with a reason on stderr; never flip-flops the row. |
| Toolchain | ratatui 0.30 needs rustc 1.88; stable updated to 1.98, `rust-version = "1.88"`. |
