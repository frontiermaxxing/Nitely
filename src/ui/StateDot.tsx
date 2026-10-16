import type { Liveness } from "./agents";
import { LIVENESS_LABEL } from "./agents";

/**
 * A session's liveness as a dot: working breathes in the live color, waiting is the attention
 * color, idle is a hollow ring, stale a dashed ring, ended a faint fill.
 */
export function StateDot(props: { state: Liveness | null; title?: string }) {
  const state = props.state ?? "ended";
  return <span className={`ui-dot ui-dot-${state}`} title={props.title ?? LIVENESS_LABEL[state]} role="img" aria-label={LIVENESS_LABEL[state]} />;
}
