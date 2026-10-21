// The Leaderboard's numbers, as `leaderboard_stats` returns them (crates/nitely-crew/src/leaderboard.rs).
// Read from this machine's database only: reading them sends nothing.

export type PeriodKey = "week" | "30d" | "all";

export interface Spread {
  /** Seconds. */
  median: number;
  p90: number;
  count: number;
}

export interface WeekPoint {
  /** Monday 00:00 local, seconds since the epoch. */
  week_start: number;
  landed: number;
  active_days: number;
}

export interface DayCount {
  day_start: number;
  landed: number;
}

export interface VendorShare {
  vendor: string;
  landed: number;
  share: number;
  first_pass_rate: number | null;
}

export interface PeriodStats {
  period: PeriodKey;
  from: number;
  to: number;
  landed: number;
  landed_per_day: number;
  first_pass_rate: number | null;
  reviewed: number;
  lead_time: Spread | null;
  agent_hours: number;
  /** Agent sessions that ran in the period. */
  sessions: number;
  tokens: number;
  inbox_reply: Spread | null;
  /** Answered Inbox questions by how long they waited: REPLY_BUCKETS. */
  reply_buckets: number[];
  steadiest_week: WeekPoint | null;
  vendors: VendorShare[];
  review_rounds: { mean: number; max: number } | null;
  /** Reviewed changes that landed after 1, 2, 3 and 4 or more rounds. */
  round_counts: number[];
}

export interface Records {
  best_day: DayCount | null;
  best_week: WeekPoint | null;
  longest_streak_days: number;
  fastest_lead: { issue: string; seconds: number } | null;
}

export interface LeaderboardStats {
  generated_at: number;
  projects: string[];
  periods: PeriodStats[];
  /** The last 26 weeks, oldest first, this week last. */
  weekly: WeekPoint[];
  /** Hours agents' sessions ran in each week of `weekly`. */
  weekly_agent_hours: number[];
  /** Every day of those 26 weeks, oldest first: Monday of the first week to Sunday of this one. */
  daily: DayCount[];
  records: Records;
  hours_saved: HoursSaved;
}

/**
 * What the agents' hours would have taken one average developer: agent hours times a multiplier
 * (a placeholder, set in one place: DEVELOPER_MULTIPLIER in crates/nitely-crew/src/leaderboard.rs).
 */
export interface HoursSaved {
  multiplier: number;
  work_week_hours: number;
  /** In the order of `periods`; `all` is the all-time total. */
  periods: { period: PeriodKey; agent_hours: number; developer_hours: number; work_weeks: number }[];
  /** Developer hours in each week of `weekly`. */
  weekly: number[];
}

/** Where each reply-time bucket ends, in seconds; the last runs on from a day. */
export const REPLY_BUCKETS: { label: string; upTo: number }[] = [
  { label: "under 5m", upTo: 5 * 60 },
  { label: "5 to 30m", upTo: 30 * 60 },
  { label: "30m to 2h", upTo: 2 * 3600 },
  { label: "2 to 8h", upTo: 8 * 3600 },
  { label: "8h to a day", upTo: 86400 },
  { label: "a day or more", upTo: Infinity },
];

export const ROUND_LABELS = ["1 round", "2 rounds", "3 rounds", "4+"];

export const PERIOD_LABEL: Record<PeriodKey, string> = { week: "This week", "30d": "30 days", all: "All time" };

export function periodOf(stats: LeaderboardStats, key: PeriodKey): PeriodStats | null {
  return stats.periods.find((p) => p.period === key) ?? null;
}

/** "47m", "1h 04m", "2d 3h", "4m 12s". */
export function duration(seconds: number): string {
  const s = Math.max(0, Math.round(seconds));
  if (s < 60) return `${s}s`;
  if (s < 600) return `${Math.floor(s / 60)}m${s % 60 ? ` ${String(s % 60).padStart(2, "0")}s` : ""}`;
  if (s < 3600) return `${Math.round(s / 60)}m`;
  if (s < 86400) {
    const m = Math.round((s % 3600) / 60);
    return `${Math.floor(s / 3600)}h${m ? ` ${String(m).padStart(2, "0")}m` : ""}`;
  }
  const h = Math.round((s % 86400) / 3600);
  return `${Math.floor(s / 86400)}d${h ? ` ${h}h` : ""}`;
}

/** 1,284 · 12.9K · 41.8M. */
export function compact(n: number): string {
  if (n < 10_000) return Math.round(n).toLocaleString("en-US");
  if (n < 1_000_000) return `${(n / 1000).toFixed(n < 100_000 ? 1 : 0)}K`;
  if (n < 1_000_000_000) return `${(n / 1_000_000).toFixed(n < 100_000_000 ? 1 : 0)}M`;
  return `${(n / 1_000_000_000).toFixed(1)}B`;
}

/** Hours with one decimal under a hundred, whole above. */
export function hours(n: number): string {
  return n < 100 ? (Math.round(n * 10) / 10).toLocaleString("en-US") : Math.round(n).toLocaleString("en-US");
}

export function percent(rate: number | null): string {
  return rate === null ? "–" : `${Math.round(rate * 100)}%`;
}

export function shortDate(seconds: number): string {
  return new Date(seconds * 1000).toLocaleDateString("en-US", { month: "short", day: "numeric" });
}
