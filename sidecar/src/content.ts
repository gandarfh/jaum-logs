// Conversion between the wire ContentBlock and the SDK message shapes.

import type { ContentBlock } from "./protocol.js";

export type UserMessageContent =
  | string
  | Array<
      | { type: "text"; text: string }
      | {
          type: "image";
          source: { type: "base64"; media_type: string; data: string };
        }
    >;

// Builds the user message content for the SDK. Text-only turns collapse into
// a plain string; any image forces the block-array form.
export function toUserContent(blocks: ContentBlock[]): UserMessageContent {
  const hasImage = blocks.some((b) => b.type === "image");
  if (!hasImage) {
    return blocks
      .map((b) => (b.type === "text" ? b.text : ""))
      .filter((t) => t !== "")
      .join("\n");
  }
  return blocks.map((b) =>
    b.type === "text"
      ? { type: "text" as const, text: b.text }
      : { type: "image" as const, source: b.source },
  );
}

// Normalizes a tool_result's content (string, block array, or absent) into
// wire ContentBlocks. Unknown block types degrade to their JSON text so the
// daemon never loses information silently.
export function normalizeToolResultContent(raw: unknown): ContentBlock[] {
  if (raw == null) {
    return [];
  }
  if (typeof raw === "string") {
    return raw === "" ? [] : [{ type: "text", text: raw }];
  }
  if (!Array.isArray(raw)) {
    return [{ type: "text", text: JSON.stringify(raw) }];
  }
  const out: ContentBlock[] = [];
  for (const block of raw) {
    const b = block as Record<string, unknown>;
    if (b["type"] === "text" && typeof b["text"] === "string") {
      out.push({ type: "text", text: b["text"] });
    } else if (b["type"] === "image" && b["source"] != null) {
      const src = b["source"] as {
        type: string;
        media_type?: string;
        data?: string;
      };
      if (
        src.type === "base64" &&
        typeof src.media_type === "string" &&
        typeof src.data === "string"
      ) {
        out.push({
          type: "image",
          source: {
            type: "base64",
            media_type: src.media_type,
            data: src.data,
          },
        });
      } else {
        out.push({ type: "text", text: JSON.stringify(block) });
      }
    } else {
      out.push({ type: "text", text: JSON.stringify(block) });
    }
  }
  return out;
}
