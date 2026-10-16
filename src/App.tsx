import { lazy, Suspense, useCallback, useEffect, useRef, useState } from "react";
import { invoke, isPreview, listen } from "./backend";
import type { AgentPlan, Board, Project } from "./types";
import { AgentSetup, pendingChanges, setupErrors } from "./Agents";
import { Inbox } from "./Inbox";
import { Now } from "./Now";
import { Pulse } from "./Pulse";
import { Backlog } from "./Backlog";
import { IssuePanel } from "./IssuePanel";
import { AddProject } from "./AddProject";
import { nowSecs } from "./util";
import { CountBadge } from "./ui";

// The browser preview's page of every primitive in every state; never in a production build.
const Kit = import.meta.env.DEV ? lazy(() => import("./dev/Kit")) : () => null;

// The backend pushes CHANGED_EVENT the moment the database or a repo's refs change. Uncommitted
// edits and relative times are caught by this slow fallback.
const FALLBACK_MS = 30000;
const CHANGED_EVENT = "nitely://changed";
const REFRESH_GAP_MS = 1000;

export type View =
  | { kind: "pulse" }
  | { kind: "inbox" }
  | { kind: "now" }
  | { kind: "project"; key: string }
  | { kind: "add" }
  | { kind: "agents" }
  | { kind: "kit" };

