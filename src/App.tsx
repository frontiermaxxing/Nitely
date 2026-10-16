import { lazy, Suspense, useCallback, useEffect, useRef, useState } from "react";
import { invoke, isLive, isPreview, listen } from "./backend";
import type { AgentPlan, Board, Project } from "./types";
import { AgentSetup, pendingChanges, setupErrors } from "./Agents";
import { Inbox, useDecisionDetails } from "./Inbox";
import { decisionsOf } from "./decisions";
import { Network } from "./Network";
import { Now } from "./Now";
import { Pulse } from "./Pulse";
import { Activity } from "./Activity";
import { ProjectPage, isProjectTab, type ProjectTab } from "./Project";
import { IssuePanel } from "./IssuePanel";
import { AddProject } from "./AddProject";
import { HOME_LABEL, nowSecs } from "./util";
import { KeysSheet, Palette, useKeys } from "./Palette";
import { reposOf } from "./BranchMap";
import { CountBadge, Loading, livenessOf } from "./ui";
import { WindowControls, ownTitleBar } from "./WindowControls";
import { useNotifications } from "./Notifications";
import { FakeToasts } from "./dev/FakeToasts";

// The browser preview's page of every primitive in every state; never in a production build.
const Kit = import.meta.env.DEV ? lazy(() => import("./dev/Kit")) : () => null;

// The backend pushes CHANGED_EVENT the moment the database or a repo's refs change. Uncommitted
// edits and relative times are caught by this slow fallback.
const FALLBACK_MS = 30000;
const CHANGED_EVENT = "nitely://changed";
const OPEN_ISSUE_EVENT = "nitely://open-issue";
/** Sent with a decision's id when its notification is clicked (src-tauri/src/notify.rs). */
const OPEN_DECISION_EVENT = "nitely://open-decision";
const REFRESH_GAP_MS = 1000;

export type View =
  | { kind: "pulse" }
  | { kind: "inbox" }
  | { kind: "now" }
  | { kind: "activity" }
  | { kind: "project"; key: string; tab?: ProjectTab }
  | { kind: "add" }
  | { kind: "agents" }
  | { kind: "network" }
  | { kind: "kit" };

