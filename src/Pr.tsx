import type { PrRecord } from "./types";
import { openExternal } from "./util";

const CHECKS_LABEL = { success: "checks pass", failure: "checks fail", pending: "checks running" } as const;

/** A PR's number, state, CI checks and approvals, compact enough for a table cell. */
export function PrCell({ pr }: { pr: PrRecord }) {
  const approvals = pr.approved_by.length;
  const state = pr.state.toLowerCase();
  return (
    <>
      <div className="cell-line">
        <a href={pr.url} onClick={openExternal(pr.url)} className="mono">
          #{pr.number}
        </a>
        <span className={state === "open" ? "" : "muted"}>{state}</span>
        {pr.checks && <ChecksMark checks={pr.checks} />}
      </div>
      <div className="cell-sub">
        {pr.approved_by_me ? "you approved" : approvals ? `${approvals} approval${approvals === 1 ? "" : "s"}` : "no approvals"}
        {pr.comment_count > 0 && ` · ${pr.comment_count} comments`}
      </div>
    </>
  );
}

/** CI checks as a mark: a quiet tick when they pass, a red cross when they fail, a ring while they run. */
function ChecksMark({ checks }: { checks: "success" | "failure" | "pending" }) {
  return (
    <span className={`checks checks-${checks}`} title={CHECKS_LABEL[checks]}>
      <svg width="12" height="12" viewBox="0 0 12 12" aria-hidden="true">
        {checks === "success" && <path d="M2.5 6.3 5 8.6 9.5 3.6" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round" />}
        {checks === "failure" && <path d="M3 3l6 6M9 3 3 9" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" />}
        {checks === "pending" && <circle cx="6" cy="6" r="3.8" fill="none" stroke="currentColor" strokeWidth="1.5" strokeDasharray="4 2.2" />}
      </svg>
      {CHECKS_LABEL[checks]}
    </span>
  );
}
