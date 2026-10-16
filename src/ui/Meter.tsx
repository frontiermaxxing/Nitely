/** One part of a meter: how many, in which color token, called what. */
export interface MeterPart {
  value: number;
  color: string;
  label: string;
}

/**
 * Parts of a whole as one segmented bar (open issues by phase, checks by result). Empty parts
 * take no space; each segment names itself on hover.
 */
export function Meter(props: { parts: MeterPart[]; width?: number | string; label: string }) {
  const total = props.parts.reduce((n, p) => n + p.value, 0);
  const summary = props.parts
    .filter((p) => p.value > 0)
    .map((p) => `${p.value} ${p.label}`)
    .join(", ");
  return (
    <div className="ui-meter" style={{ width: props.width ?? 160 }} role="img" aria-label={`${props.label}: ${summary || "none"}`} title={summary || undefined}>
      {total === 0 ? (
        <span className="ui-meter-seg" style={{ flex: 1, background: "var(--line)" }} />
      ) : (
        props.parts
          .filter((p) => p.value > 0)
          .map((p) => <span key={p.label} className="ui-meter-seg" style={{ flex: p.value, background: p.color }} title={`${p.value} ${p.label}`} />)
      )}
    </div>
  );
}
