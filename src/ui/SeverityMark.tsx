import type { ReviewRecord } from "../types";
import { describeFindings } from "./rounds";

/** A review finding's severity, P0 to P3. Only P0 and P1 block a land, so only they are red. */
export function SeverityMark(props: { priority: number }) {
  const p = Math.max(0, Math.min(3, props.priority));
  return <span className={`ui-sev ui-sev-p${p}`}>P{p}</span>;
}

const MAX_DOTS = 8;

/**
 * A review's findings counted as one small square each, most severe first, colored like
 * SeverityMark. A finding carried over from an earlier round (raised and not fixed) is hollow.
 * No findings is a single faint dash, so a clean review still has a mark in its column.
 */
export function FindingDots(props: { findings: ReviewRecord["findings"] }) {
  const sorted = [...props.findings].sort((a, b) => a.priority - b.priority || Number(!!b.carried) - Number(!!a.carried));
  const shown = sorted.slice(0, MAX_DOTS);
  const more = sorted.length - shown.length;
  const label = describeFindings(props.findings);
  return (
    <span className="ui-findings" role="img" aria-label={label} title={label}>
      {shown.length === 0 && <span className="ui-findings-none" />}
      {shown.map((f, i) => {
        const p = Math.max(0, Math.min(3, f.priority));
        return <span key={i} className={`ui-finding ui-finding-p${p}${f.carried ? " ui-finding-carried" : ""}`} />;
      })}
      {more > 0 && <span className="ui-findings-more">+{more}</span>}
    </span>
  );
}
