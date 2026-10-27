import { useCallback, useEffect, useMemo, useState } from "react";
import { invoke, listen } from "../backend";
import type { AgentPlan, AgentsUpdated, InstallPlan, Project, Tool } from "../types";
import { AgentChanges } from "../Agents";
import { AddProject } from "../AddProject";
import { Button } from "../ui";
import { canContinue, openingStep, STEPS, stepStates, stepSummary, type SetupFacts, type StepId } from "./steps";
import { SignInStep, ToolsStep, type InstallState } from "./ToolsStep";
import { SkillsStep } from "./SkillsStep";
import "./firstRun.css";

const STEP_KEY = "nitely.firstRun.step";
const SEEN_KEY = "nitely.firstRun.seen";
const CLOSED_KEY = "nitely.firstRun.closed";

// Per-viewer conveniences: where the flow was left, what was seen, and whether it was closed. Any
// of them may be missing (a private window, cleared storage); the flow still works without.
function read<T>(key: string, fallback: T): T {
  try {
    const raw = localStorage.getItem(key);
    return raw === null ? fallback : (JSON.parse(raw) as T);
  } catch {
    return fallback;
  }
}
function write(key: string, value: unknown) {
  try {
    localStorage.setItem(key, JSON.stringify(value));
  } catch {
    // Storage refused: the flow carries on, only without remembering.
  }
}

/** Whether the person closed the setup: then the app does not open it by itself again. */
export const setupClosed = () => read<boolean>(CLOSED_KEY, false);

const TEXT: Record<StepId, { title: string; lede: string; next: string; skip?: string }> = {
  welcome: {
    title: "Set up Nitely on this machine",
    lede: "Nitely runs your coding agents on your own projects and keeps their work on a board you can see. First it needs a few tools and a project to work in. Each step below has a button for what used to be a command.",
    next: "Start",
    skip: "I've done this before",
  },
  tools: {
    title: "Install the tools your agents use",
    lede: "Each tool installs with its official installer, as your user. Nitely asks for admin only when the installer itself needs it, and says so before it runs.",
    next: "Continue",
  },
  signin: { title: "Sign in to each tool", lede: "Each tool signs in on its own. Sign in opens its login in a terminal window; Nitely only checks the result.", next: "Continue", skip: "Skip for now" },
  project: { title: "Add your first project", lede: "Pick a folder with code in it. Nitely keeps its board, branches and worktrees beside it and never touches other folders.", next: "Continue", skip: "Skip for now" },
  agents: {
    title: "Connect your agents to Nitely",
    lede: "Each agent gets the nitely tools (an MCP server) and the skills, so it can take tasks, report and hand in its work. Here is exactly what will be written.",
    next: "Continue",
    skip: "Skip for now",
  },
  skills: { title: "Three things to ask your agents", lede: "The skills are installed with your agents. Each is a short prompt you paste into an agent; copy one to start.", next: "Open Nitely" },
};

const WELCOME_DOES = [
  ["The tools", "git, and Claude Code or Codex (or both). GitHub CLI if you want pull requests."],
  ["Sign-ins", "Each tool's own login, in its own window."],
  ["Your first project", "A folder of code, made a git repository if it isn't one."],
  ["Your agents", "The nitely tools and skills in each agent you have."],
  ["The skills", "Three prompts that start the work."],
];
const WELCOME_NEVER = [
  ["No admin unless an installer needs it", "Everything installs as you. When a platform installer asks for admin, Nitely tells you before it runs."],
  ["Never sees your logins", "You sign in inside each tool's own window. Nitely only asks each tool whether it's signed in."],
  ["Nothing outside the folders you pick", "Your code stays where it is. Settings it writes are backed up first and can be undone."],
];

/**
 * The first-run setup (NIT-690), from the Claude Design artboards: Welcome, Tools, Sign in, First
 * project, Agents, Skills. Shown on first launch when no project is connected or a needed tool is
 * missing, and reopened from Settings, where it opens on the first step not done. It can be left at
 * any step and comes back there; a step is done when what it sets up reports ready.
 */
