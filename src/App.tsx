import { lazy, Suspense, useCallback, useEffect, useRef, useState } from "react";
import { ApprovalNotice, UpdateLine, useUpdateStatus } from "./Update";
import { invoke, isLive, isPreview, listen } from "./backend";
import type { AgentPlan, Board, Project } from "./types";
import { AgentSetup, pendingChanges, setupErrors } from "./Agents";
import { Inbox, storeInboxProject, storedInboxProject, useDecisionDetails } from "./Inbox";
import { decisionsOf, projectCounts, projectOf, type Decision } from "./decisions";
import { waitLeft } from "./refresh";
import { Settings, isSettingsSection, type SettingsSection } from "./Settings";
import { Now, movingCounts, movingTitle } from "./Now";
import { Pulse } from "./Pulse";
import { Activity } from "./Activity";
import { Performance } from "./Performance";
import { Leaderboard, useProfile } from "./leaderboard";
import { ProjectPage, isProjectTab, type ProjectTab } from "./Project";
import type { GoalFocus } from "./GoalRows";
import { IssuePanel } from "./IssuePanel";
import { AddProject } from "./AddProject";
import { HOME_LABEL, nowSecs } from "./util";
import { KeysSheet, Palette, useKeys } from "./Palette";
import { reposOf } from "./BranchMap";
import { CountBadge, Loading, livenessOf, setAgentIcons } from "./ui";
import { WindowControls, dragTitleBar } from "./WindowControls";
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
/** For the app's performance checks (scripts/harness/perf.e2e.mjs): a mark when the first board
 * is drawn, and an event each time a board is drawn. Neither is kept, so neither grows. */
const FIRST_BOARD_MARK = "nitely:first-board";
const BOARD_DRAWN_EVENT = "nitely:board-drawn";

export type View =
  | { kind: "pulse" }
  /** `project`: the Inbox filtered to one project; null for all, undefined for the one it had last. */
  | { kind: "inbox"; project?: string | null }
  | { kind: "now" }
  | { kind: "activity" }
  | { kind: "performance" }
  | { kind: "leaderboard" }
  | { kind: "project"; key: string; tab?: ProjectTab; goal?: GoalFocus }
  | { kind: "add" }
  | { kind: "agents" }
  | { kind: "settings"; section?: SettingsSection; project?: string }
  | { kind: "kit" };

