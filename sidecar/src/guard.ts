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

// Shell quoting must not hide a guarded token: `git "merge"` or `git mer\ge`
// reach git as plain `merge` after the shell strips quotes and escapes, so
// every guard also runs against the stripped text.
function stripShellQuoting(command: string): string {
  return command.replace(/["'\\]/g, "");
}

// Constraint patterns follow the POSIX ERE dialect (the documented contract
// of `Constraint.pattern`); JS RegExp does not know POSIX classes and either
// fails to compile or silently matches a different set, so the standard
// classes are translated before compiling.
const POSIX_CLASSES: Record<string, string> = {
  "[:alnum:]": "0-9A-Za-z",
  "[:alpha:]": "A-Za-z",
  "[:blank:]": " \\t",
  "[:cntrl:]": "\\x00-\\x1f\\x7f",
  "[:digit:]": "0-9",
  "[:graph:]": "!-~",
  "[:lower:]": "a-z",
  "[:print:]": " -~",
  "[:punct:]": "!-/:-@\\[-`{-~",
  "[:space:]": " \\t\\r\\n\\v\\f",
  "[:upper:]": "A-Z",
  "[:word:]": "0-9A-Za-z_",
  "[:xdigit:]": "0-9A-Fa-f",
};

export function posixClassesToJs(pattern: string): string {
  return pattern.replace(
    /\[:(?:alnum|alpha|blank|cntrl|digit|graph|lower|print|punct|space|upper|word|xdigit):\]/g,
    (cls) => POSIX_CLASSES[cls] ?? cls,
  );
}

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
  const stripped = stripShellQuoting(target);
  if (toolName === "Bash" && (MERGE_RE.test(target) || MERGE_RE.test(stripped))) {
    return {
      blocked: true,
      reason: "merge blocked by the tool (PR-only; merge is your command)",
    };
  }
  for (const { pattern, reason } of patterns) {
    const translated = posixClassesToJs(pattern);
    let re: RegExp | null = null;
    // A leftover [:name:] is a POSIX class this table does not know: JS
    // would compile it as an ordinary character class and silently match
    // a different set, so it is treated as uncompilable.
    if (!/\[:[a-z]+:\]/i.test(translated)) {
      try {
        re = new RegExp(translated, "i");
      } catch {
        re = null;
      }
    }
    if (re === null) {
      // enforce: hook is a hard guarantee: an unusable pattern fails
      // closed. A loudly blocked session gets its constraint fixed; a
      // silently skipped one ships the violation.
      log(`guard pattern does not compile, failing closed: ${pattern}`);
      return {
        blocked: true,
        reason: `constraint (enforce: hook): pattern does not compile (${pattern}); fix the constraint`,
      };
    }
    if (target !== "" && (re.test(target) || re.test(stripped))) {
      return { blocked: true, reason: `constraint (enforce: hook): ${reason}` };
    }
  }
  return { blocked: false };
}
