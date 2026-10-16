// The browser preview's page of every shared primitive in every state (`#kit`). Look here first
// when building a view, and add a row here when a primitive gains a state.

import type { IssueRecord } from "../types";
import { nowSecs } from "../util";
import {
  AgentMark,
  CountBadge,
  EmptyState,
  IssueToken,
  Meter,
  PriorityMark,
  SeverityMark,
  Sparkline,
  STATION_LABEL,
  STATIONS,
  StateDot,
  StationGlyph,
  StationTrack,
  type Liveness,
  type Station,
  type Track,
} from "../ui";

const NOW = nowSecs();
const H = 3600;

function track(station: Station, extra: Partial<Track> = {}): Track {
  const reached: Track["reached"] = {};
  STATIONS.slice(0, STATIONS.indexOf(station) + 1).forEach((s, i, all) => {
    if (s !== "needs" || station === "needs") reached[s] = NOW - (all.length - i) * 2.5 * H;
  });
  return { station, reached, rounds: 0, stuck: null, asking: false, closed: null, backlog: false, ...extra };
}

const TRACKS: { label: string; track: Track }[] = [
  { label: "Filed, waiting to be taken", track: track("todo") },
  { label: "An agent is working on it", track: track("working") },
  { label: "Stuck: the gate failed", track: track("gate", { stuck: "Gate failed" }) },
  { label: "In review after two rounds of changes", track: track("review", { rounds: 2 }) },
  { label: "Needs you, and the agent asked something", track: track("needs", { asking: true }) },
  { label: "Landing, Needs you skipped", track: track("landing") },
  { label: "Done", track: track("done", { rounds: 1 }) },
  { label: "Canceled", track: track("todo", { closed: "canceled" }) },
];

const ISSUE: IssueRecord = {
  key: "NIT-31",
  origin: "nitely",
  project: "NIT",
  title: "Pulse home: one glance at what needs you",
  body: "",
  url: "",
  state: "In Progress",
  state_type: "started",
  kind: "feature",
  source: "claude",
  priority: 1,
  labels: [],
  evidence: "",
  claimed_by: "claude:c:/development/nitely",
  created_at: "",
  updated_at: "",
  blocked_by: [],
  blocks: [],
  urgency: 1,
};

const VENDORS = ["claude", "codex", "cursor", "human", "other"];
const LIVENESS: (Liveness | null)[] = [null, "working", "waiting", "idle", "stale", "ended"];

const COLORS: [string, string][] = [
  ["--bg", "page"],
  ["--surface", "cards, panels"],
  ["--sunken", "sidebar"],
  ["--line", "rules"],
  ["--fg", "text"],
  ["--fg-2", "secondary text"],
  ["--fg-3", "faded: done, stale"],
  ["--attention", "needs the human, only"],
  ["--live", "working now"],
  ["--st-stuck", "stuck"],
  ["--vendor-claude", "Claude"],
  ["--vendor-codex", "Codex"],
  ["--vendor-cursor", "Cursor"],
  ["--sev-p0", "P0 finding"],
];