export function FirstRun(props: {
  reopened: boolean;
  projects: Project[];
  agents: AgentPlan[] | null;
  onAgentsChanged: () => void;
  onProjectAdded: (key: string) => void;
  /** Close (from Settings, back to it) or finish: the app shows its views again. */
  onClose: (finished: boolean) => void;
}) {
  const [tools, setTools] = useState<Tool[] | null>(null);
  const [installs, setInstalls] = useState<Record<string, InstallState>>({});
  const [plans, setPlans] = useState<InstallPlan[]>([]);
  const [signingIn, setSigningIn] = useState<string[]>([]);
  const [signInErrors, setSignInErrors] = useState<Record<string, string>>({});
  const [checking, setChecking] = useState(false);
  // Reopened from Settings, Welcome was seen: the setup ran before.
  const [seen, setSeen] = useState<StepId[]>(() => {
    const s = read<StepId[]>(SEEN_KEY, []);
    return props.reopened && !s.includes("welcome") ? [...s, "welcome"] : s;
  });
  const facts: SetupFacts = useMemo(() => ({ tools, projects: props.projects, agents: props.agents, seen }), [tools, props.projects, props.agents, seen]);
  const [step, setStep] = useState<StepId | null>(null);

  const check = useCallback(async () => {
    setChecking(true);
    try {
      setTools(await invoke<Tool[]>("tools_check"));
    } catch {
      setTools([]);
    }
    // How each missing tool would install: the exact command, shown before it runs (NIT-688).
    invoke<InstallPlan[]>("tool_install_plans").then(setPlans, () => setPlans([]));
    setChecking(false);
  }, []);

  // Each line an installer prints, as it prints it.
  useEffect(() => {
    const off = listen<{ tool: string; line: string }>("tool-install", (e) => {
      const { tool, line } = e.payload;
      setInstalls((all) => (all[tool]?.state === "installing" ? { ...all, [tool]: { ...all[tool], log: [...all[tool].log, line].slice(-8) } } : all));
    });
    return () => {
      off.then((f) => f());
    };
  }, []);

  // Each tool's own login, in a terminal window of the person's (NIT-689): Nitely never sees it,
  // and checks again when the window comes back into focus.
  const signIn = async (command: string) => {
    setSignInErrors((all) => {
      const rest = { ...all };
      delete rest[command];
      return rest;
    });
    try {
      await invoke("tool_sign_in", { command });
      setSigningIn((all) => (all.includes(command) ? all : [...all, command]));
    } catch (e) {
      setSignInErrors((all) => ({ ...all, [command]: String(e) }));
    }
  };

  // One install at a time, as confirmed by the click: the plan's command, as shown.
  const install = async (tool: string) => {
    const plan = plans.find((p) => p.tool === tool);
    setInstalls((all) => ({ ...all, [tool]: { state: "installing", progress: plan ? `Installing with ${plan.via === "Vendor" ? "its own installer" : plan.via.toLowerCase()}` : "Installing", log: [] } }));
    try {
      const after = await invoke<Tool>("install_tool", { tool });
      setTools((all) => (all ? all.map((t) => (t.command === tool ? after : t)) : all));
      // A new agent gets its setup planned now, so the Agents step offers it (NIT-693 review).
      if (tool === "claude" || tool === "codex") props.onAgentsChanged();
      setInstalls((all) => {
        const rest = { ...all };
        delete rest[tool];
        return rest;
      });
      check();
    } catch (e) {
      setInstalls((all) => ({ ...all, [tool]: { state: "failed", log: all[tool]?.log ?? [], error: String(e) } }));
    }
  };
  useEffect(() => {
    check();
  }, [check]);
  // Coming back to the window (after a sign-in or an install in a terminal) checks again.
  useEffect(() => {
    const onFocus = () => check();
    window.addEventListener("focus", onFocus);
    return () => window.removeEventListener("focus", onFocus);
  }, [check]);

  // Where it opens: decided once the tools were checked, so a reopened flow skips done steps.
  useEffect(() => {
    if (step === null && tools !== null) setStep(openingStep(facts, props.reopened, read<StepId | null>(STEP_KEY, null)));
  }, [step, tools, facts, props.reopened]);

  const go = (next: StepId) => {
    setStep(next);
    write(STEP_KEY, next);
  };
  // Welcome and Skills are done once shown, whichever way the person got to them.
  useEffect(() => {
    if ((step === "welcome" || step === "skills") && !seen.includes(step)) {
      const s = [...seen, step];
      setSeen(s);
      write(SEEN_KEY, s);
    }
  }, [step, seen]);
  const leave = (finished: boolean) => {
    write(CLOSED_KEY, true);
    props.onClose(finished);
  };

  const states = stepStates(facts);
  const current = step ?? "welcome";
  const i = STEPS.findIndex((s) => s.id === current);
  const text = TEXT[current];
  const next = () => (i === STEPS.length - 1 ? leave(true) : go(STEPS[i + 1].id));
  const held = !canContinue(current, facts);
  const projectKey = props.projects[0]?.key ?? "APP";

  return (
    <div className="fr-win">
      <aside className="fr-rail">
        <div className="fr-brand">Nitely</div>
        <div className="fr-rail-title">{props.reopened ? "Setup, from Settings" : "Set up Nitely"}</div>
        {STEPS.map((s) => {
          const st = s.id === current ? "current" : states[s.id];
          return (
            <button key={s.id} className={`fr-step fr-step-${st}`} onClick={() => go(s.id)} aria-current={s.id === current ? "step" : undefined}>
              <span className="fr-glyph">{st === "done" ? "✓" : st === "needs" ? "!" : ""}</span>
              <span className="fr-step-text">
                <span className="fr-step-name">{s.name}</span>
                <span className="fr-step-sub">{(states[s.id] !== "todo" && stepSummary(s.id, facts)) || s.sub}</span>
              </span>
            </button>
          );
        })}
        <div className="fr-rail-foot">{props.reopened ? "Done steps stay done. Close returns to Settings." : "You can reopen this from Settings any time."}</div>
      </aside>
      <main className="fr-main">
        <div className="fr-track">
          <div className="fr-minis">
            {STEPS.map((s) => (
              <span key={s.id} className={`fr-mini fr-mini-${s.id === current ? "current" : states[s.id]}`} />
            ))}
          </div>
          <span className="fr-track-name">{STEPS[i].name}</span>
          <span className="fr-count">
            Step {i + 1} of {STEPS.length}
          </span>
        </div>
        <div className="fr-content">
          {props.reopened && current !== "welcome" && (
            <div className="fr-notice">
              <strong>Opened from Settings.</strong> Done steps stay done; this opens on the first that isn't. Open a done step from the list to change it.
            </div>
          )}
          <div className="fr-head">
            <div className="fr-crumb">
              Step {i + 1} of {STEPS.length}
            </div>
            <h1 className="fr-h1">{text.title}</h1>
            <p className="fr-lede">{text.lede}</p>
          </div>

          {current === "welcome" && (
            <>
              <div className="fr-grid2">
                <div className="fr-card">
                  <div className="fr-card-title">What it sets up</div>
                  {WELCOME_DOES.map(([t, d], n) => (
                    <div key={t} className="fr-item">
                      <span className="fr-item-n">{n + 1}</span>
                      <span className="fr-item-text">
                        <span className="fr-item-title">{t}</span>
                        <span className="fr-note">{d}</span>
                      </span>
                    </div>
                  ))}
                </div>
                <div className="fr-card">
                  <div className="fr-card-title">What it never does</div>
                  {WELCOME_NEVER.map(([t, d]) => (
                    <div key={t} className="fr-item">
                      <span className="fr-dot fr-item-dot" />
                      <span className="fr-item-text">
                        <span className="fr-item-title">{t}</span>
                        <span className="fr-note">{d}</span>
                      </span>
                    </div>
                  ))}
                </div>
              </div>
              <div className="fr-note">About five minutes, most of it installs. Nothing is sent anywhere by setting up.</div>
            </>
          )}

          {current === "tools" && (
            <>
              <ToolsStep tools={tools} plans={plans} installs={installs} onInstall={install} />
              {held && tools && (
                <div className="fr-notice fr-notice-needs">
                  <strong>Continue waits for git and one agent.</strong> Install what's missing, then check again. GitHub CLI can wait.
                </div>
              )}
              <div className="fr-actions">
                <Button disabled={checking} onClick={check}>
                  {checking ? "Checking…" : "Check again"}
                </Button>
              </div>
            </>
          )}

          {current === "signin" && <SignInStep tools={tools} onCheck={check} checking={checking} onSignIn={signIn} waiting={signingIn} errors={signInErrors} />}

          {current === "project" &&
            (props.projects.length > 0 ? (
              <div className="fr-notice">
                <strong>{props.projects.length === 1 ? `${props.projects[0].name} (${props.projects[0].key}) is connected.` : `${props.projects.length} projects are connected.`}</strong> Add more any time from the sidebar.
              </div>
            ) : (
              <AddProject embedded onAdded={props.onProjectAdded} />
            ))}

          {current === "agents" && <AgentsStep agents={props.agents} onChanged={props.onAgentsChanged} />}

          {current === "skills" && <SkillsStep projectKey={projectKey} />}
        </div>
        <div className="fr-foot">
          {i > 0 && (
            <Button variant="quiet" onClick={() => go(STEPS[i - 1].id)}>
              Back
            </Button>
          )}
          <span className="fr-foot-gap" />
          {props.reopened ? (
            <Button variant="quiet" onClick={() => leave(false)}>
              Close
            </Button>
          ) : (
            text.skip && (
              <Button variant="quiet" onClick={() => (current === "welcome" ? leave(false) : next())}>
                {text.skip}
              </Button>
            )
          )}
          <Button variant="primary" disabled={held} onClick={next}>
            {text.next}
          </Button>
        </div>
      </main>
    </div>
  );
}

