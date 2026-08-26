---
name: jaum-new-task
description: Create a new backlog task in this project's .backlog/ deterministically via `jaum task new`, instead of hand-writing a TASK-*.md file or relying on `jaum ingest`/capture. Use whenever the user asks to create, add, log, or file a new task/backlog item for this project.
argument-hint: "[objective]"
allowed-tools: Bash
---

## Process

1. **Gather fields from the conversation.** You need, at minimum:
   - `objective`: one clear sentence/paragraph of what the task is for.
   - at least one `criteria`: concrete, checkable acceptance criteria.
   - `type`: `impl` or `spike` — ask if it isn't obvious (`spike` = produces
     a document/decision, no PR, no play; `impl` = ships code). Never guess
     a third value; those two are the only ones the tool accepts.
   Optional: `rfc`/`adr` references already known to apply, and a `repo`/
   `branch` pair if this task is about to start work on a specific repo
   (`impl` only — a `spike` never gets a PR link).
   If anything required is missing, ask the user — do not invent it.

2. **Locate the binary.** Prefer the globally installed `jaum` (installed via
   `make install`, on PATH) so this works from any project, not just the
   jaum-logs repo itself. Only fall back to a local build when you're
   actually developing the `jaum` CLI (cwd is the jaum-logs repo and the
   global `jaum` isn't installed or you need an unreleased change):
   ```
   command -v jaum >/dev/null && BIN=jaum \
     || { test -x target/debug/jaum && BIN=target/debug/jaum; } \
     || BIN="cargo run --quiet --bin jaum --"
   ```

3. **Confirm which project this targets before running.** `jaum` resolves
   the project by exact match of the current directory against each
   registered project's root (`~/jaum/config.toml`) — not by subdirectory.
   If the cwd doesn't match any registered project, it silently falls back
   to the *first* configured project instead of erroring. So: run from the
   project's root directory (the one that was passed to `jaum init`), and
   after creating the task, sanity-check that the `path:` line in the
   output actually lands under the project you meant (e.g.
   `~/jaum/<expected-project>/backlog/...`) — if it doesn't, stop and tell
   the user instead of assuming it's fine.

4. **Run the command**, one real flag per value — never comma-join multiple
   `--criteria`/`--rfc`/`--adr` into one string (each occurrence is its own
   token, so this is unambiguous even if the text itself has commas):
   ```
   $BIN task new \
     --type impl \
     --objective "..." \
     --criteria "..." \
     --criteria "..." \
     [--rfc RFC-XXX] [--adr ADR-XXX] \
     [--repo org/name --branch feat/x]
   ```

5. **On success (exit 0):** the first line of stdout is the task id
   (`TASK-NNN`). Report it to the user; do not create or edit any file
   yourself.

6. **On failure (exit non-zero):** read stderr — it names exactly which
   field is missing or invalid (e.g. an unsupported `--type`, an empty
   `--objective`, an ambiguous `--repo` when multiple are configured). Fix
   only that field by asking the user or re-deriving it, then retry from
   step 4.
   - Never fall back to writing a `.backlog/TASK-*.md` file by hand.
   - Never invent a `type` value beyond `impl`/`spike`.
   - Never guess or silently normalize what the error is asking for.

7. Do not use `jaum ingest`/capture for this — that flow is non-deterministic
   and being phased out.
