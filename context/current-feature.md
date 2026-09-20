# Current Feature

<!-- Feature Name -->

Project context files

## Status

<!-- Not Started|In Progress|Completed -->

Completed

## Goals

<!-- Goals & requirements -->

Set up `context/` and `CLAUDE.md` so Claude Code has the project's context on
every session without being re-told, following the same shape as quiz-app and
devstash.

- `context/project-overview.md` — what grain is, the vault/grain/SuperMemo split,
  the file format and schema contracts, M0 features, architecture, build order,
  accepted limitations, open questions
- `context/coding-standards.md` — the two contracts first, then Rust, ratatui
  0.30, SQLite, vault, testing and domain invariants
- `context/ai-interaction.md` — the branch-and-draft-PR workflow; merging is mine
- `context/features/m0-offline-core-spec.md` — the M0 brief as given, kept as
  the record of the contract
- `CLAUDE.md` — entry point pointing at the four context files, plus real commands

## Notes

<!-- Any extra notes -->

- M0 was built and committed directly on `main` before this repo existed on
  GitHub. That is recorded in `ai-interaction.md` so it does not look like the
  workflow was ignored; from M1 on, branches and draft PRs.
- Local toolchain was Rust 1.82; ratatui 0.30 needs 1.88, so `rustup update
  stable` was run (now 1.98). `rust-version = "1.88"` is in `Cargo.toml`.
- Running the binary against `fixtures/vault` writes three `sm_id`s into
  checked-in files. Restore with `git checkout -- fixtures` and delete
  `fixtures/vault/.grain`. The tests never do this; they copy the fixtures to a
  temp dir.

## History

<!-- Keep this updated. Earliest to latest -->

- **M0** built 2026-09-20: vault scan and frontmatter round-trip that preserves
  unknown keys, `Q:`/`A:` extraction with line-start and first-occurrence rules,
  embeds kept per side, SQLite sidecar with the exact schema, mtime-diff refresh,
  `sm_id` allocation and write-back, rename detection, queue and review screens,
  grade journaling in one transaction, undo with the `synced` guard. 53 tests,
  clippy clean with `unwrap_used`/`expect_used` denied. Acceptance checklist run
  against the real binary in a pty.
- Repo created at `github.com/limkoaniun/grain`, made public, MIT license
  (copyright Guanyu Lin), README added.
- Project context files (this feature).

## Next up

M1 — SuperMemo API client. Needs its own spec first: SM-20 endpoints, auth,
what to do with `interval_returned`, how `due`/`interval` get written back into
the file, and how the async sync is driven from a single-threaded event loop
(reqwest + tokio, or a worker thread with a channel). The journal-first invariant
is already in place; M1 only adds the step after the commit.
