import { useState } from "react";
import { invoke } from "./backend";
import type { Move } from "./decisions";
import { PlaybookDiff } from "./PlaybookDiff";
import type { PlaybookProposal } from "./types";
import { VENDOR_LABEL, vendorOf } from "./ui";

/**
 * A playbook change an agent proposed, decided once: Apply change (writes or commits it) or
 * Reject… with a reason the agent reads. The Inbox and the issue panel both show it, so the two
 * never offer different things for it (NIT-227). Once decided, or after acting, it shows the
 * change as it was and what came of it.
 */
export function PlaybookChangeDecision(props: {
  proposal: PlaybookProposal;
  issueKey: string;
  /** The card's action is on its way or done: show, don't offer. */
  resolved: boolean;
  busy: boolean;
  onAct: (move: Move, run: () => Promise<unknown>, answer?: string) => void;
}) {
  const p = props.proposal;
  const [rejecting, setRejecting] = useState(false);
  const [why, setWhy] = useState("");
  const pending = p.status === "pending" && !props.resolved;
  const apply = () =>
    props.onAct(
      "apply",
      () => invoke("move_issue", { key: props.issueKey, state: "Approved", evidence: null, reason: null }),
      p.in_repo ? "Committed on the integration branch." : "The playbook is changed.",
    );
  const reject = () =>
    why.trim() && props.onAct("reject", () => invoke("move_issue", { key: props.issueKey, state: "Canceled", evidence: null, reason: why.trim() }), why.trim());

  return (
    <div className="pb-decision">
      <p className="decision-matter">
        {VENDOR_LABEL[vendorOf(p.created_by)]} proposes a change to {p.in_repo ? ".nitely/playbook.toml" : "the playbook Nitely keeps"} for {p.repo}.
      </p>
      <p className="decision-why pb-why">{p.why}</p>
      <PlaybookDiff lines={p.diff} />
      {p.status !== "pending" ? (
        p.outcome && <p className="decision-why pb-applies">{p.outcome}</p>
      ) : p.stale ? (
        <p className="error small">It no longer applies to the playbook as it is now: {p.stale}. Reject it and ask for a new one.</p>
      ) : (
        <p className="decision-why pb-applies">
          {p.in_repo ? (
            <>
              Approving commits it to <span className="mono">.nitely/playbook.toml</span> on the integration branch it names.
            </>
          ) : (
            <>
              Approving writes it to <span className="mono">{p.path}</span>
            </>
          )}
        </p>
      )}
      {pending && (
        <>
          <div className="actions">
            <button className="btn primary" disabled={props.busy || !!p.stale} onClick={apply}>
              Apply change
            </button>
            <button className="btn" disabled={props.busy} onClick={() => setRejecting((r) => !r)} aria-expanded={rejecting}>
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
              <input autoFocus value={why} onChange={(e) => setWhy(e.target.value)} placeholder="Why not? The agent reads this." aria-label="Why the change is rejected" />
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
