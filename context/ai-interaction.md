# AI Interaction Guidelines

## Communication

- Be concise and direct
- Explain non-obvious decisions briefly
- Ask before changing the file format or the SQLite schema; they are contracts
- Don't add features not in the milestone spec
- Never delete files without clarification
- Report outcomes faithfully. If a test fails, say so with the output. If a step
  was skipped, say that. Don't imply coverage that doesn't exist.

## Workflow

`/feature load|start|test|review|explain|complete` drives this. The skill lives in
`.claude/skills/feature/` and reads and writes `@context/current-feature.md`. The
steps it enforces:

1. **Document** — write the feature into `@context/current-feature.md` before
   starting. A milestone also gets a spec at
   `context/features/<milestone>-spec.md`. Build order lives in
   `@context/project-overview.md` §7, not in separate plan documents.
2. **Branch** — new branch off `main`. Never work on `main`.
3. **Implement** — test-first, and build what `@context/current-feature.md`
   describes, nothing more.
4. **Test** — two gates, both must pass:
   ```bash
   cargo test
   cargo clippy --all-targets
   ```
   Then run the real binary against `fixtures/vault` and check it. Tests passing
   is not the same as the app working in a terminal.
5. **Iterate** — fix and adjust as needed.
6. **Commit** — only after both gates pass and it works. **Ask first.**
7. **Push and open a PR** — as a **draft**.
8. **Review** — I review the PR. Say what changed and what you verified.
9. **Ready** — the PR stays draft until I say "mark #N ready". Never flip it out
   of draft yourself.
10. **Merge** — I merge. You never do.
11. **Delete branch** — ask, then delete after the merge is confirmed.
12. **Record** — mark completed in `@context/current-feature.md` and add it to the
    History section.

M0 was committed straight to `main` because the GitHub repo was created after the
work was done. From M1 on, the branch-and-draft-PR flow above applies.

## Branching

- One branch per feature or fix, off `main`
- Name it `feature/<thing>`, `fix/<thing>`, `docs/<thing>` or `chore/<thing>`
- Ask before deleting a branch, and only after confirming the merge landed

## Commits

- **Ask before committing.** Never auto-commit.
- Conventional messages: `feat:`, `fix:`, `chore:`, `docs:`, `refactor:`, `test:`
- One feature or fix per commit
- **One commit per PR.** Squash before opening it.
- Explain *why* in the body when the change is non-obvious, especially when a
  version was pinned or a default overridden
- Never put "Generated with Claude" or a Claude co-author trailer in a message

### Never rewrite pushed history

Never force-push `main`, and never push code to `main` directly. A feature branch
is force-pushed with lease only after a squash, and only with permission.

One exception, and only this one: after a PR is merged, `/feature complete`
archives the tracker, resets it, and updates the build order. That docs-only
bookkeeping commit goes straight to `main`, with approval asked first.

## When stuck

- If something isn't working after 2–3 attempts, stop and explain the issue
- Don't keep trying random fixes
- Ask for clarification if requirements are unclear
- Distinguish "the app is broken" from "the tooling is misbehaving". A ratatui
  API that "doesn't exist" is almost always a pre-0.30 example, not a bug.

## Code changes

- Minimal change to accomplish the task
- Don't refactor unrelated code unless asked
- Don't add "nice to have" features
- Preserve existing patterns in the codebase
- Read `@context/coding-standards.md` first. The two contracts and the
  no-unwrap rule are the easiest things to break by habit.

## Verification, not assertion

Before claiming something works, run the command and look at the output. In this
project specifically:

- Gates: `cargo test && cargo clippy --all-targets`
- Fixtures untouched after a test run: `git status --short fixtures`
- Real run: `cargo run -- --vault fixtures/vault`, then
  `sqlite3 fixtures/vault/.grain/grain.db 'select * from journal'`
- Restore fixtures after a manual run: `git checkout -- fixtures && rm -rf fixtures/vault/.grain`
- Nothing stale committed: `git show --stat` before pushing

## Code review

Review AI-generated code periodically, especially for:

- **The contracts** — any diff touching frontmatter keys, `Q:`/`A:` parsing or
  `db.rs` schema strings
- **Domain invariants** — journal commit before UI advance; undo never touches
  `synced = 1`; markers never rendered
- **Logic errors** — review position after undo, the end-of-session boundary,
  first-occurrence marker rules
- **Patterns** — pure render functions, all SQL in `db.rs`, one vault writer
- **Panics** — anything that could panic on a malformed vault file must become
  a skipped file with a reason, not a crash

## Related

- `@context/project-overview.md` for what this project is
- `@context/coding-standards.md` for how the code is written
- `@context/current-feature.md` for what is in flight
