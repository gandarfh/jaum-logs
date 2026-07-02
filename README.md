# jaum-logs

Backlog orchestrator on top of **Claude Code** (the `claude` CLI). It manages a
markdown backlog and drives Claude Code sessions in a controlled way, with two
core guarantees:

1. **Constraints are not forgotten.** "Don't do X" directives are reinjected on
   every session iteration (not just at startup).
2. **Scope has a boundary.** Extra scope becomes `deferred` + a new backlog item,
   instead of bloating the current task.

## Principles

- The **`.backlog/` directory (markdown) is the single source of truth**.
  GitHub/`gh` is downstream — PR number and merge state are *read*, never
  duplicated.
- The tool is a **thin, tool-agnostic shell**: it does not write feature code,
  does not write RFCs/ADRs, does not merge. It orchestrates.
- **Claude Code is a pluggable executor** behind a trait; swapping tools =
  swapping just the adapter.
- **Reusable core** (`jaum-core`) kept separate from the **adapters** (git, gh,
  executor, ui).

## Structure

```
jaum-logs/
  Cargo.toml            # workspace
  crates/
    core/               # jaum-core: data model + .backlog/ store
      src/{model,store,error}.rs
      tests/store.rs
    cli/                # jaum: binary (ratatui TUI lands in phase 7)
      src/main.rs
```

## Guarantees

The tool classifies what is guaranteed as **hard**, **detective**, and
**signal-only** — and is explicit about each level.

### Mechanical (hard) — actually blocked

- **No-merge**: the tool never runs `git merge`/`gh pr merge`. Merge is a user
  command, outside the tool.
- **Terminal, not API**: uses only the `claude` CLI, never the Anthropic API.
- **PR-only**: play opens a PR (`gh pr create`), never merges.
- **Parallelism across different repos**: one worktree per linked repo.
- **Constraints `enforce: hook`**: become a PreToolUse hook (regex) that blocks
  preventively on every call (path, command, migration, merge).

### Detective (mandatory, not optional)

- **Constraints `enforce: review`** (semantic: new abstraction, stable API,
  refactor): the hook does not catch these, so the review is **required** to
  check them item by item. `is_clean` fails if any of them fails.

### Signal-only

- **Overlap in the same repo**: `wip` tasks touching the same resource are
  signaled, not blocked.
- **Doc lifecycle discipline** (RFC/ADR).
- **Infinite project**: contained via `deferred`, but not prevented.

## Data model

Each task is a markdown file with YAML frontmatter in `.backlog/TASK-NNN.md`:

```markdown
---
id: TASK-012
type: impl            # impl | spike
status: wip           # backlog | ready | wip | review | merged
rfcs: [RFC-003, RFC-007]
adrs: [ADR-011]
prs:
  - repo: tono-lang/parser
    pr: 142             # 0 = not created yet (read from gh)
    branch: feat/task-012
deferred:
  - "primitivo decimal fica pra TASK-024"
constraints:
  - text: "nao tocar em src/legacy/"
    enforce: hook       # mechanical -> PREVENTIVE block via PreToolUse hook
  - text: "manter API estavel"
    enforce: review     # semantic  -> mandatory DETECTIVE check in review
---

## Objetivo
...

## Criterio de aceite
- [ ] ...
```

`type: spike` produces a document (RFC/ADR), has **no** PR and no play. Review
reports live in `.backlog/TASK-NNN.review.md`.

## Stack

`ratatui` + `crossterm` (TUI) · `portable-pty` + `tui-term` (embedded `claude`
session) · `tokio` (subprocesses) · `gray_matter` + `serde` + `serde_yaml_ng`
(frontmatter) · `anyhow` + `thiserror` (errors) · `git`/`gh` via `std::process`.

## Quality gates

CI (GitHub Actions, `.github/workflows/ci.yml`) owns the quality gates. It runs
on every PR and on push to `main`:

- **Formatting**: `cargo fmt --check`
- **Lint**: `cargo clippy --all-targets -- -D warnings`
- **Build and tests**: `cargo build --all-targets` + `cargo test --workspace`
- **Coverage**: `cargo llvm-cov`, minimum of 95% line coverage. The gate is
  currently in `warn` mode (reports without failing); flip `COVERAGE_GATE` to
  `block` in the workflow once coverage reaches the target.

The Makefile only has development targets (build, run, test, fmt, demo,
install), no CI rules. To run the equivalent of the gates locally:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --workspace
cargo llvm-cov --workspace --summary-only   # requires: cargo install cargo-llvm-cov
```

## Implementation status

- [x] Phase 1 — `store` + data model + frontmatter parse/write (tests)
- [x] Phase 2 — `git` and `gh` adapters
- [x] Phase 3 — `executor` trait + Claude Code impl (oneshot, then PTY)
- [x] Phase 4 — `play` (prompt, guard flags, PreToolUse hook + reinjection via UserPromptSubmit)
- [x] Phase 5 — `review` (full context, report, semantic check, is_clean)
- [x] Phase 6 — `conflict` + `finish`
- [ ] Phase 7 — ratatui TUI
