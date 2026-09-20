# Review Action

A gate, not a summary. It ends in a verdict.

## 1. Read the contract

`context/current-feature.md` — `## Objective`, `## Acceptance Criteria`, `## Scope`,
`## Contract Impact`.

## 2. Read the change

```bash
git diff main --stat
git diff main
```

## 3. Judge

| | Check |
|---|---|
| ✅ | Every acceptance criterion genuinely met — trace each one to specific code and a test |
| ❌ | Criteria unmet, or done only in part |
| 🚫 | Scope creep — changes not traceable to a criterion or to In Scope |
| 🚫 | A frontmatter key, `Q:`/`A:` rule, table, column or meta key changed without the approval recorded in `## Contract Impact` |
| 🚫 | `unwrap` / `expect` outside a test module, or a lint suppressed to pass |
| ⚠️ | Journal invariant broken — UI advances before the commit, or undo can reach a `synced = 1` row |
| ⚠️ | Markers `Q:` / `A:` reach the screen, or a vault write outside `vault/index.rs` |
| ⚠️ | Borders, tabs, titles, or RGB-only colors in `ui/` |
| ⚠️ | A new dependency without a reason in the commit body, or one of reqwest / rodio / ratatui-image / tokio before its milestone |
| ⚠️ | ratatui API used from memory that does not exist in 0.30 (it will not compile, but check for pre-0.30 patterns copied into comments or docs) |

For visible changes, walk `## Terminal UI` in a real terminal: run the binary
against `fixtures/vault`, press the keys, look. Tick only what you actually
checked. Then restore fixtures.

## 4. Run the checks

```bash
cargo test && cargo clippy --all-targets && cargo build --release
git status --short fixtures    # must be empty
```

Record failures verbatim. Tick the boxes in `## Testing and Validation` →
Automated Checks that actually passed.

Do not add `#[allow(...)]` or loosen a lint in `Cargo.toml` to make a check pass.
A suppressed error is a finding, not a fix.

## 5. Verdict

End with a single line — **Ready to complete** or **Needs changes** — followed by
the specific blocking items. Do not soften a failing verdict, and do not report
completion for anything you did not verify.
