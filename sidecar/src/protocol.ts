// Wire protocol between the daemon (Rust) and this sidecar: one JSON message
// per line over stdio. The shapes here are pinned by the golden fixtures under
// crates/cli/tests/fixtures/sidecar/ (serde on the Rust side decodes the same
// files), so any change is a cross-language contract change.

import { createHmac, timingSafeEqual } from "node:crypto";

export type ContentBlock =
  | { type: "text"; text: string }
  | {
      type: "image";
      source: { type: "base64"; media_type: string; data: string };
    };

export type GuardPattern = { pattern: string; reason: string };

export type ChatCommand = {
  type: "chat";
  request_id: string;
  session_id: string;
  resume: string | null;
  cwd: string | null;
  model: string | null;
  allowed_tools: string[];
  disallowed_tools: string[];
  system_prompt_append: string | null;
  guard_patterns: GuardPattern[];
  content: ContentBlock[];
};

export type PermissionDecision =
  | { behavior: "allow" }
  | { behavior: "deny"; message?: string };

export type PermissionResponseCommand = {
  type: "permission_response";
  permission_id: string;
  decision: PermissionDecision;
};

export type AbortCommand = { type: "abort"; request_id: string };

export type PingCommand = { type: "ping" };

export type Command =
  | ChatCommand
  | PermissionResponseCommand
  | AbortCommand
  | PingCommand;

export type Usage = {
  input_tokens: number;
  output_tokens: number;
  cache_read_tokens: number;
};

export type ErrorCategory =
  | "auth"
  | "rate_limit"
  | "network"
  | "invalid_input"
  | "internal";

export type Event =
  | { type: "session"; request_id: string; claude_session_id: string }
  | { type: "text_delta"; request_id: string; text: string }
  | {
      type: "tool_use";
      request_id: string;
      tool_use_id: string;
      name: string;
      input: Record<string, unknown>;
    }
  | {
      type: "tool_result";
      request_id: string;
      tool_use_id: string;
      content: ContentBlock[];
      is_error: boolean;
    }
  | {
      type: "permission_request";
      request_id: string;
      permission_id: string;
      tool_name: string;
      tool_input: Record<string, unknown>;
    }
  | {
      type: "done";
      request_id: string;
      usage: Usage | null;
      stop_reason: string | null;
    }
  | {
      type: "error";
      request_id: string;
      category: ErrorCategory;
      message: string;
    }
  | { type: "pong" };

export function computeHmac(secret: string, payload: string): string {
  return createHmac("sha256", secret).update(payload, "utf8").digest("hex");
}

export function verifyHmac(
  secret: string,
  payload: string,
  expected: string,
): boolean {
  const computed = Buffer.from(computeHmac(secret, payload), "utf8");
  const given = Buffer.from(expected, "utf8");
  if (computed.length !== given.length) {
    return false;
  }
  return timingSafeEqual(computed, given);
}

// Serializes an event to a single line, wrapping it in the HMAC envelope
// {hmac, payload} when a secret is configured.
export function encodeLine(event: Event, secret?: string): string {
  const payload = JSON.stringify(event);
  if (!secret) {
    return payload;
  }
  return JSON.stringify({ hmac: computeHmac(secret, payload), payload });
}

// Parses one incoming line into a command. With a secret, only enveloped and
// correctly signed lines are accepted; without one, the raw JSON is used.
export function decodeLine(line: string, secret?: string): Command {
  const trimmed = line.trim();
  if (trimmed === "") {
    throw new Error("empty line");
  }
  const raw = JSON.parse(trimmed) as Record<string, unknown>;
  let body: unknown = raw;
  if (secret) {
    const hmac = raw["hmac"];
    const payload = raw["payload"];
    if (typeof hmac !== "string" || typeof payload !== "string") {
      throw new Error("missing hmac envelope");
    }
    if (!verifyHmac(secret, payload, hmac)) {
      throw new Error("hmac verification failed");
    }
    body = JSON.parse(payload);
  }
  return validateCommand(body);
}

function isString(v: unknown): v is string {
  return typeof v === "string";
}

// Structural validation so a malformed command fails here, with a clear
// message, instead of deep inside a handler.
function validateCommand(body: unknown): Command {
  const cmd = body as Record<string, unknown>;
  switch (cmd["type"]) {
    case "chat": {
      if (!isString(cmd["request_id"]) || !isString(cmd["session_id"])) {
        throw new Error("chat: request_id and session_id must be strings");
      }
      if (!Array.isArray(cmd["content"])) {
        throw new Error("chat: content must be an array");
      }
      for (const field of ["allowed_tools", "disallowed_tools", "guard_patterns"]) {
        if (!Array.isArray(cmd[field])) {
          throw new Error(`chat: ${field} must be an array`);
        }
      }
      return cmd as unknown as ChatCommand;
    }
    case "permission_response": {
      const decision = cmd["decision"] as Record<string, unknown> | null;
      if (!isString(cmd["permission_id"])) {
        throw new Error("permission_response: permission_id must be a string");
      }
      if (
        decision == null ||
        (decision["behavior"] !== "allow" && decision["behavior"] !== "deny")
      ) {
        throw new Error("permission_response: decision.behavior must be allow or deny");
      }
      return cmd as unknown as PermissionResponseCommand;
    }
    case "abort": {
      if (!isString(cmd["request_id"])) {
        throw new Error("abort: request_id must be a string");
      }
      return cmd as unknown as AbortCommand;
    }
    case "ping":
      return { type: "ping" };
    default:
      throw new Error(`unknown command type: ${String(cmd["type"])}`);
  }
}
