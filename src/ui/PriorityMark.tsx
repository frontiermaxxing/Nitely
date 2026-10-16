import { PRIORITY } from "../util";

/** Issue priority as three rising bars: Low fills one, Medium two, High three; Urgent is red. */
export function PriorityMark(props: { priority: number }) {
  const p = props.priority;
  const filled = p === 1 || p === 2 ? 3 : p === 3 ? 2 : p === 4 ? 1 : 0;
  const color = p === 1 ? "var(--sev-p0)" : "var(--fg-2)";
  const label = PRIORITY[p] || "No priority";
  return (
    <svg className="ui-prio" width="12" height="12" viewBox="0 0 12 12" role="img" aria-label={label}>
      <title>{label}</title>
      {[0, 1, 2].map((i) => (
        <rect
          key={i}
          x={1 + i * 3.75}
          y={8 - i * 3}
          width="2.5"
          height={3 + i * 3}
          rx="0.8"
          fill={i < filled ? color : "var(--line-strong)"}
          opacity={p === 0 ? 0.6 : 1}
        />
      ))}
    </svg>
  );
}
