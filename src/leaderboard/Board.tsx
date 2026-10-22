import { useEffect, useState } from "react";
import { invoke } from "../backend";
import { AgentMark, Button, EmptyState, Loading, Sparkline } from "../ui";
import { openExternal } from "../util";
import { Avatar, type AvatarChoice } from "./Avatar";
import type { Profile } from "./profile";
import { compact, duration, hours, percent, type PeriodKey } from "./stats";

// L2: the public board. It comes from the leaderboard's service (`leaderboard_board`, the
// board.json NIT-447's service publishes), so it is fetched only once you joined or asked to see
// it: fetching it is a request to the service, though it carries nothing about you.

interface Spread {
  median: number;
  p90: number;
  count: number;
}

export interface BoardEntry {
  id: string;
  display_name: string;
  links: { github?: string; x?: string; website?: string };
  /** Which link gives the picture; the page builds its address (NIT-630). */
  avatar?: AvatarChoice;
  projects: number;
  week: string;
  landed: number;
  landed_per_day: number;
  reviewed: number;
  first_pass_rate: number | null;
  lead_time: Spread | null;
  agent_hours: number;
  tokens: number;
  inbox_reply: Spread | null;
  steadiest_week: { landed: number; active_days: number } | null;
  vendors: { vendor: string; landed: number; share: number }[];
  weekly: number[];
}

export interface BoardData {
  schema: number;
  period: PeriodKey;
  week: string;
  generated_at: number;
  entries: BoardEntry[];
}

type Metric = "landed" | "first_pass" | "lead" | "hours" | "tokens" | "reply" | "steady";

/** Each ranking: its chip, how an entry scores (null ranks last), and whether less is better. */
const METRICS: { key: Metric; label: string; score: (e: BoardEntry) => number | null; low?: boolean }[] = [
  { key: "landed", label: "Landed", score: (e) => e.landed },
  { key: "first_pass", label: "First-pass review", score: (e) => e.first_pass_rate },
  { key: "lead", label: "Lead time", score: (e) => e.lead_time?.median ?? null, low: true },
  { key: "hours", label: "Agent hours", score: (e) => e.agent_hours },
  { key: "tokens", label: "Tokens used", score: (e) => e.tokens },
  { key: "reply", label: "Inbox reply", score: (e) => e.inbox_reply?.median ?? null, low: true },
  // The week with landings on the most days, then the most landed in it.
  { key: "steady", label: "Steadiest week", score: (e) => (e.steadiest_week ? e.steadiest_week.active_days * 1000 + e.steadiest_week.landed : null) },
];

/** Entries in rank order for `metric`: best first, those without a number last, ties by name. */
export function ranked(entries: BoardEntry[], metric: Metric): BoardEntry[] {
  const m = METRICS.find((x) => x.key === metric)!;
  return [...entries].sort((a, b) => {
    const sa = m.score(a);
    const sb = m.score(b);
    if (sa === null || sb === null) return sa === null ? (sb === null ? a.display_name.localeCompare(b.display_name) : 1) : -1;
    return (m.low ? sa - sb : sb - sa) || a.display_name.localeCompare(b.display_name);
  });
}

/** How many rows the table shows before your own, pinned below. */
const SHOWN = 50;
/** Weeks in each row's little chart. */
const WEEKS = 8;

// Asking to see the board holds for this run of the app, not beyond.
let viewed = false;

