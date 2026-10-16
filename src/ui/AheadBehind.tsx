const HALF = 28;

/** Bar length for `n` commits when the longest bar in view stands for `max`: square root, so 2 and 200 both read. */
function len(n: number, max: number): number {
  if (n <= 0) return 0;
  return Math.max(3, Math.round((HALF - 2) * Math.min(1, Math.sqrt(n) / Math.sqrt(Math.max(max, 1)))));
}

/**
 * How far a branch has moved from its base, as two bars around a center tick: commits behind the
 * base to the left (faint), commits of its own to the right. `max` is the largest count among the
 * rows shown together, so their bars compare; the exact numbers sit at the ends.
 */
export function AheadBehind(props: { ahead: number | null; behind: number | null; base: string; max?: number }) {
  const { ahead, behind, base } = props;
  if (ahead === null || behind === null) {
    return (
      <span className="ui-ab ui-ab-unknown" title={`Not counted against ${base}`}>
        –
      </span>
    );
  }
  const max = props.max ?? Math.max(ahead, behind, 1);
  const a = len(ahead, max);
  const b = len(behind, max);
  const label = `${ahead} ahead of ${base}, ${behind} behind`;
  return (
    <span className="ui-ab" title={label} role="img" aria-label={label}>
      <span className="ui-ab-n ui-ab-behind">{behind > 0 ? behind : ""}</span>
      <svg width={HALF * 2 + 1} height="10" viewBox={`0 0 ${HALF * 2 + 1} 10`} aria-hidden="true">
        {b > 0 && <rect x={HALF - b} y="3" width={b} height="4" rx="2" className="ui-ab-bar-behind" />}
        {a > 0 && <rect x={HALF + 1} y="3" width={a} height="4" rx="2" className="ui-ab-bar-ahead" />}
        <rect x={HALF} y="0" width="1" height="10" className="ui-ab-base" />
      </svg>
      <span className="ui-ab-n ui-ab-ahead">{ahead > 0 ? ahead : ""}</span>
    </span>
  );
}
