// When the Leaderboard's sends go, in your words (NIT-629).

/**
 * When a send goes, in your words: "now", "today 18:00", "tomorrow 18:00", or the weekday and
 * time ("Sun 18:00") further out. `now` and `at` are seconds; times are local.
 */
export function when(now: number, at: number): string {
  if (at <= now + 60) return "now";
  const d = new Date(at * 1000);
  const time = `${String(d.getHours()).padStart(2, "0")}:${String(d.getMinutes()).padStart(2, "0")}`;
  const day = (t: number) => {
    const x = new Date(t * 1000);
    return new Date(x.getFullYear(), x.getMonth(), x.getDate()).getTime();
  };
  const days = Math.round((day(at) - day(now)) / 86_400_000);
  if (days === 0) return `today ${time}`;
  if (days === 1) return `tomorrow ${time}`;
  return `${d.toLocaleDateString("en-US", { weekday: "short" })} ${time}`;
}
