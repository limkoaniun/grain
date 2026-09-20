# grain

Terminal spaced-repetition and incremental-reading app. Knowledge lives as plain
markdown in an Obsidian-compatible vault; grain is the review engine on top.
Rust, ratatui 0.30, SQLite sidecar. Scheduling goes through the SuperMemo API
from M1; M0 is fully offline.

## Context Files

Read the following to get the full context of the project. `context/` and
`.claude/skills/` are gitignored: they live only on this machine.

- @context/project-overview.md
- @context/coding-standards.md
- @context/ai-interaction.md
- @context/current-feature.md

## Two rules that are easy to break by habit

1. **The file format and the SQLite schema are contracts.** Frontmatter keys,
   the `Q:`/`A:` rules, tables, columns and meta keys do not change without
   asking. See `context/project-overview.md` §3 and §4.
2. **No `unwrap` or `expect` outside tests.** Clippy denies both. Use
   `anyhow::Result` with `.context()` at I/O and parse boundaries.

## ratatui is 0.30

`ratatui::run`, `frame.area().layout(&Layout::vertical([...]))`, widgets from
`ratatui::widgets`. Pre-0.30 examples online will not compile. When unsure, read
the pinned crate source in `~/.cargo/registry/src/` rather than guessing.

The project-scoped `ratatui-tui` skill (`.claude/skills/ratatui-tui/`, from
blacktop/dotfiles) is the reference for 0.30 API notes, widget and layout
patterns, async event handling (M1) and ratatui-image (later). Follow its API
advice; ignore its stack and styling advice where it conflicts with this repo:

- anyhow, not color-eyre; edition 2021, not 2024
- no tokio, clap, tracing, textwrap or tui-shimmer until a milestone needs them
- no blocks, borders, shadows or cyan palette — the three-row chromeless layout
  and amber hints in `context/coding-standards.md` win
- its project templates are for greenfield apps; grain already has its shape

## Commands

```bash
cargo run -- --vault fixtures/vault   # sample vault, 8 items
cargo test                            # 53 tests, must stay green
cargo clippy --all-targets            # must be clean
cargo build --release                 # binary at target/release/grain
```

A manual run writes `sm_id` into three fixture files and creates
`fixtures/vault/.grain/`. Restore before committing:

```bash
git checkout -- fixtures && rm -rf fixtures/vault/.grain
```

## Feature workflow

`/feature load <spec>` → `start` → `test` → `review` → `complete`. The skill in
`.claude/skills/feature/` owns `context/current-feature.md`; read its `SKILL.md`
before touching that file by hand.

## Before committing

Both gates must pass, then check the real binary:

```bash
cargo test && cargo clippy --all-targets
```

Ask before committing. Branch off `main`; PRs open as drafts and stay that way
until I say "mark #N ready". I do the merging.
