import type { PrRecord } from "../types";
import { openExternal } from "../util";
import { CHECKS_LABEL, ChecksMark } from "./ChecksMark";

/**
 * A pull request in one small mark: its number and its checks. Merged and declined ones fade.
 * `link` makes the number open the PR in the browser; leave it off inside a clickable card.
 */
export function PrMark(props: { pr: PrRecord; link?: boolean }) {
  const { pr } = props;
  const state = pr.state.toLowerCase();
  const approvals = pr.approved_by.length;
  const hover = [
    `Pull request #${pr.number}`,
    state,
    pr.checks ? CHECKS_LABEL[pr.checks] : "",
    pr.approved_by_me ? "you approved" : approvals ? `${approvals} approval${approvals === 1 ? "" : "s"}` : "",
  ]
    .filter(Boolean)
    .join(" · ");
  const number = props.link ? (
    <a className="ui-pr-number" href={pr.url} onClick={openExternal(pr.url)}>
      #{pr.number}
    </a>
  ) : (
    <span className="ui-pr-number">#{pr.number}</span>
  );
  return (
    <span className={`ui-pr${state === "open" ? "" : " ui-pr-closed"}`} title={hover}>
      {number}
      {pr.checks && state === "open" && <ChecksMark checks={pr.checks} />}
    </span>
  );
}
