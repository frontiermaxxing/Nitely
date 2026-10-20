import { useEffect, useState } from "react";
import { invoke } from "./backend";
import type { Board, IssueRecord, Project } from "./types";
import { Backlog, Summary } from "./Backlog";
import { BoardFilters, NO_FILTER, ProjectBoard, type Filters } from "./ProjectBoard";
import { BranchMap } from "./BranchMap";
import { CrewControl } from "./CrewControl";
import { ProjectBrief } from "./ProjectBrief";
import { GoalRows, type GoalFocus } from "./GoalRows";
import { ProjectSettings } from "./ProjectSettings";
import { useProjectYolo } from "./ProjectYolo";
import { ProjectRules } from "./Rules";
import { EmptyState, Loading } from "./ui";
import "./project.css";

export const PROJECT_TABS = ["list", "board", "branches", "brief", "rules", "settings"] as const;
export type ProjectTab = (typeof PROJECT_TABS)[number];

const TAB_LABEL: Record<ProjectTab, string> = { list: "List", board: "Board", branches: "Branches", brief: "Brief", rules: "Rules", settings: "Settings" };
const TAB_KEY = "nitely.project.tab";

export function isProjectTab(t: string | undefined): t is ProjectTab {
  return !!t && (PROJECT_TABS as readonly string[]).includes(t);
}

/** The tab last picked on any project page, so opening a project shows the view you use. */
function rememberedTab(): ProjectTab {
  try {
    const t = localStorage.getItem(TAB_KEY) ?? undefined;
    return isProjectTab(t) && t !== "settings" ? t : "list";
  } catch {
    return "list";
  }
}

function remember(tab: ProjectTab) {
  try {
    localStorage.setItem(TAB_KEY, tab);
  } catch {
    // Storage can be refused; the tab still switches for this visit.
  }
}

/**
 * A project: its issues as a list or a board by station, the branch map of its repos, its brief,
 * and its settings. The header says how the project stands before any of them.
 */
export function ProjectPage(props: {
  project: Project | undefined;
  projectKey: string;
  tab: ProjectTab | undefined;
  /** A goal to bring into view (from the issue panel): its row shows on the list or the board. */
  goal?: GoalFocus;
  onTab: (tab: ProjectTab) => void;
  board: Board | null;
  now: number;
  version: number;
  onOpenIssue: (key: string) => void;
  onChanged: () => void;
  /** The project goes by another key now, or was removed (null). */
  onMoved: (key: string | null) => void;
  /** Opens Settings on this project's crew. */
  onCrewSettings: () => void;
  /** Decisions of this project waiting on you, and the Inbox filtered to them. */
  waiting?: number;
  onInbox?: () => void;
}) {
  const remembered = rememberedTab();
  const tab = props.tab ?? (props.goal != null && remembered !== "list" && remembered !== "board" ? "list" : remembered);
  const [issues, setIssues] = useState<IssueRecord[] | null>(null);
  const [showClosed, setShowClosed] = useState(false);
  const [filters, setFilters] = useState<Filters>(NO_FILTER);
  const [error, setError] = useState<string | null>(null);
  const yolo = useProjectYolo(props.projectKey, props.version, props.onChanged);

  // Closed issues are always loaded: the header counts what landed lately and the board shows
  // recent Done ones, even while the list hides them.
  useEffect(() => {
    let live = true;
    invoke<IssueRecord[]>("issues", { project: props.projectKey, includeClosed: true })
      .then((i) => live && (setIssues(i), setError(null)))
      .catch((e) => live && setError(String(e)));
    return () => {
      live = false;
    };
  }, [props.projectKey, props.version]);

  // Filters belong to one project; a different one starts unfiltered.
  useEffect(() => setFilters(NO_FILTER), [props.projectKey]);

  const pick = (t: ProjectTab) => {
    // Settings is a place you visit, not a way to see the project: it is never the default.
    if (t !== "settings") remember(t);
    props.onTab(t);
  };
  const views = tab === "list" || tab === "board";

  return (
    <div className={`page${tab === "board" || tab === "branches" ? " page-wide" : ""}`}>
      <div className="page-head project-head">
        <h1>
          {props.project?.name ?? props.projectKey} <span className="muted mono">{props.projectKey}</span>
        </h1>
        {!!props.waiting && props.onInbox && (
          <button className="link head-note attention-ink" onClick={props.onInbox} title="Open the Inbox on this project's decisions">
            {props.waiting} waiting on you
          </button>
        )}
        <CrewControl
          projectKey={props.projectKey}
          version={props.version}
          now={props.now}
          onSettings={props.onCrewSettings}
        />
        <div className="seg" role="tablist" aria-label="Project view">
          {PROJECT_TABS.map((t) => (
            <button key={t} role="tab" aria-selected={tab === t} className={tab === t ? "on" : undefined} onClick={() => pick(t)}>
              {TAB_LABEL[t]}
            </button>
          ))}
        </div>
        {tab === "list" && (
          <label className="toggle">
            <input type="checkbox" checked={showClosed} onChange={(e) => setShowClosed(e.target.checked)} />
            Show closed
          </label>
        )}
        {tab === "board" && issues && <BoardFilters issues={issues} board={props.board} filters={filters} onChange={setFilters} />}
      </div>
      {yolo.value?.enabled && <p className="settings-hint" role="status"><strong>YOLO mode is on.</strong> Automatic gates and PR reviews are skipped for this project. <button className="link" onClick={() => pick("settings")}>Manage in Settings</button></p>}
      {views && issues && issues.length > 0 && <Summary issues={issues} projectKey={props.projectKey} version={props.version} />}
      {views && <GoalRows project={props.projectKey} version={props.version} focus={props.goal} onOpenIssue={props.onOpenIssue} />}
      {tab !== "brief" && error && <p className="error">{error}</p>}
      {((views && !issues) || (tab === "settings" && !issues) || (tab === "branches" && !props.board)) && !error && <Loading />}
      {tab === "settings" && issues && props.project && (
        <ProjectSettings project={props.project} yolo={yolo} issueCount={issues.length} onChanged={props.onChanged} onMoved={props.onMoved} onCrewSettings={props.onCrewSettings} />
      )}
      {views && issues?.length === 0 && (
        <EmptyState title="No issues yet.">
          Seed the board from the repo: ask an agent there to run <span className="mono">/nitely-setup</span> (in Claude
          Code; any agent can follow the <span className="mono">setup_guide</span> tool). It reads the repo without changing it
          and files what it finds. Agents also file issues as they work.
        </EmptyState>
      )}
      {tab === "list" && issues && <Backlog issues={issues} showClosed={showClosed} onOpenIssue={props.onOpenIssue} />}
      {tab === "board" && issues && issues.length > 0 && (
        <ProjectBoard issues={issues} board={props.board} now={props.now} filters={filters} onOpenIssue={props.onOpenIssue} />
      )}
      {tab === "branches" && props.board && <BranchMap board={props.board} projectKey={props.projectKey} now={props.now} onOpenIssue={props.onOpenIssue} />}
      {tab === "rules" && <ProjectRules projectKey={props.projectKey} />}
      {tab === "brief" && <ProjectBrief projectKey={props.projectKey} version={props.version} now={props.now} onOpenIssue={props.onOpenIssue} />}
    </div>
  );
}
