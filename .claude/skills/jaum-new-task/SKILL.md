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

2. **Locate the binary.** From the repo root, prefer the already-built
   binary if present, falling back to `cargo run`:
   ```
   test -x target/debug/jaum && BIN=target/debug/jaum || BIN="cargo run --quiet --bin jaum --"
   ```

3. **Run the command**, one real flag per value — never comma-join multiple
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

4. **On success (exit 0):** the first line of stdout is the task id
   (`TASK-NNN`). Report it to the user; do not create or edit any file
   yourself.

5. **On failure (exit non-zero):** read stderr — it names exactly which
   field is missing or invalid (e.g. an unsupported `--type`, an empty
   `--objective`, an ambiguous `--repo` when multiple are configured). Fix
   only that field by asking the user or re-deriving it, then retry from
   step 3.
   - Never fall back to writing a `.backlog/TASK-*.md` file by hand.
   - Never invent a `type` value beyond `impl`/`spike`.
   - Never guess or silently normalize what the error is asking for.

6. Do not use `jaum ingest`/capture for this — that flow is non-deterministic
   and being phased out.
