import type { DayCount, LeaderboardStats, PeriodKey, PeriodStats, WeekPoint } from "../leaderboard/stats";

// The preview's Leaderboard numbers: half a year of a crew that grew, from a fixed seed so every
// load draws the same page. `?leaderboard=empty` is a machine where nothing landed yet.

const DAY = 86400;

function seeded(seed: number) {
  let x = seed;
  return () => (x = (x * 9301 + 49297) % 233280) / 233280;
}

/** Monday 00:00 local of the week `t` falls in. */
function mondayOf(t: number): number {
  const d = new Date(t * 1000);
  d.setHours(0, 0, 0, 0);
  d.setDate(d.getDate() - ((d.getDay() + 6) % 7));
  return Math.floor(d.getTime() / 1000);
}

export function leaderboardFixture(): LeaderboardStats {
  const now = Math.floor(Date.now() / 1000);
  const empty = new URLSearchParams(location.search).get("leaderboard") === "empty";
  const r = seeded(11);
  const firstMonday = mondayOf(now) - 25 * 7 * DAY;
  const daily: DayCount[] = Array.from({ length: 182 }, (_, i) => {
    const w = Math.floor(i / 7);
    const weekend = i % 7 >= 5;
    const start = firstMonday + i * DAY;
    const v = empty || start > now ? 0 : Math.round(r() * (1 + w * 0.9) * (weekend ? 0.4 : 1) * (w < 3 ? 0.3 : 1));
    return { day_start: start, landed: v };
  });
  const weekly: WeekPoint[] = Array.from({ length: 26 }, (_, w) => {
    const days = daily.slice(w * 7, w * 7 + 7);
    return {
      week_start: firstMonday + w * 7 * DAY,
      landed: days.reduce((n, d) => n + d.landed, 0),
      active_days: days.filter((d) => d.landed > 0).length,
      agent_hours: empty ? 0 : Math.round((days.reduce((n, d) => n + d.landed, 0) * 0.37 + r() * 3) * 10) / 10,
    };
  });
  const period = (key: PeriodKey, from: number, scale: number): PeriodStats => {
    const landed = daily.filter((d) => d.day_start >= from - DAY + 1).reduce((n, d) => n + d.landed, 0);
    const hours = weekly.filter((w) => w.week_start >= from - 6 * DAY).reduce((n, w) => n + w.agent_hours, 0);
    if (empty) return { period: key, from, to: now, landed: 0, landed_per_day: 0, first_pass_rate: null, reviewed: 0, lead_time: null, agent_hours: 0, sessions: 0, tokens: 0, inbox_reply: null, reply_buckets: [0, 0, 0, 0, 0, 0], steadiest_week: null, vendors: [], review_rounds: null, round_counts: [0, 0, 0, 0] };
    const reviewed = Math.round(landed * 0.92);
    const claude = Math.round(landed * 0.58);
    const codex = Math.round(landed * 0.34);
    return {
      period: key,
      from,
      to: now,
      landed,
      landed_per_day: landed / Math.max(1, (now - from) / DAY),
      first_pass_rate: 0.74,
      reviewed,
      lead_time: { median: 47 * 60, p90: 3 * 3600 + 20 * 60, count: landed },
      agent_hours: hours,
      sessions: Math.round(hours * 1.7),
      tokens: Math.round(hours * 1_100_000),
      inbox_reply: { median: 18 * 60, p90: 5 * 3600, count: Math.round(38 * scale) },
      reply_buckets: [9, 14, 11, 7, 3, 4].map((n) => Math.round(n * scale)),
      steadiest_week: weekly[24],
      vendors: [
        { vendor: "claude", landed: claude, share: claude / landed, first_pass_rate: 0.78 },
        { vendor: "codex", landed: codex, share: codex / landed, first_pass_rate: 0.71 },
        { vendor: "cursor", landed: landed - claude - codex, share: (landed - claude - codex) / landed, first_pass_rate: 0.62 },
      ],
      review_rounds: { mean: 1.3, max: 4 },
      round_counts: [0.74, 0.2, 0.05, 0.01].map((f) => Math.round(reviewed * f)),
    };
  };
  const best = daily.reduce((a, b) => (b.landed > a.landed ? b : a), daily[0]);
  const bestWeek = weekly.reduce((a, b) => (b.landed > a.landed ? b : a), weekly[0]);
  return {
    generated_at: now,
    projects: [],
    periods: [period("week", mondayOf(now), 1), period("30d", now - 30 * DAY, 4), period("all", firstMonday, 20)],
    weekly,
    daily,
    records: empty
      ? { best_day: null, best_week: null, longest_streak_days: 0, fastest_lead: null }
      : { best_day: best, best_week: bestWeek, longest_streak_days: 11, fastest_lead: { issue: "NIT-52", seconds: 252 } },
  };
}
