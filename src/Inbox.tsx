import { useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { Ask, Board, IssueRecord } from "./types";
import { agentLabel, ago } from "./util";

/** Everything waiting on the human, each with its action in place. */
export function Inbox(props: { board: Board; now: number; onOpenIssue: (key: string) => void; onChanged: () => void }) {
  const { board, now } = props;
  const flagged = board.repos.flatMap((r) => r.rows.filter((row) => row.needs_me).map((row) => ({ repo: r.name, row })));
  const empty = board.asks.length === 0 && board.needs_me_issues.length === 0 && flagged.length === 0;

  return (
    <div className="page">
      <h1>Inbox</h1>
      {empty && <p className="muted">Nothing needs you. Agents will show up here when they ask something or wait on an approval.</p>}

      {board.asks.length > 0 && (
        <section>
          <h2>Questions</h2>
          {board.asks.map((a) => (
            <AskCard key={a.id} ask={a} now={now} onOpenIssue={props.onOpenIssue} onChanged={props.onChanged} />
          ))}
        </section>
      )}

      {board.needs_me_issues.length > 0 && (
        <section>
          <h2>Waiting on you</h2>
          {board.needs_me_issues.map((i) => (
            <WaitingIssue key={i.key} issue={i} onOpenIssue={props.onOpenIssue} onChanged={props.onChanged} />
          ))}
        </section>
      )}

      {flagged.length > 0 && (
        <section>
          <h2>Stuck worktrees</h2>
          {flagged.map(({ repo, row }) => (
            <div className="card" key={row.worktree}>
              <div className="card-title">
                {row.issue_key ? (
                  <button className="link mono" onClick={() => props.onOpenIssue(row.issue_key!)}>{row.issue_key}</button>
                ) : (
                  <span className="mono">{row.branch ?? "detached"}</span>
                )}
                <span className="muted"> · {repo}</span>
              </div>
              <p>
                {row.state?.rebase_in_progress
                  ? "A rebase stopped on a conflict in this worktree."
                  : row.session?.state === "waiting"
                    ? `The agent is waiting for input: ${row.session.last_activity ?? ""}`
                    : row.issue?.title}
              </p>
              <p className="muted small mono">{row.worktree}</p>
            </div>
          ))}
        </section>
      )}
    </div>
  );
}

function AskCard(props: { ask: Ask; now: number; onOpenIssue: (key: string) => void; onChanged: () => void }) {
  const { ask } = props;
  const [text, setText] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const answer = async (value: string) => {
    if (!value.trim()) return;
    setBusy(true);
    try {
      await invoke("answer_ask", { id: ask.id, answer: value });
      props.onChanged();
    } catch (e) {
      setError(String(e));
    }
    setBusy(false);
  };

  return (
    <div className="card">
      <div className="card-meta">
        {agentLabel(ask.asked_by)} · {ago(props.now, ask.asked_at)}
        {ask.issue_key && (
          <>
            {" · "}
            <button className="link mono" onClick={() => props.onOpenIssue(ask.issue_key!)}>{ask.issue_key}</button>
          </>
        )}
      </div>
      <p className="card-title">{ask.question}</p>
      <div className="actions">
        {ask.options.map((o) => (
          <button key={o} className="btn" disabled={busy} onClick={() => answer(o)}>
            {o}
          </button>
        ))}
        <form
          className="answer"
          onSubmit={(e) => {
            e.preventDefault();
            answer(text);
          }}
        >
          <input value={text} onChange={(e) => setText(e.target.value)} placeholder={ask.options.length ? "Or type an answer" : "Type an answer"} />
          <button className="btn primary" disabled={busy || !text.trim()}>Send</button>
        </form>
      </div>
      {error && <p className="error small">{error}</p>}
    </div>
  );
}

function WaitingIssue(props: { issue: IssueRecord; onOpenIssue: (key: string) => void; onChanged: () => void }) {
  const { issue } = props;
  const [error, setError] = useState<string | null>(null);
  const move = async (state: string) => {
    try {
      await invoke("move_issue", { key: issue.key, state, evidence: null, reason: null });
      props.onChanged();
    } catch (e) {
      setError(String(e));
    }
  };
  return (
    <div className="card">
      <div className="card-title">
        <button className="link mono" onClick={() => props.onOpenIssue(issue.key)}>{issue.key}</button> {issue.title}
      </div>
      {issue.origin === "nitely" ? (
        <div className="actions">
          <button className="btn primary" onClick={() => move("Approved")}>Approve</button>
          <button className="btn" onClick={() => move("Todo")}>Send back to Todo</button>
        </div>
      ) : (
        <p className="muted small">In Linear. Act on it there.</p>
      )}
      {error && <p className="error small">{error}</p>}
    </div>
  );
}
