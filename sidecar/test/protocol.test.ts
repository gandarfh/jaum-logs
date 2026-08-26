import { describe, expect, test } from "bun:test";
import {
  computeHmac,
  decodeLine,
  encodeLine,
  verifyHmac,
  type Command,
  type Event,
} from "../src/protocol.js";

const chat: Command = {
  type: "chat",
  request_id: "req-1",
  session_id: "11111111-1111-4111-8111-111111111111",
  resume: null,
  cwd: "/tmp/wt",
  model: "claude-fable-5",
  allowed_tools: [],
  disallowed_tools: ["Bash(git merge)"],
  system_prompt_append: "constraints",
  guard_patterns: [{ pattern: "src/legacy/", reason: "do not touch legacy" }],
  content: [{ type: "text", text: "hello" }],
};

describe("decodeLine", () => {
  test("parses each command type", () => {
    expect(decodeLine(JSON.stringify(chat))).toEqual(chat);
    expect(decodeLine('{"type":"ping"}')).toEqual({ type: "ping" });
    expect(decodeLine('{"type":"abort","request_id":"r"}')).toEqual({
      type: "abort",
      request_id: "r",
    });
    expect(
      decodeLine(
        '{"type":"permission_response","permission_id":"p","decision":{"behavior":"allow"}}',
      ),
    ).toEqual({
      type: "permission_response",
      permission_id: "p",
      decision: { behavior: "allow" },
    });
  });

  test("rejects unknown types, empty lines, and invalid json", () => {
    expect(() => decodeLine('{"type":"nope"}')).toThrow("unknown command");
    expect(() => decodeLine("   ")).toThrow("empty line");
    expect(() => decodeLine("{oops")).toThrow();
  });

  test("rejects structurally invalid commands with clear messages", () => {
    expect(() => decodeLine('{"type":"chat","request_id":"r"}')).toThrow(
      "request_id and session_id must be strings",
    );
    expect(() =>
      decodeLine('{"type":"chat","request_id":"r","session_id":"s"}'),
    ).toThrow("content must be an array");
    expect(() =>
      decodeLine(
        '{"type":"chat","request_id":"r","session_id":"s","content":[]}',
      ),
    ).toThrow("allowed_tools must be an array");
    expect(() =>
      decodeLine(
        '{"type":"chat","request_id":"r","session_id":"s","content":[],"allowed_tools":[],"disallowed_tools":[]}',
      ),
    ).toThrow("guard_patterns must be an array");
    expect(() => decodeLine('{"type":"abort"}')).toThrow(
      "request_id must be a string",
    );
    expect(() =>
      decodeLine('{"type":"permission_response","permission_id":"p"}'),
    ).toThrow("behavior must be allow or deny");
    expect(() =>
      decodeLine(
        '{"type":"permission_response","permission_id":"p","decision":{"behavior":"maybe"}}',
      ),
    ).toThrow("behavior must be allow or deny");
    expect(() =>
      decodeLine(
        '{"type":"permission_response","decision":{"behavior":"allow"}}',
      ),
    ).toThrow("permission_id must be a string");
  });
});

describe("hmac envelope", () => {
  const secret = "s3cret";

  test("encode/decode roundtrip with signature", () => {
    const line = encodeLine({ type: "pong" } as Event, secret);
    const outer = JSON.parse(line);
    expect(outer.hmac).toBe(computeHmac(secret, outer.payload));
    // A signed command decodes back to the original value.
    const signed = JSON.stringify({
      hmac: computeHmac(secret, JSON.stringify(chat)),
      payload: JSON.stringify(chat),
    });
    expect(decodeLine(signed, secret)).toEqual(chat);
  });

  test("rejects tampered payloads and missing envelopes", () => {
    const payload = JSON.stringify(chat);
    const bad = JSON.stringify({
      hmac: computeHmac(secret, payload),
      payload: payload.replace("hello", "hacked"),
    });
    expect(() => decodeLine(bad, secret)).toThrow("hmac verification failed");
    expect(() => decodeLine(payload, secret)).toThrow("missing hmac envelope");
  });

  test("verifyHmac is length-safe", () => {
    expect(verifyHmac("k", "payload", "short")).toBe(false);
    expect(verifyHmac("k", "payload", computeHmac("k", "payload"))).toBe(true);
  });

  test("without a secret the line is plain json", () => {
    const line = encodeLine({ type: "pong" } as Event);
    expect(JSON.parse(line)).toEqual({ type: "pong" });
  });
});
