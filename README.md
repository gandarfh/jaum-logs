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
    cli/                # jaum: binary (TUI + daemon + sidecar client)
      src/main.rs
  sidecar/              # TypeScript bridge over @anthropic-ai/claude-agent-sdk
    src/{index,main,protocol,chat,guard,content}.ts
```

## Sidecar

Play sessions run through a TypeScript sidecar built on the Claude Agent SDK.
The daemon spawns one node process and talks JSONL over stdio: `chat` sends a
turn, the sidecar streams `text_delta`/`tool_use`/`tool_result`/`done` back,
`permission_request`/`permission_response` route tool approvals through the
daemon, and `abort` interrupts a running turn. Auth always rides on the
`claude` CLI login (Claude Max): the sidecar drops `ANTHROPIC_API_KEY` from its
environment.

Permission requests are currently logged and auto-allowed by the daemon: the
mechanical guards (no-merge, `enforce: hook` constraints) already ran inside
the sidecar before the request was ever routed, and no client surface can
answer a prompt yet. The interactive flow (wait for a decision, deny after
120s and mark the session blocked) is implemented and tested behind the
`route_permissions` switch, and turns on once the structured chat panel can
present the prompt.

Every session appends its events to `~/jaum/<project>/.sessions/<id>.jsonl`;
the log survives daemon restarts and is replayed (bounded) on attach. There is
no automatic GC: the finish flow archives the logs of a closed task.

The daemon looks for the bundle at `~/jaum/sidecar/jaum-sidecar.mjs`
(override with `JAUM_SIDECAR_BUNDLE`). Build and install it with:

```sh
make sidecar-install   # requires bun; the daemon itself only needs node
```

The wire protocol is pinned by golden fixtures in
`crates/cli/tests/fixtures/sidecar/`: the Rust tests generate and roundtrip
them, and the sidecar's bun tests decode the same files.

## Guarantees

The tool classifies what is guaranteed as **hard** and **signal-only** — and
is explicit about each level.

### Mechanical (hard) — actually blocked

- **No-merge**: the tool never runs `git merge`/`gh pr merge`. Merge is a user
  command, outside the tool.
- **Terminal, not API**: uses only the `claude` CLI, never the Anthropic API.
- **PR-only**: play opens a PR (`gh pr create`), never merges.
- **Parallelism across different repos**: one worktree per linked repo.
- **Constraints `enforce: hook`**: become guard patterns (regex) the sidecar
  applies inside `canUseTool`, blocking preventively on every call (path,
  command, migration, merge) before any approval is asked.

### Signal-only

- **Constraints `enforce: review`** (semantic: new abstraction, stable API,
  refactor): nothing checks these automatically — they're surfaced in the
  session prompt as the agent's own responsibility, and it's on the human to
  verify them before merging.
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
    enforce: review     # semantic  -> your call, nothing checks it for you
---

## Objetivo
...

## Criterio de aceite
- [ ] ...
```

`type: spike` produces a document (RFC/ADR), has **no** PR and no play.

## Creating tasks

`jaum task new` creates a backlog task deterministically — no `claude` call,
and it fails before writing anything if a required field is missing or
`--type` isn't `impl`/`spike`:

```sh
jaum task new --type impl \
  --objective "..." \
  --criteria "..." [--criteria "..."] \
  [--rfc RFC-XXX] [--adr ADR-XXX] \
  [--repo org/name --branch feat/x]
```

A paired Claude Code Skill (`skills/jaum-new-task/`) drives this from a
conversation — gathering the fields, running the command, and surfacing any
validation error back for correction — instead of hand-writing a
`TASK-*.md` file. This repo is also a Claude Code plugin, so the skill can be
installed at any scope:

```
/plugin marketplace add gandarfh/jaum-logs
/plugin install jaum-logs --scope user
```

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
- **Sidecar**: `bun run typecheck` + `bun test` + `bun run build` in `sidecar/`;
  the job enforces the same 95% line coverage minimum from the lcov report
- **Coverage**: `cargo llvm-cov`, minimum of 95% line coverage. The gate is
  blocking: the job fails below the minimum.

The Makefile only has development targets (build, run, test, fmt, demo,
install, sidecar), no CI rules. To run the equivalent of the gates locally:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --workspace
make sidecar-test
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
