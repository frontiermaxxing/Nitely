import { useEffect, useMemo, useState } from "react";
import type { CSSProperties, MouseEvent } from "react";
import { invoke } from "./backend";
import type { Board, IssueDetail, IssueRecord, PrRecord, TaskRow } from "./types";
import {
  AgentMark,
  CountBadge,
  IssueToken,
  LIVENESS_LABEL,
  PrMark,
  PriorityMark,
  STATION_LABEL,
  STATIONS,
  VENDOR_LABEL,
  livenessOf,
  stationColor,
  trackOf,
  vendorOf,
  type Liveness,
  type Station,
  type Track,
  type Vendor,
} from "./ui";

/** Done keeps only what landed lately; the list has the rest. */
const DONE_DAYS = 7;
const DONE_MAX = 12;
const DAY = 86400;

export interface Filters {
  kind: string;
  /** A vendor, "none" for issues no agent holds, or "" for all. */
  agent: string;
}
export const NO_FILTER: Filters = { kind: "", agent: "" };

/** One issue on the board, joined with the worktree and session working on it. */
interface Card {
  issue: IssueRecord;
  track: Track;
  vendor: Vendor | null;
  live: Liveness | null;
  row: TaskRow | null;
  pr: PrRecord | null;
  /** When it arrived in its column, as far as is known. */
  since: number;
  needsYou: boolean;
}

/** The worktree row for each issue key, the most recently active one when there are several. */
function rowsByIssue(board: Board | null): Map<string, TaskRow> {
  const out = new Map<string, TaskRow>();
  const seen = (r: TaskRow) => Math.max(r.session?.last_seen_at ?? 0, r.state?.last_commit_at ?? 0);
  for (const r of board?.repos.flatMap((repo) => repo.rows) ?? []) {
    if (r.is_main || !r.issue_key) continue;
    const had = out.get(r.issue_key);
    if (!had || seen(r) > seen(had)) out.set(r.issue_key, r);
  }
  return out;
}

function vendorFor(issue: IssueRecord, row: TaskRow | null): Vendor | null {
  const v = row?.session?.vendor ?? row?.vendor ?? issue.claimed_by;
  return v ? vendorOf(v) : null;
}

/** The agents holding the project's issues, for the agent filter. */
function agentsOf(issues: IssueRecord[], board: Board | null): Vendor[] {
  const rows = rowsByIssue(board);
  const found = new Set<Vendor>();
  for (const i of issues) {
    const v = vendorFor(i, rows.get(i.key) ?? null);
    if (v) found.add(v);
  }
  return [...found].sort();
}

/** The board's filters: issue kind and agent, as two quiet selects in the page header. */
export function BoardFilters(props: { issues: IssueRecord[]; board: Board | null; filters: Filters; onChange: (f: Filters) => void }) {
  const kinds = [...new Set(props.issues.map((i) => i.kind).filter((k): k is string => !!k))].sort();
  const agents = agentsOf(props.issues, props.board);
  const { filters } = props;
  return (
    <div className="board-filters">
      <select aria-label="Kind" className={filters.kind ? "set" : undefined} value={filters.kind} onChange={(e) => props.onChange({ ...filters, kind: e.target.value })}>
        <option value="">All kinds</option>
        {kinds.map((k) => (
          <option key={k} value={k}>
            {k}
          </option>
        ))}
      </select>
      <select aria-label="Agent" className={filters.agent ? "set" : undefined} value={filters.agent} onChange={(e) => props.onChange({ ...filters, agent: e.target.value })}>
        <option value="">All agents</option>
        {agents.map((v) => (
          <option key={v} value={v}>
            {VENDOR_LABEL[v]}
          </option>
        ))}
        <option value="none">No agent</option>
      </select>
    </div>
  );
}

/**
 * The project's issues in columns by station, the same stations as the lifecycle track, so a
 * column's position says how far its issues got. What needs the human or is stuck sorts first
 * and stands out; Done keeps only the last week.
 */
