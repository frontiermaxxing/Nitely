import type { Liveness } from "./agents";
import { LIVENESS_LABEL } from "./agents";

/**
 * A session's liveness as a dot: working breathes in the live color, waiting is the attention
 * color (ringed once the wait is long), idle is a hollow ring, stale a faded ring, ended a faint fill.
 */
export function StateDot(props: { state: Liveness | null; long?: boolean; title?: string }) {
  const state = props.state ?? "ended";
  const long = props.long && state === "waiting";
  return (
    <span
      className={`ui-dot ui-dot-${state}${long ? " ui-dot-long" : ""}`}
      title={props.title ?? `${LIVENESS_LABEL[state]}${long ? ", for a while" : ""}`}
      role="img"
      aria-label={LIVENESS_LABEL[state]}
    />
  );
}
