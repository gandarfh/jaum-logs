import { describe, expect, test } from "bun:test";
import { checkGuards, guardTarget } from "../src/guard.js";

describe("merge guard", () => {
  test.each([
    "git merge feature",
    "git   merge --no-ff x",
    "gh pr merge 42",
    "GIT MERGE main",
    "cd repo && git merge other",
    "git -C /some/path merge feature",
    "git --no-pager merge main",
    "git --work-tree /x -c user.name=x merge main",
    "gh --repo owner/name pr merge 42",
    "gh pr --repo owner/name merge 42",
  ])("blocks %p", (command) => {
    const v = checkGuards("Bash", { command }, []);
    expect(v.blocked).toBe(true);
    if (v.blocked) {
      expect(v.reason).toContain("merge blocked");
    }
  });

  test("does not block unrelated commands or other tools", () => {
    expect(checkGuards("Bash", { command: "git status" }, []).blocked).toBe(
      false,
    );
    // merge as a plain argument is not the merge subcommand
    expect(
      checkGuards("Bash", { command: 'git commit -m "merge notes"' }, [])
        .blocked,
    ).toBe(false);
    expect(
      checkGuards("Bash", { command: "git branch -d merge-helper" }, [])
        .blocked,
    ).toBe(false);
    // The merge regex only applies to Bash commands, not file paths.
    expect(
      checkGuards("Read", { file_path: "/repo/git merge.txt" }, []).blocked,
    ).toBe(false);
  });
});

describe("constraint patterns", () => {
  const patterns = [{ pattern: "src/legacy/", reason: "do not touch legacy" }];

  test("blocks matching bash commands and file paths", () => {
    const bash = checkGuards("Bash", { command: "rm -rf src/legacy/x" }, patterns);
    expect(bash).toEqual({
      blocked: true,
      reason: "constraint (enforce: hook): do not touch legacy",
    });
    const edit = checkGuards(
      "Edit",
      { file_path: "src/legacy/mod.rs" },
      patterns,
    );
    expect(edit.blocked).toBe(true);
  });

  test("allows non-matching targets and empty input", () => {
    expect(
      checkGuards("Edit", { file_path: "src/new/mod.rs" }, patterns).blocked,
    ).toBe(false);
    expect(checkGuards("Glob", { pattern: "src/**" }, patterns).blocked).toBe(
      false,
    );
  });

  test("skips uncompilable patterns instead of blocking everything", () => {
    const logs: string[] = [];
    const v = checkGuards(
      "Bash",
      { command: "echo hi" },
      [{ pattern: "[[:space:]](", reason: "bad" }],
      (m) => logs.push(m),
    );
    expect(v.blocked).toBe(false);
    expect(logs.length).toBe(1);
  });

  test("matches case-insensitively like the old grep -iE hook", () => {
    const v = checkGuards(
      "Bash",
      { command: "cat SRC/LEGACY/a" },
      [{ pattern: "src/legacy/", reason: "legacy" }],
    );
    expect(v.blocked).toBe(true);
  });
});

describe("guardTarget", () => {
  test("prefers command, then path-like fields", () => {
    expect(guardTarget({ command: "ls", file_path: "/x" })).toBe("ls");
    expect(guardTarget({ file_path: "/x" })).toBe("/x");
    expect(guardTarget({ path: "/y" })).toBe("/y");
    expect(guardTarget({ notebook_path: "/z" })).toBe("/z");
    expect(guardTarget({ other: 1 })).toBe("");
  });
});
