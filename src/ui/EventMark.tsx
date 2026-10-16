import type { MarkKind } from "../types";

/** What each mark says, for hover text and legends. */
export const EVENT_LABEL: Record<MarkKind, string> = {
  start: "started an issue",
  commit: "commit",
  gate_pass: "gate passed",
  gate_fail: "gate failed",
  submit: "submitted",
  review_pass: "review passed",
  review_changes: "review asked for changes",
  review_fail: "review did not finish",
  land: "landed",
  land_fail: "land stopped",
  ask: "asked you",
  handoff: "handoff note",
};

/**
 * Something that happened on an agent's lane, as a 12px shape. The shape says what it was (ring:
 * started, dot: commit, square: gate, diamond: review, target: land, bar: handoff); the color
 * says how it went, in the track's station colors, with red only for failures and the attention
 * color only for an ask.
 */
export function EventMark(props: { kind: MarkKind; title?: string; size?: number }) {
  const size = props.size ?? 12;
  const label = props.title ?? EVENT_LABEL[props.kind];
  return (
    <svg className={`ui-event ui-event-${props.kind}`} width={size} height={size} viewBox="0 0 12 12" role="img" aria-label={label}>
      <title>{label}</title>
      {shape(props.kind)}
    </svg>
  );
}

function shape(kind: MarkKind) {
  switch (kind) {
    case "start":
      return <circle cx="6" cy="6" r="3.6" fill="var(--mark-bg, var(--surface))" stroke="var(--st-started)" strokeWidth="1.8" />;
    case "commit":
      return <circle cx="6" cy="6" r="2.1" fill="var(--fg-2)" />;
    case "gate_pass":
    case "gate_fail":
      return <rect x="2.5" y="2.5" width="7" height="7" rx="1.2" fill={kind === "gate_pass" ? "var(--st-gate)" : "var(--st-stuck)"} />;
    case "review_pass":
    case "review_fail":
      return <path d="M6 1.4 10.6 6 6 10.6 1.4 6Z" fill={kind === "review_pass" ? "var(--st-review)" : "var(--st-stuck)"} strokeLinejoin="round" />;
    case "review_changes":
    case "submit":
      return (
        <path
          d="M6 2.2 9.8 6 6 9.8 2.2 6Z"
          fill="var(--mark-bg, var(--surface))"
          stroke={kind === "submit" ? "var(--fg-3)" : "var(--st-changes)"}
          strokeWidth="1.6"
          strokeLinejoin="round"
        />
      );
    case "land":
    case "land_fail":
      return (
        <>
          <circle cx="6" cy="6" r="4.6" fill={kind === "land" ? "var(--st-landing)" : "var(--st-stuck)"} />
          <circle cx="6" cy="6" r="1.7" fill="var(--mark-bg, var(--surface))" />
        </>
      );
    case "ask":
      return <circle cx="6" cy="6" r="4" fill="var(--attention)" />;
    case "handoff":
      return <rect x="5" y="1.5" width="2" height="9" rx="1" fill="var(--fg-3)" />;
  }
}
