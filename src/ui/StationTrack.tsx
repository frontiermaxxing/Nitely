import type { CSSProperties } from "react";
import { ago } from "../util";
import type { Station, Track } from "./lifecycle";
import { STATION_LABEL, STATIONS, describeTrack, phaseOf, stationColor, stationIndex } from "./lifecycle";

type Status = "passed" | "skipped" | "current" | "upcoming";

function statusOf(track: Track, s: Station): Status {
  const i = stationIndex(s);
  const cur = stationIndex(track.station);
  if (i === cur) return "current";
  if (i > cur) return "upcoming";
  // With no events at all every earlier station counts as passed; with events, one never
  // reached was skipped (most issues go from Review straight to Landing).
  const known = Object.keys(track.reached).length > 0;
  return !known || track.reached[s] !== undefined ? "passed" : "skipped";
}

/**
 * The lifecycle track: Todo, Started, Working, Gate, Review, Needs you, Landing, Done.
 * `full` draws every station with its label and time, the "changes asked" loop above it and a
 * stuck station in red; `mini` is a 62px bar for list rows.
 */
export function StationTrack(props: { track: Track; size?: "full" | "mini"; now?: number }) {
  return props.size === "full" ? <FullTrack track={props.track} now={props.now} /> : <MiniTrack track={props.track} />;
}

const SEG = 6;
const GAP = 2;

function MiniTrack({ track }: { track: Track }) {
  const cur = stationIndex(track.station);
  const color = track.closed ? "var(--st-closed)" : stationColor(track.station, !!track.stuck);
  const width = STATIONS.length * SEG + (STATIONS.length - 1) * GAP;
  // Rounds of changes draw a small dashed arc from Review back over to Working above the bar.
  const looped = track.rounds > 0 && !track.closed;
  const top = looped ? 5 : 0;
  const x = (s: Station) => stationIndex(s) * (SEG + GAP) + SEG / 2;
  return (
    <svg
      className="ui-mini-track"
      width={width}
      height={6 + top}
      viewBox={`0 0 ${width} ${6 + top}`}
      role="img"
      aria-label={describeTrack(track)}
    >
      <title>{describeTrack(track)}</title>
      {looped && (
        <path
          d={`M${x("working")} ${top} V2.6 Q${x("working")} 1 ${x("working") + 2} 1 H${x("review") - 2} Q${x("review")} 1 ${x("review")} 2.6 V${top}`}
          fill="none"
          stroke="var(--st-changes)"
          strokeWidth="1.2"
          strokeDasharray={track.rounds > 1 ? "2 1.4" : undefined}
          strokeLinecap="round"
        />
      )}
      {STATIONS.map((s, i) => (
        <rect
          key={s}
          x={i * (SEG + GAP)}
          y={1 + top}
          width={SEG}
          height="4"
          rx="1.5"
          fill={i <= cur ? color : "var(--line-strong)"}
          opacity={i < cur ? 0.4 : i > cur ? 0.55 : 1}
        />
      ))}
    </svg>
  );
}

/** How many nested arcs the "changes asked" coil draws; more rounds than this still read as many. */
const MAX_COILS = 5;
const COIL_STEP = 4;
const COIL_SPREAD = 4;

function FullTrack({ track, now }: { track: Track; now?: number }) {
  const cur = stationIndex(track.station);
  const n = STATIONS.length;
  const center = (i: number) => `${((i + 0.5) / n) * 100}%`;
  const closedLabel = track.closed === "duplicate" ? "Duplicate" : "Canceled";
  // Each round of changes is one more arc, so five rounds read as a coil, not a label.
  const looped = track.rounds > 0 && !track.closed;
  const coils = Math.min(track.rounds, MAX_COILS);
  return (
    <div
      className={`ui-track${track.closed ? " ui-track-closed" : ""}${looped ? " ui-track-looped" : ""}`}
      style={looped ? { paddingTop: 34 + (coils - 1) * COIL_STEP } : undefined}
    >
      {looped && (
        <div
          className="ui-track-coil"
          style={{ left: center(stationIndex("working")), width: `${(2 / n) * 100}%`, height: 16 + (coils - 1) * COIL_STEP }}
          title={`Review asked for changes ${track.rounds} time${track.rounds === 1 ? "" : "s"}`}
        >
          {Array.from({ length: coils }, (_, i) => (
            <span key={i} className="ui-track-loop" style={{ height: 16 + i * COIL_STEP, left: -i * COIL_SPREAD, right: -i * COIL_SPREAD, opacity: i ? 0.6 - i * 0.08 : 1 }} />
          ))}
          <span className="ui-track-loop-label">
            {track.rounds} round{track.rounds === 1 ? "" : "s"} of changes
          </span>
        </div>
      )}
      <ol className="ui-track-stations" aria-label={describeTrack(track)}>
        {STATIONS.map((s, i) => {
          const status = statusOf(track, s);
          const stuck = status === "current" && !!track.stuck;
          const at = track.reached[s];
          const style = { "--c": track.closed ? "var(--st-closed)" : stationColor(s, stuck) } as CSSProperties;
          const label = status === "current" && track.closed ? closedLabel : STATION_LABEL[s];
          const time = stuck ? track.stuck : at && now && (status === "current" || status === "passed") ? ago(now, at) : "";
          return (
            <li
              key={s}
              className={`ui-st ui-st-${status} ui-phase-${phaseOf(s)}${stuck ? " ui-st-stuck" : ""}${i < cur ? " ui-st-behind" : ""}`}
              style={style}
              title={at && now ? `${STATION_LABEL[s]} · ${ago(now, at)}` : STATION_LABEL[s]}
            >
              <span className="ui-st-node">{status === "current" && track.asking && <span className="ui-st-ask" />}</span>
              <span className="ui-st-label">{label}</span>
              <span className="ui-st-time">{time}</span>
            </li>
          );
        })}
      </ol>
    </div>
  );
}