/** Agent setup inline: what each agent gets, and one button that writes it (NIT-690). */
function AgentsStep(props: { agents: AgentPlan[] | null; onChanged: () => void }) {
  const [busy, setBusy] = useState(false);
  const [result, setResult] = useState<AgentsUpdated | null>(null);
  const [error, setError] = useState<string | null>(null);
  if (!props.agents) return <p className="fr-note">Looking for your agents…</p>;
  const changes = props.agents.filter((a) => a.found).flatMap((a) => a.changes.filter((c) => c.action !== "unchanged" && c.action !== "manual")).length;
  const apply = async () => {
    setBusy(true);
    try {
      setResult(await invoke<AgentsUpdated>("agent_setup_apply"));
      setError(null);
      props.onChanged();
    } catch (e) {
      setError(String(e));
    }
    setBusy(false);
  };
  return (
    <>
      <AgentChanges agents={props.agents} />
      {error && <p className="error small">{error}</p>}
      {result && <p className="fr-note">{result.failed.length ? `${result.done.length} done, ${result.failed.length} failed: ${result.failed.join("; ")}` : `Set up: ${result.done.length} change${result.done.length === 1 ? "" : "s"} made.`}</p>}
      {changes > 0 && !result ? (
        <div className="fr-actions">
          <Button variant="primary" disabled={busy} onClick={apply}>
            Set up agents ({changes} change{changes === 1 ? "" : "s"})
          </Button>
        </div>
      ) : (
        !result && <p className="fr-note">Every agent found here is set up.</p>
      )}
      {changes > 0 && !result && <div className="fr-note">Nothing is written until you set them up. Each file is backed up first, and Agent setup in the sidebar undoes it.</div>}
    </>
  );
}
