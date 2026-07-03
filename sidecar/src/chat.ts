// Chat turns over the Agent SDK: one live query per request_id, permission
// requests routed to the daemon, merge and constraint guards enforced before
// any approval is even asked.

import type {
  AbortCommand,
  ChatCommand,
  ErrorCategory,
  Event,
  PermissionResponseCommand,
} from "./protocol.js";
import { checkGuards } from "./guard.js";
import { normalizeToolResultContent, toUserContent } from "./content.js";

// Structural view of the SDK's Query: enough for iteration and abort, and
// small enough for tests to fake without the full control surface.
export type QueryLike = AsyncIterable<SdkMessageLike> & {
  interrupt(): Promise<void>;
};

export type SdkMessageLike = {
  type: string;
  subtype?: string;
  session_id?: string;
  parent_tool_use_id?: string | null;
  message?: { content?: unknown };
  event?: { type?: string; delta?: { type?: string; text?: string } };
  usage?: {
    input_tokens?: number;
    output_tokens?: number;
    cache_read_input_tokens?: number;
  };
  stop_reason?: string | null;
  errors?: string[];
};

export type QueryParams = {
  prompt: AsyncIterable<unknown>;
  options: Record<string, unknown>;
};

export type QueryFn = (params: QueryParams) => QueryLike;

export type SidecarDeps = {
  queryFn: QueryFn;
  send: (event: Event) => void;
  log?: (msg: string) => void;
  claudeCliPath?: string;
};

export function categorizeError(message: string): ErrorCategory {
  const m = message.toLowerCase();
  if (
    m.includes("authentication") ||
    m.includes("unauthorized") ||
    m.includes("api key") ||
    m.includes("billing") ||
    m.includes("log in")
  ) {
    return "auth";
  }
  if (m.includes("rate limit") || m.includes("overloaded") || m.includes("429")) {
    return "rate_limit";
  }
  if (
    m.includes("network") ||
    m.includes("econnrefused") ||
    m.includes("etimedout") ||
    m.includes("fetch failed")
  ) {
    return "network";
  }
  if (m.includes("invalid") || m.includes("not found")) {
    return "invalid_input";
  }
  return "internal";
}

// Yields exactly one user message and then stays open. Streaming input keeps
// the SDK's control channel alive so interrupt() works mid-turn; the gate is
// released when the turn finishes so the iterable (and the subprocess) close.
function singleMessageStream(
  content: ReturnType<typeof toUserContent>,
  finished: Promise<void>,
): AsyncIterable<unknown> {
  return {
    async *[Symbol.asyncIterator]() {
      yield {
        type: "user" as const,
        message: { role: "user" as const, content },
        parent_tool_use_id: null,
      };
      await finished;
    },
  };
}

