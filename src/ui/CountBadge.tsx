/**
 * A count. `attention` fills it with the attention color and is only for things waiting on the
 * human; `live` is for work in motion; `quiet` (the default) is plain faint figures. Zero hides.
 */
export function CountBadge(props: { count: number; tone?: "attention" | "live" | "quiet"; title?: string }) {
  if (!props.count) return null;
  return (
    <span className={`ui-count ui-count-${props.tone ?? "quiet"}`} title={props.title}>
      {props.count}
    </span>
  );
}
