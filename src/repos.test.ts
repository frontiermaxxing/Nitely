// Runs with `npm test` (scripts/test.mjs: tsc, then node:test).

import { test } from "node:test";
import assert from "node:assert/strict";
import { reposOf } from "./repos.ts";
import type { Board, BranchRow, RepoView } from "./types.ts";

function repo(name: string, project: string | null, branchKeys: string[]): RepoView {
  const branches = branchKeys.map(
    (key): BranchRow => ({ name: `claude/${key.toLowerCase()}-x`, vendor: "claude", issue_key: key, ahead: 1, behind: 0, last_commit_subject: null, last_commit_at: null, issue: null, pr: null }),
  );
  return { name, root: `/src/${name}`, integration_branch: "main", project, rows: [], branches, error: null, unmatched_sessions: [] };
}

const board = (repos: RepoView[]) => ({ repos }) as unknown as Board;

test("NIT-51: a repo that names the project and an unconfigured one holding its keys both show", () => {
  const repos = [repo("app", "NIT", []), repo("tools", null, ["NIT-4"]), repo("site", null, ["WEB-2"]), repo("other", "WEB", ["NIT-9"])];
  assert.deepEqual(
    reposOf(board(repos), "nit").map((r) => r.name),
    ["app", "tools"],
  );
});

test("unconfigured repos match by key when none names the project", () => {
  const repos = [repo("tools", null, ["NIT-4"]), repo("site", null, ["WEB-2"])];
  assert.deepEqual(
    reposOf(board(repos), "NIT").map((r) => r.name),
    ["tools"],
  );
});