// The view and the open issue live in the URL hash (`#now`, `#project/NIT`, `#inbox@NIT-12`), so
// a reload keeps them and the browser preview can open any screen directly.
function fromHash(hash: string): { view: View; issue: string | null } {
  let raw = hash.replace(/^#\/?/, "");
  try {
    raw = decodeURIComponent(raw);
  } catch {
    // A malformed escape: use the hash as it is rather than fail to start.
  }
  const [path, issue] = raw.split("@");
  const [kind, key] = path.split("/");
  const plain = ["pulse", "inbox", "now", "add", "agents"].concat(isPreview ? ["kit"] : []);
  const view: View =
    kind === "project" && key ? { kind: "project", key: key.toUpperCase() } : plain.includes(kind) ? ({ kind } as View) : { kind: "pulse" };
  return { view, issue: issue ? issue.toUpperCase() : null };
}

function toHash(view: View, issue: string | null): string {
  return `#${view.kind === "project" ? `project/${view.key}` : view.kind}${issue ? `@${issue}` : ""}`;
}

export function App() {
  const [board, setBoard] = useState<Board | null>(null);
  const [projects, setProjects] = useState<Project[]>([]);
  const [view, setView] = useState<View>(() => fromHash(location.hash).view);
  const [openIssue, setOpenIssue] = useState<string | null>(() => fromHash(location.hash).issue);
  const [error, setError] = useState<string | null>(null);
  const [now, setNow] = useState(nowSecs);
  // Bumped after any change made in the app, so open lists refetch at once.
  const [version, setVersion] = useState(0);

  // One refresh at a time: a snapshot runs git in every worktree and can take seconds, so changes
  // that arrive meanwhile queue a single follow-up refresh, at most one per REFRESH_GAP_MS, instead
  // of piling up in parallel while agents are busy.
  const running = useRef(false);
  const again = useRef(false);
  const refresh = useCallback(async () => {
    if (running.current) {
      again.current = true;
      return;
    }
    running.current = true;
    for (let first = true; first || again.current; first = false) {
      if (!first) await new Promise((r) => setTimeout(r, REFRESH_GAP_MS));
      again.current = false;
      try {
        const [b, p] = await Promise.all([invoke<Board>("snapshot"), invoke<Project[]>("projects")]);
        setBoard(b);
        setProjects(p);
        setError(null);
      } catch (e) {
        setError(String(e));
      }
      setNow(nowSecs());
    }
    running.current = false;
  }, []);

  const changed = useCallback(() => {
    setVersion((v) => v + 1);
    refresh();
  }, [refresh]);

  // What Nitely writes for agents changes only with Nitely itself, so this is checked at start and
  // after the app changes it, not on every refresh.
  const [agents, setAgents] = useState<AgentPlan[]>([]);
  const [agentsError, setAgentsError] = useState<string | null>(null);
  const loadAgents = useCallback(() => {
    invoke<AgentPlan[]>("agent_setup").then(
      (a) => {
        setAgents(a);
        setAgentsError(null);
      },
      (e) => setAgentsError(String(e)),
    );
  }, []);
  useEffect(loadAgents, [loadAgents]);
  const agentChanges = pendingChanges(agents);
  const agentErrors = setupErrors(agents);

  useEffect(() => {
    const next = toHash(view, openIssue);
    if (location.hash !== next) history.replaceState(null, "", next);
  }, [view, openIssue]);
  useEffect(() => {
    const onHash = () => {
      const h = fromHash(location.hash);
      setView(h.view);
      setOpenIssue(h.issue);
    };
    window.addEventListener("hashchange", onHash);
    return () => window.removeEventListener("hashchange", onHash);
  }, []);

  useEffect(() => {
    refresh();
    const id = setInterval(changed, FALLBACK_MS);
    const unlisten = listen(CHANGED_EVENT, changed);
    return () => {
      clearInterval(id);
      unlisten.then((stop) => stop());
    };
  }, [refresh, changed]);

  const inboxCount =
    (board?.asks.length ?? 0) +
    (board?.needs_me_issues.length ?? 0) +
    (board?.repos.flatMap((r) => r.rows).filter((r) => r.needs_me).length ?? 0);
  const working = board?.repos.flatMap((r) => r.rows).filter((r) => r.session?.state === "working").length ?? 0;

  return (
    <div className="shell">
      <nav className="sidebar">
        <div className="brand">
          <BrandMark />
          Nitely
        </div>
        <NavItem active={view.kind === "pulse"} onClick={() => setView({ kind: "pulse" })} label="Pulse" />
        <NavItem active={view.kind === "inbox"} onClick={() => setView({ kind: "inbox" })} label="Inbox" count={inboxCount} tone="attention" />
        <NavItem active={view.kind === "now"} onClick={() => setView({ kind: "now" })} label="Now" count={working} tone="live" />
        <div className="nav-heading">Projects</div>
        {projects.map((p) => (
          <NavItem
            key={p.key}
            active={view.kind === "project" && view.key === p.key}
            onClick={() => setView({ kind: "project", key: p.key })}
            label={p.name}
            hint={p.key}
            count={p.open_issues}
          />
        ))}
        <button className={`nav-item nav-add${view.kind === "add" ? " active" : ""}`} onClick={() => setView({ kind: "add" })}>
          <span className="nav-label">Add project</span>
        </button>
        {isPreview && <PreviewFoot active={view.kind === "kit"} onKit={() => setView({ kind: "kit" })} />}
      </nav>

      <main className="content">
        {error && <p className="error banner">{error}</p>}
        {agentsError && <p className="error banner small">Could not check your agents' Nitely setup: {agentsError}</p>}
        {(agentChanges > 0 || agentErrors > 0) && view.kind !== "agents" && (
          <p className="banner notice small">
            {agentChanges > 0
              ? `Your agents have an older copy of Nitely's instructions or setup (${agentChanges} change${agentChanges === 1 ? "" : "s"}).`
              : `Nitely could not read the setup of ${agentErrors} agent${agentErrors === 1 ? "" : "s"}.`}{" "}
            <button
              className="link"
              onClick={() => {
                loadAgents();
                setView({ kind: "agents" });
              }}
            >
              {agentChanges > 0 ? "Review and update" : "See why"}
            </button>
          </p>
        )}
        {view.kind === "agents" && <AgentSetup agents={agents} onUpdated={loadAgents} onRecheck={loadAgents} />}
        {view.kind === "pulse" && <Pulse board={board} now={now} version={version} onOpenIssue={setOpenIssue} onGo={setView} />}
        {view.kind === "inbox" && board && <Inbox board={board} now={now} onOpenIssue={setOpenIssue} onChanged={changed} />}
        {view.kind === "now" && board && <Now board={board} now={now} onOpenIssue={setOpenIssue} onChanged={changed} />}
        {view.kind === "add" && (
          <AddProject
            onAdded={(key) => {
              changed();
              loadAgents();
              setView({ kind: "project", key });
            }}
          />
        )}
        {view.kind === "kit" && (
          <Suspense fallback={null}>
            <Kit />
          </Suspense>
        )}
        {view.kind === "project" && (
          <Backlog
            project={projects.find((p) => p.key === view.key)}
            projectKey={view.key}
            version={version}
            onOpenIssue={setOpenIssue}
          />
        )}
      </main>

      {openIssue && (
        <IssuePanel issueKey={openIssue} now={now} version={version} onClose={() => setOpenIssue(null)} onChanged={changed} />
      )}
    </div>
  );
}

function NavItem(props: {
  active: boolean;
  onClick: () => void;
  label: string;
  hint?: string;
  count?: number;
  tone?: "attention" | "live";
}) {
  return (
    <button className={`nav-item${props.active ? " active" : ""}`} onClick={props.onClick} aria-current={props.active ? "page" : undefined}>
      <span className="nav-label">{props.label}</span>
      {props.hint && <span className="nav-hint">{props.hint}</span>}
      <CountBadge count={props.count ?? 0} tone={props.tone} />
    </button>
  );
}

/** Three stations on a line: the lifecycle track, which is the whole app in one mark. */
function BrandMark() {
  return (
    <svg width="22" height="10" viewBox="0 0 22 10" aria-hidden="true">
      <path d="M3 5H19" stroke="var(--fg-3)" strokeWidth="1.5" strokeLinecap="round" />
      <circle cx="3.5" cy="5" r="2.5" fill="var(--fg-3)" />
      <circle cx="11" cy="5" r="3.5" fill="var(--fg)" />
      <circle cx="18.5" cy="5" r="2.5" fill="var(--sunken)" stroke="var(--fg-3)" strokeWidth="1.5" />
    </svg>
  );
}

type Theme = "auto" | "light" | "dark";
const THEME_KEY = "nitely.preview.theme";

/** `?theme=dark` in the URL wins, so a screenshot tool can pick the theme; then the last choice. */
function readTheme(): Theme {
  try {
    const t = new URLSearchParams(location.search).get("theme") ?? localStorage.getItem(THEME_KEY);
    return t === "light" || t === "dark" ? t : "auto";
  } catch {
    return "auto";
  }
}

/** The preview's own corner: a theme switch and the kit page. */
function PreviewFoot(props: { active: boolean; onKit: () => void }) {
  const [theme, setTheme] = useState<Theme>(readTheme);
  useEffect(() => {
    if (theme === "auto") delete document.documentElement.dataset.theme;
    else document.documentElement.dataset.theme = theme;
    try {
      localStorage.setItem(THEME_KEY, theme);
    } catch {
      // Private windows may refuse storage; the switch still works for this page.
    }
  }, [theme]);
  return (
    <div className="preview-foot">
      <div className="nav-heading">Preview</div>
      <button className={`nav-item${props.active ? " active" : ""}`} onClick={props.onKit}>
        <span className="nav-label">Visual kit</span>
      </button>
      <div className="preview-theme" role="group" aria-label="Theme">
        {(["auto", "light", "dark"] as const).map((t) => (
          <button key={t} className={theme === t ? "on" : undefined} onClick={() => setTheme(t)} aria-pressed={theme === t}>
            {t === "auto" ? "Auto" : t === "light" ? "Light" : "Dark"}
          </button>
        ))}
      </div>
      <p className="preview-note">Sample data from src/dev/fixtures.ts</p>
    </div>
  );
}