export function Board(props: { period: PeriodKey; profile: Profile | null; onEdit: () => void }) {
  const joined = props.profile !== null;
  const [allowed, setAllowed] = useState(joined || viewed);
  const [metric, setMetric] = useState<Metric>("landed");
  const [board, setBoard] = useState<{ period: PeriodKey; data: BoardData } | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [attempt, setAttempt] = useState(0);
  const fetching = allowed || joined;
  useEffect(() => {
    if (!fetching) return;
    let live = true;
    setError(null);
    invoke<BoardData>("leaderboard_board", { period: props.period }).then(
      (data) => live && setBoard({ period: props.period, data }),
      (e) => live && setError(String(e)),
    );
    return () => {
      live = false;
    };
  }, [fetching, props.period, attempt]);

  if (!fetching)
    return (
      <EmptyState
        title="The board comes from the leaderboard's service"
        action={
          <Button
            variant="primary"
            onClick={() => {
              viewed = true;
              setAllowed(true);
            }}
          >
            View the board
          </Button>
        }
      >
        Viewing it sends nothing about you, but it is a request over the internet, so the service sees the address it comes from
        (and doesn't store it). Your own numbers are under You, from this machine alone.
      </EmptyState>
    );
  if (error)
    return (
      <EmptyState title="Could not fetch the board" action={<Button onClick={() => setAttempt(attempt + 1)}>Try again</Button>}>
        {error}
      </EmptyState>
    );
  if (!board || board.period !== props.period) return <Loading />;

  const rows = ranked(board.data.entries, metric);
  const mine = props.profile?.id ?? null;
  const myIndex = mine ? rows.findIndex((e) => e.id === mine) : -1;
  const m = METRICS.find((x) => x.key === metric)!;
  if (!rows.length)
    return <EmptyState title="Nobody is on this board yet">Reports arrive once a week; the first ones show here.</EmptyState>;

  return (
    <div className="lb-board">
      <div className="lb-ranked">
        <span className="lb-sub">Ranked by</span>
        <div className="lb-chips" role="group" aria-label="Ranked by">
          {METRICS.map((x) => (
            <button key={x.key} className={`lb-rank-chip${x.key === metric ? " lb-on" : ""}`} aria-pressed={x.key === metric} onClick={() => setMetric(x.key)}>
              {x.label}
            </button>
          ))}
        </div>
        <span className="lb-sub lb-ranked-count">
          {rows.length.toLocaleString("en-US")} {rows.length === 1 ? "person" : "people"} · {board.data.week} · updates Sunday
        </span>
      </div>

      <div className="lb-podium">
        {rows.slice(0, 3).map((e, i) => (
          <section key={e.id} className={`lb-card lb-podium-card${e.id === mine ? " lb-me" : ""}`}>
            <div className="lb-podium-head">
              <span className={`lb-rank-badge${i === 0 ? " lb-first" : ""}`}>{i + 1}</span>
              <Avatar name={e.display_name} choice={e.avatar} links={e.links ?? {}} size={28} />
              <span className="lb-podium-name">{e.display_name}</span>
              <Crew entry={e} />
            </div>
            <div className="lb-podium-figure">
              <span className="lb-figure">{valueOf(e, metric)}</span>
              <span className="lb-unit">{m.label.toLowerCase()}</span>
            </div>
            <Sparkline kind="bars" values={e.weekly.slice(-WEEKS)} width={180} height={28} color="var(--fg-3)" label={`Landed per week, the last ${WEEKS} weeks: ${e.weekly.slice(-WEEKS).join(", ")}`} />
            <div className="lb-podium-meta">
              <span>{hours(e.agent_hours)} agent hours</span>
              <span>{compact(e.tokens)} tokens</span>
              <span>{percent(e.first_pass_rate)} first pass</span>
            </div>
            <Links entry={e} />
          </section>
        ))}
      </div>

      <div className="lb-table-wrap">
        <table className="lb-table">
          <thead>
            <tr>
              <th className="lb-num">Rank</th>
              <th>Name</th>
              <th>Crew</th>
              <th>Landed, by week</th>
              {(["landed", "hours", "tokens", "first_pass", "lead", "reply"] as Metric[]).map((k) => (
                <th key={k} className={`lb-num${k === metric ? " lb-sorted" : ""}`} aria-sort={k === metric ? (METRICS.find((x) => x.key === k)!.low ? "ascending" : "descending") : undefined}>
                  {COLUMN[k]}
                </th>
              ))}
            </tr>
          </thead>
          <tbody>
            {rows.slice(0, SHOWN).map((e, i) => (
              <Row key={e.id} entry={e} rank={i + 1} me={e.id === mine} onEdit={props.onEdit} />
            ))}
            {myIndex >= SHOWN && (
              <>
                <tr className="lb-gap" aria-hidden="true">
                  <td colSpan={10}>⋯</td>
                </tr>
                <Row entry={rows[myIndex]} rank={myIndex + 1} me onEdit={props.onEdit} />
              </>
            )}
          </tbody>
        </table>
      </div>
      {joined && myIndex < 0 && (
        <p className="lb-note">
          You're not on this board yet: it shows reports the service took, and{" "}
          {props.profile?.last_sent_week ? "yours isn't in this period." : "you haven't sent one."}{" "}
          <button className="link" onClick={props.onEdit}>
            Your profile
          </button>
        </p>
      )}
    </div>
  );
}

const COLUMN: Partial<Record<Metric, string>> = { landed: "Landed", hours: "Agent hours", tokens: "Tokens", first_pass: "First pass", lead: "Lead time", reply: "Inbox reply" };

function valueOf(e: BoardEntry, metric: Metric): string {
  switch (metric) {
    case "landed":
      return e.landed.toLocaleString("en-US");
    case "first_pass":
      return percent(e.first_pass_rate);
    case "lead":
      return e.lead_time ? duration(e.lead_time.median) : "–";
    case "hours":
      return hours(e.agent_hours);
    case "tokens":
      return compact(e.tokens);
    case "reply":
      return e.inbox_reply ? duration(e.inbox_reply.median) : "–";
    case "steady":
      return e.steadiest_week ? `${e.steadiest_week.active_days} days` : "–";
  }
}

function Row(props: { entry: BoardEntry; rank: number; me: boolean; onEdit: () => void }) {
  const e = props.entry;
  return (
    <tr className={props.me ? "lb-me" : undefined}>
      <td className="lb-num lb-quiet">{props.rank}</td>
      <td className="lb-row-name">
        <Avatar name={e.display_name} choice={e.avatar} links={e.links ?? {}} size={22} />
        {props.me ? (
          <button className="link lb-me-name" onClick={props.onEdit} title="Edit your leaderboard profile">
            {e.display_name}
          </button>
        ) : (
          e.display_name
        )}
        {props.me && <span className="lb-me-tag">you</span>}
      </td>
      <td>
        <Crew entry={e} />
      </td>
      <td>
        <Sparkline kind="bars" values={e.weekly.slice(-WEEKS)} width={84} height={18} color={props.me ? "var(--live)" : "var(--fg-3)"} label={`Landed per week: ${e.weekly.slice(-WEEKS).join(", ")}`} />
      </td>
      <td className="lb-num lb-strong">{e.landed.toLocaleString("en-US")}</td>
      <td className="lb-num lb-quiet">{hours(e.agent_hours)}</td>
      <td className="lb-num lb-quiet">{compact(e.tokens)}</td>
      <td className="lb-num lb-quiet">{percent(e.first_pass_rate)}</td>
      <td className="lb-num lb-quiet">{e.lead_time ? duration(e.lead_time.median) : "–"}</td>
      <td className="lb-num lb-quiet">{e.inbox_reply ? duration(e.inbox_reply.median) : "–"}</td>
    </tr>
  );
}

function Crew(props: { entry: BoardEntry }) {
  return (
    <span className="lb-crew-marks">
      {props.entry.vendors.map((v) => (
        <AgentMark key={v.vendor} vendor={v.vendor} size={16} title={`${v.vendor}: ${v.landed} landed`} />
      ))}
    </span>
  );
}

/** Public links, as people gave them: only https ones, opened in the system browser, never fetched here. */
function Links(props: { entry: BoardEntry }) {
  const l = props.entry.links ?? {};
  const items: [string, string | undefined][] = [
    ["GitHub", l.github],
    ["X", l.x],
    ["Web", l.website],
  ];
  const shown = items.filter(([, v]) => typeof v === "string" && v.startsWith("https://"));
  if (!shown.length) return null;
  return (
    <div className="lb-podium-links">
      {shown.map(([k, v]) => (
        <a key={k} className="lb-link" href={v} onClick={openExternal(v!)} rel="noopener noreferrer">
          <span className="lb-quiet">{k}</span>
          {v!.replace(/^https:\/\/(www\.)?(github\.com\/|x\.com\/)?/, "")}
        </a>
      ))}
    </div>
  );
}
