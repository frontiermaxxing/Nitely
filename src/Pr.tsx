import type { PrRecord } from "./types";
import { openExternal } from "./util";

const CHECKS_LABEL = { success: "checks pass", failure: "checks fail", pending: "checks running" } as const;

/** A PR's number, state, CI checks and approvals, compact enough for a table cell. */
export function PrCell({ pr }: { pr: PrRecord }) {
  const approvals = pr.approved_by.length;
  return (
    <>
      <a href={pr.url} onClick={openExternal(pr.url)}>
        #{pr.number}
      </a>{" "}
      <span className={`chip pr-${pr.state.toLowerCase()}`}>{pr.state.toLowerCase()}</span>
      {pr.checks && <span className={`chip checks-${pr.checks}`}>{CHECKS_LABEL[pr.checks]}</span>}
      <div className="small muted">
        {pr.approved_by_me ? "you approved" : approvals ? `${approvals} approval${approvals === 1 ? "" : "s"}` : "no approvals"}
        {pr.comment_count > 0 && ` · ${pr.comment_count} comments`}
      </div>
    </>
  );
}
