import { useEffect, useState } from "react";
import { invoke } from "./backend";
import { open } from "@tauri-apps/plugin-dialog";
import type { Applied, ConnectPlan, FirstCommitPreview } from "./types";
import { bytesWords } from "./lanes";
import { Checkbox } from "./ui/Toggles";
import { AgentChanges, pendingChanges } from "./Agents";

const PLAYBOOK_SOURCE: Record<string, string> = {
  repo: "the repo's own .nitely/playbook.toml",
  existing: "the one Nitely already keeps for it",
  generated: "a starting one from the repo's branch and scripts",
};

/** How Nitely says a folder is in no git repo (connect.rs NOT_A_REPO): the page then offers to make it one. */
const NOT_A_REPO = "is not in a git repo";

/** A made-up repo path in the platform's usual form, for placeholders and examples. */
export const exampleRepoPath = () => {
  const platform = navigator.platform;
  if (platform.startsWith("Win")) return "C:\\Code\\my-app";
  return platform.startsWith("Mac") ? "/Users/you/code/my-app" : "/home/you/code/my-app";
};

/** Pick a repo folder, see everything Nitely will set up, then add it. */
export function AddProject(props: { onAdded: (projectKey: string) => void }) {
  const [path, setPath] = useState("");
  const [key, setKey] = useState("");
  const [name, setName] = useState("");
  const [plan, setPlan] = useState<ConnectPlan | null>(null);
  const [applied, setApplied] = useState<Applied | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [made, setMade] = useState<string | null>(null);

  const load = async (p: string, k: string, n: string) => {
    if (!p.trim()) return;
    setBusy(true);
    try {
      const next = await invoke<ConnectPlan>("connect_plan", { path: p, key: k || null, name: n || null });
      setPlan(next);
      setKey(next.project_key);
      setName(next.project_name);
      setError(null);
    } catch (e) {
      setPlan(null);
      setError(String(e));
    }
    setBusy(false);
  };

  useEffect(() => {
    setApplied(null);
    setMade(null);
  }, [path]);

  // A folder in no git repo can be made one here, and then looked at again (NIT-652).
  const makeRepo = async () => {
    setBusy(true);
    try {
      const said = await invoke<string>("connect_init", { path });
      setBusy(false);
      await load(path, key, name);
      setMade(said);
    } catch (e) {
      setError(String(e));
      setBusy(false);
    }
  };

  const choose = async () => {
    const picked = await open({ directory: true, title: "Choose a repo folder" }).catch(() => {
      // Only the desktop app has a folder picker; the browser preview does not.
      setError("The folder picker needs the desktop app. Type the repo's path instead.");
      return null;
    });
    if (typeof picked === "string") {
      setPath(picked);
      setKey("");
      setName("");
      load(picked, "", "");
    }
  };

  const add = async () => {
    setBusy(true);
    try {
      const result = await invoke<Applied>("connect_apply", { path, key: key || null, name: name || null });
      setApplied(result);
      setError(null);
      if (result.failed.length === 0) props.onAdded(result.project);
    } catch (e) {
      setError(String(e));
    }
    setBusy(false);
  };

  const changes = plan ? pendingChanges(plan.agents) : 0;

  return (
    <div className="page page-form">
      <div className="page-head">
        <h1>Add a project</h1>
      </div>
      <p className="page-lead">
        Point Nitely at a repo you already work in. It creates the project, watches the repo and its worktrees, and
        connects your agents so they take work from Nitely and report back. Nothing is written into the repo.
      </p>

      <form
        className="answer"
        onSubmit={(e) => {
          e.preventDefault();
          load(path, "", "");
        }}
      >
        <input value={path} onChange={(e) => setPath(e.target.value)} placeholder={exampleRepoPath()} />
        <button type="button" className="btn" onClick={choose}>Choose…</button>
        <button className="btn" disabled={busy || !path.trim()}>Look</button>
      </form>
      {error && (
        <p className="error">
          {error}
          {error.includes(NOT_A_REPO) && (
            <>
              {" "}
              <button type="button" className="btn small" disabled={busy} onClick={makeRepo}>
                Make it a git repo
              </button>
            </>
          )}
        </p>
      )}
      {made && <p className="muted small">{made}</p>}

      {plan && (
        <>
          <section>
            <h2>Project</h2>
            <div className="fields">
              <label>
                Key
                <input value={key} onChange={(e) => setKey(e.target.value.toUpperCase())} onBlur={() => load(path, key, name)} className="mono" />
              </label>
              <label>
                Name
                <input value={name} onChange={(e) => setName(e.target.value)} onBlur={() => load(path, key, name)} />
              </label>
            </div>
            <p className="small">
              {plan.project_exists ? "Joins the existing project" : "A new project"} for <span className="mono">{plan.root}</span>.
              Tasks branch from and land on <span className="mono">{plan.integration_branch}</span>.
            </p>
            <p className="small">
              Playbook: {PLAYBOOK_SOURCE[plan.playbook.source] ?? plan.playbook.source}, kept at{" "}
              <span className="mono">{plan.playbook.path}</span>.
            </p>
            {(plan.playbook.source === "generated" || !plan.project_exists) && (
              <p className="small">
                {plan.playbook.source === "generated" && "Until you approve its review rules, every change waits for you. "}
                {!plan.project_exists && (
                  <>
                    Nitely files a first issue: an agent sweeps the repo without changing it, seeds the board and proposes those
                    rules for you to approve in the Inbox (in Claude Code, <span className="mono">/nitely-setup</span>).
                  </>
                )}
              </p>
            )}
            {plan.playbook.text && (
              <details>
                <summary className="small">Show the playbook</summary>
                <pre className="file">{plan.playbook.text}</pre>
              </details>
            )}
          </section>

          {plan.first_commit_needed && (
            <FirstCommit
              root={plan.root}
              branch={plan.integration_branch}
              onMade={async (said) => {
                await load(path, key, name);
                setMade(said);
              }}
            />
          )}

          <AgentChanges agents={plan.agents} />

          {plan.warnings.map((w) => (
            <p key={w} className="error small">{w}</p>
          ))}

          {!applied && (
            <div className="actions">
              <button className="btn primary" disabled={busy || !key.trim() || plan.first_commit_needed} onClick={add}>
                Add {key || "project"}
                {changes > 0 ? ` and make ${changes} change${changes === 1 ? "" : "s"}` : ""}
              </button>
            </div>
          )}
          {applied && (
            <section>
              <h2>Done</h2>
              <ul className="small">
                {applied.done.map((d) => (
                  <li key={d}>{d}</li>
                ))}
              </ul>
              {applied.failed.length > 0 && (
                <ul className="small error">
                  {applied.failed.map((f) => (
                    <li key={f}>{f}</li>
                  ))}
                </ul>
              )}
            </section>
          )}
        </>
      )}
    </div>
  );
}