export function ProjectBoard(props: {
  issues: IssueRecord[];
  board: Board | null;
  now: number;
  filters: Filters;
  onOpenIssue: (key: string) => void;
}) {
  const { issues, board, now, filters } = props;

  // Issues in an agent's hands (and the last week's Done) load their timelines, so the board knows Started from Working
  // from Gate, sees stuck work, and can tell how long each card has sat in its column.
  const [details, setDetails] = useState<Map<string, IssueDetail>>(new Map());
  useEffect(() => {
    let live = true;
    // Recently landed ones too: their worktree is gone, so the timeline is where their PR is.
    const recent = (i: IssueRecord) => i.state_type === "completed" && Date.now() / 1000 - Date.parse(i.updated_at) / 1000 < DONE_DAYS * DAY;
    const keys = issues.filter((i) => i.state_type === "started" || recent(i)).map((i) => i.key);
    Promise.all(keys.map((key) => invoke<IssueDetail>("issue_detail", { key }).catch(() => null))).then((all) => {
      if (live) setDetails(new Map(all.filter((d): d is IssueDetail => !!d).map((d) => [d.issue.key, d])));
    });
    return () => {
      live = false;
    };
  }, [issues]);

  const cards = useMemo(() => {
    const rows = rowsByIssue(board);
    return issues.map((issue): Card => {
      const d = details.get(issue.key);
      const track = d ? trackOf(issue, d.timeline, d.reviews) : trackOf(issue);
      const row = rows.get(issue.key) ?? null;
      const live = livenessOf(row?.session, now);
      // Without a timeline: filed (Todo) or last changed; mirrored issues may lack either, then now.
      const stamp = (iso: string) => (iso ? Date.parse(iso) / 1000 : NaN);
      const known = [track.station === "todo" ? stamp(issue.created_at) : NaN, stamp(issue.updated_at)].find(Number.isFinite);
      const fallback = known ?? now;
      return {
        issue,
        track,
        vendor: vendorFor(issue, row),
        live,
        row,
        pr: row?.pr ?? d?.prs[0] ?? null,
        since: track.since ?? fallback,
        needsYou: track.station === "needs" || track.asking || !!row?.needs_me,
      };
    });
  }, [issues, details, board, now]);

  const shown = cards.filter(
    (c) =>
      !c.track.closed &&
      (!filters.kind || c.issue.kind === filters.kind) &&
      (!filters.agent || (filters.agent === "none" ? !c.vendor : c.vendor === filters.agent)),
  );

  const columns = STATIONS.map((station) => {
    let inCol = shown.filter((c) => c.track.station === station).sort(order);
    let older = 0;
    if (station === "done") {
      const recent = inCol.filter((c) => now - c.since < DONE_DAYS * DAY).sort((a, b) => b.since - a.since);
      older = inCol.length - Math.min(recent.length, DONE_MAX);
      inCol = recent.slice(0, DONE_MAX);
    }
    return { station, cards: inCol, older };
  });
  const filtered = filters.kind || filters.agent;

  return (
    <div className="board">
      {columns.map(({ station, cards, older }, i) => (
        <Column
          key={station}
          station={station}
          cards={cards}
          older={older}
          first={i === 0}
          last={i === columns.length - 1}
          now={now}
          filtered={!!filtered}
          onOpenIssue={props.onOpenIssue}
        />
      ))}
    </div>
  );
}

const PRIORITY_ORDER = (p: number) => (p === 0 ? 9 : p);

/** What needs the human, then stuck work, then live work, then the most urgent, then the longest waiting. */
function order(a: Card, b: Card): number {
  return (
    Number(b.needsYou) - Number(a.needsYou) ||
    Number(!!b.track.stuck) - Number(!!a.track.stuck) ||
    Number(a.track.backlog) - Number(b.track.backlog) ||
    Number(b.live === "working") - Number(a.live === "working") ||
    PRIORITY_ORDER(a.issue.urgency || a.issue.priority) - PRIORITY_ORDER(b.issue.urgency || b.issue.priority) ||
    a.since - b.since
  );
}

