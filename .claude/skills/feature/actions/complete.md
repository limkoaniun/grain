# Complete Action

Takes a finished branch to a squash-merged PR.

**Every step that changes shared state waits for the user to approve that specific
step.** Approval to commit is not approval to push. Approval to push is not approval
to merge. Ask each time.

## 1. Gate

Run `actions/review.md` in full. If the verdict is **Needs changes**, stop here and
report it. Do not "just fix it and continue".

## 2. Check what is in the tree

```bash
git status --porcelain
git status --short fixtures
```

Tests stay in. `fixtures/` must be clean and `fixtures/vault/.grain/` must not
exist (it is gitignored, but a dirty fixture file is a sign of an unrestored run).
No `target/`, no stray scratch files.

## 3. Squash into one commit

The branch ships as exactly one commit, so the PR reads as a single change.

```bash
git reset --soft $(git merge-base HEAD main)
git status
```

Show the user the staged diff and the proposed message, then **ask for approval to
commit**.

Subject:

```text
<type>: <Title from ## Feature, verbatim>
```

`feat:`, `fix:`, `docs:`, `chore:`, `refactor:` or `test:`. The body explains the
change for a reviewer still learning the codebase: what changed, why, what was
deliberately left out, and why any dependency was added or any version pinned.
No "Generated with Claude" line and no co-author trailer.

The tracker (`context/current-feature.md`) is part of this commit, with
`## Status` at `In Review` once the PR exists.

## 4. Push

**Ask for approval to push.** Then push the feature branch only:

```bash
git push -u origin <branch> --force-with-lease
```

Force-push is expected here after the squash — but on the feature branch only.
Never force-push `main`.

## 5. Open the PR as a draft

```bash
gh pr create --draft --title "<type>: <Title, verbatim>" --body-file <file>
```

The title must equal the commit subject. The body says what changed and what you
verified, in that order.

Then update the tracker: `## Feature` → **Pull Request** gets the number, `## Status`
→ `In Review`, and tick `Pull request opened` in `## Delivery`. Amend that into the
single commit and push again with lease (with approval).

**Stop here.** Draft is the user's review gate — nothing should look review-ready or
notify reviewers before they have looked. Never run `gh pr ready` on your own
initiative, however green the checks are. Wait for the user to say "mark #N ready".

## 6. After the user marks it ready

- address review feedback, re-squash to one commit, force-push with lease
- confirm the checks pass
- **ask for approval to squash merge**

```bash
gh pr merge <N> --squash --delete-branch
git switch main && git pull
```

## 7. Archive and reset

Only once the merge has landed, and on a fresh `main`.

1. Fill in `## Completion`: Status `Completed`, PR number, merge date, squash commit
   hash, the final summary bullets, and the final validation boxes.
2. Copy the completed tracker to `context/features/<key>-<topic>-tracker.md`. That
   is the durable record, next to the spec.
3. Append one line to `## History`, then reset the tracker from the template,
   carrying `## History` over:

```bash
cp .claude/skills/feature/template.md context/current-feature.md
# then paste the preserved ## History section back in place of the template's
```

4. Update `context/project-overview.md` §7 so the milestone reads as done.
5. **Ask for approval** to commit this bookkeeping as
   `docs: archive <key> tracker and reset` and push it. This is the one direct
   commit to `main` the workflow allows: docs only, after a merge, with approval.
   Anything beyond the tracker, the archive and §7 goes through a branch.

## Never

- take a PR out of draft on your own initiative
- commit, push, or merge without approval for that specific step
- force-push `main`
- rename a branch with an open PR — it closes the PR instead of retargeting it
- change a contract to make a criterion pass — that is a `## Contract Impact` conversation
- report a step as done when it was skipped or only partly done
