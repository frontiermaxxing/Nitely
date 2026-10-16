import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { IssueRecord, Project } from "./types";
import { agentLabel, PRIORITY } from "./util";

const GROUPS = ["Needs You", "In Progress", "In Review", "Approved", "Todo", "Backlog", "Done", "Canceled", "Duplicate"];

export function Backlog(props: {
  project: Project | undefined;
  projectKey: string;
  version: number;
  onOpenIssue: (key: string) => void;
}) {
  const [issues, setIssues] = useState<IssueRecord[] | null>(null);
  const [showClosed, setShowClosed] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let live = true;
    const load = () =>
      invoke<IssueRecord[]>("issues", { project: props.projectKey, includeClosed: showClosed })
        .then((i) => live && (setIssues(i), setError(null)))
        .catch((e) => live && setError(String(e)));
    load();
    return () => {
      live = false;
    };
  }, [props.projectKey, showClosed, props.version]);

  return (
    <div className="page">
      <div className="page-head">
        <h1>
          {props.project?.name ?? props.projectKey} <span className="muted mono">{props.projectKey}</span>
        </h1>
        <label className="toggle">
          <input type="checkbox" checked={showClosed} onChange={(e) => setShowClosed(e.target.checked)} />
          Show closed
        </label>
      </div>
      {error && <p className="error">{error}</p>}
      {issues?.length === 0 && (
        <p className="muted">
          No issues yet. Agents file them with the <span className="mono">file_issue</span> tool, or run{" "}
          <span className="mono">nitely issue create</span>.
        </p>
      )}
      {GROUPS.map((state) => {
        const group = issues?.filter((i) => i.state === state) ?? [];
        if (group.length === 0) return null;
        return (
          <section key={state}>
            <h2>
              {state} <span className="muted">{group.length}</span>
            </h2>
            <div className="list">
              {group.map((i) => (
                <button key={i.key} className="list-row" onClick={() => props.onOpenIssue(i.key)}>
                  <span className={`prio p${i.priority}`} title={PRIORITY[i.priority] || "No priority"} />
                  <span className="mono key">{i.key}</span>
                  <span className="title">{i.title}</span>
                  {i.kind && <span className="tag">{i.kind}</span>}
                  {i.claimed_by && <span className="muted small">{agentLabel(i.claimed_by)}</span>}
                </button>
              ))}
            </div>
          </section>
        );
      })}
    </div>
  );
}
