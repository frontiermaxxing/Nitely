// The workflows' concurrency groups (NIT-540): a pull request whose head is already on main runs
// with github.ref = refs/heads/main, so a group keyed by the ref alone puts it beside main's push
// run, and a pull request cancels in progress: every push run on main was cancelled.

import { test } from "node:test";
import assert from "node:assert/strict";
import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";

const dir = ".github/workflows";

test("a workflow that cancels pull request runs keeps them apart from push runs", () => {
  const checked = [];
  for (const file of readdirSync(dir).filter((f) => /\.ya?ml$/.test(f))) {
    const text = readFileSync(join(dir, file), "utf8").replace(/\r\n/g, "\n");
    const block = text.match(/^concurrency:\n((?:[ \t]+.*\n)+)/m);
    if (!block || !/cancel-in-progress:.*pull_request/.test(block[1])) continue;
    const group = block[1].match(/group:\s*(.+)/)[1];
    assert.match(group, /github\.event_name/, `${file}: the group must name the event: ${group}`);
    assert.match(group, /github\.event\.pull_request\.number/, `${file}: a pull request's group must be its own: ${group}`);
    checked.push(file);
  }
  assert.deepEqual(checked.sort(), ["app.yml", "ci.yml", "perf.yml"]);
});
