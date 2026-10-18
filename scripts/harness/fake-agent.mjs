// The crew worker of the end-to-end scenario (scenario.e2e.mjs), started by the crew's supervisor
// as the playbook's `[crew.commands] claude`: `node fake-agent.mjs {nitely}`, in the issue's
// worktree, with the brief on stdin. It does what the brief asks of a real agent, through the same
// MCP tools: reads the issue, reports, makes the change and commits it, submits without waiting,
// leaves a handoff note and ends.

import { execFileSync } from "node:child_process";
import { writeFileSync } from "node:fs";
import { connect } from "./mcp.mjs";

// The crew fills {nitely} in as a TOML string (for Codex's -c settings): a literal string in single
// quotes when the path has backslashes (Windows), else a basic one in double quotes.
const arg = process.argv[2];
const nitely = arg.startsWith("'") ? arg.slice(1, -1) : arg.startsWith('"') ? JSON.parse(arg) : arg;
const brief = await new Promise((resolve) => {
  let text = "";
  process.stdin.setEncoding("utf8");
  process.stdin.on("data", (d) => (text += d));
  process.stdin.on("end", () => resolve(text));
});
const key = brief.match(/work on one issue, ([A-Z][A-Z0-9]*-\d+)/)?.[1];
if (!key) {
  console.error(`no issue in the brief:\n${brief}`);
  process.exit(1);
}

const agent = await connect(nitely, { cwd: process.cwd(), env: process.env });
try {
  const issue = await agent.call("get_issue", { key });
  await agent.call("report", { key, text: `Working on ${issue.issue.title}` });
  writeFileSync(`${key.toLowerCase()}.txt`, `${issue.issue.title}\n`);
  const git = (...args) => execFileSync("git", args, { stdio: "pipe" });
  git("add", ".");
  git("-c", "user.name=Fake agent", "-c", "user.email=agent@example.com", "commit", "-q", "-m", `${issue.issue.title} (${key})`);
  const submitted = await agent.call("submit", { key });
  console.log(JSON.stringify(submitted));
  await agent.call("note", { key, text: "Made the change and submitted it; the gate and the review run in the background.", handoff: true });
} finally {
  await agent.close();
}
