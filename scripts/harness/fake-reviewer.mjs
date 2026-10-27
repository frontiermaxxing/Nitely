// The reviewer of the end-to-end scenario (scenario.e2e.mjs), run by submit as the playbook's
// `[review] commands`: it reads the prompt on stdin and answers in the reviewer's format, alone or
// for a batch, from what each change's diff touches. A change touching `risky.txt` gets one P1
// finding, unless the prompt says the person accepted it; everything else is clean. Previous
// findings are answered fixed. It waits REVIEW_SECONDS first (default 4), so a batch under review
// can be seen on the board.

import { execFileSync } from "node:child_process";

const FINDING = "Risky default: risky.txt, top: changes the default greeting, which the person must choose";

const prompt = await new Promise((resolve) => {
  let text = "";
  process.stdin.setEncoding("utf8");
  process.stdin.on("data", (d) => (text += d));
  process.stdin.on("end", () => resolve(text));
});
await new Promise((r) => setTimeout(r, Number(process.env.REVIEW_SECONDS ?? 4) * 1000));

// A fresh review of a disputed finding (NIT-318): the risky default still holds, as it does in
// the code, so the request goes on to the person.
if (prompt.includes("the change's author disputes it")) {
  console.log(`ANSWER: risky.txt still changes the default greeting.
HOLDS: ${prompt.includes("Risky default") ? "yes" : "no"}`);
  process.exit(0);
}

const touched = (range) => execFileSync("git", ["diff", "--name-only", range], { encoding: "utf8" }).split(/\r?\n/).filter(Boolean);
const accepted = prompt.includes("Risky default") && /\(accepted: /.test(prompt);
const risky = (files) => files.includes("risky.txt") && !accepted;
const out = [];

const changes = [...prompt.matchAll(/^## CHANGE (\d+): (\S+) .*\n.*\nRun `git diff ([^`]+)`/gm)];
if (changes.length > 0) {
  // A batch: one VERDICT per change, its findings named by change.
  for (const m of prompt.matchAll(/^(\d+)\.(\d+)\. \[P\d\]/gm)) out.push(`PREVIOUS ${m[1]}.${m[2]}: fixed`);
  const verdicts = [];
  for (const [, k, , range] of changes) {
    const bad = risky(touched(range));
    if (bad) out.push(`- [P1] CHANGE ${k}: ${FINDING}`);
    verdicts.push(`VERDICT ${k}: ${bad ? "findings" : "clean"}`);
  }
  out.push(...verdicts);
} else {
  const base = prompt.match(/git diff ([^`.]+)\.\.\.HEAD/)?.[1] ?? "main";
  for (const m of prompt.matchAll(/^(\d+)\. \[P\d\]/gm)) out.push(`PREVIOUS ${m[1]}: fixed`);
  const bad = risky(touched(`${base}...HEAD`));
  if (bad) out.push(`- [P1] ${FINDING}`);
  out.push(`VERDICT: ${bad ? "findings" : "clean"}`);
}
console.log(`Reviewed by the harness's fake reviewer.\n${out.join("\n")}`);
