# grain

A terminal spaced-repetition and incremental-reading app. Your knowledge lives as plain
markdown files in an Obsidian-compatible vault; grain is the review engine on top.

Status: **M0**. Fully offline. Open a vault, index it, show a queue, review cards, journal
every grade, quit. Scheduling through the SuperMemo API, a reading screen, images, audio,
and collection import come in later milestones.

## Run

Requires Rust 1.88 or newer.

```sh
cargo run -- --vault fixtures/vault     # sample vault: 6 cards, 2 articles
cargo run -- --vault ~/notes            # your own vault (default: ./vault)
```

The first run allocates an `sm_id` for any file missing one and writes it back into the
file's frontmatter. That is the only time grain edits your notes.

## Keys

| key | action |
|-----|--------|
| `j` / `k` | move in the queue |
| `enter` | open the selected item |
| `tab` | cycle queue / review |
| `space` | reveal the answer |
| `0`–`5` | grade (after reveal) |
| `u` | undo the most recent grade |
| `q` | quit |

Grades: `0 null · 1 bad · 2 fail · 3 pass · 4 good · 5 bright`. Unknown keys are ignored.

## File format

A card is a markdown file with YAML frontmatter and `Q:` / `A:` markers at line start.

```markdown
---
type: card
sm_id: 1042        # allocated by grain if absent
due: 2026-09-25    # absent means due now
interval: 6
prio: 28           # absent means 50; lower sorts first
source: "[[citrus-vocab]]"
range: 2210-2380
---
Q: Large citrus fruit with a thick rind and a mild grapefruit-like taste?
![[pomelo.png]]

A: pomelo /ˈpɒmɪloʊ/
![[pomelo.mp3]]
```

- The first line-start `Q:` and the first line-start `A:` after it win. Both sides may span
  several lines. A `Q:` in the middle of a line, or inside the answer, is ordinary text.
- `![[file]]` and `![](file)` embed lines are kept in the data model on the side they
  appear. M0 renders them as a dim placeholder such as `[image: pomelo.png]`.
- An article is `type: article` with optional `prio` and `read_pos`. Its body is ordinary
  markdown.
- Unknown frontmatter keys are preserved when grain rewrites a file.
- `sm_id` is the item's identity. Renaming or moving a file keeps its id and its history.

## Sidecar

grain keeps a SQLite database at `<vault>/.grain/grain.db` (WAL mode). It is a rebuildable
cache of the vault plus the grade journal. Deleting it and restarting reproduces the same
queue; only journal history is lost. The index refreshes on startup by comparing each file's
path and mtime with the stored row.

A card is due when it has no `due` date or `due` is today or earlier (local date). The queue
orders every item by `prio ASC, due ASC`.

## Layout

```
src/main.rs        args, terminal setup, event loop
src/app.rs         state machine: screens, key dispatch, review session, undo
src/ui/            one module per screen, pure render functions
src/vault/         frontmatter parse/serialize, Q/A extraction, scan, index refresh
src/db.rs          schema, migrations, queries
fixtures/vault/    sample vault used by tests and for a first look
```

```sh
cargo test
cargo clippy --all-targets
```

## Roadmap

- M1: SuperMemo API client (SM-20 scheduling). Grades already journal locally first; the
  sync happens after the commit.
- M2: incremental-reading screen for articles.
- Later: images (ratatui-image), audio (rodio), SuperMemo collection import.
