import type { PrRecord } from "./types";
import { openExternal } from "./util";
import { ResultMark } from "./ui";

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

/** CI checks as a mark with its label. */
export function ChecksMark({ checks }: { checks: "success" | "failure" | "pending" }) {
  return (
    <span className={`checks checks-${checks}`} title={CHECKS_LABEL[checks]}>
      <ResultMark result={checks === "success" ? "pass" : checks === "failure" ? "fail" : "pending"} />
      {CHECKS_LABEL[checks]}
    </span>
  );
}
