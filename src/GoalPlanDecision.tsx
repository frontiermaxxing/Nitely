import { useState } from "react";
import { invoke } from "./backend";
import type { Move } from "./decisions";
import type { GoalPlanView } from "./types";
import { VENDOR_LABEL, vendorOf } from "./ui";
import "./goal-rows.css";

/**
 * A goal and its issues an agent drafted, decided once: edit the goal, leave issues out, then
 * approve (creates the goal and files the issues into it) or reject with a reason (files nothing).
 * Once decided, or after acting, it shows the plan as it was and what came of it.
 */
export function GoalPlanDecision(props: {
  plan: GoalPlanView;
  issueKey: string;
  /** The card's action is on its way or done: show, don't offer. */
  resolved: boolean;
  onAct: (move: Move, run: () => Promise<unknown>, answer?: string) => void;
}) {
  const { plan: p } = props.plan;
  const pending = props.plan.status === "pending" && !props.resolved;
  const [title, setTitle] = useState(p.title);
  const [outcome, setOutcome] = useState(p.outcome);
  const [target, setTarget] = useState(p.target_date ?? "");
  const [left, setLeft] = useState<Set<string>>(new Set());
  const [rejecting, setRejecting] = useState(false);
  const [why, setWhy] = useState("");
  const keep = p.issues.filter((i) => !left.has(i.ref));
  const who = VENDOR_LABEL[vendorOf(props.plan.created_by)];

  const toggle = (r: string) =>
    setLeft((s) => {
      const n = new Set(s);
      if (n.has(r)) n.delete(r);
      else n.add(r);
      return n;
    });
  const approve = () => {
    const edit = { title, outcome, target_date: target, drop: [...left] };
    const n = keep.length + p.existing.length;
    props.onAct("apply", () => invoke("approve_goal_plan", { key: props.issueKey, edit }), `Goal created with ${n} issue${n === 1 ? "" : "s"}.`);
  };
  const reject = () =>
    why.trim() && props.onAct("reject", () => invoke("move_issue", { key: props.issueKey, state: "Canceled", evidence: null, reason: why.trim() }), why.trim());

  return (
    <div className="gp">
      <p className="decision-matter">
        {who} drafted a goal from “{p.sentence}”.
      </p>
      {pending ? (
        <div className="gp-fields">
          <input className="gp-title" value={title} onChange={(e) => setTitle(e.target.value)} aria-label="The goal's title" />
          <textarea className="gp-outcome" rows={3} value={outcome} onChange={(e) => setOutcome(e.target.value)} aria-label="Its outcome: what is true once it is reached" />
          <label className="gp-date">
            Target <input type="date" value={target} onChange={(e) => setTarget(e.target.value)} />
          </label>
        </div>
      ) : (
        <p className="decision-why">
          <strong>{p.title}</strong>: {p.outcome}
          {p.target_date && ` Target ${p.target_date}.`}
        </p>
      )}
      <ul className="gp-items" aria-label="The issues it files">
        {p.issues.map((i) => (
          <li key={i.ref} className={left.has(i.ref) ? "gp-left" : undefined}>
            <label>
              {pending && <input type="checkbox" checked={!left.has(i.ref)} onChange={() => toggle(i.ref)} aria-label={`File ${i.title}`} />}
              <span className="mono gp-ref">{i.ref}</span>
              <span className="gp-item-title">{i.title}</span>
              <span className="gp-item-meta">
                {[i.kind, i.priority ? `P${i.priority}` : null, i.blocked_by?.length ? `after ${i.blocked_by.join(", ")}` : null].filter(Boolean).join(" · ")}
              </span>
            </label>
          </li>
        ))}
      </ul>
      {p.existing.length > 0 && <p className="decision-why">It also gathers {p.existing.join(", ")}.</p>}
      {props.plan.outcome && !pending && <p className="decision-why">{props.plan.outcome}</p>}
      {pending && (
        <>
          <div className="actions">
            <button className="btn primary" disabled={!title.trim() || !outcome.trim() || keep.length + p.existing.length === 0} onClick={approve}>
              Approve and file {keep.length + p.existing.length}
            </button>
            <button className="btn" onClick={() => setRejecting((r) => !r)} aria-expanded={rejecting}>
              Reject…
            </button>
          </div>
          {rejecting && (
            <form
              className="dd-words"
              onSubmit={(e) => {
                e.preventDefault();
                reject();
              }}
            >
              <input autoFocus value={why} onChange={(e) => setWhy(e.target.value)} placeholder="Why not? The agent reads this." aria-label="Why the plan is rejected" />
              <button className="btn" type="submit" disabled={!why.trim()}>
                Reject
              </button>
            </form>
          )}
        </>
      )}
    </div>
  );
}
