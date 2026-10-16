import type { Track } from "./lifecycle";
import { phaseOf } from "./lifecycle";

const C = 7;
const R = 5.25;

/** The glyph's form: one of seven, which say where the issue is and whose move it is. */
export type GlyphKind = "backlog" | "todo" | "active" | "needs" | "landing" | "done" | "failed" | "closed";

export function glyphKindOf(t: Track): GlyphKind {
  if (t.closed) return "closed";
  if (t.stuck) return "failed";
  if (t.backlog) return "backlog";
  if (t.station === "landing") return "landing";
  const phase = phaseOf(t.station);
  return phase === "queued" ? "todo" : phase === "human" ? "needs" : phase === "done" ? "done" : "active";
}

/**
 * Where an issue stands, as one 14px mark that follows the colour rule: a grey ring is queued
 * (dashed in Backlog), a teal ring with a dot is an agent moving it, an amber ring with a halo
 * waits on you, solid teal is landing, solid grey is done, red has failed, struck through is closed.
 */
export function StationGlyph(props: { track: Track; size?: number }) {
  const t = props.track;
  const size = props.size ?? 14;
  const kind = glyphKindOf(t);
  return (
    <svg className={`ui-glyph ui-glyph-${kind}`} width={size} height={size} viewBox="0 0 14 14" aria-hidden="true">
      {shape(kind)}
      {t.asking && kind !== "needs" && kind !== "closed" && (
        <circle cx="11.8" cy="2.2" r="2.3" fill="var(--attention)" stroke="var(--mark-bg, var(--surface))" strokeWidth="1.2" />
      )}
    </svg>
  );
}

function ring(color: string, dashed = false) {
  return <circle cx={C} cy={C} r={R} fill="none" stroke={color} strokeWidth="1.5" strokeDasharray={dashed ? "2.2 1.7" : undefined} />;
}

function shape(kind: GlyphKind) {
  switch (kind) {
    case "backlog":
      return ring("var(--st-todo)", true);
    case "todo":
      return ring("var(--st-todo)");
    case "active":
      return (
        <>
          {ring("var(--live)")}
          <circle cx={C} cy={C} r="2.6" fill="var(--live)" />
        </>
      );
    case "needs":
      return (
        <>
          <circle cx={C} cy={C} r="7" fill="var(--attention-wash)" />
          {ring("var(--attention)")}
          <circle cx={C} cy={C} r="3" fill="var(--attention)" />
        </>
      );
    case "landing":
      return (
        <>
          <circle cx={C} cy={C} r="6" fill="var(--live)" />
          <circle cx={C} cy={C} r="2.2" fill="var(--mark-bg, var(--surface))" />
        </>
      );
    case "failed":
      return (
        <>
          {ring("var(--fail)")}
          <circle cx={C} cy={C} r="2.6" fill="var(--fail)" />
        </>
      );
    case "closed":
      return (
        <>
          {ring("var(--st-closed)")}
          <path d="M4.4 9.6 9.6 4.4" stroke="var(--st-closed)" strokeWidth="1.5" strokeLinecap="round" />
        </>
      );
    default:
      return <circle cx={C} cy={C} r="6" fill="var(--st-done)" />;
  }
}
