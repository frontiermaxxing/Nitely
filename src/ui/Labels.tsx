/**
 * An issue's labels (areas and platforms) as quiet `#words`, no pills. Past `max`, the rest are a
 * "+N" whose title names them. Nothing when there are none.
 */
export function Labels(props: { labels: string[] | undefined; max?: number }) {
  const labels = props.labels ?? [];
  if (labels.length === 0) return null;
  const max = props.max ?? labels.length;
  const shown = labels.slice(0, max);
  const rest = labels.slice(max);
  return (
    <span className="ui-labels" title={labels.map((l) => `#${l}`).join(" ")}>
      {shown.map((l) => (
        <span key={l} className="ui-label">
          #{l}
        </span>
      ))}
      {rest.length > 0 && <span className="ui-label">+{rest.length}</span>}
    </span>
  );
}
