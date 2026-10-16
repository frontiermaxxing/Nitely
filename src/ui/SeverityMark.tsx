/** A review finding's severity, P0 to P3. Only P0 and P1 block a land, so only they are red. */
export function SeverityMark(props: { priority: number }) {
  const p = Math.max(0, Math.min(3, props.priority));
  return <span className={`ui-sev ui-sev-p${p}`}>P{p}</span>;
}
