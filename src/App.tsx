import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { Board, Project } from "./types";
import { Inbox } from "./Inbox";
import { Now } from "./Now";
import { Backlog } from "./Backlog";
import { IssuePanel } from "./IssuePanel";
import { AddProject } from "./AddProject";
import { nowSecs } from "./util";

// The backend pushes CHANGED_EVENT the moment the database or a repo's refs change. Uncommitted
// edits and relative times are caught by this slow fallback.
const FALLBACK_MS = 30000;
const CHANGED_EVENT = "nitely://changed";
const REFRESH_GAP_MS = 1000;

export type View = { kind: "inbox" } | { kind: "now" } | { kind: "project"; key: string } | { kind: "add" };

export function App() {
  const [board, setBoard] = useState<Board | null>(null);
  const [projects, setProjects] = useState<Project[]>([]);
  const [view, setView] = useState<View>({ kind: "inbox" });
  const [openIssue, setOpenIssue] = useState<string | null>(null);
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
        <div className="brand">Nitely</div>
        <NavItem active={view.kind === "inbox"} onClick={() => setView({ kind: "inbox" })} label="Inbox" count={inboxCount} alert />
        <NavItem active={view.kind === "now"} onClick={() => setView({ kind: "now" })} label="Now" count={working} />
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
          <span className="nav-label">+ Add project</span>
        </button>
      </nav>

      <main className="content">
        {error && <p className="error banner">{error}</p>}
        {view.kind === "inbox" && board && <Inbox board={board} now={now} onOpenIssue={setOpenIssue} onChanged={changed} />}
        {view.kind === "now" && board && <Now board={board} now={now} onOpenIssue={setOpenIssue} onChanged={changed} />}
        {view.kind === "add" && (
          <AddProject
            onAdded={(key) => {
              changed();
              setView({ kind: "project", key });
            }}
          />
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
  alert?: boolean;
}) {
  return (
    <button className={`nav-item${props.active ? " active" : ""}`} onClick={props.onClick}>
      <span className="nav-label">{props.label}</span>
      {props.hint && <span className="nav-hint">{props.hint}</span>}
      {!!props.count && <span className={`nav-count${props.alert ? " alert" : ""}`}>{props.count}</span>}
    </button>
  );
}
