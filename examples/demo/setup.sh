#!/usr/bin/env bash
# Builds the demo sandbox with everything a play session needs: an isolated
# HOME, the sidecar bundle, the claude CLI login, a toy git repo, and a task
# linked to it. Run through `make demo` (expects the repo root as cwd).
set -euo pipefail

DEMO_DIR="$1"
BIN="$2"

# A daemon from a previous sandbox run keeps the old binary and config alive;
# stop it before wiping its state.
if [ -d "$DEMO_DIR/home" ]; then
  HOME="$DEMO_DIR/home" "$BIN" shutdown >/dev/null 2>&1 || true
fi
rm -rf "$DEMO_DIR"
mkdir -p "$DEMO_DIR/home/jaum/sidecar"
cp -R examples/demo/. "$DEMO_DIR/"
rm -f "$DEMO_DIR/setup.sh"

# The sidecar authenticates through the claude CLI login; share the real
# CLI state with the sandbox HOME.
if [ -e "$HOME/.claude" ]; then
  ln -s "$HOME/.claude" "$DEMO_DIR/home/.claude"
fi

cp sidecar/dist/jaum-sidecar.mjs "$DEMO_DIR/home/jaum/sidecar/"

# Toy repo the play task links to. Created before `init` so the repo
# detection registers it (no origin remote: the slug is the folder name).
git -C "$DEMO_DIR" init -q -b main repo
git -C "$DEMO_DIR/repo" config user.email demo@jaum.local
git -C "$DEMO_DIR/repo" config user.name "jaum demo"
echo "# demo repo" > "$DEMO_DIR/repo/README.md"
git -C "$DEMO_DIR/repo" add -A
git -C "$DEMO_DIR/repo" commit -qm "init"

(cd "$DEMO_DIR" && HOME="$DEMO_DIR/home" "$BIN" init)

# Play-ready task in the project backlog (the external area init just made).
# The objective exercises the streaming chat and the merge guard in one turn.
cat > "$DEMO_DIR/home/jaum/jaum-demo/backlog/TASK-001.md" <<'EOF'
---
id: TASK-001
type: impl
status: ready
prs:
  - repo: repo
    pr: 0
    branch: feat/demo-play
---

## Objective
Create a hello.txt file with a short greeting in it. Then try to run
`git merge main` and report the exact error message you get back.
Do not try to push or open a PR: this is a local sandbox.

## Acceptance criteria
- [ ] hello.txt exists
- [ ] the merge attempt result is reported
EOF

echo "demo sandbox ready at $DEMO_DIR (select TASK-001 and press p)"
