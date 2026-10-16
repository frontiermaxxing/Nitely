import { useState } from "react";
import { invoke } from "./backend";
import type { Ask, Board, IssueRecord } from "./types";
import { agentLabel, ago } from "./util";
import { AgentMark, CountBadge, EmptyState, IssueToken, StateDot, livenessOf } from "./ui";

/** Everything waiting on the human, each with its action in place. */
export function Inbox(props: { board: Board; now: number; onOpenIssue: (key: string) => void; onChanged: () => void }) {
  const { board, now } = props;
  const flagged = board.repos.flatMap((r) => r.rows.filter((row) => row.needs_me).map((row) => ({ repo: r.name, row })));
  const total = board.asks.length + board.needs_me_issues.length + flagged.length;
  // The issues the board knows, so a question can show its issue as a token.
  const known = new Map<string, IssueRecord>();
  for (const i of [...board.repos.flatMap((r) => r.rows.map((row) => row.issue)), ...board.needs_me_issues]) if (i) known.set(i.key, i);

  return (
    <div className="page">
      <div className="page-head">
        <h1>Inbox</h1>
        {total > 0 && <span className="head-note">{total === 1 ? "1 thing waits on you" : `${total} things wait on you`}</span>}
      </div>
      {total === 0 && (
        <EmptyState title="Nothing needs you.">Agents show up here when they ask something, wait on your approval, or get stuck.</EmptyState>
      )}

      {board.asks.length > 0 && (
        <section>
          <h2>
            Questions <CountBadge count={board.asks.length} tone="attention" />
          </h2>
          {board.asks.map((a) => (
            <AskCard key={a.id} ask={a} issue={a.issue_key ? known.get(a.issue_key) : undefined} now={now} onOpenIssue={props.onOpenIssue} onChanged={props.onChanged} />
          ))}
        </section>
      )}

      {board.needs_me_issues.length > 0 && (
        <section>
          <h2>
            Waiting on your approval <CountBadge count={board.needs_me_issues.length} tone="attention" />
          </h2>
          {board.needs_me_issues.map((i) => (
            <WaitingIssue key={i.key} issue={i} onOpenIssue={props.onOpenIssue} onChanged={props.onChanged} />
          ))}
        </section>
      )}

      {flagged.length > 0 && (
        <section>
          <h2>
            Stuck worktrees <CountBadge count={flagged.length} tone="attention" />
          </h2>
          {flagged.map(({ repo, row }) => (
            <div className="card" key={row.worktree}>
              <div className="card-meta">
                {row.issue ? (
                  <IssueToken issue={row.issue} onOpen={props.onOpenIssue} />
                ) : (
                  <span className="mono">{row.issue_key ?? row.branch ?? "detached"}</span>
                )}
                <span>{repo}</span>
                {row.session && <AgentMark vendor={row.session.vendor} state={livenessOf(row.session, now)} />}
              </div>
              <p className="card-title">
                {row.state?.rebase_in_progress
                  ? "A rebase stopped on a conflict in this worktree."
                  : row.session?.state === "waiting"
                    ? `The agent is waiting for input: ${row.session.last_activity ?? ""}`
                    : row.issue?.title}
              </p>
              <p className="muted small mono path">{row.worktree}</p>
            </div>
          ))}
        </section>
      )}
    </div>
  );
}

function AskCard(props: { ask: Ask; issue?: IssueRecord; now: number; onOpenIssue: (key: string) => void; onChanged: () => void }) {
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
        <StateDot state="waiting" title="Waiting for your answer" />
        <AgentMark vendor={ask.asked_by} />
        <span>{agentLabel(ask.asked_by)}</span>
        <span>{ago(props.now, ask.asked_at)}</span>
        {props.issue ? (
          <IssueToken issue={props.issue} onOpen={props.onOpenIssue} />
        ) : (
          ask.issue_key && (
            <button className="link mono" onClick={() => props.onOpenIssue(ask.issue_key!)}>
              {ask.issue_key}
            </button>
          )
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
          <button className="btn primary" disabled={busy || !text.trim()}>
            Send
          </button>
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
      <div className="card-meta">
        <IssueToken issue={issue} onOpen={props.onOpenIssue} />
        {issue.kind && <span>{issue.kind}</span>}
      </div>
      <p className="card-title">{issue.title}</p>
      {issue.origin === "nitely" ? (
        <div className="actions">
          <button className="btn primary" onClick={() => move("Approved")}>
            Approve
          </button>
          <button className="btn quiet" onClick={() => move("Todo")}>
            Send back to Todo
          </button>
        </div>
      ) : (
        <p className="muted small">In Linear. Act on it there.</p>
      )}
      {error && <p className="error small">{error}</p>}
    </div>
  );
}