const LEFT_OUT: Record<string, string> = { build: "build or dependency folder", large: "over 50 MB", secret: "looks like a secret" };

/**
 * A repo with no commits yet (one "Make it a git repo" just made, or any fresh one) gets its first
 * commit here, so the files are in every agent's worktree (NIT-685). It shows what goes in and what
 * stays out first, offers a .gitignore built from what is there, asks for a git identity when git
 * has none, and commits once, locally. It opens on its own, so the auto path is one confirmation.
 */
function FirstCommit(props: { root: string; branch: string; onMade: (said: string) => void }) {
  const [preview, setPreview] = useState<FirstCommitPreview | null>(null);
  // The .gitignore lines kept, or, when the repo has its own .gitignore, the paths left out.
  const [kept, setKept] = useState<Set<string>>(new Set());
  const [gitName, setGitName] = useState("");
  const [gitEmail, setGitEmail] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    invoke<FirstCommitPreview>("first_commit_preview", { path: props.root })
      .then((p) => {
        setPreview(p);
        setKept(new Set(p.gitignore ?? p.flagged.map((f) => f.path)));
      })
      .catch((e) => setError(String(e)));
  }, [props.root]);

  const toggle = (item: string) =>
    setKept((k) => {
      const next = new Set(k);
      if (next.has(item)) next.delete(item);
      else next.add(item);
      return next;
    });

  const commit = async () => {
    if (!preview) return;
    setBusy(true);
    try {
      const items = preview.gitignore ?? preview.flagged.map((f) => f.path);
      const chosen = items.filter((i) => kept.has(i));
      const said = await invoke<string>("first_commit", {
        path: props.root,
        gitignore: preview.gitignore ? chosen : null,
        exclude: preview.gitignore ? [] : chosen,
        name: preview.needs_identity ? gitName : null,
        email: preview.needs_identity ? gitEmail : null,
      });
      setError(null);
      props.onMade(said);
    } catch (e) {
      setError(String(e));
    }
    setBusy(false);
  };

  const items = preview ? (preview.gitignore ?? preview.flagged.map((f) => f.path)) : [];
  const identityMissing = !!preview?.needs_identity && (!gitName.trim() || !gitEmail.trim());
  return (
    <section>
      <h2>First commit</h2>
      <p className="small">
        <span className="mono">{props.branch}</span> has no commits yet. Agents start their worktrees from it, so files
        never committed would be missing from every one. Nitely makes one local commit, never pushed.
      </p>
      {!preview && !error && <p className="muted small">Looking at the folder…</p>}
      {preview && (
        <>
          <p className="small">
            {preview.files} file{preview.files === 1 ? "" : "s"}, {bytesWords(preview.bytes)}
            {preview.largest.length > 0 && <>; the largest folders: {preview.largest.map(([f, b]) => `${f} (${bytesWords(b)})`).join(", ")}</>}.
          </p>
          {items.length > 0 && (
            <>
              <p className="small">
                {preview.gitignore ? "There is no .gitignore. This one keeps out what should not go in; untick a line to commit it after all:" : "Left out of the commit; untick one to commit it after all:"}
              </p>
              <ul className="small first-commit-items">
                {items.map((i) => {
                  const why = preview.gitignore ? undefined : preview.flagged.find((f) => f.path === i);
                  return (
                    <li key={i}>
                      <Checkbox
                        checked={kept.has(i)}
                        onChange={() => toggle(i)}
                        label={
                          <>
                            <span className="mono">{i}</span>
                            {why && <span className="muted"> ({LEFT_OUT[why.reason]})</span>}
                          </>
                        }
                      />
                    </li>
                  );
                })}
              </ul>
              {preview.gitignore && (
                <p className="muted small">
                  Found: {preview.flagged.map((f) => `${f.path} (${LEFT_OUT[f.reason]})`).join(", ")}.
                </p>
              )}
            </>
          )}
          {preview.needs_identity && (
            <div className="fields">
              <label>
                Your name, for git
                <input value={gitName} onChange={(e) => setGitName(e.target.value)} />
              </label>
              <label>
                Your email, for git
                <input value={gitEmail} onChange={(e) => setGitEmail(e.target.value)} />
              </label>
            </div>
          )}
          {preview.needs_identity && <p className="muted small">Git has no name and email yet; they are set for this repo only.</p>}
          <div className="actions">
            <button className="btn primary" disabled={busy || identityMissing || preview.files === 0} onClick={commit}>
              Make the first commit
            </button>
          </div>
        </>
      )}
      {error && <p className="error small">{error}</p>}
    </section>
  );
}
