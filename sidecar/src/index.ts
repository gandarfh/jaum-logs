// Entrypoint: reads JSONL commands from stdin, emits JSONL events on stdout.
// Diagnostics go to stderr only; stdout is reserved for the protocol.

import { createInterface } from "node:readline";
import { query } from "@anthropic-ai/claude-agent-sdk";
import { decodeLine, encodeLine, type Event } from "./protocol.js";
import { createSidecar, type QueryFn } from "./chat.js";

// Auth always rides on the claude CLI login (Claude Max subscription); a
// leaked API key in the environment must never take precedence.
delete process.env.ANTHROPIC_API_KEY;
delete process.env.ANTHROPIC_AUTH_TOKEN;

const secret = process.env.SIDECAR_HMAC_SECRET || undefined;

function send(event: Event): void {
  process.stdout.write(encodeLine(event, secret) + "\n");
}

function log(msg: string): void {
  process.stderr.write(`[jaum-sidecar] ${msg}\n`);
}

const sidecar = createSidecar({
  queryFn: query as unknown as QueryFn,
  send,
  log,
  claudeCliPath: process.env.CLAUDE_CLI_PATH || undefined,
});

const rl = createInterface({ input: process.stdin, crlfDelay: Infinity });

rl.on("line", (line: string) => {
  if (line.trim() === "") {
    return;
  }
  let cmd;
  try {
    cmd = decodeLine(line, secret);
  } catch (err) {
    log(`dropping malformed command: ${err instanceof Error ? err.message : err}`);
    return;
  }
  switch (cmd.type) {
    case "chat":
      sidecar.handleChat(cmd).catch((err) => {
        log(`chat handler failed: ${err instanceof Error ? err.message : err}`);
      });
      break;
    case "abort":
      sidecar.handleAbort(cmd);
      break;
    case "permission_response":
      sidecar.handlePermissionResponse(cmd);
      break;
    case "ping":
      sidecar.handlePing();
      break;
  }
});

rl.on("close", () => {
  process.exit(0);
});

log("ready");
