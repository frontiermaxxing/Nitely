import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { IssueDetail, ReviewRecord } from "./types";
import { agentLabel, ago, openExternal, PRIORITY } from "./util";
import { PrCell } from "./Pr";

const KIND_LABEL: Record<string, string> = {
  issue_created: "filed",
  issue_moved: "moved",
  claimed: "claimed",
  status: "status",
  note: "note",
  handoff: "handoff",
  ask: "asked",
  answer: "answered",
  worktree: "started",
  gate_passed: "gate passed",
  gate_failed: "gate failed",
  review_started: "review",
  review_failed: "review failed",
  submit: "submitted",
};

/** One issue and its timeline, in a panel over the right side. */
export function IssuePanel(props: { issueKey: string; now: number; version: number; onClose: () => void; onChanged: () => void }) {
  const [detail, setDetail] = useState<IssueDetail | null>(null);
  const [error, setError] = useState<string | null>(null);
  // Done needs evidence and Canceled a reason, typed in place.
  const [pending, setPending] = useState<"Done" | "Canceled" | null>(null);
  const [why, setWhy] = useState("");

  useEffect(() => {
    let live = true;
    const load = () =>
      invoke<IssueDetail>("issue_detail", { key: props.issueKey })
        .then((d) => live && (setDetail(d), setError(null)))
        .catch((e) => live && setError(String(e)));
    load();
    const id = setInterval(load, 4000);
    return () => {
      live = false;
      clearInterval(id);
    };
  }, [props.issueKey, props.version]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && props.onClose();
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [props.onClose]);

  const move = async (state: string, text?: string) => {
    const evidence = state === "Done" ? text ?? null : null;
    const reason = state === "Canceled" ? text ?? null : null;
    try {
      await invoke("move_issue", { key: props.issueKey, state, evidence, reason });
      setPending(null);
      setWhy("");
      props.onChanged();
    } catch (e) {
      setError(String(e));
    }
  };

  const issue = detail?.issue;
  return (
    <aside className="panel">
      <div className="panel-head">
        <span className="mono muted">{props.issueKey}</span>
        <button className="btn ghost" onClick={props.onClose} title="Close (Esc)">✕</button>
      </div>
      {error && <p className="error small">{error}</p>}
      {issue && (
        <>
          <h2 className="panel-title">{issue.title}</h2>
          <div className="facts">
            <span className="tag strong">{issue.state}</span>
            {issue.kind && <span className="tag">{issue.kind}</span>}
            {issue.priority > 0 && <span className="tag">{PRIORITY[issue.priority]}</span>}
            {issue.source && <span className="tag">from {issue.source}</span>}
            {issue.claimed_by && <span className="tag">{agentLabel(issue.claimed_by)}</span>}
            {issue.url && (
              <a href={issue.url} onClick={openExternal(issue.url)}>
                Open in {issue.origin}
              </a>
            )}
          </div>
          {issue.body && <p className="body">{issue.body}</p>}
          {issue.evidence && (
            <p className="small">
              <span className="muted">Evidence: </span>
              {issue.evidence}
            </p>
          )}
          {issue.origin === "nitely" && (
            <div className="actions">
              {issue.state === "Needs You" && <button className="btn primary" onClick={() => move("Approved")}>Approve</button>}
              {issue.state !== "Todo" && <button className="btn" onClick={() => move("Todo")}>Todo</button>}
              {issue.state !== "Done" && <button className="btn" onClick={() => setPending("Done")}>Done…</button>}
              {issue.state !== "Canceled" && <button className="btn" onClick={() => setPending("Canceled")}>Cancel…</button>}
            </div>
          )}
          {pending && (
            <form
              className="answer"
              onSubmit={(e) => {
                e.preventDefault();
                if (why.trim()) move(pending, why.trim());
              }}
            >
              <input
                autoFocus
                value={why}
                onChange={(e) => setWhy(e.target.value)}
                placeholder={pending === "Done" ? "What shows it is done?" : "Why cancel it?"}
              />
              <button className="btn primary" disabled={!why.trim()}>{pending === "Done" ? "Mark done" : "Cancel issue"}</button>
              <button type="button" className="btn ghost" onClick={() => setPending(null)}>Back</button>
            </form>
          )}
          {detail.reviews[0] && <LatestReview review={detail.reviews[0]} now={props.now} />}
          {detail.prs.length > 0 && (
            <>
              <h3>Pull requests</h3>
              {detail.prs.map((pr) => (
                <div key={`${pr.provider}-${pr.repo}-${pr.number}`} className="pr-row">
                  <PrCell pr={pr} />
                  <div className="small">{pr.title}</div>
                </div>
              ))}
            </>
          )}
          <h3>Timeline</h3>
          <ol className="timeline">
            {detail.timeline.map((t, n) => (
              <li key={n} className={`tl-${t.kind}`}>
                <div className="tl-meta">
                  <span className="tl-kind">{KIND_LABEL[t.kind] ?? t.kind}</span> {agentLabel(t.actor)} ·{" "}
                  {ago(props.now, t.ts)}
                </div>
                {t.summary && <div className="tl-text">{t.summary}</div>}
              </li>
            ))}
            {detail.timeline.length === 0 && <li className="muted small">Nothing recorded yet.</li>}
          </ol>
        </>
      )}
    </aside>
  );
}

function LatestReview(props: { review: ReviewRecord; now: number }) {
  const r = props.review;
  const verdict = r.verdict === "pass" ? "passed" : r.verdict === "changes" ? "changes requested" : "no review ran";
  return (
    <>
      <h3>Review</h3>
      <div className="review">
        <div>
          <span className={`chip ${r.verdict === "changes" ? "bad" : r.verdict === "pass" ? "good" : ""}`}>{verdict}</span>
          {r.reviewer && <span className="muted small"> by {r.reviewer}</span>}
          <span className="muted small"> · {ago(props.now, r.ts)} · <span className="mono">{r.commit_sha}</span></span>
        </div>
        <div className="small">
          {r.approver === "human" ? "Waits for your approval:" : "No approval needed from you."}
        </div>
        {r.reasons.length > 0 && (
          <ul className="small reasons">
            {r.reasons.map((x) => (
              <li key={x}>{x}</li>
            ))}
          </ul>
        )}
        {r.findings.length > 0 && (
          <ul className="small findings">
            {r.findings.map((f) => (
              <li key={f.text}>
                <span className={`prio-tag p${f.priority}`}>P{f.priority}</span> {f.text}
              </li>
            ))}
          </ul>
        )}
      </div>
    </>
  );
}
