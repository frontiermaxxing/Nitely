import { useEffect, useRef, useState } from "react";
import { invoke } from "./backend";
import type { IssueDetail, IssueRecord, LinkedIssue } from "./types";
import { agentLabel, ago, openExternal, PRIORITY } from "./util";
import { IssueJourney, WhoHadIt } from "./IssueJourney";
import { typing } from "./Palette";
import { AgentMark, IssueToken, Loading, PrMark, PriorityMark, StationGlyph, StationTrack, VENDOR_LABEL, trackOf, vendorOf } from "./ui";
import "./issue.css";

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
  landed: "landed",
  land_failed: "land stopped",
};

/** One issue and its timeline, in a panel over the right side. */
export function IssuePanel(props: {
  issueKey: string;
  now: number;
  version: number;
  onClose: () => void;
  onChanged: () => void;
  onOpenIssue?: (key: string) => void;
}) {
  const [detail, setDetail] = useState<IssueDetail | null>(null);
  const [error, setError] = useState<string | null>(null);
  // Done needs evidence and Canceled a reason, typed in place.
  const [pending, setPending] = useState<"Done" | "Canceled" | null>(null);
  const [why, setWhy] = useState("");
  // After "m", the next key picks where the issue moves.
  const [moving, setMoving] = useState(false);
  // The sheet takes the focus when it opens, so its keys act on this issue and nothing behind it;
  // closing gives the focus back to where it was.
  const sheet = useRef<HTMLElement>(null);
  useEffect(() => {
    const was = document.activeElement as HTMLElement | null;
    sheet.current?.focus({ preventScroll: true });
    return () => {
      if (was?.isConnected) was.focus({ preventScroll: true });
    };
  }, [props.issueKey]);

  useEffect(() => {
    let live = true;
    const load = () =>
      invoke<IssueDetail>("issue_detail", { key: props.issueKey })
        .then((d) => live && (setDetail(d), setError(null)))
        .catch((e) => live && setError(String(e)));
    load();
    return () => {
      live = false;
    };
  }, [props.issueKey, props.version]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && !e.defaultPrevented && props.onClose();
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
  const track = detail && trackOf(detail.issue, detail.timeline, detail.reviews);
  const waiting = issue?.state === "Needs You";
  const waitedSince = detail ? [...detail.timeline].reverse().find((e) => e.kind === "submit" || (e.kind === "issue_moved" && /→ Needs You/.test(e.summary ?? ""))) : undefined;
  const review = detail?.reviews[0];
  const pr = detail?.prs[0];
  const lastGate = detail ? [...detail.timeline].reverse().find((e) => e.kind === "gate_passed" || e.kind === "gate_failed") : undefined;
  const native = issue?.origin === "nitely";
  // Keys act only while the focus is in this sheet: a approve, t back to Todo, m then t, d or c.
  const onKeyDown = (e: React.KeyboardEvent) => {
    if (!issue || !native || e.ctrlKey || e.metaKey || e.altKey || typing(e.target)) return;
    const key = e.key.toLowerCase();
    if (moving) {
      e.preventDefault();
      setMoving(false);
      if (key === "t" && issue.state !== "Todo") move("Todo");
      else if (key === "d" && issue.state !== "Done") setPending("Done");
      else if (key === "c" && issue.state !== "Canceled") setPending("Canceled");
      return;
    }
    if (key === "a" && waiting) {
      e.preventDefault();
      move("Approved");
    } else if (key === "t" && issue.state !== "Todo") {
      e.preventDefault();
      move("Todo");
    } else if (key === "m") {
      e.preventDefault();
      setMoving(true);
    }
  };
  return (
    <>
      <div className="panel-veil" onMouseDown={props.onClose} aria-hidden="true" />
      <aside className="panel" aria-label={`Issue ${props.issueKey}`} ref={sheet} tabIndex={-1} onKeyDown={onKeyDown} onBlur={() => setMoving(false)}>
        <div className="panel-main">
          <div className="panel-head">
            <span className="panel-crumb">
              {issue?.project && <span>{issue.project}</span>}
              {issue?.project && <span>/</span>}
              <span className="mono">{props.issueKey}</span>
            </span>
            <button className="btn ghost" onClick={props.onClose} title="Close (Esc)">✕</button>
          </div>
          {error && <p className="error small">{error}</p>}
          {!issue && !error && <Loading />}
          {issue && (
            <>
              <div className="panel-title-row">
                {track && <StationGlyph track={track} size={16} />}
                <h2 className="panel-title">{issue.title}</h2>
              </div>
              {track && (
                <div className="panel-track">
                  <StationTrack key={issue.key} track={track} size="full" now={props.now} />
                </div>
              )}
              <WhoHadIt detail={detail} now={props.now} />
              <IssueJourney key={issue.key} detail={detail} now={props.now} />
              <details className="panel-timeline">
                <summary>
                  Timeline <span className="muted">· {detail.timeline.length} event{detail.timeline.length === 1 ? "" : "s"}</span>
                </summary>
                <ol className="timeline">
                  {detail.timeline.map((t, n) => (
                    <li key={n} className={`tl-${t.kind}`}>
                      <span className="tl-mark">
                        <AgentMark vendor={t.actor} size={14} title={agentLabel(t.actor)} />
                      </span>
                      <div className="tl-meta">
                        <span className="tl-kind">{KIND_LABEL[t.kind] ?? t.kind}</span> {agentLabel(t.actor)} · {ago(props.now, t.ts)}
                      </div>
                      {t.summary && <div className="tl-text">{t.summary}</div>}
                    </li>
                  ))}
                  {detail.timeline.length === 0 && <li className="muted small">Nothing recorded yet.</li>}
                </ol>
              </details>
            </>
          )}
        </div>
        {issue && detail && (
          <div className="panel-rail">
            {waiting && issue.origin === "nitely" && (
              <div className="panel-waiting">
                <p className="panel-waiting-head">Waiting on you{waitedSince && ` · ${ago(props.now, waitedSince.ts).replace(" ago", "")}`}</p>
                <p className="panel-waiting-why">
                  {review?.reasons.length ? `${capital(review.reasons.slice(0, 2).join("; "))}.` : "The playbook wants a person to approve before it lands."}
                  {review?.reviewer && review.verdict === "pass" && ` ${VENDOR_LABEL[vendorOf(review.reviewer)]} passed it.`}
                </p>
                <div className="actions">
                  <button className="btn primary" onClick={() => move("Approved")} title="Approve (a)">
                    Approve <kbd>a</kbd>
                  </button>
                  <button className="btn" onClick={() => move("Todo")} title="Back to Todo (t)">
                    Back to Todo <kbd>t</kbd>
                  </button>
                </div>
              </div>
            )}
            <dl className="panel-facts">
              <dt>State</dt>
              <dd>{issue.state}</dd>
              {issue.claimed_by && (
                <>
                  <dt>Agent</dt>
                  <dd>
                    <AgentMark vendor={issue.claimed_by} size={16} /> {VENDOR_LABEL[vendorOf(issue.claimed_by)]}
                  </dd>
                </>
              )}
              {review?.reviewer && (
                <>
                  <dt>Reviewer</dt>
                  <dd>
                    <AgentMark vendor={review.reviewer} size={16} /> {VENDOR_LABEL[vendorOf(review.reviewer)]}
                    {review.note && <span className="muted"> ({review.note})</span>}
                  </dd>
                </>
              )}
              {pr && (
                <>
                  <dt>Branch</dt>
                  <dd className="mono path">{pr.branch}</dd>
                  <dt>Pull request</dt>
                  <dd>
                    <PrMark pr={pr} link /> <span className="muted">{pr.state.toLowerCase()}</span>
                  </dd>
                </>
              )}
              {(issue.kind || issue.priority > 0) && (
                <>
                  <dt>Kind</dt>
                  <dd>
                    {issue.priority > 0 && <PriorityMark priority={issue.priority} />}
                    {[issue.kind, issue.priority > 0 && PRIORITY[issue.priority]].filter(Boolean).join(" · ")}
                  </dd>
                </>
              )}
              {issue.source && (
                <>
                  <dt>Filed by</dt>
                  <dd>{issue.source}</dd>
                </>
              )}
              {lastGate && (
                <>
                  <dt>Gate</dt>
                  <dd className={lastGate.kind === "gate_failed" ? "error" : "live-ink"} title={lastGate.summary ?? undefined}>
                    {lastGate.kind === "gate_failed" ? "failed" : "passed"} {ago(props.now, lastGate.ts)}
                  </dd>
                </>
              )}
            </dl>
            <Relations
              issueKey={issue.key}
              relations={detail.relations}
              editable={issue.origin === "nitely"}
              onOpenIssue={props.onOpenIssue}
              onChanged={props.onChanged}
            />
            {(issue.url && issue.origin !== "nitely") || issue.external_url ? (
              <div className="facts">
                {issue.url && issue.origin !== "nitely" && (
                  <a href={issue.url} onClick={openExternal(issue.url)}>
                    Open in {issue.origin}
                  </a>
                )}
                {issue.external_url && (
                  <a href={issue.external_url} onClick={openExternal(issue.external_url)}>
                    {externalName(issue.external_url)}
                  </a>
                )}
              </div>
            ) : null}
            {(issue.body || issue.evidence) && <div className="panel-rule" />}
            {issue.body && <p className="body">{issue.body}</p>}
            {issue.evidence && (
              <p className="small">
                <span className="muted">Evidence: </span>
                {issue.evidence}
              </p>
            )}
            {moving && (
              <p className="panel-moving" role="status">
                Move to: <kbd>t</kbd> Todo · <kbd>d</kbd> Done… · <kbd>c</kbd> Cancel… · <kbd>esc</kbd> stay
              </p>
            )}
            {issue.origin === "nitely" && (
              <div className="actions panel-moves">
                {!waiting && issue.state !== "Todo" && <button className="btn" onClick={() => move("Todo")}>Back to Todo</button>}
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
          </div>
        )}
      </aside>
    </>
  );
}

const capital = (s: string) => s.charAt(0).toUpperCase() + s.slice(1);

/** "Imported from Linear" for a Linear address, else the host. */
function externalName(url: string): string {
  try {
    const host = new URL(url).hostname.replace(/^www\./, "");
    return host === "linear.app" ? "Imported from Linear" : `From ${host}`;
  } catch {
    return "Where it came from";
  }
}

/** An issue on the other end of a relation, shaped as an issue record so IssueToken can draw it. */
function asIssue(l: LinkedIssue): IssueRecord {
  return {
    key: l.key,
    origin: "nitely",
    project: null,
    title: l.title,
    body: "",
    url: "",
    state: l.state,
    state_type: l.open ? "" : l.state === "Done" ? "completed" : "canceled",
    kind: null,
    source: null,
    priority: 0,
    labels: [],
    evidence: "",
    claimed_by: null,
    created_at: "",
    updated_at: "",
    blocked_by: [],
    blocks: [],
    urgency: 0,
  };
}

/** "Waiting on" and "Blocks", with a way to add or remove a blocker. */
function Relations(props: {
  issueKey: string;
  relations: IssueDetail["relations"];
  editable: boolean;
  onOpenIssue?: (key: string) => void;
  onChanged: () => void;
}) {
  const { blocked_by, blocks } = props.relations;
  const [adding, setAdding] = useState(false);
  const [key, setKey] = useState("");
  const [error, setError] = useState<string | null>(null);
  const set = async (blocker: string, blocked: boolean) => {
    try {
      await invoke("set_blocker", { key: props.issueKey, blocker, blocked });
      setError(null);
      setAdding(false);
      setKey("");
      props.onChanged();
    } catch (e) {
      setError(String(e));
    }
  };
  if (blocked_by.length === 0 && blocks.length === 0 && !props.editable) return null;
  return (
    <div className="relations">
      {blocked_by.length > 0 && (
        <div className="relation">
          <span className="relation-label">Waiting on</span>
          <span className="relation-items">
            {blocked_by.map((l) => (
              <span key={l.key} className="relation-item">
                <IssueToken issue={asIssue(l)} title onOpen={props.onOpenIssue} />
                {props.editable && (
                  <button className="btn ghost relation-remove" title={`Stop waiting on ${l.key}`} onClick={() => set(l.key, false)}>
                    ✕
                  </button>
                )}
              </span>
            ))}
          </span>
        </div>
      )}
      {blocks.length > 0 && (
        <div className="relation">
          <span className="relation-label">Blocks</span>
          <span className="relation-items">
            {blocks.map((l) => (
              <span key={l.key} className="relation-item">
                <IssueToken issue={asIssue(l)} title onOpen={props.onOpenIssue} />
              </span>
            ))}
          </span>
        </div>
      )}
      {props.editable &&
        (adding ? (
          <form
            className="answer relation-add"
            onSubmit={(e) => {
              e.preventDefault();
              if (key.trim()) set(key.trim().toUpperCase(), true);
            }}
          >
            <input autoFocus value={key} onChange={(e) => setKey(e.target.value)} placeholder="Key of the issue this waits on, e.g. NIT-12" />
            <button className="btn primary" disabled={!key.trim()}>Add</button>
            <button type="button" className="btn ghost" onClick={() => setAdding(false)}>Back</button>
          </form>
        ) : (
          <button className="link relation-new small" onClick={() => setAdding(true)}>
            Add a blocker
          </button>
        ))}
      {error && <p className="error small">{error}</p>}
    </div>
  );
}
