import { describe, expect, test } from "bun:test";
import { normalizeToolResultContent, toUserContent } from "../src/content.js";
import type { ContentBlock } from "../src/protocol.js";

const image: ContentBlock = {
  type: "image",
  source: { type: "base64", media_type: "image/png", data: "aWpn" },
};

describe("toUserContent", () => {
  test("collapses text-only turns into a string", () => {
    expect(
      toUserContent([
        { type: "text", text: "a" },
        { type: "text", text: "b" },
      ]),
    ).toBe("a\nb");
  });

  test("keeps blocks when an image is present", () => {
    const content = toUserContent([{ type: "text", text: "see" }, image]);
    expect(content).toEqual([{ type: "text", text: "see" }, image]);
  });
});

describe("normalizeToolResultContent", () => {
  test("handles absent and string content", () => {
    expect(normalizeToolResultContent(undefined)).toEqual([]);
    expect(normalizeToolResultContent("")).toEqual([]);
    expect(normalizeToolResultContent("ok")).toEqual([
      { type: "text", text: "ok" },
    ]);
  });

  test("passes through text and image blocks", () => {
    expect(
      normalizeToolResultContent([{ type: "text", text: "t" }, image]),
    ).toEqual([{ type: "text", text: "t" }, image]);
  });

  test("degrades unknown shapes to json text", () => {
    expect(normalizeToolResultContent({ weird: true })).toEqual([
      { type: "text", text: '{"weird":true}' },
    ]);
    expect(
      normalizeToolResultContent([{ type: "document", data: "x" }]),
    ).toEqual([{ type: "text", text: '{"type":"document","data":"x"}' }]);
    expect(
      normalizeToolResultContent([{ type: "image", source: { type: "url" } }]),
    ).toEqual([
      { type: "text", text: '{"type":"image","source":{"type":"url"}}' },
    ]);
  });
});
