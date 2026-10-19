// One scenario through the whole app (NIT-253), on a throwaway repo with fake agents and a fake
// reviewer, in the harness's isolated app: connect, setup, a crew worker, async submit, review
// alone and batched, gates in a build lane, the land queue landing two changes on one gate, a
// review finding the agent asks the person to accept, its notification, and the signed Accept from
// the Inbox. Each step checks what the app shows (the board, the issue's track and timeline) as
// well as what was recorded, so a break between features that each pass their own tests shows
// here. CI runs it with `npm run app:test` (.github/workflows/app.yml); never on a person's PC
// without NITELY_HARNESS_LOCAL=1 (AGENTS.md, "Driving the real app").

import { test } from "node:test";
import assert from "node:assert/strict";
import { execFileSync, spawn, spawnSync } from "node:child_process";
import { mkdirSync, readFileSync, readdirSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { harnessNitely, launch, nitely, notifications } from "./app.mjs";
import { connect } from "./mcp.mjs";

const HERE = dirname(fileURLToPath(import.meta.url));
const SCREENS = join("target", "harness", "screens");
mkdirSync(SCREENS, { recursive: true });

// Commits the scenario and the land's rebase make: the CI runner has no git identity.
const GIT = { GIT_AUTHOR_NAME: "Harness", GIT_AUTHOR_EMAIL: "harness@example.com", GIT_COMMITTER_NAME: "Harness", GIT_COMMITTER_EMAIL: "harness@example.com" };
Object.assign(process.env, GIT);

const slash = (p) => resolve(p).replace(/\\/g, "/");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const git = (cwd, ...args) => execFileSync("git", args, { cwd, encoding: "utf8", stdio: "pipe" }).trim();

/**
 * The repo's playbook: the fake reviewer, a gate that fails on a file named broken.txt, one build
 * lane, the land queue waiting 15 s for more changes, and a crew of fake claude workers (none until
 * it is turned on, as Nitely's default is).
 */
function playbook({ batch, workers }) {
  const node = slash(process.execPath);
  return `[branch]
integration = "main"

[worktree]
dir = "../my-app-worktrees/{key}-{slug}"

[gate]
commands = ["node check.mjs"]

[review]
reviewer = { "*" = "fake" }
commands = { fake = ["${node}", "${slash(join(HERE, "fake-reviewer.mjs"))}"] }
batch = "${batch}"
batch_window = "20s"
batch_max = 3

[lanes]
count = 1
keep = []

[land]
batch = "all"
batch_window = "15s"

[crew]
workers = ${workers}
agents = ["claude"]

[crew.commands]
claude = ["${node}", "${slash(join(HERE, "fake-agent.mjs"))}", "{nitely}"]
`;
}

/** A throwaway repo with the playbook committed, connected to Nitely as project APP. */
function seedRepo(home) {
  const repo = join(home, "repos", "my-app");
  mkdirSync(join(repo, ".nitely"), { recursive: true });
  git(repo, "init", "-q", "-b", "main");
  writeFileSync(join(repo, "README.md"), "# my-app\n\nA throwaway repo for the harness's end-to-end scenario.\n");
  writeFileSync(join(repo, "check.mjs"), `import { existsSync } from "node:fs";\nif (existsSync("broken.txt")) { console.error("broken.txt is in the tree"); process.exit(1); }\nconsole.log("checked");\n`);
  writeFileSync(join(repo, ".nitely", "playbook.toml"), playbook({ batch: "off", workers: 0 }));
  git(repo, "add", ".");
  git(repo, "commit", "-q", "-m", "Start my-app");
  nitely(home, "connect", repo, "--project", "APP", "--name", "Acme", "--yes");
  return repo;
}

test("one scenario through the app: connect, setup, crew, submit, review alone and batched, lanes, the land queue, a notification and a signed Accept", { timeout: 20 * 60 * 1000 }, async () => {
  let repo;
  const app = await launch({ seed: (home) => (repo = seedRepo(home)) });
  const { page, home } = app;
  const env = { ...process.env, NITELY_HOME: home, NITELY_AGENT_HOME: join(home, "agents"), ...GIT };
  const shot = (name) => page.screenshot(join(SCREENS, `scenario-${name}.png`));
  // What the app itself reads: the issue as its panel shows it, and the board.
  const detail = (key) => page.invoke("issue_detail", { key });
  const kinds = async (key) => (await detail(key)).timeline.map((t) => t.kind);
  const until = async (what, check, timeoutMs = 180000) => {
    const end = Date.now() + timeoutMs;
    let last;
    while (Date.now() < end) {
      last = await check().catch((e) => `error: ${e.message}`);
      if (last && !String(last).startsWith("error:")) return last;
      await sleep(1000);
    }
    throw new Error(`timed out waiting for ${what} (last: ${JSON.stringify(last)?.slice(0, 400)})`);
  };
  const state = async (key) => (await detail(key)).issue.state;
  const nitelyExe = harnessNitely();
  const agent = await connect(nitelyExe, { cwd: repo, env });
  const lands = [];
  try {
    // 1. Connected: the project is in the sidebar, the repo on Now.
    await page.waitFor(`return [...document.querySelectorAll(".nav-project")].some((b) => b.textContent.includes("Acme"));`, { what: "Acme in the sidebar" });
    await page.open("#now");
    await page.waitFor(`return document.body.textContent.includes("my-app");`, { what: "the repo on Now" });
    await shot("1-connected");

    // Human reviewer settings go through the native commands and commit only the previewed file.
    const reviewerViews = await page.invoke("rules", { project: "APP" });
    const reviewerRepo = reviewerViews[0].repo;
    assert.equal(reviewerViews[0].reviewer_editable, true);
    assert.equal(reviewerViews[0].editable, false, "other committed rules remain protected");
    const reviewerEdit = { reviewer: { "*": "fake", codex: "fake" } };
    const reviewerPreview = await page.invoke("reviewer_settings_preview", { repo: reviewerRepo, edit: reviewerEdit });
    assert.match(reviewerPreview.diff, /codex/);
    const reviewerSaved = await page.invoke("reviewer_settings_save", { repo: reviewerRepo, edit: reviewerEdit, preview: reviewerPreview });
    assert.equal(reviewerSaved.view.reviewer.from, "project");
    assert.equal(reviewerSaved.view.reviewer.value.codex, "fake");
    assert.equal(git(repo, "show", "--format=", "--name-only", "HEAD"), ".nitely/playbook.toml");
    const staleReviewerSave = await page.eval(`
      try {
        await window.__TAURI_INTERNALS__.invoke("reviewer_settings_save", ${JSON.stringify({ repo: reviewerRepo, edit: reviewerEdit, preview: reviewerPreview })});
        return "unexpected success";
      } catch (error) { return String(error); }
    `);
    assert.match(staleReviewerSave, /changed since/);

    // 2. Setup, as the setup sweep does it: connect filed the setup issue, the agent takes it, writes
    // a brief, files the first issues and marks setup done. Only the first new issue is Todo: the
    // crew takes it; the agent here takes the others itself.
    const project = await agent.call("get_project", {});
    assert.equal(project.setup ?? null, null, "a new project is not set up yet");
    const setup = await agent.call("next_task", {});
    const setupKey = setup.issue?.key ?? setup.key;
    assert.match(JSON.stringify(setup), /Set up Acme/, `next_task hands out connect's setup issue: ${JSON.stringify(setup).slice(0, 300)}`);
    await agent.call("set_project_brief", { brief: "my-app greets people. `node check.mjs` is the gate.", revision: 0 });
    const filed = await agent.call("file_issues", {
      issues: [
        { ref: "greet", title: "Add a greeting", kind: "feature", body: "Say hello.", evidence: "harness setup", priority: 2, state: "Todo" },
        { ref: "bye", title: "Add a farewell", kind: "feature", body: "Say goodbye.", evidence: "harness setup", priority: 3, state: "Backlog" },
        { ref: "default", title: "Choose the default greeting", kind: "feature", body: "Pick which greeting is the default.", evidence: "harness setup", priority: 3, state: "Backlog" },
        { ref: "welcome", title: "Add a welcome page", kind: "feature", body: "Welcome people.", evidence: "harness setup", priority: 3, state: "Backlog" },
      ],
    });
    const { greet: A, bye: B, default: C, welcome: D } = filed.keys;
    assert.ok(A && B && C && D, `four issues filed: ${JSON.stringify(filed)}`);
    await agent.call("mark_setup_complete", { sha: git(repo, "rev-parse", "HEAD") });
    assert.ok((await agent.call("get_project", {})).setup, "setup is marked done");
    await agent.call("update_issue", { key: setupKey, state: "Done", evidence: "Brief written, first issues filed, setup marked complete." });
    assert.equal(await state(setupKey), "Done");

    // Focus is set by the person in the app and read by external agents and Crew alike.
    // An empty focused goal must not spill into the unrelated urgent issue (or the greeting).
    const focusedGoal = await agent.call("save_goal", { title: "A welcoming first run", outcome: "New users see a greeting." });
    const urgent = await agent.call("file_issue", { title: "Repair the export checksum", kind: "bug", body: "The checksum is wrong.", evidence: "harness focus scenario", priority: 1 });
    await page.invoke("set_project_focus", { project: "APP", goal: focusedGoal.id, allowUrgent: false });
    assert.equal((await agent.call("get_project", {})).focus.goal, focusedGoal.id);
    const waitingForFocus = await agent.call("next_task", {});
    assert.equal(waitingForFocus.issue, null, "focus never spills into other Todo work");
    assert.match(waitingForFocus.message, /focus/i, "the agent is told why it waits");
    await page.invoke("set_issue_goal", { key: A, goal: focusedGoal.id });

    // The person turns the crew on: one worker.
    writeFileSync(join(repo, ".nitely", "playbook.toml"), playbook({ batch: "off", workers: 1 }));
    git(repo, "commit", "-q", "-am", "Turn the crew on");

    // 3. The crew: the app keeps a supervisor for a playbook with workers (this build's own nitely,
    // under the harness), which starts the fake claude worker on ${A}. It submits without
    // waiting; the gate runs in the build lane and the review runs alone (batching is off). Once
    // it passes, the supervisor lands it: a crew's change needs nobody.
    await until(`a crew worker on ${A}`, async () => (await kinds(A)).includes("crew_started"));
    await until(`${A} reviewed`, async () => ["In Review", "Done"].includes(await state(A)), 240000);
    await page.open(`#now@${A}`);
    await page.waitFor(`return document.querySelector(".panel")?.textContent.includes("Add a greeting");`, { what: `${A}'s panel` });
    await shot("3-crew-reviewed");
    await until(`the crew to land ${A}`, async () => (await state(A)) === "Done", 240000);
    const one = await detail(A);
    for (const k of ["crew_started", "claimed", "worktree", "status", "submit", "lane_run", "gate_passed", "review_started", "handoff", "landed"]) {
      assert.ok(one.timeline.some((t) => t.kind === k), `${A}'s timeline has ${k}: ${one.timeline.map((t) => t.kind).join(", ")}`);
    }
    assert.equal(one.issue.track.station, "done");
    assert.equal(one.reviews[0].verdict, "pass");
    assert.equal(one.reviews[0].batch_id ?? null, null, "reviewed alone: the playbook batches nothing yet");
    assert.equal(await state(urgent.key), "Todo", "Crew respected focus even with an unrelated urgent issue ready");
    assert.equal((await agent.call("next_task", {})).issue, null, "finished focused work still does not spill over");
    // Pause Crew before testing the external agent's urgent selection, so it cannot race the claim.
    await page.invoke("crew_pause", { project: "APP", paused: true });
    await page.invoke("set_project_focus", { project: "APP", goal: focusedGoal.id, allowUrgent: true });
    const interrupt = await agent.call("next_task", {});
    assert.equal(interrupt.issue.key, urgent.key, "the person's urgent-interruption choice is honored by external agents");
    await agent.call("update_issue", { key: urgent.key, state: "Canceled", reason: "The focus scenario has verified urgent selection." });
    await page.invoke("set_project_focus", { project: "APP", goal: null, allowUrgent: true });
    assert.equal((await page.invoke("project_focus", { project: "APP" })).goal, null, "clearing focus restores ordinary selection");
    await page.invoke("crew_pause", { project: "APP", paused: false });
    await page.open("#now");

    // 4. Batched review: the crew goes off (the rest is the agent's here) and the playbook now
    // reviews small changes together. The agent makes three changes and submits them; one touches
    // risky.txt, which the reviewer finds a P1 in.
    writeFileSync(join(repo, ".nitely", "playbook.toml"), playbook({ batch: "small", workers: 0 }));
    git(repo, "commit", "-q", "-am", "Crew off; review small changes together");
    const change = async (key, files) => {
      await agent.call("update_issue", { key, state: "In Progress" });
      const started = await agent.call("start_task", { key });
      for (const [name, text] of Object.entries(files)) writeFileSync(join(started.worktree, name), text);
      git(started.worktree, "add", ".");
      git(started.worktree, "commit", "-q", "-m", `${key}: the change`);
      return started.worktree;
    };
    const two = await change(B, { "farewell.txt": "Goodbye\n" });
    const three = await change(C, { "risky.txt": "Hello, stranger\n" });
    const four = await change(D, { "welcome.txt": "Welcome\n" });
    const inWorktree = async (worktree) => connect(nitelyExe, { cwd: worktree, env });
    for (const [key, wt] of [[B, two], [C, three], [D, four]]) {
      const there = await inWorktree(wt);
      await there.call("submit", { key, solution_title: "Add a greeting variant", solution_summary: "Add the requested greeting file while retaining the existing greetings." });
      await there.close();
    }
    // While its reviewer runs, the batch is one row on the board, each issue's station says so.
    await page.waitFor(`return [...document.querySelectorAll(".batch-row")].some((r) => r.textContent.includes("Review batch · 3 changes"));`, { timeoutMs: 120000, what: "the review batch on the board" });
    await shot("4-review-batch");
    await until("the batched reviews", async () => (await state(B)) === "In Review" && (await state(D)) === "In Review" && (await detail(C)).reviews.length > 0);
    const [d2, d3, d4] = [await detail(B), await detail(C), await detail(D)];
    assert.ok(d2.reviews[0].batch_id && d2.reviews[0].batch_id === d3.reviews[0].batch_id && d3.reviews[0].batch_id === d4.reviews[0].batch_id, "one batch reviewed all three");
    assert.equal(d2.reviews[0].solution.title, "Add a greeting variant");
    assert.equal(d2.reviews[0].gate.commit, d2.reviews[0].commit_sha);
    assert.equal(d2.reviews[0].verdict, "pass");
    assert.equal(d4.reviews[0].verdict, "pass");
    assert.equal(d3.reviews[0].verdict, "changes", "its own P1, from the same batch");
    assert.match(d3.reviews[0].findings[0].text, /Risky default/);
    assert.ok(d2.timeline.some((t) => t.kind === "review_started" && /in a batch with /.test(t.summary ?? "") && (t.summary ?? "").includes(C) && (t.summary ?? "").includes(D)), "the timeline says which batch");
    assert.equal(d3.batches.reviews[0].sent_back?.why, "own", "and why the batch sent it back");
    assert.equal(d3.issue.state, "In Progress", "back to its agent");

    // 5. The agent asks the person to accept the finding: a decision, told as a notification
    // (the app is not in front), and decided in the Inbox with a signed Accept.
    const asked = await agent.call("request_acceptance", { key: C, finding: "1", why: "The default greeting is the person's call; the reviewer cannot know it." });
    await until("the decision's notification", async () => notifications(home).find((n) => `${n.title} ${n.body}`.includes(C)));
    await page.open("#inbox");
    await page.click({ css: ".inbox-row", text: C });
    await page.waitFor(`return document.querySelector(".dd-decide-finding input") !== null;`, { what: "the finding's Accept" });
    await page.click({ css: ".dd-decide-finding input" });
    await page.type("Hello, stranger is the default we want");
    await shot("5-accept");
    await page.click({ css: ".dd-decide-finding button", text: "Accept" });
    await page.waitFor(`return document.querySelector(".dd-outcome.done")?.textContent;`, { what: "the Accept's outcome" });
    const answer = await agent.call("get_answer", { id: asked.id });
    assert.match(JSON.stringify(answer), /Accepted: Hello, stranger is the default we want/);
    // The agent reads the answer and goes on, as a real one does.
    await agent.call("report", { key: C, text: "The person accepted the finding; the change can land." });

    // 6. Accepted, the finding no longer holds the change back: it is in review again, to land.
    // (Landing it is left out: nothing the harness app's throwaway key signed may land, NIT-157.)
    await until(`${C} back in review`, async () => (await state(C)) === "In Review");
    const accepted = await detail(C);
    assert.equal(accepted.issue.track.station, "review");
    assert.ok(accepted.timeline.some((t) => t.kind === "note" && /Accepted .*Risky default/.test(t.summary ?? "")), "the Accept is on its timeline");

    // 7. The land queue: ${B} and ${D} land together, stacked and gated once in the lane; the
    // board shows the run while it waits for more.
    const land = (key) => {
      const child = spawn(nitelyExe, ["land", key], { cwd: repo, env, stdio: ["ignore", "pipe", "pipe"] });
      let out = "";
      child.stdout.on("data", (d) => (out += d));
      child.stderr.on("data", (d) => (out += d));
      const done = new Promise((r) => child.once("exit", (code) => r({ key, code, out })));
      lands.push(child);
      return done;
    };
    const both = Promise.all([land(B), sleep(2000).then(() => land(D))]);
    await page.open("#now");
    await page.waitFor(`return document.body.textContent.includes("in the land queue");`, { timeoutMs: 60000, what: "the land queue's length by the lanes" });
    await shot("7-land-queue");
    for (const r of await both) assert.equal(r.code, 0, `${r.key} landed:\n${r.out}`);
    const l1 = (await detail(B)).batches.lands[0];
    assert.equal(l1.state, "landed", JSON.stringify(l1));
    assert.deepEqual([...l1.run.landed].sort(), [B, D].sort(), "one queue run landed both");
    assert.equal(l1.run.gates, 1, "on one gate");
    for (const key of [B, D]) assert.equal(await state(key), "Done");

    // 8. What landed is on main; every track says where it ended.
    const files = git(repo, "ls-files");
    for (const f of [`${A.toLowerCase()}.txt`, "farewell.txt", "welcome.txt"]) assert.ok(files.includes(f), `${f} is on main`);
    assert.ok(!files.includes("risky.txt"), "the accepted change waits to be landed");
    const tracks = await Promise.all([A, B, C, D].map(async (k) => (await detail(k)).issue.track.station));
    assert.deepEqual(tracks, ["done", "done", "review", "done"]);

    // Nothing waits on the person now.
    await page.open("#inbox");
    await page.waitFor(`return document.querySelectorAll(".inbox-row").length === 0;`, { timeoutMs: 30000, what: "the Inbox to empty" });
    await shot("8-done");
  } catch (e) {
    await shot("failed").catch(() => {});
    // Where every issue stood, and what it did last.
    for (const key of ["APP-1", "APP-2", "APP-3", "APP-4", "APP-5"]) {
      const d = await detail(key).catch(() => null);
      if (d) console.error(`${key} ${d.issue.state} (${d.issue.track.station}): ${d.timeline.slice(-6).map((t) => `${t.kind}: ${t.summary ?? ""}`).join(" | ")}`);
    }
    console.error(app.log());
    console.error(`--- the crew's logs ---\n${crewLogs(home)}`);
    throw e;
  } finally {
    for (const c of lands) if (c.exitCode === null) c.kill();
    await agent.close();
    // The crew's supervisor and any submit job run apart from the app, on this build's nitely:
    // they end with the run, or they would hold its folders.
    stopAll(nitelyExe);
    await app.close();
  }
});

/** Every process still running `exe` (the harness's own nitely), ended. */
function stopAll(exe) {
  if (process.platform !== "win32") {
    spawnSync("pkill", ["-f", exe], { stdio: "ignore" });
    return;
  }
  const path = exe.replace(/'/g, "''");
  spawnSync("powershell.exe", ["-NoProfile", "-Command", `Get-CimInstance Win32_Process | Where-Object { $_.ExecutablePath -eq '${path}' } | ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }`], { stdio: "ignore" });
}

/** For a failed run: what the crew's supervisor and each of its workers said. */
function crewLogs(home) {
  const dir = join(home, "crew", "APP");
  try {
    return readdirSync(dir)
      .filter((f) => f.endsWith(".log"))
      .map((f) => `--- ${f} ---\n${readFileSync(join(dir, f), "utf8")}`)
      .join("\n");
  } catch {
    return "";
  }
}
