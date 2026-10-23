import { useEffect, useState } from "react";
import { invoke } from "./backend";
import type { Ask } from "./types";
import { agentLabel, agentTitle, ago } from "./util";
import { Loading } from "./ui";
import { Markdown } from "./ui/Markdown";
import "./issue.css";

export { askIdOf } from "./askToken";

/**
 * One ask as its link opens it (NIT-546): the question, its options, the answer and when, who it
 * reached, and its issue. An answered ask shows as answered, not as missing from the Inbox.
 */
export function AskPanel(props: { id: number; now: number; version: number; onClose: () => void; onOpenIssue: (key: string) => void }) {
  const [ask, setAsk] = useState<Ask | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    let live = true;
    invoke<Ask>("ask_detail", { id: props.id }).then(
      (a) => live && (setAsk(a), setError(null)),
      (e) => live && setError(String(e)),
    );
    return () => {
      live = false;
    };
  }, [props.id, props.version]);
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && props.onClose();
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [props.onClose]);

  return (
    <>
      <div className="panel-veil" onMouseDown={props.onClose} aria-hidden="true" />
      <aside className="panel ask-panel" aria-label={`Ask ${props.id}`} tabIndex={-1}>
        <div className="panel-main">
          <div className="panel-head">
            <span className="panel-crumb">
              {ask?.issue_key && <span className="mono">{ask.issue_key}</span>}
              {ask?.issue_key && <span>/</span>}
              <span className="mono">ask {props.id}</span>
            </span>
            <button className="btn ghost" onClick={props.onClose} title="Close (Esc)">
              ✕
            </button>
          </div>
          {error && <p className="error">{error}</p>}
          {!ask && !error && <Loading />}
          {ask && (
            <div className="ask-body">
              <p className="ask-meta muted small">
                <span title={agentTitle(ask.asked_by)}>{agentLabel(ask.asked_by)}</span> asked {ago(props.now, ask.asked_at)}
              </p>
              <div className="ask-question"><Markdown src={ask.question} onOpenIssue={props.onOpenIssue} /></div>
              {ask.options.length > 0 && (
                <ul className="ask-options" aria-label="Options">
                  {ask.options.map((o) => (
                    <li key={o} className={o === ask.answer ? "chosen" : undefined}>
                      {o}
                    </li>
                  ))}
                </ul>
              )}
              <AskOutcome ask={ask} now={props.now} />
              {ask.issue_key && (
                <p className="ask-issue">
                  About{" "}
                  <button className="link mono" onClick={() => props.onOpenIssue(ask.issue_key!)}>
                    {ask.issue_key}
                  </button>
                </p>
              )}
            </div>
          )}
        </div>
      </aside>
    </>
  );
}

/** Where the ask stands: answered (and who the answer reached), closed unanswered, or waiting. */
function AskOutcome({ ask, now }: { ask: Ask; now: number }) {
  if (ask.answer != null) {
    return (
      <div className="ask-answer">
        <span className="muted small">You answered {ask.answered_at ? ago(now, ask.answered_at) : ""}</span>
        <p>{ask.answer}</p>
        <span className="muted small">
          {ask.delivered_to
            ? `It reached ${agentLabel(ask.delivered_to)}${ask.delivered_at ? ` ${ago(now, ask.delivered_at)}` : ""}.`
            : "No agent has picked the answer up yet."}
        </span>
      </div>
    );
  }
  if (ask.closed) return <p className="ask-answer muted">Closed without an answer: {ask.closed}.</p>;
  return <p className="ask-answer muted">Waiting for your answer, in the Inbox.</p>;
}
