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
  return (
    <svg className="ui-mini-track" width={width} height="6" viewBox={`0 0 ${width} 6`} role="img" aria-label={describeTrack(track)}>
      <title>{describeTrack(track)}</title>
      {STATIONS.map((s, i) => (
        <rect
          key={s}
          x={i * (SEG + GAP)}
          y="1"
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

function FullTrack({ track, now }: { track: Track; now?: number }) {
  const cur = stationIndex(track.station);
  const n = STATIONS.length;
  const center = (i: number) => `${((i + 0.5) / n) * 100}%`;
  const closedLabel = track.closed === "duplicate" ? "Duplicate" : "Canceled";
  return (
    <div className={`ui-track${track.closed ? " ui-track-closed" : ""}${track.rounds > 0 && !track.closed ? " ui-track-looped" : ""}`}>
      {track.rounds > 0 && !track.closed && (
        <div
          className="ui-track-loop"
          style={{ left: center(stationIndex("working")), width: `${(2 / n) * 100}%` }}
          title={`Review asked for changes ${track.rounds} time${track.rounds === 1 ? "" : "s"}`}
        >
          <span>
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
