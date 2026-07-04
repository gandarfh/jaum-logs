// Wiring of the sidecar process: reads JSONL commands from an input stream
// and emits JSONL events on an output stream. Extracted from the entrypoint
// so the loop runs under tests with fake streams and a fake query.

import { existsSync } from "node:fs";
import { delimiter, join } from "node:path";
import { createInterface } from "node:readline";
import type { Readable, Writable } from "node:stream";
import { decodeLine, encodeLine, type Event } from "./protocol.js";
import { createSidecar, type QueryFn } from "./chat.js";

export type SidecarProcess = {
  input: Readable;
  output: Writable;
  errput: Writable;
  env: Record<string, string | undefined>;
  queryFn: QueryFn;
  fileExists?: (path: string) => boolean;
};

// The SDK locates its claude executable by require.resolve on its optional
// platform package, which fails inside a single-file bundle (no node_modules
// next to the installed .mjs). The path must therefore be explicit:
// CLAUDE_CLI_PATH wins, otherwise the claude CLI installed on PATH is used.
export function resolveClaudeCli(
  env: Record<string, string | undefined>,
  exists: (path: string) => boolean = existsSync,
): string | undefined {
  const override = env.CLAUDE_CLI_PATH;
  if (override && override.trim() !== "") {
    return override;
  }
  for (const dir of (env.PATH ?? "").split(delimiter)) {
    if (dir === "") {
      continue;
    }
    const candidate = join(dir, "claude");
    if (exists(candidate)) {
      return candidate;
    }
  }
  return undefined;
}

export function runSidecar(proc: SidecarProcess): { closed: Promise<void> } {
  // Auth always rides on the claude CLI login (Claude Max subscription); a
  // leaked API key in the environment must never take precedence.
  delete proc.env.ANTHROPIC_API_KEY;
  delete proc.env.ANTHROPIC_AUTH_TOKEN;

  // The channel secret must not be visible to the agent the guards contain:
  // the claude subprocess (and every tool it runs) inherits this env.
  const secret = proc.env.SIDECAR_HMAC_SECRET || undefined;
  delete proc.env.SIDECAR_HMAC_SECRET;

  function send(event: Event): void {
    proc.output.write(encodeLine(event, secret) + "\n");
  }

  function log(msg: string): void {
    proc.errput.write(`[jaum-sidecar] ${msg}\n`);
  }

  const sidecar = createSidecar({
    queryFn: proc.queryFn,
    send,
    log,
    claudeCliPath: resolveClaudeCli(proc.env, proc.fileExists),
  });

  const rl = createInterface({ input: proc.input, crlfDelay: Infinity });

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

  const closed = new Promise<void>((resolve) => {
    rl.on("close", () => resolve());
  });

  log("ready");
  return { closed };
}
