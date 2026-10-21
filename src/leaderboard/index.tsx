import { useEffect, useState } from "react";
import { invoke } from "../backend";
import { EmptyState, Loading, Segmented } from "../ui";
import { loadProfile, onProfileChange, type Profile } from "./profile";
import { PERIOD_LABEL, type LeaderboardStats, type PeriodKey } from "./stats";
import { You } from "./You";
import "./leaderboard.css";

export { leaderboardJoined } from "./profile";

/** Your leaderboard profile, kept current when it is saved or cleared. */
export function useProfile(): Profile | null {
  const [profile, setProfile] = useState(loadProfile);
  useEffect(() => onProfileChange(() => setProfile(loadProfile())), []);
  return profile;
}

const PERIODS: PeriodKey[] = ["week", "30d", "all"];

/** The leaderboard page. Your own numbers read this machine's database; nothing here sends anything. */
export function Leaderboard(props: { now: number; version: number }) {
  const profile = useProfile();
  const [period, setPeriod] = useState<PeriodKey>("week");
  const [stats, setStats] = useState<LeaderboardStats | null>(null);
  const [error, setError] = useState<string | null>(null);
  const projects = profile?.projects ?? [];
  const projectsKey = projects.join(",");
  useEffect(() => {
    let live = true;
    invoke<LeaderboardStats>("leaderboard_stats", { projects }).then(
      (s) => live && (setStats(s), setError(null)),
      (e) => live && setError(String(e)),
    );
    return () => {
      live = false;
    };
  }, [projectsKey, props.version]);

  return (
    <div className="page lb">
      <div className="page-head lb-head">
        <h1>Leaderboard</h1>
        {profile ? (
          <span className="lb-chip lb-chip-on">
            <span className="lb-chip-dot" aria-hidden="true" />
            Joined as {profile.display_name}
          </span>
        ) : (
          <span className="lb-chip">
            <span className="lb-chip-dot" aria-hidden="true" />
            Not sharing
          </span>
        )}
        <span className="lb-head-note">Your stats are always here, sharing or not</span>
        <Segmented aria-label="Period" value={period} onChange={setPeriod} options={PERIODS.map((p) => ({ value: p, label: PERIOD_LABEL[p] }))} />
      </div>
      {error ? (
        <EmptyState title="Could not read your numbers">{error}</EmptyState>
      ) : !stats ? (
        <Loading />
      ) : (
        <You stats={stats} period={period} profile={profile} now={props.now} />
      )}
    </div>
  );
}