function Column(props: {
  station: Station;
  cards: Card[];
  older: number;
  first: boolean;
  last: boolean;
  now: number;
  filtered: boolean;
  onOpenIssue: (key: string) => void;
}) {
  const { station, cards } = props;
  const empty = cards.length === 0 && props.older === 0;
  const label = STATION_LABEL[station];
  const cls = ["board-col", `board-col-${station}`, empty && "board-col-empty", props.first && "board-col-first", props.last && "board-col-last"]
    .filter(Boolean)
    .join(" ");
  return (
    <section className={cls} style={{ "--c": stationColor(station) } as CSSProperties} aria-label={`${label}: ${cards.length}`}>
      <header className="board-col-head" title={empty ? `${label}: nothing here${props.filtered ? " with these filters" : ""}` : undefined}>
        <span className="board-track" aria-hidden="true">
          <span className="board-node" />
        </span>
        <span className="board-col-label">{label}</span>
        {!empty && <CountBadge count={cards.length} tone={station === "needs" ? "attention" : "quiet"} />}
      </header>
      {!empty && (
        <div className="board-cards">
          {cards.map((c) => (
            <BoardCard key={c.issue.key} card={c} now={props.now} onOpenIssue={props.onOpenIssue} />
          ))}
          {props.older > 0 && (
            <p className="board-older">
              {props.older} landed earlier. The list has them all.
            </p>
          )}
        </div>
      )}
    </section>
  );
}

/** "3h", "4d": how long a card has sat in its column. */
function span(secs: number): string {
  const s = Math.max(0, secs);
  if (s < 3600) return `${Math.max(1, Math.round(s / 60))}m`;
  if (s < DAY) return `${Math.round(s / 3600)}h`;
  return `${Math.round(s / DAY)}d`;
}

/** The one thing about a card that is out of the ordinary, if any, and its tone. */
function flagOf(c: Card): { text: string; tone: "attention" | "stuck" | "quiet" } | null {
  if (c.row?.state?.rebase_in_progress) return { text: "Rebase stopped on a conflict", tone: "attention" };
  if (c.track.asking) return { text: "Asked you something", tone: "attention" };
  if (c.live === "waiting") return { text: "Waiting for you", tone: "attention" };
  if (c.track.stuck) return { text: c.track.stuck, tone: "stuck" };
  if (c.issue.blocked_by.length) return { text: `Blocked by ${c.issue.blocked_by.join(", ")}`, tone: "quiet" };
  return null;
}

function BoardCard(props: { card: Card; now: number; onOpenIssue: (key: string) => void }) {
  const c = props.card;
  const { issue, track } = c;
  const flag = flagOf(c);
  const blocked = issue.blocked_by.length > 0;
  const done = track.station === "done";
  const quiet = done || c.live === "stale";
  const age = props.now - c.since;
  const cls = [
    "board-card",
    c.needsYou && "board-card-needs",
    track.stuck && !c.needsYou && "board-card-stuck",
    blocked && !c.needsYou && !track.stuck && "board-card-blocked",
    quiet && "board-card-quiet",
  ]
    .filter(Boolean)
    .join(" ");
  // The whole card opens the issue for a pointer; the key inside is the button for the keyboard.
  const native = issue.origin === "nitely";
  const open = (e: MouseEvent) => {
    if (native && !(e.target as HTMLElement).closest("a, button")) props.onOpenIssue(issue.key);
  };
  const where = STATION_LABEL[track.station];
  return (
    <article className={cls} onClick={open}>
      <div className="board-card-top">
        <IssueToken issue={issue} track={track} onOpen={props.onOpenIssue} />
        <PriorityMark priority={issue.priority} />
        <span className="board-card-agent">
          {c.vendor && (
            <AgentMark
              vendor={c.vendor}
              state={c.live}
              title={`${VENDOR_LABEL[c.vendor]}${c.live ? ` · ${LIVENESS_LABEL[c.live]}` : ""}${c.row?.session?.last_activity ? ` · ${c.row.session.last_activity}` : ""}`}
            />
          )}
        </span>
      </div>
      <p className="board-card-title">{issue.title}</p>
      {flag && <p className={`board-card-flag board-flag-${flag.tone}`}>{flag.text}</p>}
      <div className="board-card-foot">
        {issue.kind && <span className="board-card-kind">{issue.kind}</span>}
        {c.pr && <PrMark pr={c.pr} link />}
        {track.rounds > 0 && !done && (
          <span className="board-card-rounds" title={`${track.rounds} round${track.rounds === 1 ? "" : "s"} of review changes`}>
            {track.rounds}× changes
          </span>
        )}
        <span className="board-card-age" title={`${done ? "Done" : `In ${where}`} for ${span(age)}, since ${new Date(c.since * 1000).toLocaleString()}`}>
          {span(age)}
        </span>
      </div>
    </article>
  );
}
