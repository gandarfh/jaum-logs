// Mechanical guardrails evaluated inside canUseTool, before any human
// approval: merge is always blocked, and each task constraint arrives as a
// regex from the daemon.

import type { GuardPattern } from "./protocol.js";

// Always blocked, independent of task constraints: merging is a manual user
// command, never the agent's. Flags between the binary and the subcommand
// (git -C <path> merge, git --no-pager merge, gh --repo x pr merge) must not
// slip through, but merge as a plain argument (git commit -m "merge notes")
// must not trip it. REST and GraphQL merges through `gh api` are blocked
// too, erring on the side of denying reads of merge endpoints.
const GIT_FLAGS = String.raw`(?:\s+-{1,2}[^-\s]\S*(?:\s+[^-\s]\S*)?)*`;
const MERGE_RE = new RegExp(
  [
    String.raw`\bgit${GIT_FLAGS}\s+merge\b`,
    String.raw`\bgh${GIT_FLAGS}\s+pr${GIT_FLAGS}\s+merge\b`,
    String.raw`\bgh\b[^|;&]*\bapi\b[^|;&]*(?:\bmerge\b|\bmergePullRequest\b)`,
  ].join("|"),
  "i",
);

// Input fields that carry the target a pattern is matched against: command
// for Bash, paths for the file tools.
const TARGET_FIELDS = ["command", "file_path", "path", "notebook_path"];

export function guardTarget(input: Record<string, unknown>): string {
  for (const field of TARGET_FIELDS) {
    const value = input[field];
    if (typeof value === "string" && value !== "") {
      return value;
    }
  }
  return "";
}

export type GuardVerdict = { blocked: false } | { blocked: true; reason: string };

export function checkGuards(
  toolName: string,
  input: Record<string, unknown>,
  patterns: GuardPattern[],
  log: (msg: string) => void = () => {},
): GuardVerdict {
  const target = guardTarget(input);
  if (toolName === "Bash" && MERGE_RE.test(target)) {
    return {
      blocked: true,
      reason: "merge blocked by the tool (PR-only; merge is your command)",
    };
  }
  for (const { pattern, reason } of patterns) {
    let re: RegExp;
    try {
      re = new RegExp(pattern, "i");
    } catch {
      // Fail-open on an uncompilable pattern: blocking every tool over one
      // bad constraint regex would brick the session instead of one rule.
      log(`skipping invalid guard pattern: ${pattern}`);
      continue;
    }
    if (target !== "" && re.test(target)) {
      return { blocked: true, reason: `constraint (enforce: hook): ${reason}` };
    }
  }
  return { blocked: false };
}
