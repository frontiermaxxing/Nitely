import { useState } from "react";
import { Button, EmptyState } from "../ui";
import { CompareBars, Heatmap, ReplyHistogram, RoundsBars, VendorSplit, WeeklyBars } from "./charts";
import type { Profile } from "./profile";
import {
  DEVELOPER_MULTIPLIER,
  PERIOD_LABEL,
  WORK_WEEK_HOURS,
  compact,
  developerHours,
  duration,
  hours,
  percent,
  periodOf,
  shortDate,
  type LeaderboardStats,
  type PeriodKey,
  type PeriodStats,
} from "./stats";

const IN_PERIOD: Record<PeriodKey, string> = { week: "this week", "30d": "in the last 30 days", all: "since you started" };
const WORK_YEAR_HOURS = 52 * WORK_WEEK_HOURS;

/** L3: your own numbers, always here, sharing or not. */
export function You(props: { stats: LeaderboardStats; period: PeriodKey; profile: Profile | null; now: number; onJoin?: () => void; onEdit?: () => void }) {
  const { stats } = props;
  const p = periodOf(stats, props.period);
  const all = periodOf(stats, "all");
  if (!p || !all) return null;
  return (
    <div className="lb-you">
      <ProfileRow profile={props.profile} onJoin={props.onJoin} onEdit={props.onEdit} />
      {all.landed === 0 && all.agent_hours === 0 ? (
        <EmptyState title="Nothing landed yet">Your numbers fill in as your agents land changes in the projects that count.</EmptyState>
      ) : (
        <>
          <HoursSaved stats={stats} p={p} all={all} period={props.period} />
          <Headline p={p} />
          <div className="lb-grid lb-grid-wide">
            <section className="lb-card">
              <h2>
                Landed, last 26 weeks
                <span className="lb-sub">
                  {stats.daily.reduce((n, d) => n + d.landed, 0).toLocaleString("en-US")} changes
                  {stats.records.best_day && ` · best day ${stats.records.best_day.landed}, ${shortDate(stats.records.best_day.day_start)}`}
                </span>
              </h2>
              <Heatmap daily={stats.daily} now={props.now} />
            </section>
            <section className="lb-card">
              <h2>
                Your crew <span className="lb-sub">{PERIOD_LABEL[props.period].toLowerCase()}</span>
              </h2>
              {p.vendors.length ? <VendorSplit vendors={p.vendors} /> : <p className="lb-none">Nothing landed {IN_PERIOD[props.period]}.</p>}
            </section>
          </div>
          <div className="lb-grid lb-grid-three">
            <section className="lb-card">
              <h2>
                Review rounds to land <span className="lb-sub">{PERIOD_LABEL[props.period].toLowerCase()}</span>
              </h2>
              {p.reviewed ? (
                <>
                  <RoundsBars counts={p.round_counts} />
                  {p.review_rounds && (
                    <p className="lb-note">
                      {(Math.round(p.review_rounds.mean * 10) / 10).toLocaleString("en-US")} rounds on average, at most {p.review_rounds.max}.
                    </p>
                  )}
                </>
              ) : (
                <p className="lb-none">No reviewed change landed {IN_PERIOD[props.period]}.</p>
              )}
            </section>
            <section className="lb-card">
              <h2>
                How fast you unblock them <span className="lb-sub">Inbox, {PERIOD_LABEL[props.period].toLowerCase()}</span>
              </h2>
              {p.inbox_reply ? (
                <>
                  <ReplyHistogram buckets={p.reply_buckets} />
                  <p className="lb-note">
                    Median {duration(p.inbox_reply.median)}.{" "}
                    {(p.reply_buckets.at(-1) ?? 0) > 0
                      ? `${p.reply_buckets.at(-1)} waited a day or more.`
                      : `${p.inbox_reply.count} answered, none waited a day.`}
                  </p>
                </>
              ) : (
                <p className="lb-none">You answered nothing in the Inbox {IN_PERIOD[props.period]}.</p>
              )}
            </section>
            <Records stats={stats} />
          </div>
        </>
      )}
    </div>
  );
}

function ProfileRow(props: { profile: Profile | null; onJoin?: () => void; onEdit?: () => void }) {
  const p = props.profile;
  if (!p)
    return (
      <section className="lb-profile">
        <span className="lb-avatar" aria-hidden="true">
          ?
        </span>
        <div className="lb-profile-text">
          <span className="lb-profile-name">Not on the board</span>
          <span className="lb-sub">These numbers stay on this machine. Join to compare them with other people's.</span>
        </div>
        {props.onJoin && (
          <Button variant="primary" onClick={props.onJoin}>
            Join the leaderboard
          </Button>
        )}
      </section>
    );
  const links: [string, string | null][] = [
    ["GitHub", p.github],
    ["X", p.x],
    ["Web", p.website],
  ];
  return (
    <section className="lb-profile">
      <span className="lb-avatar" aria-hidden="true">
        {p.display_name.slice(0, 1).toUpperCase()}
      </span>
      <div className="lb-profile-text">
        <span className="lb-profile-name">
          {p.display_name}
          <span className="lb-sub">joined {new Date(p.joined_at * 1000).toLocaleDateString("en-US", { month: "short", year: "numeric" })}</span>
        </span>
        <span className="lb-links">
          {links.map(([k, v]) =>
            v ? (
              <span key={k} className="lb-link">
                <span className="lb-quiet">{k}</span>
                {v}
              </span>
            ) : props.onEdit ? (
              <button key={k} className="lb-link lb-link-add" onClick={props.onEdit}>
                + {k}
              </button>
            ) : null,
          )}
        </span>
      </div>
      {props.onEdit && <Button onClick={props.onEdit}>Edit profile</Button>}
    </section>
  );
}