export default function Kit() {
  return (
    <div className="page kit">
      <div className="page-head">
        <h1>Visual kit</h1>
        <span className="head-note">Every shared primitive in src/ui, in every state. docs/visualization.md says when to use each.</span>
      </div>

      <section>
        <h2>StationTrack, full</h2>
        <div className="kit-tracks">
          {TRACKS.map((t) => (
            <div key={t.label} className="kit-track">
              <div className="kit-caption">{t.label}</div>
              <StationTrack track={t.track} size="full" now={NOW} />
            </div>
          ))}
        </div>
      </section>

      <section>
        <h2>StationGlyph and StationTrack, mini</h2>
        <div className="kit-grid">
          {STATIONS.map((s) => (
            <div key={s} className="kit-cell">
              <StationGlyph track={track(s)} />
              <StationTrack track={track(s)} />
              <span className="kit-caption">{STATION_LABEL[s]}</span>
            </div>
          ))}
          <div className="kit-cell">
            <StationGlyph track={track("todo", { backlog: true })} />
            <StationTrack track={track("todo", { backlog: true })} />
            <span className="kit-caption">Backlog</span>
          </div>
          <div className="kit-cell">
            <StationGlyph track={track("gate", { stuck: "Gate failed" })} />
            <StationTrack track={track("gate", { stuck: "Gate failed" })} />
            <span className="kit-caption">Stuck</span>
          </div>
          <div className="kit-cell">
            <StationGlyph track={track("working", { asking: true })} />
            <StationTrack track={track("working", { asking: true })} />
            <span className="kit-caption">Asked you</span>
          </div>
          <div className="kit-cell">
            <StationGlyph track={track("todo", { closed: "canceled" })} />
            <StationTrack track={track("todo", { closed: "canceled" })} />
            <span className="kit-caption">Canceled</span>
          </div>
        </div>
      </section>

      <section>
        <h2>IssueToken</h2>
        <div className="kit-row">
          <IssueToken issue={ISSUE} onOpen={() => {}} />
          <IssueToken issue={{ ...ISSUE, key: "AND-268", state: "Needs You" }} onOpen={() => {}} />
          <IssueToken issue={{ ...ISSUE, key: "NIT-22", state: "Done", state_type: "completed" }} />
        </div>
        <div className="kit-row kit-wide">
          <IssueToken issue={ISSUE} title onOpen={() => {}} />
        </div>
      </section>

      <section>
        <h2>AgentMark and StateDot</h2>
        <table className="kit-marks">
          <thead>
            <tr>
              <th />
              {LIVENESS.map((l) => (
                <th key={l ?? "none"}>{l ?? "no state"}</th>
              ))}
            </tr>
          </thead>
          <tbody>
            {VENDORS.map((v) => (
              <tr key={v}>
                <th>{v}</th>
                {LIVENESS.map((l) => (
                  <td key={l ?? "none"}>
                    <AgentMark vendor={v} state={l} size={20} />
                  </td>
                ))}
              </tr>
            ))}
            <tr>
              <th>StateDot</th>
              {LIVENESS.map((l) => (
                <td key={l ?? "none"}>{l && <StateDot state={l} />}</td>
              ))}
            </tr>
          </tbody>
        </table>
      </section>

      <section>
        <h2>Counts, meters, sparklines</h2>
        <div className="kit-row">
          <span className="kit-pair">
            <CountBadge count={3} tone="attention" /> attention
          </span>
          <span className="kit-pair">
            <CountBadge count={4} tone="live" /> live
          </span>
          <span className="kit-pair">
            <CountBadge count={17} /> quiet
          </span>
        </div>
        <div className="kit-row">
          <Meter
            label="Open issues"
            parts={[
              { value: 3, color: "var(--attention)", label: "need you" },
              { value: 12, color: "var(--st-working)", label: "with agents" },
              { value: 9, color: "var(--st-todo)", label: "queued" },
            ]}
          />
          <Sparkline label="Events per hour" values={[2, 4, 3, 6, 9, 5, 7, 12, 8, 6, 10, 14]} />
          <Sparkline label="Landed per day" kind="bars" values={[1, 0, 2, 3, 1, 0, 0, 4, 2, 1, 3, 5, 2, 1]} />
        </div>
      </section>

      <section>
        <h2>PriorityMark and SeverityMark</h2>
        <div className="kit-row">
          {[1, 2, 3, 4, 0].map((p) => (
            <span key={p} className="kit-pair">
              <PriorityMark priority={p} /> {["None", "Urgent", "High", "Medium", "Low"][p]}
            </span>
          ))}
        </div>
        <div className="kit-row">
          {[0, 1, 2, 3].map((p) => (
            <SeverityMark key={p} priority={p} />
          ))}
        </div>
      </section>

      <section>
        <h2>EmptyState</h2>
        <EmptyState title="Nothing needs you.">Agents show up here when they ask something, wait on your approval, or get stuck.</EmptyState>
      </section>

      <section>
        <h2>Color roles</h2>
        <div className="kit-colors">
          {COLORS.map(([token, use]) => (
            <div key={token} className="kit-color">
              <span className="kit-swatch" style={{ background: `var(${token})` }} />
              <span className="mono">{token}</span>
              <span className="muted">{use}</span>
            </div>
          ))}
        </div>
      </section>
    </div>
  );
}
