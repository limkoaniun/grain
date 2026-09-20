# Explain Action

Teach the change; do not just inventory it. Assume a reader who knows what grain is
for but is still learning Rust and ratatui — say *why* an approach was chosen, not
only that it was used.

## 1. Gather

```bash
git diff main --name-status
git diff main
```

Read `## Objective` and `## Technical Notes` from `context/current-feature.md`.

## 2. Produce this shape

```markdown
## What this feature does

One paragraph tying the change back to the objective.

## Files changed

**`src/path/file.rs`** (new | modified | deleted)
What it does and what changed. Name the key types and functions — and where the
choice was not obvious, say what the alternative was and why this won.

## How it connects

The data and control flow between the pieces: what the key press does in `app.rs`,
what `db.rs` commits, what `ui/` renders from, where a vault file is read or written.

## Worth knowing

Trade-offs taken, ownership or lifetime choices, patterns worth reusing, follow-ups
deliberately left out.
```

## 3. Rules

- Explain reasoning behind non-obvious choices; skip narrating the obvious ones
- Cite real symbols and real `file.rs:42` locations — never invented ones
- If the diff contains a change you cannot justify, flag it rather than
  rationalising it. That is a `review` finding, and it should surface here first.
