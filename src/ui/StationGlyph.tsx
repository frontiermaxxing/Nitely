import type { Track } from "./lifecycle";
import { STATIONS, stationColor, stationIndex } from "./lifecycle";

const C = 7;
const R = 5.6;
const PIE = 3.3;

/** A pie wedge from 12 o'clock, `f` of the way round. */
function wedge(f: number): string {
  const a = f * 2 * Math.PI;
  const x = C + PIE * Math.sin(a);
  const y = C - PIE * Math.cos(a);
  return `M${C} ${C} L${C} ${C - PIE} A${PIE} ${PIE} 0 ${f > 0.5 ? 1 : 0} 1 ${x.toFixed(2)} ${y.toFixed(2)} Z`;
}

/**
 * The track folded into one glyph: a ring that fills as the issue moves from Todo to Done,
 * colored by whose move it is. Stuck is red, closed is struck through, Backlog is dashed.
 */
export function StationGlyph(props: { track: Track; size?: number }) {
  const t = props.track;
  const size = props.size ?? 14;
  const color = t.closed ? "var(--st-closed)" : stationColor(t.station, !!t.stuck);
  const f = stationIndex(t.station) / (STATIONS.length - 1);
  return (
    <svg className="ui-glyph" width={size} height={size} viewBox="0 0 14 14" aria-hidden="true">
      <circle cx={C} cy={C} r={R} fill="none" stroke={color} strokeWidth="1.5" strokeDasharray={t.backlog ? "2 1.6" : undefined} />
      {t.closed ? (
        <path d="M4.3 9.7 9.7 4.3" stroke={color} strokeWidth="1.5" strokeLinecap="round" />
      ) : f >= 1 ? (
        <>
          <circle cx={C} cy={C} r={R} fill={color} />
          <path d="M4.6 7.2 6.3 8.8 9.4 5.5" fill="none" stroke="var(--surface)" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" />
        </>
      ) : (
        f > 0 && <path d={wedge(f)} fill={color} />
      )}
      {t.asking && !t.closed && <circle cx="11.6" cy="2.6" r="2.4" fill="var(--attention)" stroke="var(--surface)" strokeWidth="1.2" />}
    </svg>
  );
}
