import { describe, expect, test } from "bun:test";
import { categorizeError, createSidecar, type QueryFn, type QueryParams, type SdkMessageLike } from "../src/chat.js";
import type { ChatCommand, Event } from "../src/protocol.js";

function chatCommand(overrides: Partial<ChatCommand> = {}): ChatCommand {
  return {
    type: "chat",
    request_id: "req-1",
    session_id: "11111111-1111-4111-8111-111111111111",
    resume: null,
    cwd: null,
    model: null,
    allowed_tools: [],
    disallowed_tools: [],
    system_prompt_append: null,
    guard_patterns: [],
    content: [{ type: "text", text: "do the thing" }],
    ...overrides,
  };
}

type Harness = {
  sent: Event[];
  captured: { params?: QueryParams; interrupted: boolean };
  sidecar: ReturnType<typeof createSidecar>;
};

// Fake query: records params, yields the scripted messages, and supports
// interrupt. The generator drains the scripted list; interrupt stops it.
function harness(
  script: SdkMessageLike[],
  opts: { fail?: string; claudeCliPath?: string; hang?: boolean } = {},
): Harness {
  const sent: Event[] = [];
  const captured: Harness["captured"] = { interrupted: false };
  let releaseHang: () => void = () => {};
  const hangUntilInterrupt = new Promise<void>((resolve) => {
    releaseHang = resolve;
  });
  const queryFn: QueryFn = (params) => {
    captured.params = params;
    if (opts.fail) {
      throw new Error(opts.fail);
    }
    return {
      interrupt: async () => {
        captured.interrupted = true;
        releaseHang();
      },
      async *[Symbol.asyncIterator]() {
        for (const msg of script) {
          if (captured.interrupted) {
            return;
          }
          yield msg;
        }
        if (opts.hang) {
          await hangUntilInterrupt;
        }
      },
    };
  };
  const sidecar = createSidecar({
    queryFn,
    send: (e) => sent.push(e),
    claudeCliPath: opts.claudeCliPath,
  });
  return { sent, captured, sidecar };
}

const RESULT: SdkMessageLike = {
  type: "result",
  subtype: "success",
  usage: { input_tokens: 10, output_tokens: 5, cache_read_input_tokens: 3 },
  stop_reason: "end_turn",
};

describe("handleChat event mapping", () => {
  test("session, text_delta, tool_use, tool_result, done", async () => {
    const { sent, captured, sidecar } = harness([
      { type: "system", subtype: "init", session_id: "sess-1" },
      {
        type: "stream_event",
        parent_tool_use_id: null,
        event: {
          type: "content_block_delta",
          delta: { type: "text_delta", text: "hi " },
        },
      },
      {
        type: "assistant",
        parent_tool_use_id: null,
        message: {
          content: [
            { type: "text", text: "ignored, already streamed" },
            { type: "tool_use", id: "tu-1", name: "Bash", input: { command: "ls" } },
          ],
        },
      },
      {
        type: "user",
        message: {
          content: [
            {
              type: "tool_result",
              tool_use_id: "tu-1",
              content: "file.txt",
              is_error: false,
            },
          ],
        },
      },
      RESULT,
    ]);

    await sidecar.handleChat(chatCommand());

    expect(sent).toEqual([
      { type: "session", request_id: "req-1", claude_session_id: "sess-1" },
      { type: "text_delta", request_id: "req-1", text: "hi " },
      {
        type: "tool_use",
        request_id: "req-1",
        tool_use_id: "tu-1",
        name: "Bash",
        input: { command: "ls" },
      },
      {
        type: "tool_result",
        request_id: "req-1",
        tool_use_id: "tu-1",
        content: [{ type: "text", text: "file.txt" }],
        is_error: false,
      },
      {
        type: "done",
        request_id: "req-1",
        usage: { input_tokens: 10, output_tokens: 5, cache_read_tokens: 3 },
        stop_reason: "end_turn",
      },
    ]);
    expect(sidecar.activeCount()).toBe(0);

    // The single user message carries the collapsed text prompt.
    const messages: unknown[] = [];
    for await (const m of captured.params!.prompt) {
      messages.push(m);
      break;
    }
    expect(messages[0]).toMatchObject({
      type: "user",
      message: { role: "user", content: "do the thing" },
    });
  });

  test("replayed tool_results from previous turns are filtered", async () => {
    const { sent, sidecar } = harness([
      {
        type: "user",
        message: {
          content: [
            { type: "tool_result", tool_use_id: "old-turn", content: "stale" },
          ],
        },
      },
      RESULT,
    ]);
    await sidecar.handleChat(chatCommand());
    expect(sent.filter((e) => e.type === "tool_result")).toEqual([]);
  });

  test("subagent messages do not leak into the stream", async () => {
    const { sent, sidecar } = harness([
      {
        type: "stream_event",
        parent_tool_use_id: "tu-parent",
        event: {
          type: "content_block_delta",
          delta: { type: "text_delta", text: "nested" },
        },
      },
      {
        type: "assistant",
        parent_tool_use_id: "tu-parent",
        message: {
          content: [{ type: "tool_use", id: "tu-9", name: "Read", input: {} }],
        },
      },
      RESULT,
    ]);
    await sidecar.handleChat(chatCommand());
    expect(sent.map((e) => e.type)).toEqual(["done"]);
  });

  test("error result subtype emits error then done", async () => {
    const { sent, sidecar } = harness([
      {
        type: "result",
        subtype: "error_max_turns",
        errors: ["ran out of turns"],
        stop_reason: null,
      },
    ]);
    await sidecar.handleChat(chatCommand());
    expect(sent).toEqual([
      {
        type: "error",
        request_id: "req-1",
        category: "internal",
        message: "ran out of turns",
      },
      { type: "done", request_id: "req-1", usage: null, stop_reason: null },
    ]);
  });

  test("a thrown query error is categorized and still closes the turn", async () => {
    const { sent, sidecar } = harness([], { fail: "fetch failed" });
    await sidecar.handleChat(chatCommand());
    expect(sent).toEqual([
      {
        type: "error",
        request_id: "req-1",
        category: "network",
        message: "fetch failed",
      },
      { type: "done", request_id: "req-1", usage: null, stop_reason: "error" },
    ]);
  });

  test("a query ending without result still emits done", async () => {
    const { sent, sidecar } = harness([
      { type: "system", subtype: "init", session_id: "sess-2" },
    ]);
    await sidecar.handleChat(chatCommand());
    expect(sent.at(-1)).toEqual({
      type: "done",
      request_id: "req-1",
      usage: null,
      stop_reason: null,
    });
  });
});