// The view and the open issue live in the URL hash (`#now`, `#project/NIT/board`, `#inbox@NIT-12`), so
// a reload keeps them and the browser preview can open any screen directly.
function fromHash(hash: string): { view: View; issue: string | null } {
  let raw = hash.replace(/^#\/?/, "");
  try {
    raw = decodeURIComponent(raw);
  } catch {
    // A malformed escape: use the hash as it is rather than fail to start.
  }
  const [path, issue] = raw.split("@");
  const [kind, key, tab] = path.split("/");
  const plain = ["pulse", "inbox", "now", "activity", "add", "agents", "network"].concat(isPreview ? ["kit"] : []);
  // The home view was called Pulse; old links still land on it.
  const named = kind === "overview" ? "pulse" : kind;
  const view: View =
    kind === "project" && key
      ? { kind: "project", key: key.toUpperCase(), ...(isProjectTab(tab) ? { tab } : {}) }
      : plain.includes(named) ? ({ kind: named } as View) : { kind: "pulse" };
  return { view, issue: issue ? issue.toUpperCase() : null };
}

function toHash(view: View, issue: string | null): string {
  const path = view.kind === "project" ? `project/${view.key}${view.tab ? `/${view.tab}` : ""}` : view.kind === "pulse" ? "overview" : view.kind;
  return `#${path}${issue ? `@${issue}` : ""}`;
}

export function App() {
  const [board, setBoard] = useState<Board | null>(null);
  const [projects, setProjects] = useState<Project[]>([]);
  const [view, setView] = useState<View>(() => fromHash(location.hash).view);
  const [openIssue, setOpenIssue] = useState<string | null>(() => fromHash(location.hash).issue);
  const [palette, setPalette] = useState(false);
  const openPalette = useCallback(() => setPalette(true), []);
  const [keys, setKeys] = useState(false);
  const openKeys = useCallback(() => setKeys(true), []);
  const closeKeys = useCallback(() => setKeys(false), []);
  useKeys(openPalette, palette || keys, openKeys, !!openIssue);
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

  // A nitely://issue/KEY link opens that issue's panel: one that started the app is waiting in the
  // backend (opened_link), later ones arrive as events while the app runs.
  useEffect(() => {
    let live = true;
    const unlisten = listen<string>(OPEN_ISSUE_EVENT, (e) => {
      setOpenIssue(e.payload);
      invoke("opened_link").catch(() => {});
    });
    // Only once listening, read a link that came first: one arriving in between is then either
    // still waiting here or heard as an event, never lost.
    unlisten.then(() => live && invoke<string | null>("opened_link").then((key) => live && key && setOpenIssue(key), () => {}));
    return () => {
      live = false;
      unlisten.then((stop) => stop());
    };
  }, []);

  // A click on a notification opens the Inbox on that decision (or on the first, for a batch).
  const [inboxPick, setInboxPick] = useState<{ id: string; at: number } | null>(null);
  useEffect(() => {
    const unlisten = listen<string>(OPEN_DECISION_EVENT, (e) => {
      setOpenIssue(null);
      setView({ kind: "inbox" });
      if (e.payload) setInboxPick({ id: e.payload, at: Date.now() });
    });
    return () => {
      unlisten.then((stop) => stop());
    };
  }, []);
  const focused = useWindowFocused();

  // The Inbox's decisions, counted the same way the Inbox lists them.
  const inboxDetails = useDecisionDetails(board);
  // What waits on the human: the Inbox lists it, and the nav badge, the Overview and Now count it.
  // null until the board has loaded: "nothing needs you" must never be said before it is known.
  const decisions = board ? decisionsOf(board, inboxDetails, now) : null;
  // New ones are told outside the window too, except while the Inbox is in front of the person.
  useNotifications(decisions, inboxDetails, now, focused && view.kind === "inbox" && !openIssue);
  const inboxCount = decisions?.length ?? 0;
  const working = board?.repos.flatMap((r) => r.rows).filter((r) => r.session?.state === "working").length ?? 0;

  return (
    <div className="shell">
      <WindowControls />
      {isPreview && <FakeToasts />}
      <nav className="sidebar">
        {/* On Windows the brand row is where the window is dragged from, as a title bar would be. */}
        <div className="brand" data-tauri-drag-region={ownTitleBar || undefined}>
          <BrandMark />
          Nitely
        </div>
        <button className="nav-goto" onClick={openPalette} title="Go to a view, project or issue">
          <span>Go to…</span> <kbd>{navigator.platform.startsWith("Mac") ? "⌘K" : "Ctrl K"}</kbd>
        </button>
        <NavItem active={view.kind === "pulse"} onClick={() => setView({ kind: "pulse" })} label={HOME_LABEL} />
        <NavItem active={view.kind === "inbox"} onClick={() => setView({ kind: "inbox" })} label="Inbox" count={inboxCount} tone="attention" />
        <NavItem active={view.kind === "now"} onClick={() => setView({ kind: "now" })} label="Now" count={working} tone="live" />
        <NavItem active={view.kind === "activity"} onClick={() => setView({ kind: "activity" })} label="Activity" />
        <NavItem
          active={view.kind === "network"}
          onClick={() => setView({ kind: "network" })}
          label="Other machines"
          count={(board?.remote_sessions ?? []).filter((s) => livenessOf(s, now) === "working").length}
          tone="live"
        />
        <div className="nav-heading">Projects</div>
        {projects.map((p) => (
          <button
            key={p.key}
            className={`nav-item nav-project${view.kind === "project" && view.key === p.key ? " active" : ""}`}
            onClick={() => setView({ kind: "project", key: p.key })}
            aria-current={view.kind === "project" && view.key === p.key ? "page" : undefined}
            title={`${p.name}: ${p.open_issues} open`}
          >
            <span className="nav-label">{p.name}</span>
            <LiveDots states={board ? liveStates(board, p.key, now) : []} />
            <span className="nav-hint">{p.key}</span>
          </button>
        ))}
        <button className={`nav-item nav-add${view.kind === "add" ? " active" : ""}`} onClick={() => setView({ kind: "add" })}>
          <span className="nav-label">+ Add project</span>
        </button>
        <div className="nav-foot">
          <button
            className={`nav-item${view.kind === "agents" ? " active" : ""}`}
            onClick={() => {
              loadAgents();
              setView({ kind: "agents" });
            }}
            aria-current={view.kind === "agents" ? "page" : undefined}
          >
            <span className="nav-label">Agent setup</span>
            <span className="nav-dots" aria-hidden="true">
              {agents
                .filter((a) => a.found || a.error)
                .map((a) => (
                  <span key={a.agent} className={`nav-dot nav-dot-${setupTone(a)}`} title={`${a.agent}: ${SETUP_WORDS[setupTone(a)]}`} />
                ))}
            </span>
          </button>
          <div className="nav-status">
            <span className={`nav-dot nav-dot-${error ? "fail" : "ok"}`} />
            {error ? "Local · not reading" : isLive ? "Local · live data" : isPreview ? "Local · sample data" : "Local · live"}
          </div>
          {isPreview && <PreviewFoot active={view.kind === "kit"} onKit={() => setView({ kind: "kit" })} />}
        </div>
      </nav>

      <main className="content">
        {error && <p className="error banner">{error}</p>}
        {isLive && (
          <p className="banner live small" role="status">
            Live data, read-only: this preview reads your real Nitely database. Approving, answering and moving are
            refused here; use the app for those.
          </p>
        )}
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
        {view.kind === "pulse" && <Pulse board={board} decisions={decisions} now={now} version={version} onOpenIssue={setOpenIssue} onGo={setView} />}
        {(view.kind === "inbox" || view.kind === "now") && !board && !error && (
          <div className="page">
            <Loading />
          </div>
        )}
        {view.kind === "inbox" && board && <Inbox board={board} details={inboxDetails} now={now} pick={inboxPick} onOpenIssue={setOpenIssue} onChanged={changed} />}
        {view.kind === "now" && board && <Now board={board} needs={inboxCount} now={now} onOpenIssue={setOpenIssue} onChanged={changed} onGo={setView} />}
        {view.kind === "activity" && <Activity now={now} version={version} projects={projects} onOpenIssue={setOpenIssue} />}
        {view.kind === "network" && <Network board={board} projects={projects} now={now} />}
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
          <ProjectPage
            project={projects.find((p) => p.key === view.key)}
            projectKey={view.key}
            tab={view.tab}
            onTab={(tab) => setView({ kind: "project", key: view.key, tab })}
            board={board}
            now={now}
            version={version}
            onOpenIssue={setOpenIssue}
            onChanged={changed}
            onMoved={(key) => setView(key ? { kind: "project", key, tab: "settings" } : { kind: "pulse" })}
          />
        )}
      </main>

      {palette && <Palette projects={projects} onGo={setView} onOpenIssue={setOpenIssue} onChanged={changed} onClose={() => setPalette(false)} />}
      {keys && <KeysSheet onClose={closeKeys} />}
      {openIssue && (
        <IssuePanel issueKey={openIssue} now={now} version={version} onClose={() => setOpenIssue(null)} onChanged={changed} onOpenIssue={setOpenIssue} />
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

/** Each agent session in the project's repos that is working (teal) or waiting on you (amber). */
function liveStates(board: Board, projectKey: string, now: number): ("working" | "waiting")[] {
  const states = reposOf(board, projectKey).flatMap((r) => r.rows.map((x) => livenessOf(x.session, now)));
  return [...states.filter((s) => s === "waiting"), ...states.filter((s) => s === "working")] as ("working" | "waiting")[];
}

const MAX_DOTS = 5;

function LiveDots(props: { states: ("working" | "waiting")[] }) {
  if (!props.states.length) return null;
  const waiting = props.states.filter((s) => s === "waiting").length;
  const label = `${props.states.length - waiting} working, ${waiting} waiting on you`;
  return (
    <span className="nav-dots" title={label} aria-label={label}>
      {props.states.slice(0, MAX_DOTS).map((s, i) => (
        <span key={i} className={`nav-dot nav-dot-${s === "waiting" ? "warn" : "ok"}`} />
      ))}
    </span>
  );
}

type SetupTone = "ok" | "warn" | "fail";
const SETUP_WORDS: Record<SetupTone, string> = { ok: "up to date", warn: "an update is ready", fail: "needs a look" };

function setupTone(a: AgentPlan): SetupTone {
  if (a.error) return "fail";
  return a.changes.some((c) => c.action !== "unchanged") ? "warn" : "ok";
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

/** This page with `?data=live` set or removed, keeping the theme and the view. */
function liveHref(live: boolean): string {
  const params = new URLSearchParams(location.search);
  if (live) params.set("data", "live");
  else params.delete("data");
  const query = params.toString();
  return `${location.pathname}${query ? `?${query}` : ""}${location.hash}`;
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
      <p className="preview-note">
        {isLive ? "Live data, read-only. " : "Sample data from src/dev/fixtures.ts. "}
        <a href={liveHref(!isLive)}>{isLive ? "Use sample data" : "Use live data"}</a>
      </p>
    </div>
  );
}

/** True while the window has the focus and shows: what the person sees now, they need not be told. */
function useWindowFocused(): boolean {
  const [focused, setFocused] = useState(() => document.hasFocus());
  useEffect(() => {
    const check = () => setFocused(document.hasFocus() && document.visibilityState === "visible");
    window.addEventListener("focus", check);
    window.addEventListener("blur", check);
    document.addEventListener("visibilitychange", check);
    return () => {
      window.removeEventListener("focus", check);
      window.removeEventListener("blur", check);
      document.removeEventListener("visibilitychange", check);
    };
  }, []);
  return focused;
}