export function createSidecar(deps: SidecarDeps) {
  const { queryFn, send } = deps;
  const log = deps.log ?? (() => {});
  const activeQueries = new Map<string, QueryLike>();
  const pendingPermissions = new Map<
    string,
    (decision: PermissionResponseCommand["decision"]) => void
  >();
  let permissionCounter = 0;

  function buildOptions(cmd: ChatCommand) {
    const options: Record<string, unknown> = {
      permissionMode: "default",
      includePartialMessages: true,
      // Hard guarantee, independent of the system prompt: no AI attribution
      // trailer on commits.
      settings: { includeCoAuthoredBy: false },
      systemPrompt: {
        type: "preset",
        preset: "claude_code",
        ...(cmd.system_prompt_append
          ? { append: cmd.system_prompt_append }
          : {}),
      },
      canUseTool: async (
        toolName: string,
        input: Record<string, unknown>,
        { signal }: { signal: AbortSignal },
      ) => {
        const verdict = checkGuards(toolName, input, cmd.guard_patterns, log);
        if (verdict.blocked) {
          return { behavior: "deny" as const, message: verdict.reason };
        }
        const permissionId = `perm_${++permissionCounter}`;
        send({
          type: "permission_request",
          request_id: cmd.request_id,
          permission_id: permissionId,
          tool_name: toolName,
          tool_input: input,
        });
        const decision = await new Promise<
          PermissionResponseCommand["decision"]
        >((resolve, reject) => {
          pendingPermissions.set(permissionId, resolve);
          signal.addEventListener("abort", () => {
            pendingPermissions.delete(permissionId);
            reject(new Error("aborted"));
          });
        });
        if (decision.behavior === "allow") {
          return { behavior: "allow" as const, updatedInput: input };
        }
        return {
          behavior: "deny" as const,
          message: decision.message ?? "denied by the daemon",
        };
      },
    };
    if (cmd.resume) {
      options["resume"] = cmd.resume;
    } else {
      options["sessionId"] = cmd.session_id;
    }
    if (cmd.cwd) {
      options["cwd"] = cmd.cwd;
    }
    if (cmd.model) {
      options["model"] = cmd.model;
    }
    // Pre-approved tools bypass canUseTool in the SDK, so an allowlist can
    // never punch through the guards: Bash entries are always dropped (the
    // merge guard reads Bash commands), and any guard pattern (they match
    // file paths too) disables the allowlist entirely for the turn.
    const allowedTools =
      cmd.guard_patterns.length > 0
        ? []
        : cmd.allowed_tools.filter((t) => !/^Bash\b/.test(t));
    if (allowedTools.length > 0) {
      options["allowedTools"] = allowedTools;
    }
    if (cmd.disallowed_tools.length > 0) {
      options["disallowedTools"] = cmd.disallowed_tools;
    }
    if (deps.claudeCliPath) {
      options["pathToClaudeCodeExecutable"] = deps.claudeCliPath;
    }
    return options;
  }

  async function handleChat(cmd: ChatCommand): Promise<void> {
    let releaseInput: () => void = () => {};
    const inputDone = new Promise<void>((resolve) => {
      releaseInput = resolve;
    });
    const request_id = cmd.request_id;
    // tool_use ids seen in THIS turn: resumed sessions replay old user
    // messages, and their tool_results must not be re-emitted.
    const seenToolUses = new Set<string>();
    let doneSent = false;
    let deltaSeen = false;

    try {
      const q = queryFn({
        prompt: singleMessageStream(toUserContent(cmd.content), inputDone),
        options: buildOptions(cmd),
      });
      activeQueries.set(request_id, q);

      for await (const msg of q) {
        switch (msg.type) {
          case "system": {
            if (msg.subtype === "init" && msg.session_id) {
              send({
                type: "session",
                request_id,
                claude_session_id: msg.session_id,
              });
            }
            break;
          }
          case "stream_event": {
            if (
              msg.parent_tool_use_id == null &&
              msg.event?.type === "content_block_delta" &&
              msg.event.delta?.type === "text_delta" &&
              typeof msg.event.delta.text === "string"
            ) {
              deltaSeen = true;
              send({
                type: "text_delta",
                request_id,
                text: msg.event.delta.text,
              });
            }
            break;
          }
          case "assistant": {
            if (msg.parent_tool_use_id != null) {
              break;
            }
            const blocks = Array.isArray(msg.message?.content)
              ? (msg.message.content as Array<Record<string, unknown>>)
              : [];
            for (const block of blocks) {
              if (
                block["type"] === "tool_use" &&
                typeof block["id"] === "string" &&
                typeof block["name"] === "string"
              ) {
                seenToolUses.add(block["id"]);
                send({
                  type: "tool_use",
                  request_id,
                  tool_use_id: block["id"],
                  name: block["name"],
                  input: (block["input"] ?? {}) as Record<string, unknown>,
                });
              } else if (
                block["type"] === "text" &&
                typeof block["text"] === "string" &&
                !deltaSeen
              ) {
                // Fallback: assistant text normally arrives as stream deltas;
                // without them (partial messages absent) the text must still
                // reach the daemon.
                send({
                  type: "text_delta",
                  request_id,
                  text: block["text"],
                });
              }
            }
            break;
          }
          case "user": {
            const blocks = Array.isArray(msg.message?.content)
              ? (msg.message.content as Array<Record<string, unknown>>)
              : [];
            for (const block of blocks) {
              if (
                block["type"] === "tool_result" &&
                typeof block["tool_use_id"] === "string" &&
                seenToolUses.has(block["tool_use_id"])
              ) {
                send({
                  type: "tool_result",
                  request_id,
                  tool_use_id: block["tool_use_id"],
                  content: normalizeToolResultContent(block["content"]),
                  is_error: block["is_error"] === true,
                });
              }
            }
            break;
          }
          case "result": {
            if (msg.subtype && msg.subtype !== "success") {
              const message = (msg.errors ?? [msg.subtype]).join("; ");
              send({
                type: "error",
                request_id,
                category: categorizeError(message),
                message,
              });
            }
            send({
              type: "done",
              request_id,
              usage: msg.usage
                ? {
                    input_tokens: msg.usage.input_tokens ?? 0,
                    output_tokens: msg.usage.output_tokens ?? 0,
                    cache_read_tokens: msg.usage.cache_read_input_tokens ?? 0,
                  }
                : null,
              stop_reason: msg.stop_reason ?? null,
            });
            doneSent = true;
            releaseInput();
            break;
          }
          default:
            break;
        }
      }
      if (!doneSent) {
        // The query ended without a result message (e.g. interrupted).
        send({ type: "done", request_id, usage: null, stop_reason: null });
      }
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      send({
        type: "error",
        request_id,
        category: categorizeError(message),
        message,
      });
      if (!doneSent) {
        send({ type: "done", request_id, usage: null, stop_reason: "error" });
      }
    } finally {
      releaseInput();
      activeQueries.delete(request_id);
    }
  }

  function handleAbort(cmd: AbortCommand): void {
    const q = activeQueries.get(cmd.request_id);
    if (q) {
      q.interrupt().catch(() => {});
      log(`interrupted request ${cmd.request_id}`);
    }
  }

  function handlePermissionResponse(cmd: PermissionResponseCommand): void {
    const resolve = pendingPermissions.get(cmd.permission_id);
    if (resolve) {
      pendingPermissions.delete(cmd.permission_id);
      resolve(cmd.decision);
    }
  }

  function handlePing(): void {
    send({ type: "pong" });
  }

  return {
    handleChat,
    handleAbort,
    handlePermissionResponse,
    handlePing,
    activeCount: () => activeQueries.size,
    pendingPermissionCount: () => pendingPermissions.size,
  };
}