function HoursSaved(props: { stats: LeaderboardStats; p: PeriodStats; all: PeriodStats; period: PeriodKey }) {
  const { p, all } = props;
  const [how, setHow] = useState(false);
  const dev = developerHours(p.agent_hours);
  const allDev = developerHours(all.agent_hours);
  const weeks = props.stats.weekly;
  return (
    <section className="lb-card lb-saved">
      <h2>
        Hours saved {IN_PERIOD[props.period]}
        <span className="lb-sub">every agent session counted as automated</span>
        <button className="link lb-how" aria-expanded={how} onClick={() => setHow(!how)}>
          How this is estimated
        </button>
      </h2>
      {how && (
        <p className="lb-note lb-how-text">
          Agent hours are how long your agents' sessions ran in the projects that count. One average developer is assumed to
          need {DEVELOPER_MULTIPLIER} times as long for the same work. That multiplier is a placeholder until the estimate is
          settled, so read these as a rough scale, not a measurement.
        </p>
      )}
      <div className="lb-saved-figures">
        <div>
          <span className="lb-label">Agents worked</span>
          <span className="lb-figure">
            {hours(p.agent_hours)} <span className="lb-unit">hours</span>
          </span>
          <span className="lb-sub">
            {p.sessions.toLocaleString("en-US")} session{p.sessions === 1 ? "" : "s"} · {compact(p.tokens)} tokens
          </span>
        </div>
        <div>
          <span className="lb-label">One average developer would need</span>
          <span className="lb-figure">
            ≈ {hours(dev)} <span className="lb-unit">hours</span> <span className="lb-mult">×{DEVELOPER_MULTIPLIER}</span>
          </span>
          <span className="lb-sub">
            for the same {p.landed.toLocaleString("en-US")} change{p.landed === 1 ? "" : "s"}
          </span>
        </div>
        <div>
          <span className="lb-label">That is</span>
          <span className="lb-figure">
            {hours(dev / WORK_WEEK_HOURS)} <span className="lb-unit">work weeks</span>
          </span>
          <span className="lb-sub">at {WORK_WEEK_HOURS} hours a week</span>
        </div>
      </div>
      <CompareBars agents={p.agent_hours} developer={dev} format={(h) => `${hours(h)}h`} />
      <div className="lb-saved-foot">
        <div className="lb-saved-chart">
          <span className="lb-label">Developer hours saved, per week</span>
          <WeeklyBars
            values={props.stats.weekly_agent_hours.map(developerHours)}
            weekStarts={weeks.map((w) => w.week_start)}
            format={(v) => `≈ ${hours(v)} hours`}
            label={`Developer hours saved per week, the last ${weeks.length} weeks`}
          />
        </div>
        <div className="lb-saved-total">
          <span className="lb-label">Since you started</span>
          <span className="lb-total">≈ {hours(allDev)} hours</span>
          <span className="lb-sub">
            from {hours(all.agent_hours)} agent hours ·{" "}
            {allDev >= WORK_YEAR_HOURS ? `about ${hours(allDev / WORK_YEAR_HOURS)} work years` : `about ${hours(allDev / WORK_WEEK_HOURS)} work weeks`}
          </span>
        </div>
      </div>
    </section>
  );
}

/** The four numbers the board ranks on. Where you sit against everyone shows once the board exists. */
function Headline(props: { p: PeriodStats }) {
  const { p } = props;
  const tiles = [
    { k: `Landed`, v: p.landed.toLocaleString("en-US"), d: `${(Math.round(p.landed_per_day * 10) / 10).toLocaleString("en-US")} a day` },
    { k: "First-pass review", v: percent(p.first_pass_rate), d: p.reviewed ? `of ${p.reviewed} reviewed` : "nothing reviewed" },
    { k: "Lead time, start to land", v: p.lead_time ? duration(p.lead_time.median) : "–", d: p.lead_time ? `median · 90% within ${duration(p.lead_time.p90)}` : "nothing started and landed" },
    { k: "Your Inbox reply", v: p.inbox_reply ? duration(p.inbox_reply.median) : "–", d: p.inbox_reply ? `median of ${p.inbox_reply.count} answered` : "nothing answered" },
  ];
  return (
    <section className="lb-headline" aria-label="Headline numbers">
      {tiles.map((t) => (
        <div key={t.k} className="lb-tile">
          <span className="lb-label">{t.k}</span>
          <span className="lb-tile-value">{t.v}</span>
          <span className="lb-sub">{t.d}</span>
        </div>
      ))}
    </section>
  );
}

function Records(props: { stats: LeaderboardStats }) {
  const r = props.stats.records;
  const rows: [string, string, string][] = [
    ["Most landed in a day", r.best_day ? String(r.best_day.landed) : "–", r.best_day ? shortDate(r.best_day.day_start) : ""],
    ["Most landed in a week", r.best_week ? String(r.best_week.landed) : "–", r.best_week ? `week of ${shortDate(r.best_week.week_start)}` : ""],
    ["Days in a row with a land", r.longest_streak_days ? String(r.longest_streak_days) : "–", ""],
    ["Fastest start to land", r.fastest_lead ? duration(r.fastest_lead.seconds) : "–", r.fastest_lead?.issue ?? ""],
  ];
  return (
    <section className="lb-card">
      <h2>Your records</h2>
      <div className="lb-records">
        {rows.map(([k, v, w]) => (
          <div key={k} className="lb-record">
            <span className="lb-quiet">{k}</span>
            <span className="lb-record-value">
              <span className="lb-num">{v}</span>
              {w && <span className="lb-sub">{w}</span>}
            </span>
          </div>
        ))}
      </div>
    </section>
  );
}
