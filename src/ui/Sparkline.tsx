/**
 * A small inline chart of a series, oldest first: a line by default, or bars for counts per
 * bucket (events per hour). Draw it in --fg-2 unless the series itself is the attention.
 */
export function Sparkline(props: {
  values: number[];
  width?: number;
  height?: number;
  kind?: "line" | "bars";
  color?: string;
  label: string;
  /** Stretch to the container's width (bars keep `width` as their drawing grid). */
  fill?: boolean;
}) {
  const { values } = props;
  const w = props.width ?? 96;
  const h = props.height ?? 22;
  const color = props.color ?? "var(--fg-2)";
  const max = Math.max(1, ...values);
  const pad = 2;
  if (values.length === 0) return null;

  if (props.kind === "bars") {
    const step = w / values.length;
    const bw = Math.max(1, step - Math.min(2, step * 0.3));
    return (
      <svg
        className="ui-spark"
        width={props.fill ? "100%" : w}
        height={h}
        viewBox={`0 0 ${w} ${h}`}
        preserveAspectRatio={props.fill ? "none" : undefined}
        role="img"
        aria-label={props.label}
      >
        <title>{props.label}</title>
        {values.map((v, i) => {
          const bh = v === 0 ? 1 : Math.max(2, (v / max) * (h - 1));
          return <rect key={i} x={i * step} y={h - bh} width={bw} height={bh} rx={Math.min(1, bw / 2)} fill={v === 0 ? "var(--line-strong)" : color} />;
        })}
      </svg>
    );
  }

  const x = (i: number) => (values.length === 1 ? w / 2 : pad + (i / (values.length - 1)) * (w - 2 * pad));
  const y = (v: number) => h - pad - (v / max) * (h - 2 * pad);
  const points = values.map((v, i) => `${x(i).toFixed(1)},${y(v).toFixed(1)}`).join(" ");
  const last = values.length - 1;
  return (
    <svg className="ui-spark" width={w} height={h} viewBox={`0 0 ${w} ${h}`} role="img" aria-label={props.label}>
      <title>{props.label}</title>
      <polyline points={points} fill="none" stroke={color} strokeWidth="1.4" strokeLinecap="round" strokeLinejoin="round" />
      <circle cx={x(last)} cy={y(values[last])} r="2" fill={color} />
    </svg>
  );
}
