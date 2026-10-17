// `node scripts/dco.mjs BASE HEAD`: fails when a commit in BASE..HEAD (merges aside) has no
// `Signed-off-by:` line with its author's email, the Developer Certificate of Origin sign-off
// CONTRIBUTING.md asks of outside contributors. The DCO workflow runs it on pull requests from forks.

import { execFileSync } from "node:child_process";
import { pathToFileURL } from "node:url";

/** The commits whose message has no sign-off by their author. */
export function unsigned(commits) {
  return commits.filter((c) => {
    const signers = [...c.message.matchAll(/^Signed-off-by:\s*.+?<([^>\s]+)>\s*$/gim)].map((m) => m[1].toLowerCase());
    return !signers.includes(c.email.toLowerCase());
  });
}

/** BASE..HEAD's commits, merges aside, as `{ sha, email, subject, message }`. */
export function commitsBetween(base, head) {
  const out = execFileSync("git", ["log", "--no-merges", "--format=%H%x00%ae%x00%B%x00%x01", `${base}..${head}`], { encoding: "utf8" });
  return out
    .split("\x00\x01")
    .map((e) => e.replace(/^\s+/, ""))
    .filter(Boolean)
    .map((e) => {
      const [sha, email, message = ""] = e.split("\x00");
      return { sha, email, message, subject: message.split("\n")[0] };
    });
}

if (import.meta.url === pathToFileURL(process.argv[1] ?? "").href) {
  const [base, head] = process.argv.slice(2);
  if (!base || !head) {
    console.error("usage: node scripts/dco.mjs BASE HEAD");
    process.exit(2);
  }
  const commits = commitsBetween(base, head);
  const missing = unsigned(commits);
  if (missing.length === 0) {
    console.log(`All ${commits.length} commit(s) are signed off by their authors.`);
  } else {
    console.error(`${missing.length} of ${commits.length} commit(s) have no Signed-off-by line with their author's email:`);
    for (const c of missing) console.error(`  ${c.sha.slice(0, 7)} ${c.subject} (author ${c.email})`);
    console.error("\nSign them off with `git rebase --signoff " + base.slice(0, 7) + "` (or `git commit --amend -s` for the last one),");
    console.error("then push again. CONTRIBUTING.md explains the sign-off.");
    process.exit(1);
  }
}
