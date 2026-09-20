# Load Action

Points the tracker at a spec. Does **not** copy a spec into the tracker — the two
files have different jobs, and merging them loses both.

## 1. Resolve the argument

The argument is whatever followed `load` in the invocation.

| Shape | Meaning |
|---|---|
| `M1`, `m1`, or a bare filename | look for `context/features/m1*.md` (case-insensitive) |
| several words | inline feature description from the user |
| empty | stop — ask for a milestone key, spec filename, or description |

If a spec was named but no matching file exists, list `context/features/` and stop.
Do not invent a spec. A milestone without a spec gets one first — say so.

## 2. Confirm the tracker is free

Read `context/current-feature.md`. If `## Status` is anything but `Not Started`,
stop and report which feature is in flight. `complete` is what clears the tracker —
never overwrite live work.

## 3. Fill in the tracker

Fill only these, and only from information you actually have:

- `## Feature` — Key, Title, Branch (`feature/<slug>`, `fix/<slug>`, `docs/<slug>`
  or `chore/<slug>`), Pull Request (`Not opened`), Spec
- `## Objective` — one or two sentences on what the user of grain can do afterwards
- `## Acceptance Criteria` — from the spec, one unchecked box each, wording preserved
- `## Scope` — In Scope and Out of Scope
- `## Contract Impact` — answer both questions from the spec. If either is `yes`,
  say so in the report; approval is collected before `start`, not assumed
- `## References` — link the spec as `@context/features/<file>.md` rather than
  pasting its contents; delete the placeholder bullets you did not use
- `## Work Log` — append `` `YYYY-MM-DD` Created feature document from `<spec>`. ``
  using today's date

Leave `## Status` at `Not Started`. Leave every other section on its placeholder —
`start`, `test`, `review` and `complete` fill those in as the work happens.
Leave `## History` untouched.

## 4. Report

Show the key, the objective, the acceptance criteria, the contract-impact answers,
and — explicitly — every field you could not determine. Do not guess at acceptance
criteria; an invented criterion becomes a false gate later.
