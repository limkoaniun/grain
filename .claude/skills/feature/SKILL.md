---
name: feature
description: Manage the current feature workflow - load, start, test, review, explain or complete
argument-hint: load|start|test|review|explain|complete
disable-model-invocation: true
---

# Feature Workflow

Takes one feature from spec to squash-merged PR.

## Working File

@context/current-feature.md

`context/current-feature.md` is the **tracker** for the feature in flight. It is not
the spec. Durable design docs live in `context/features/<key>-<topic>-spec.md`.
Both are tracked in git here (unlike some other projects), so they travel with the
feature branch and land in the squash commit.

A pristine copy of the tracker lives at `.claude/skills/feature/template.md`. That is
the reset source, and the reference for what an unfilled placeholder looks like.

### Sections this workflow reads and writes

| Section | Holds |
|---|---|
| `## Feature` | Key, Title, Branch, Pull Request, Spec |
| `## Status` | `Not Started` / `In Progress` / `Blocked` / `In Review` / `Completed` |
| `## Objective` | the user-facing outcome |
| `## Acceptance Criteria` | the checklist `review` and `complete` gate on |
| `## Scope` | In Scope / Out of Scope — the scope-creep check |
| `## Contract Impact` | whether the file format or SQLite schema changes, and the approval |
| `## Technical Notes` | confirmed decisions and constraints |
| `## Data Changes` | frontmatter keys, schema, meta keys, migration |
| `## Terminal UI` | per-change verification boxes |
| `## Testing and Validation` | Manual Verification + Automated Checks boxes |
| `## Work Log` | append-only dated entries |
| `## Delivery` | commit and PR titles, and the approval checklist |
| `## Completion` | filled in only after the squash merge lands |
| `## History` | append-only one-liners for every completed feature; survives reset |

Never invent a section. If a step needs one that is not in the file, stop and say so.

## Task

Execute the requested action: $ARGUMENTS

**Read that action's file before doing anything else** — only the one requested.

| Action | File | Does |
|---|---|---|
| `load` | `actions/load.md` | Point the tracker at a spec or description |
| `start` | `actions/start.md` | Create the branch, implement the criteria test-first |
| `test` | `actions/test.md` | Fill test gaps and run the gates |
| `review` | `actions/review.md` | Gate on criteria, scope, contracts, and the check suite |
| `explain` | `actions/explain.md` | Walk through what changed and why |
| `complete` | `actions/complete.md` | Squash, draft PR, merge, archive, reset |

If no action was given, list these options and stop.

## Standing rules

These hold for every action, not just the one that mentions them.

- **Never** take a PR out of draft. The user says "mark #N ready" when it is time.
- **Never** commit, push, or merge without approval for that specific step. Approval
  for one step is not approval for the next.
- **Never** change the file format or the SQLite schema without the approval recorded
  in `## Contract Impact`. They are contracts with later milestones.
- Branch name comes from `## Feature` → **Branch**, verbatim. Never derive it from a
  heading. Never rename a branch that has an open PR — that closes the PR.
- Commit subject and PR title are the same conventional line:
  `feat: <title>` / `fix:` / `docs:` / `chore:` / `refactor:` / `test:`.
  No Claude co-author trailer, ever.
- One commit per branch. Squash before the PR.
- Tests are committed. They live in `mod tests` at the bottom of the file they test,
  and are written before the code they test.
- `main` is green. The gate is `cargo test && cargo clippy --all-targets` both clean,
  plus the real binary run against `fixtures/vault`.
- Restore fixtures after any manual run: `git checkout -- fixtures && rm -rf fixtures/vault/.grain`.
- Tick a checkbox only once the thing is done **and** verified.
