# Start Action

## 1. Preflight

Read `context/current-feature.md` and check all four:

- `## Acceptance Criteria` holds real criteria, not `Criterion 1`. If not, stop:
  "Run `/feature load` first."
- `## Status` is `Not Started`. If it is `In Progress` or `Blocked`, report where
  the work stands and ask whether to resume rather than restarting.
- `## Feature` → **Branch** is a real name, not the placeholder.
- `## Contract Impact` — if either answer is `yes` and **Approval** is still
  `not needed` or empty, stop and ask. Record the answer verbatim before going on.

Check the working tree is clean. If it is dirty, stop and report it — do not stash
silently. A dirty `fixtures/` usually means a manual run was not restored:
`git checkout -- fixtures && rm -rf fixtures/vault/.grain`.

## 2. Branch

```bash
git switch main && git pull
git switch -c <Branch from ## Feature>
```

Use the **Branch** field verbatim. Never rename a branch once a PR exists.

## 3. Mark in progress

- `## Status` → `In Progress`
- `## Work Log` → append `` `YYYY-MM-DD` Started implementation. ``

## 4. Read the standards that govern this change

`context/coding-standards.md` is the source of truth for how code is written here.
Read the sections that apply — do not restate its rules from memory.

Always:

| Section | Covers |
|---|---|
| The two contracts | file format and schema — what needs asking |
| Rust | no unwrap/expect outside tests, anyhow context, MSRV, no new deps without a reason |
| Testing | test-first, temp copies of fixtures, `TestBackend`, fixed `today` |
| Domain invariants | due rule, queue order, commit-before-advance, undo guard |

Then by what the change touches:

| Section | When |
|---|---|
| ratatui 0.30 | any change under `src/ui/` or the event loop — read the pinned source, not memory |
| SQLite | any change to `src/db.rs` — transactions, migrations, one writer thread |
| Vault | any change under `src/vault/` — one file writer, unknown keys preserved |
| File organisation | adding a module or moving code |

## 5. Implement

List the acceptance criteria, then work them one at a time, **test first**:

1. Write the failing test in the `mod tests` of the file under change. Run it.
   Confirm it fails for the right reason (a missing function counts).
2. Write the minimal code to pass. Run `cargo test`.
3. Refactor with the tests green.

As you go:

- tick a criterion only once it is implemented **and** verified
- record confirmed decisions in `## Technical Notes` — decisions and constraints,
  never speculation; any ratatui API you had to look up goes here with the path
- record frontmatter, schema, journal and migration changes in `## Data Changes`
- keep `ui/` pure, all SQL in `db.rs`, vault writes only in `vault/index.rs`

Stay inside `## Scope` → In Scope. If something outside it needs doing, raise it
rather than doing it — unexplained changes fail `review` as scope creep.

If a step would change a contract that `## Contract Impact` says does not change,
stop. That is a new approval, not a detail.

If you get blocked, fill in `## Blockers` (what is blocked, why, what you tried,
what you need) and set `## Status` to `Blocked`.
