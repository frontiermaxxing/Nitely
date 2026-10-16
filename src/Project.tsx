import { useEffect, useState } from "react";
import { invoke } from "./backend";
import type { Board, IssueRecord, Project } from "./types";
import { Backlog, Summary } from "./Backlog";
import { BoardFilters, NO_FILTER, ProjectBoard, type Filters } from "./ProjectBoard";
import { BranchMap } from "./BranchMap";
import { ProjectSettings } from "./ProjectSettings";
import { EmptyState, Loading } from "./ui";
import "./project.css";

export const PROJECT_TABS = ["list", "board", "branches", "settings"] as const;
export type ProjectTab = (typeof PROJECT_TABS)[number];

const TAB_LABEL: Record<ProjectTab, string> = { list: "List", board: "Board", branches: "Branches", settings: "Settings" };
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
 * A project: its issues as a list or a board by station, and the branch map of its repos. The
 * header says how the project stands before any of them.
 */
export function ProjectPage(props: {
  project: Project | undefined;
  projectKey: string;
  tab: ProjectTab | undefined;
  onTab: (tab: ProjectTab) => void;
  board: Board | null;
  now: number;
  version: number;
  onOpenIssue: (key: string) => void;
  onChanged: () => void;
  /** The project goes by another key now, or was removed (null). */
  onMoved: (key: string | null) => void;
}) {
  const tab = props.tab ?? rememberedTab();
  const [issues, setIssues] = useState<IssueRecord[] | null>(null);
  const [showClosed, setShowClosed] = useState(false);
  const [filters, setFilters] = useState<Filters>(NO_FILTER);
  const [error, setError] = useState<string | null>(null);

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
    <div className={`page${tab === "list" ? "" : " page-wide"}`}>
      <div className="page-head project-head">
        <h1>
          {props.project?.name ?? props.projectKey} <span className="muted mono">{props.projectKey}</span>
        </h1>
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
      {views && issues && issues.length > 0 && <Summary issues={issues} />}
      {error && <p className="error">{error}</p>}
      {((tab !== "branches" && !issues) || (tab === "branches" && !props.board)) && !error && <Loading />}
      {tab === "settings" && issues && props.project && (
        <ProjectSettings project={props.project} issueCount={issues.length} onChanged={props.onChanged} onMoved={props.onMoved} />
      )}
      {views && issues?.length === 0 && (

        <EmptyState title="No issues yet.">
          Agents file them with the <span className="mono">file_issue</span> tool, or run <span className="mono">nitely issue create</span>.
        </EmptyState>
      )}
      {tab === "list" && issues && <Backlog issues={issues} showClosed={showClosed} onOpenIssue={props.onOpenIssue} />}
      {tab === "board" && issues && issues.length > 0 && (
        <ProjectBoard issues={issues} board={props.board} now={props.now} filters={filters} onOpenIssue={props.onOpenIssue} />
      )}
      {tab === "branches" && props.board && <BranchMap board={props.board} projectKey={props.projectKey} now={props.now} onOpenIssue={props.onOpenIssue} />}
    </div>
  );
}
