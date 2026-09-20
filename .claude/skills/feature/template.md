# Current Feature

## Feature

- **Key:** `<M<N> | short-slug>`
- **Title:** `<one-line title, used verbatim in the commit subject>`
- **Branch:** `<feature|fix|docs|chore>/<slug>`
- **Pull Request:** `<Not opened | PR #>`
- **Spec:** `<none | @context/features/<file>.md>`

## Status

`Not Started`

Allowed values:

- `Not Started`
- `In Progress`
- `Blocked`
- `In Review`
- `Completed`

## Objective

Describe the requested outcome in one or two concise paragraphs.

Focus on what the user of grain can do afterwards, not only the implementation.

## Background

Summarize the relevant context:

- Why this work is needed
- Current behavior
- Expected behavior
- Related specs, milestone notes, or constraints

## Acceptance Criteria

Copy or adapt the acceptance criteria from the spec.

- [ ] Criterion 1
- [ ] Criterion 2
- [ ] Criterion 3

Do not mark an item complete until it has been implemented and verified.

## Scope

List the areas expected to change.

### In Scope

- `<module, screen, command, or workflow>`
- `<module, screen, command, or workflow>`

### Out of Scope

- `<related work intentionally excluded>`
- `<later milestone work not included here>`

## Contract Impact

The file format (`context/project-overview.md` §3) and the SQLite schema (§4) are
contracts with later milestones.

- **Frontmatter keys or `Q:`/`A:` rules change:** `<no | yes — what>`
- **Tables, columns, indexes or meta keys change:** `<no | yes — what>`
- **Approval:** `<not needed | approved by user on YYYY-MM-DD: "<quote>">`

If either answer is `yes`, stop and ask before writing code. Record the answer here.

## References

Add relevant references using repository paths or links.

- Spec: `@context/features/<file>.md`
- Relevant source: `@src/<path>`
- Upstream docs: `<docs.rs link, API reference>`

Remove unused placeholders.

## Current Behavior

Describe what happens before the change.

Include reproducible steps for bugs when applicable.

1. Run `cargo run -- --vault <vault>`.
2. Press `<keys>`.
3. Observe `<current result>`.

## Expected Behavior

Describe the observable result after the work is complete.

Avoid vague statements such as “works correctly.”

## Implementation Plan

Keep the plan proportional to the task.

1. Inspect the current implementation and the affected data flow.
2. Write the failing tests for each criterion.
3. Update the smallest set of files required.
4. Keep `ui/` pure, all SQL in `db.rs`, one vault writer in `vault/index.rs`.
5. Run the gates.
6. Verify the flow in a real terminal against `fixtures/vault`.
7. Review the final diff against the acceptance criteria.

Update this plan when the implementation direction changes materially.

## Technical Notes

Document confirmed implementation details, constraints, and decisions.

- `<decision or constraint>`
- `<existing pattern that must be preserved>`
- `<ratatui 0.30 API detail confirmed against the pinned source>`

Do not record speculative implementation details as confirmed decisions.

## Data Changes

### Frontmatter

`None`

Keys added, renamed, or reinterpreted. Any change here needs `## Contract Impact` approval.

### Schema

`None`

Tables, columns, indexes, or `meta` keys. Bump `schema_version` and add a migration step in `Db::migrate`.

### Journal or Sync

`None`

Anything that touches `journal` rows, the `synced` flag, or the order of commit and UI advance.

### Migration or Compatibility Notes

`None`

What happens to an existing `.grain/grain.db` and to existing vault files.

## Terminal UI

For visible changes, verify:

- [ ] Three-row layout kept: status row, content, key hints
- [ ] No borders, tabs, or title bars introduced
- [ ] Every new key appears in the hints row; unknown keys still ignored
- [ ] Status-row context string updated and covered by a test
- [ ] Renders at 80x24 and does not panic at 1x1 or 0x0 (`TestBackend`)
- [ ] Long text wraps rather than overflowing
- [ ] Colors are 16-color or 256-index, never RGB-only

## Testing and Validation

### Manual Verification

Describe the terminal checks performed.

- [ ] Ran `cargo run -- --vault fixtures/vault`
- [ ] Verified `<primary flow>`
- [ ] Verified `<edge case>`
- [ ] Verified no regression in `<related flow>`
- [ ] Restored fixtures: `git checkout -- fixtures && rm -rf fixtures/vault/.grain`

### Automated Checks

- [ ] `cargo test`
- [ ] `cargo clippy --all-targets`
- [ ] `cargo build --release`

Record failures exactly. `main` is green, so any failure is yours to fix or explain.

## Risks

- `<known risk, edge case, or rollout concern>`
- `<none identified>`

## Blockers

`None`

When blocked, record:

- What is blocked
- Why it is blocked
- What was attempted
- The decision or information needed

## Assumptions

- `<confirmed assumption>`
- `<none>`

Ask for confirmation when an assumption materially affects behavior, the contracts, or scope.

## Work Log

Keep entries concise and ordered from earliest to latest.

- `YYYY-MM-DD` Created feature document from `<spec or description>`.
- `YYYY-MM-DD` Started implementation.
- `YYYY-MM-DD` `<meaningful implementation or validation update>`.

Do not use the work log as a replacement for useful commit or pull request descriptions.

## Delivery

### Commit

Planned squash commit title:

```text
<type>: <Title, verbatim>
```

Example:

```text
feat: SuperMemo API client for grade sync
```

Conventional prefix, then the title from `## Feature` character for character. No
"Generated with Claude" line and no co-author trailer.

### Pull Request

Planned pull request title — identical to the commit subject:

```text
<type>: <Title, verbatim>
```

- [ ] User approved commit
- [ ] User approved push
- [ ] Pull request opened
- [ ] Review feedback addressed
- [ ] Required checks passed
- [ ] User approved squash merge
- [ ] Pull request squash merged
- [ ] Local `main` updated
- [ ] Branch cleanup confirmed

## Completion

Complete this section only after the pull request is squash merged.

- **Status:** `Completed`
- **Pull Request:** `#<number>`
- **Merged:** `YYYY-MM-DD`
- **Squash Commit:** `<commit hash>`
- **Archived As:** `context/features/<key>-<topic>-tracker.md`

### Final Summary

Summarize the delivered behavior in two or three bullets.

- `<delivered outcome>`
- `<important implementation detail>`
- `<known follow-up, if any>`

### Final Validation

- [ ] All acceptance criteria are complete
- [ ] `cargo test` and `cargo clippy --all-targets` clean
- [ ] Terminal verification passed
- [ ] Pull request was squash merged
- [ ] `context/current-feature.md` was archived and reset

## History

Append-only, earliest to latest. One line per completed feature. This section is
carried over when the tracker is reset.

- `YYYY-MM-DD` **<key>** — `<one-line summary>` (PR #<n>)
