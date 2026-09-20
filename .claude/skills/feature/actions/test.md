# Test Action

Tests are committed here and written first. This action is for the gaps: coverage
that `start` should have produced but did not, and the full gate run.

## 1. Identify what is worth testing

Read `context/current-feature.md` for what changed, then:

```bash
git diff main --name-only
git diff main --stat
```

For each changed source file, check its `mod tests` grew with it. Prefer logic with
branches, edge cases, or data transformation: parsers, index refresh, queries,
state transitions, render output. A pure render function is tested through
`TestBackend` and buffer text, not by eye.

Do not write tests just to have tests. If nothing in the change carries testable
logic, say so and stop.

## 2. Write them

Follow `context/coding-standards.md` → Testing:

- tests live in `mod tests` at the bottom of the file, with
  `#![allow(clippy::unwrap_used)]` at the top of the module
- state-machine tests copy `fixtures/vault` into a `tempfile::tempdir()`; never
  touch the checked-in fixtures
- use a fixed `today` (`2026-09-20`) so due logic is deterministic
- UI tests render at 80x24 and also at 1x1 / 0x0 to prove no panic

The failures that have historically mattered in this repo are marker parsing
(`Q:` mid-line or inside the answer), mtime-diff skipping a changed file, undo
touching the wrong journal row, and an sm_id write-back that changes mtime and
causes a re-index. Cover those shapes first when they are near the change.

## 3. Run

```bash
cargo test && cargo clippy --all-targets
```

`main` is green. Every failure is either yours or a real regression; there is no
"pre-existing" here. Fix it, or record it as a blocker.

## 4. Record

- tick `## Testing and Validation` → Automated Checks for what passed
- if a test found a real bug, fix the bug and note it in `## Work Log`
- report which files you covered, which you deliberately skipped, and why