describe("options mapping", () => {
  test("new sessions force the jaum uuid; resume takes precedence", async () => {
    const fresh = harness([RESULT]);
    await fresh.sidecar.handleChat(chatCommand());
    expect(fresh.captured.params!.options["sessionId"]).toBe(
      "11111111-1111-4111-8111-111111111111",
    );
    expect(fresh.captured.params!.options["resume"]).toBeUndefined();

    const resumed = harness([RESULT]);
    await resumed.sidecar.handleChat(chatCommand({ resume: "sess-old" }));
    expect(resumed.captured.params!.options["resume"]).toBe("sess-old");
    expect(resumed.captured.params!.options["sessionId"]).toBeUndefined();
  });

  test("cwd, model, tools, system prompt and cli path flow through", async () => {
    const h = harness([RESULT], { claudeCliPath: "/opt/claude" });
    await h.sidecar.handleChat(
      chatCommand({
        cwd: "/tmp/wt",
        model: "claude-fable-5",
        allowed_tools: ["Read"],
        disallowed_tools: ["Bash(git merge)"],
        system_prompt_append: "constraints here",
      }),
    );
    const o = h.captured.params!.options;
    expect(o["cwd"]).toBe("/tmp/wt");
    expect(o["model"]).toBe("claude-fable-5");
    expect(o["allowedTools"]).toEqual(["Read"]);
    expect(o["disallowedTools"]).toEqual(["Bash(git merge)"]);
    expect(o["permissionMode"]).toBe("default");
    expect(o["includePartialMessages"]).toBe(true);
    expect(o["pathToClaudeCodeExecutable"]).toBe("/opt/claude");
    expect(o["systemPrompt"]).toEqual({
      type: "preset",
      preset: "claude_code",
      append: "constraints here",
    });
  });

  test("empty lists and null fields are omitted", async () => {
    const h = harness([RESULT]);
    await h.sidecar.handleChat(chatCommand());
    const o = h.captured.params!.options;
    expect(o["allowedTools"]).toBeUndefined();
    expect(o["disallowedTools"]).toBeUndefined();
    expect(o["cwd"]).toBeUndefined();
    expect(o["model"]).toBeUndefined();
    expect(o["systemPrompt"]).toEqual({ type: "preset", preset: "claude_code" });
  });
});

