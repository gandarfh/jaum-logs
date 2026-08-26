// Entrypoint: the real process streams and the real Agent SDK query.
// Diagnostics go to stderr only; stdout is reserved for the protocol.

import { query } from "@anthropic-ai/claude-agent-sdk";
import type { QueryFn } from "./chat.js";
import { runSidecar } from "./main.js";

const { closed } = runSidecar({
  input: process.stdin,
  output: process.stdout,
  errput: process.stderr,
  env: process.env,
  queryFn: query as unknown as QueryFn,
});

void closed.then(() => process.exit(0));
