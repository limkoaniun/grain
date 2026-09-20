# Coding Standards

## The two contracts that outrank the rest

**The file format and the SQLite schema are contracts with later milestones.**
Do not change a frontmatter key, the `Q:`/`A:` rules, a table, a column or a meta
key without asking first. If a task seems to need a change there, stop and say
so; the answer is usually that the data belongs somewhere already defined.

Everything else in the repo is ordinary code and can be refactored freely.

## Rust

- Edition 2021, `rust-version = "1.88"` (ratatui 0.30 needs it). Do not lower it.
- `unsafe_code = "forbid"`.
- **No `unwrap` or `expect` outside tests.** `clippy::unwrap_used` and
  `clippy::expect_used` are deny-level in `Cargo.toml`; test modules opt out with
  `#![allow(clippy::unwrap_used)]` at the top of `mod tests`.
- Errors are `anyhow::Result`. Add `.context()` / `.with_context()` at I/O and
  parse boundaries so a failure names the file or sm_id involved.
- `let ... else` and `Option::is_none_or` are fine; MSRV allows them.
- No new dependencies without a reason in the commit body. reqwest, rodio,
  ratatui-image and tokio are deliberately absent until their milestone.

## ratatui 0.30

- **Do not trust memory for ratatui APIs.** 0.30 moved things: `ratatui::run`
  closure entry, `frame.area().layout(&Layout::vertical([...]))` returning an
  array, widgets re-exported from `ratatui::widgets`, `TableState::new()` is
  `const`. Most online examples are pre-0.30 and will not compile.
- When unsure, read the pinned source under
  `~/.cargo/registry/src/*/ratatui-0.30.*` or `ratatui-core-*` / `ratatui-widgets-*`,
  or docs.rs for the exact version.
- Colors: amber for key hints is `Color::Indexed(214)`, not RGB, so it renders
  without truecolor. Red-ish grades `LightRed`, green-ish `LightGreen`.
- Render functions take `(&mut Frame, Rect, &App)` and return nothing. No widget
  holds state across frames; `TableState` is rebuilt from `app.queue_sel` each draw.
- Chromeless: no `Block` borders, no titles, no tabs. Three rows, always.

## SQLite

- One `Db` on one thread. No connection pool, no background writer.
- Every multi-statement write is a transaction (`unchecked_transaction` then
  `commit`). A grade is exactly one insert + commit; nothing else in that
  transaction.
- Migrations step `meta.schema_version` up in `Db::migrate`. A database newer than
  the build is an error, not a silent downgrade.
- Queries return typed `ItemRow`; column parsing errors carry the sm_id.

## Vault

- `vault/index.rs` is the only code that writes to a vault file, and only to add
  `sm_id`. If a change needs another write, it goes through `Document::serialize`
  so unknown keys survive.
- Paths in the index are vault-relative, `/`-separated, and are the diff key.
  `sm_id` is identity; paths are mutable.
- Skip every dot-directory (`.grain`, `.obsidian`, `.git`).

## File organisation

```
src/main.rs        entry only: args, terminal, loop
src/app.rs         state and key dispatch
src/ui/            one file per screen plus mod.rs for shared chrome
src/vault/         parse, scan, index
src/db.rs          all SQL
fixtures/vault/    the sample vault; keep exactly 6 cards + 2 articles
```

Tests live in `mod tests` at the bottom of the file they test. No `tests/`
directory yet.

## Testing

- Test-first. Write the failing test, watch it fail for the right reason, then
  implement. Compile failure on a missing function counts as the right reason.
- State machine tests copy `fixtures/vault` into a `tempfile::tempdir()` so the
  checked-in fixtures are never mutated by a test.
- UI tests render into `TestBackend::new(80, 24)` and assert on the buffer text.
  Also render at tiny sizes (`1x1`, `0x0`) to prove nothing panics.
- Use a fixed `today` (`2026-09-20`) in tests; `App::open` takes it as a parameter
  for that reason.
- `cargo test` and `cargo clippy --all-targets` must both be clean before a commit.

## Domain invariants

- Due means `due IS NULL OR due <= today`, local date.
- Queue order is `prio ASC, due ASC, sm_id ASC`. NULL due sorts first.
- The UI advances only after the journal commit returns.
- Undo pops the session's own stack and deletes only `synced = 0` rows.
- Markers `Q:` and `A:` never reach the screen.
- Deleting `.grain` and restarting reproduces the same queue.

## Naming

- Modules and files snake_case. Types CamelCase. `sm_id` stays `sm_id` everywhere,
  including SQL, frontmatter and Rust, so grep finds every use.
- Screen context strings and status lines are built in `app.rs`
  (`queue_context`, `review_context`) so the UI and the tests share one source.

## Related

- `project-overview.md` for what this is and why
- `ai-interaction.md` for the workflow
