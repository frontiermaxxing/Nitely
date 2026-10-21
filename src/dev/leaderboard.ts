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
    return { week_start: firstMonday + w * 7 * DAY, landed: days.reduce((n, d) => n + d.landed, 0), active_days: days.filter((d) => d.landed > 0).length };
  });
  const weeklyHours = weekly.map((w) => (empty ? 0 : Math.round((w.landed * 0.37 + r() * 3) * 10) / 10));
  const period = (key: PeriodKey, from: number, scale: number): PeriodStats => {
    const landed = daily.filter((d) => d.day_start >= from - DAY + 1).reduce((n, d) => n + d.landed, 0);
    const hours = weekly.reduce((n, w, i) => (w.week_start >= from - 6 * DAY ? n + weeklyHours[i] : n), 0);
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
    weekly_agent_hours: weeklyHours,
    daily,
    records: empty
      ? { best_day: null, best_week: null, longest_streak_days: 0, fastest_lead: null }
      : { best_day: best, best_week: bestWeek, longest_streak_days: 11, fastest_lead: { issue: "NIT-52", seconds: 252 } },
  };
}

/**
 * The preview's weekly report, in the shape `leaderboard_report` returns (leaderboard_report.rs):
 * the same checks on the name and links, the same fields copied from the numbers.
 */
export function leaderboardReportFixture(args: Record<string, unknown>) {
  const profile = args.profile as { display_name: string; github: string | null; x: string | null; website: string | null };
  const projects = (args.projects as string[]) ?? [];
  const name = profile.display_name.trim();
  if (!name) throw "choose a display name";
  if (name.length > 40) throw "a display name has at most 40 characters";
  if (name.includes("@")) throw "a display name can't hold @: use a name, not an email address";
  const handle = (v: string | null, prefixes: string[], max: number, ok: RegExp, what: string, base: string) => {
    if (!v?.trim()) return undefined;
    let h = v.trim().replace(/^@/, "");
    for (const p of prefixes) if (h.startsWith(p)) h = h.slice(p.length).replace(/\/+$/, "");
    if (!h || h.length > max || !ok.test(h)) throw `${what} isn't a valid user name`;
    return `${base}${h}`;
  };
  const github = handle(profile.github, ["https://github.com/", "github.com/"], 39, /^[A-Za-z0-9-]+$/, "the GitHub link", "https://github.com/");
  const x = handle(profile.x, ["https://x.com/", "x.com/"], 15, /^[A-Za-z0-9_]+$/, "the X link", "https://x.com/");
  const site = profile.website?.trim();
  if (site && !/^https:\/\/[^/@\s]+\.[^/@\s]+/.test(site)) throw site.startsWith("https://") ? "the website isn't a valid https address" : "the website must start with https://";
  const s = leaderboardFixture();
  const r3 = (n: number) => Math.round(n * 1000) / 1000;
  const week = new Date((s.periods[0].from + 14 * 3600) * 1000);
  const thursday = new Date(Date.UTC(week.getUTCFullYear(), week.getUTCMonth(), week.getUTCDate() + 3));
  const yearStart = new Date(Date.UTC(thursday.getUTCFullYear(), 0, 1));
  const isoWeek = Math.ceil(((thursday.getTime() - yearStart.getTime()) / 86400000 + 1) / 7);
  return {
    schema: 1,
    display_name: name,
    links: { ...(github ? { github } : {}), ...(x ? { x } : {}), ...(site ? { website: site } : {}) },
    week: `${thursday.getUTCFullYear()}-W${String(isoWeek).padStart(2, "0")}`,
    projects: projects.length || 3,
    periods: s.periods.map((p) => ({
      period: p.period,
      landed: p.landed,
      landed_per_day: Math.round(p.landed_per_day * 100) / 100,
      reviewed: p.reviewed,
      first_pass_rate: p.first_pass_rate === null ? null : r3(p.first_pass_rate),
      lead_time: p.lead_time,
      agent_hours: Math.round(p.agent_hours * 10) / 10,
      tokens: p.tokens,
      inbox_reply: p.inbox_reply,
      steadiest_week: p.steadiest_week && { landed: p.steadiest_week.landed, active_days: p.steadiest_week.active_days },
      vendors: p.vendors.map((v) => ({ vendor: v.vendor, landed: v.landed, share: r3(v.share) })),
      review_rounds: p.review_rounds,
    })),
    weekly: s.weekly.map((w) => w.landed),
    records: {
      best_day: s.records.best_day?.landed ?? 0,
      best_week: s.records.best_week?.landed ?? 0,
      longest_streak_days: s.records.longest_streak_days,
      fastest_lead: s.records.fastest_lead?.seconds ?? null,
    },
  };
}

/**
 * The preview's sharing state, as leaderboard_send.rs keeps it: joining checks the profile the way
 * the report does and saves it; leaving forgets it. Nothing is sent, so leaving deletes nothing.
 * `?leaderboard=joined` starts joined.
 */
let sharing: { joined: boolean; profile: Record<string, unknown>; projects: string[]; last_sent_week: string | null } | null = null;

function sharingState() {
  sharing ??= new URLSearchParams(location.search).get("leaderboard") === "joined"
    ? { joined: true, profile: { display_name: "lowtide", github: "lowtide", x: null, website: null }, projects: [], last_sent_week: null }
    : { joined: false, profile: { display_name: "" }, projects: [], last_sent_week: null };
  return sharing;
}

export function leaderboardStatusFixture() {
  return { ...sharingState(), id: sharingState().joined ? "preview-key" : null, url: "https://api.nitely.net", due: false };
}

export function leaderboardJoinFixture(args: Record<string, unknown>) {
  leaderboardReportFixture({ profile: args.profile, projects: args.projects });
  sharing = { joined: true, profile: args.profile as Record<string, unknown>, projects: (args.projects as string[]) ?? [], last_sent_week: null };
  return leaderboardStatusFixture();
}

export function leaderboardDeleteFixture(args: Record<string, unknown>) {
  if (args.leave) sharing = { joined: false, profile: { display_name: "" }, projects: [], last_sent_week: null };
  return 0;
}
