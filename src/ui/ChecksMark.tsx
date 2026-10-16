import type { PrRecord } from "../types";
import { ResultMark } from "./ResultMark";

type Checks = Exclude<PrRecord["checks"], "">;

export const CHECKS_LABEL: Record<Checks, string> = { success: "checks pass", failure: "checks fail", pending: "checks running" };

const RESULT = { success: "pass", failure: "fail", pending: "pending" } as const;

/**
 * CI checks as a mark (a `ResultMark`): a quiet tick when they pass, a red cross when they fail,
 * a dashed ring while they run. `label` adds the words after it.
 */
export function ChecksMark(props: { checks: Checks; label?: boolean }) {
  const { checks } = props;
  return (
    <span className={`ui-checks ui-checks-${checks}`} title={CHECKS_LABEL[checks]}>
      <ResultMark result={RESULT[checks]} title={props.label ? undefined : CHECKS_LABEL[checks]} />
      {props.label && CHECKS_LABEL[checks]}
    </span>
  );
}
