import { describe, expect, test } from "bun:test";
import { PassThrough } from "node:stream";
import { delimiter, join } from "node:path";
import { computeHmac } from "../src/protocol.js";
import { resolveClaudeCli, runSidecar } from "../src/main.js";
import type { QueryFn, SdkMessageLike } from "../src/chat.js";

function fakeQuery(script: SdkMessageLike[]): QueryFn {
  return () => ({
    interrupt: async () => {},
    async *[Symbol.asyncIterator]() {
      yield* script;
    },
  });
}

type Loop = {
  input: PassThrough;
  outLines: () => string[];
  errText: () => string;
  env: Record<string, string | undefined>;
  closed: Promise<void>;
};

function startLoop(
  env: Record<string, string | undefined> = {},
  script: SdkMessageLike[] = [],
): Loop {
  const input = new PassThrough();
  const output = new PassThrough();
  const errput = new PassThrough();
  let out = "";
  let err = "";
  output.on("data", (c) => (out += String(c)));
  errput.on("data", (c) => (err += String(c)));
  const { closed } = runSidecar({
    input,
    output,
    errput,
    env,
    queryFn: fakeQuery(script),
  });
  return {
    input,
    outLines: () => out.split("\n").filter((l) => l !== ""),
    errText: () => err,
    env,
    closed,
  };
}

async function settle() {
  await Bun.sleep(20);
}

describe("runSidecar", () => {
  test("scrubs the anthropic key variables and announces readiness", () => {
    const loop = startLoop({
      ANTHROPIC_API_KEY: "leaked",
      ANTHROPIC_AUTH_TOKEN: "leaked-too",
    });
    expect(loop.env.ANTHROPIC_API_KEY).toBeUndefined();
    expect(loop.env.ANTHROPIC_AUTH_TOKEN).toBeUndefined();
    expect(loop.errText()).toContain("ready");
  });

  test("answers ping, skips blanks, and drops malformed lines", async () => {
    const loop = startLoop();
    loop.input.write("\n");
    loop.input.write("{not json\n");
    loop.input.write('{"type":"nope"}\n');
    loop.input.write('{"type":"ping"}\n');
    await settle();
    expect(loop.outLines()).toEqual(['{"type":"pong"}']);
    expect(loop.errText()).toContain("dropping malformed command");
    expect(loop.errText()).toContain("unknown command type");
  });

  test("dispatches a chat turn end to end", async () => {
    const loop = startLoop({}, [
      { type: "system", subtype: "init", session_id: "sess" },
      { type: "result", subtype: "success", stop_reason: "end_turn" },
    ]);
    loop.input.write(
      JSON.stringify({
        type: "chat",
        request_id: "r1",
        session_id: "11111111-1111-4111-8111-111111111111",
        resume: null,
        cwd: null,
        model: null,
        allowed_tools: [],
        disallowed_tools: [],
        system_prompt_append: null,
        guard_patterns: [],
        content: [{ type: "text", text: "hi" }],
      }) + "\n",
    );
    await settle();
    const types = loop.outLines().map((l) => JSON.parse(l).type);
    expect(types).toEqual(["session", "done"]);
    // abort and permission_response for finished/unknown ids are no-ops
    loop.input.write('{"type":"abort","request_id":"r1"}\n');
    loop.input.write(
      '{"type":"permission_response","permission_id":"p","decision":{"behavior":"allow"}}\n',
    );
    await settle();
    expect(loop.outLines().length).toBe(2);
  });

  test("with a secret, only signed lines are accepted and output is enveloped", async () => {
    const secret = "s3cret";
    const loop = startLoop({ SIDECAR_HMAC_SECRET: secret });
    // plain command: rejected
    loop.input.write('{"type":"ping"}\n');
    await settle();
    expect(loop.outLines()).toEqual([]);
    expect(loop.errText()).toContain("missing hmac envelope");
    // signed command: answered with a signed envelope
    const payload = '{"type":"ping"}';
    loop.input.write(
      JSON.stringify({ hmac: computeHmac(secret, payload), payload }) + "\n",
    );
    await settle();
    const outer = JSON.parse(loop.outLines()[0]!);
    expect(outer.payload).toBe('{"type":"pong"}');
    expect(outer.hmac).toBe(computeHmac(secret, outer.payload));
  });

  test("closing stdin resolves the loop", async () => {
    const loop = startLoop();
    loop.input.end();
    await loop.closed;
  });
});

describe("resolveClaudeCli", () => {
  test("the env override wins and blank overrides are ignored", () => {
    const exists = () => true;
    expect(
      resolveClaudeCli({ CLAUDE_CLI_PATH: "/opt/claude", PATH: "/bin" }, exists),
    ).toBe("/opt/claude");
    expect(resolveClaudeCli({ CLAUDE_CLI_PATH: "  ", PATH: "/bin" }, exists)).toBe(
      join("/bin", "claude"),
    );
  });

  test("scans PATH in order and skips empty entries", () => {
    const hit = join("/usr/local/bin", "claude");
    const exists = (p: string) => p === hit;
    const path = ["", "/bin", "/usr/local/bin", "/other"].join(delimiter);
    expect(resolveClaudeCli({ PATH: path }, exists)).toBe(hit);
  });

  test("returns undefined when nothing matches or PATH is unset", () => {
    expect(resolveClaudeCli({ PATH: "/bin" }, () => false)).toBeUndefined();
    expect(resolveClaudeCli({}, () => true)).toBeUndefined();
    // default existence probe: a path that cannot exist yields undefined
    expect(
      resolveClaudeCli({ PATH: "/nonexistent-jaum-test-dir" }),
    ).toBeUndefined();
  });
});