// The view and the open issue live in the URL hash (`#now`, `#project/NIT/board`, `#inbox@NIT-12`,
// `#inbox?project=NIT`), so a reload keeps them and the browser preview can open any screen directly.
function fromHash(hash: string): { view: View; issue: string | null } {
  let raw = hash.replace(/^#\/?/, "");
  try {
    raw = decodeURIComponent(raw);
  } catch {
    // A malformed escape: use the hash as it is rather than fail to start.
  }
  const [route, issue] = raw.split("@");
  const [path, query = ""] = route.split("?");
  const [kind, key, tab] = path.split("/");
  const plain = ["pulse", "inbox", "now", "activity", "performance", "leaderboard", "add", "agents"].concat(isPreview ? ["kit"] : []);
  // The home view was called Pulse, and Phone and Other machines were views of their own before
  // Settings held them: old links still land on them.
  const named = kind === "overview" ? "pulse" : kind;
  const moved: Record<string, SettingsSection> = { phone: "phone", network: "machines" };
  const view: View =
    kind === "project" && key
      ? { kind: "project", key: key.toUpperCase(), ...(isProjectTab(tab) ? { tab } : {}) }
      : kind === "settings"
        ? { kind: "settings", ...(isSettingsSection(key) ? { section: key, ...(key === "crew" && tab ? { project: tab.toUpperCase() } : {}) } : {}) }
        : moved[kind]
          ? { kind: "settings", section: moved[kind] }
          : named === "inbox"
            ? { kind: "inbox", ...(new URLSearchParams(query).get("project") ? { project: new URLSearchParams(query).get("project")!.toUpperCase() } : {}) }
            : plain.includes(named)
              ? ({ kind: named } as View)
              : { kind: "pulse" };
  return { view, issue: issue ? issue.toUpperCase() : null };
}

function toHash(view: View, issue: string | null): string {
  const path =
    view.kind === "project"
      ? `project/${view.key}${view.tab ? `/${view.tab}` : ""}`
      : view.kind === "settings"
        ? `settings${view.section ? `/${view.section}${view.project ? `/${view.project}` : ""}` : ""}`
        : view.kind === "pulse"
          ? "overview"
          : view.kind === "inbox" && view.project
            ? `inbox?project=${view.project}`
            : view.kind;
  return `#${path}${issue ? `@${issue}` : ""}`;
}

/** The Inbox opened without a project named opens on the one it was last filtered to. */
function withInboxProject(view: View): View {
  return view.kind === "inbox" && view.project === undefined ? { kind: "inbox", project: storedInboxProject() } : view;
}

export function App() {
  const [board, setBoard] = useState<Board | null>(null);
  useEffect(() => {
    if (!board) return;
    if (performance.getEntriesByName(FIRST_BOARD_MARK).length === 0) performance.mark(FIRST_BOARD_MARK);
    window.dispatchEvent(new Event(BOARD_DRAWN_EVENT));
  }, [board]);
  const [projects, setProjects] = useState<Project[]>([]);
  const [view, setViewState] = useState<View>(() => withInboxProject(fromHash(location.hash).view));
  const setView = useCallback((v: View) => setViewState(withInboxProject(v)), []);
  const [openIssue, setOpenIssue] = useState<string | null>(() => fromHash(location.hash).issue);
  const [palette, setPalette] = useState(false);
  const leaderboardProfile = useProfile();
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
  // that arrive meanwhile queue a single follow-up refresh, paced by what the last one cost
  // (refresh.ts), instead of piling up in parallel while agents are busy.
  const running = useRef(false);
  const again = useRef(false);
  // Views read their own data again (`version`) once per board refresh after a change from
  // outside the app, not once per change: the database changes several times a minute (NIT-337).
  const bump = useRef(false);
  const lastEnd = useRef<number | null>(null);
  const lastMs = useRef(0);
  const refresh = useCallback(async () => {
    if (running.current) {
      again.current = true;
      return;
    }
    running.current = true;
    for (let first = true; first || again.current; first = false) {
      // Waits out the pace; a window coming back into view reads at once (within a quarter second).
      while (waitLeft(Date.now(), lastEnd.current, lastMs.current, document.hidden) > 0) {
        await new Promise((r) => setTimeout(r, 250));
      }
      again.current = false;
      const started = Date.now();
      try {
        const [b, p] = await Promise.all([invoke<Board>("snapshot"), invoke<Project[]>("projects")]);
        setBoard(b);
        setProjects(p);
        setError(null);
      } catch (e) {
        setError(String(e));
      }
      lastEnd.current = Date.now();
      lastMs.current = lastEnd.current - started;
      if (bump.current) {
        bump.current = false;
        setVersion((v) => v + 1);
      }
      setNow(nowSecs());
    }
    running.current = false;
  }, []);

  // A change made in the app: every view reads again now, and the board too.
  const changed = useCallback(() => {
    setVersion((v) => v + 1);
    refresh();
  }, [refresh]);
  // A change from outside (an agent, a hook, a land): the board first, then the views, once.
  const changedOutside = useCallback(() => {
    bump.current = true;
    refresh();
  }, [refresh]);

  // What Nitely writes for agents changes only with Nitely itself, so this is checked at start and
  // after the app changes it, not on every refresh.
  const [agents, setAgents] = useState<AgentPlan[]>([]);
  const [update, setUpdate] = useUpdateStatus();
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
  // Each agent's own icon, from the copy installed here; the letters stay where none is.
  useEffect(() => {
    invoke<Record<string, number[] | null>>("agent_icons").then(setAgentIcons, () => {});
  }, []);
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
    const id = setInterval(changedOutside, FALLBACK_MS);
    const unlisten = listen(CHANGED_EVENT, changedOutside);
    return () => {
      clearInterval(id);
      unlisten.then((stop) => stop());
    };
  }, [refresh, changedOutside]);

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

  // A click on a notification opens the Inbox on that decision (or on the first, for a batch). One
  // outside the project the Inbox is filtered to switches the filter to its project, here, so the
  // Inbox opens already on it.
  const [inboxPick, setInboxPick] = useState<{ id: string; at: number } | null>(null);
  const decisionsNow = useRef<Decision[] | null>(null);
  useEffect(() => {
    const unlisten = listen<string>(OPEN_DECISION_EVENT, (e) => {
      setOpenIssue(null);
      // A batch's notification names no one decision: it opens every project's.
      const filter = e.payload ? storedInboxProject() : null;
      const d = e.payload ? decisionsNow.current?.find((x) => x.id === e.payload) : undefined;
      const project = d && filter && projectOf(d)?.toUpperCase() !== filter.toUpperCase() ? projectOf(d) : filter;
      setView({ kind: "inbox", project });
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
  useNotifications(decisions, inboxDetails, now, focused && view.kind === "inbox" && !openIssue, view.kind === "inbox" ? (view.project ?? null) : null);
  decisionsNow.current = decisions;
  const inboxCount = decisions?.length ?? 0;
  // Each project's share of it, for the links to its filtered Inbox (the sidebar, a project's head).
  const inboxByProject = decisions ? projectCounts(decisions) : [];
  const waitingIn = (key: string) => inboxByProject.find((c) => c.project === key.toUpperCase())?.count ?? 0;
  // The Inbox's project filter, in the URL; however it was set (a chip, a link, the palette), it is
  // kept for the next time the Inbox opens.
  const filterInbox = useCallback((project: string | null) => setView({ kind: "inbox", project }), [setView]);
  const inboxProject = view.kind === "inbox" ? (view.project ?? null) : undefined;
  useEffect(() => {
    if (inboxProject !== undefined) storeInboxProject(inboxProject);
  }, [inboxProject]);
  // Agents moving, here and on other machines: state, so on the sidebar where it is seen at a glance.
  const moving = board ? movingCounts(board, now) : { here: 0, elsewhere: 0, total: 0 };

  return (
    <div className="shell">
      <WindowControls />
      {isPreview && <FakeToasts />}
      <nav className="sidebar">
        {/* On Windows and the Mac the brand row is where the window is dragged from, as a title bar would be. */}
        <div className="brand" data-tauri-drag-region={dragTitleBar || undefined}>
          <BrandMark />
          Nitely
        </div>
        <button className="nav-goto" onClick={openPalette} title="Go to a view, project or issue">
          <span>Go to…</span> <kbd>{navigator.platform.startsWith("Mac") ? "⌘K" : "Ctrl K"}</kbd>
        </button>
        <NavItem active={view.kind === "pulse"} onClick={() => setView({ kind: "pulse" })} label={HOME_LABEL} />
        <NavItem active={view.kind === "inbox"} onClick={() => setView({ kind: "inbox" })} label="Inbox" count={inboxCount} tone="attention" />
        <NavItem active={view.kind === "now"} onClick={() => setView({ kind: "now" })} label="Now" count={moving.total} tone="live" title={movingTitle(moving)} />
        <NavItem active={view.kind === "activity"} onClick={() => setView({ kind: "activity" })} label="Activity" />
        <div className="nav-heading">Projects</div>
        {projects.map((p) => (
          <div key={p.key} className="nav-project-row">
            <button
              className={`nav-item nav-project${view.kind === "project" && view.key === p.key ? " active" : ""}`}
              onClick={() => setView({ kind: "project", key: p.key })}
              aria-current={view.kind === "project" && view.key === p.key ? "page" : undefined}
              title={`${p.name}: ${p.open_issues} open`}
            >
              <span className="nav-label">{p.name}</span>
              <LiveDots states={board ? liveStates(board, p.key, now) : []} />
              <span className={`nav-hint${waitingIn(p.key) ? " nav-hint-under" : ""}`}>{p.key}</span>
            </button>
            {/* What waits on you in it, where its key is: a link to the Inbox filtered to it. */}
            {waitingIn(p.key) > 0 && (
              <button className="nav-project-inbox" onClick={() => filterInbox(p.key)} title={`${waitingIn(p.key)} waiting on you in ${p.name}: open the Inbox on them`}>
                {waitingIn(p.key)}
              </button>
            )}
          </div>
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
          <button
            className={`nav-item${view.kind === "leaderboard" ? " active" : ""}`}
            onClick={() => setView({ kind: "leaderboard" })}
            aria-current={view.kind === "leaderboard" ? "page" : undefined}
          >
            <span className="nav-label">Leaderboard</span>
            {/* Until you join, nothing is shared: the tag says so where the entry is. */}
            {leaderboardProfile === null && <span className="nav-tag">opt in</span>}
          </button>
          <NavItem active={view.kind === "performance"} onClick={() => setView({ kind: "performance" })} label="Performance" />
          <button
            className={`nav-item${view.kind === "settings" ? " active" : ""}`}
            onClick={() => setView({ kind: "settings" })}
            aria-current={view.kind === "settings" ? "page" : undefined}
          >
            <span className="nav-label">Settings</span>
          </button>
          <div className="nav-status">
            <span className={`nav-dot nav-dot-${error ? "fail" : "ok"}`} />
            {error ? "Local · not reading" : isLive ? "Local · live data" : isPreview ? "Local · sample data" : "Local · live"}
          </div>
          <UpdateLine status={update} onStatus={setUpdate} />
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
        <ApprovalNotice />
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
        {view.kind === "inbox" && board && (
          <Inbox
            board={board}
            details={inboxDetails}
            now={now}
            pick={inboxPick}
            onPicked={() => setInboxPick(null)}
            onOpenIssue={setOpenIssue}
            onChanged={changed}
            onNotifySettings={() => setView({ kind: "settings", section: "notifications" })}
            project={view.project ?? null}
            onProject={filterInbox}
            projects={projects}
            version={version}
          />
        )}
        {view.kind === "now" && board && <Now board={board} version={version} needs={inboxCount} now={now} onOpenIssue={setOpenIssue} onChanged={changed} onGo={setView} />}
        {view.kind === "activity" && <Activity now={now} version={version} projects={projects} onOpenIssue={setOpenIssue} />}
        {view.kind === "leaderboard" && <Leaderboard now={now} version={version} projects={projects} />}
        {view.kind === "performance" && <Performance onOpenIssue={setOpenIssue} onSettings={() => setView({ kind: "settings", section: "lanes" })} />}
        {view.kind === "settings" && (
          <Settings
            asked={view}
            section={view.section}
            project={view.project}
            board={board}
            projects={projects}
            now={now}
            version={version}
            update={update}
            onUpdate={setUpdate}
            onGo={setView}
            onChanged={changed}
          />
        )}
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
            goal={view.goal}
            onTab={(tab) => setView({ kind: "project", key: view.key, tab })}
            board={board}
            now={now}
            version={version}
            onOpenIssue={setOpenIssue}
            onChanged={changed}
            onMoved={(key) => setView(key ? { kind: "project", key, tab: "settings" } : { kind: "pulse" })}
            onCrewSettings={() => setView({ kind: "settings", section: "crew", project: view.key })}
            waiting={waitingIn(view.key)}
            onInbox={() => filterInbox(view.key)}
          />
        )}
      </main>

      {palette && <Palette projects={projects} onGo={setView} onOpenIssue={setOpenIssue} onChanged={changed} onClose={() => setPalette(false)} />}
      {keys && <KeysSheet onClose={closeKeys} />}
      {openIssue && (
        <IssuePanel
          issueKey={openIssue}
          now={now}
          version={version}
          onClose={() => setOpenIssue(null)}
          onChanged={changed}
          onOpenIssue={setOpenIssue}
          onOpenGoal={(key, goal) => {
            setOpenIssue(null);
            // Stamped, so opening the same goal again scrolls to it again.
            setView({ kind: "project", key, goal: { id: goal, at: Date.now() } });
          }}
        />
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
  /** What the count is made of, on hover. */
  title?: string;
}) {
  return (
    <button className={`nav-item${props.active ? " active" : ""}`} onClick={props.onClick} aria-current={props.active ? "page" : undefined} title={props.title}>
      <span className="nav-label">{props.label}</span>
      {props.hint && <span className="nav-hint">{props.hint}</span>}
      <CountBadge count={props.count ?? 0} tone={props.tone} />
    </button>
  );
}

/** Each agent session in the project's repos that is working (teal) or waiting on you (amber). */
function liveStates(board: Board, projectKey: string, now: number): ("working" | "waiting")[] {
  // One per session: several agents often run from one main checkout (NIT-273).
  const repos = new Set(reposOf(board, projectKey).map((r) => r.name));
  const states = (board.agents ?? []).filter((a) => a.repo !== null && repos.has(a.repo)).map((a) => livenessOf(a.session, now));
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