describe("canUseTool", () => {
  type CanUseTool = (
    name: string,
    input: Record<string, unknown>,
    ctx: { signal: AbortSignal },
  ) => Promise<{ behavior: string; message?: string; updatedInput?: unknown }>;

  async function withCanUseTool(cmd: ChatCommand) {
    const h = harness([RESULT]);
    await h.sidecar.handleChat(cmd);
    return { ...h, canUseTool: h.captured.params!.options["canUseTool"] as CanUseTool };
  }

  test("merge is denied without asking the daemon", async () => {
    const { canUseTool, sent } = await withCanUseTool(chatCommand());
    const result = await canUseTool(
      "Bash",
      { command: "gh pr merge 1" },
      { signal: new AbortController().signal },
    );
    expect(result.behavior).toBe("deny");
    expect(result.message).toContain("merge blocked");
    expect(sent.some((e) => e.type === "permission_request")).toBe(false);
  });

  test("guard patterns are denied without asking the daemon", async () => {
    const { canUseTool } = await withCanUseTool(
      chatCommand({
        guard_patterns: [{ pattern: "src/legacy/", reason: "no legacy" }],
      }),
    );
    const result = await canUseTool(
      "Edit",
      { file_path: "src/legacy/a.rs" },
      { signal: new AbortController().signal },
    );
    expect(result.behavior).toBe("deny");
    expect(result.message).toContain("no legacy");
  });

  test("other tools round-trip through permission_request/response", async () => {
    const { canUseTool, sent, sidecar } = await withCanUseTool(chatCommand());

    const allowP = canUseTool(
      "Write",
      { file_path: "/tmp/x" },
      { signal: new AbortController().signal },
    );
    const req = sent.find((e) => e.type === "permission_request");
    expect(req).toMatchObject({
      request_id: "req-1",
      tool_name: "Write",
      tool_input: { file_path: "/tmp/x" },
    });
    expect(sidecar.pendingPermissionCount()).toBe(1);
    sidecar.handlePermissionResponse({
      type: "permission_response",
      permission_id: (req as { permission_id: string }).permission_id,
      decision: { behavior: "allow" },
    });
    const allowed = await allowP;
    expect(allowed).toEqual({
      behavior: "allow",
      updatedInput: { file_path: "/tmp/x" },
    });
    expect(sidecar.pendingPermissionCount()).toBe(0);

    const denyP = canUseTool(
      "Bash",
      { command: "rm -rf /" },
      { signal: new AbortController().signal },
    );
    const req2 = sent.filter((e) => e.type === "permission_request").at(-1);
    sidecar.handlePermissionResponse({
      type: "permission_response",
      permission_id: (req2 as { permission_id: string }).permission_id,
      decision: { behavior: "deny", message: "too dangerous" },
    });
    const denied = await denyP;
    expect(denied).toEqual({ behavior: "deny", message: "too dangerous" });
  });

  test("deny without message gets a default", async () => {
    const { canUseTool, sent, sidecar } = await withCanUseTool(chatCommand());
    const p = canUseTool(
      "Write",
      { file_path: "/tmp/y" },
      { signal: new AbortController().signal },
    );
    const req = sent.filter((e) => e.type === "permission_request").at(-1);
    sidecar.handlePermissionResponse({
      type: "permission_response",
      permission_id: (req as { permission_id: string }).permission_id,
      decision: { behavior: "deny" },
    });
    expect(await p).toEqual({ behavior: "deny", message: "denied by the daemon" });
  });

  test("an aborted signal rejects the pending permission", async () => {
    const { canUseTool, sidecar } = await withCanUseTool(chatCommand());
    const ctrl = new AbortController();
    const p = canUseTool("Write", { file_path: "/tmp/z" }, { signal: ctrl.signal });
    ctrl.abort();
    await expect(p).rejects.toThrow("aborted");
    expect(sidecar.pendingPermissionCount()).toBe(0);
  });

  test("responses for unknown permission ids are ignored", async () => {
    const { sidecar } = await withCanUseTool(chatCommand());
    sidecar.handlePermissionResponse({
      type: "permission_response",
      permission_id: "perm_unknown",
      decision: { behavior: "allow" },
    });
    expect(sidecar.pendingPermissionCount()).toBe(0);
  });
});

describe("abort and ping", () => {
  test("abort interrupts the active query only", async () => {
    // Hanging script: the query only ends via interrupt.
    const h = harness(
      [{ type: "system", subtype: "init", session_id: "s" }],
      { hang: true },
    );
    const done = h.sidecar.handleChat(chatCommand());
    await Bun.sleep(0);
    h.sidecar.handleAbort({ type: "abort", request_id: "req-1" });
    await done;
    expect(h.captured.interrupted).toBe(true);
    // Aborting a finished/unknown request is a no-op.
    h.sidecar.handleAbort({ type: "abort", request_id: "req-1" });
  });

  test("ping answers pong", () => {
    const { sent, sidecar } = harness([]);
    sidecar.handlePing();
    expect(sent).toEqual([{ type: "pong" }]);
  });
});

describe("categorizeError", () => {
  const cases: Array<[string, ReturnType<typeof categorizeError>]> = [
    ["please log in to claude", "auth"],
    ["invalid API key provided", "auth"],
    ["rate limit exceeded", "rate_limit"],
    ["API overloaded (529)", "rate_limit"],
    ["fetch failed: ECONNREFUSED", "network"],
    ["invalid request body", "invalid_input"],
    ["model not found", "invalid_input"],
    ["something exploded", "internal"],
  ];
  test.each(cases)("%p -> %p", (message, category) => {
    expect(categorizeError(message)).toBe(category);
  });
});
