# grain

A terminal spaced-repetition and incremental-reading app. Your knowledge lives as plain
markdown files in an Obsidian-compatible vault; grain is the review engine on top.

Status: **M10**. Learn session with final drill, SuperMemo API scheduling (offline
without a key), incremental reading with extracts and clozes, add and import, pictures
and sound, a stats screen, auto-postpone, and a config file with a settings screen.

## Run

Requires Rust 1.88 or newer.

```sh
cargo run -- --vault fixtures/vault     # sample vault: 6 cards, 2 articles
cargo run -- --vault ~/notes            # your own vault (default: ./vault)
cargo install --path .                  # puts `grain` on your PATH (~/.cargo/bin)
grain                                   # opens the vault named in ~/.config/grain/config
```

grain reads `~/.config/grain/config` (or `$XDG_CONFIG_HOME/grain/config`;
`GRAIN_CONFIG=<path>` overrides), plain `key = value` lines: `vault` (default `./vault`),
`postpone` (default `50`, or `off`), `final_drill` (default `ask`; `on` drills without
asking, `off` skips the drill), `collection` (default `all`, the name in the status row).
Press `o` on the table to edit them from inside grain. `--vault` and `--postpone` override
the file for one launch.

The first run allocates an `sm_id` for any file missing one and writes it back into the
file's frontmatter. Later writes are `due`/`interval`, `read_pos`, `prio` and `done` on
the file itself, plus new child, added and imported files, always through the frontmatter
round-trip that keeps every other key intact.

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

- Manual Postpone and Mercy, postpone count and skip conditions.
- Cloze hints and several blanks per card.
- SuperMemo collection import, monthly workload and the workload graph.

## License

MIT. See [LICENSE](LICENSE).
