import { useEffect, useState } from "react";
import { invoke } from "../backend";
import { EmptyState, Loading, Segmented } from "../ui";
import type { Project } from "../types";
import { Board } from "./Board";
import { OptIn } from "./OptIn";
import { sharing, useProfile } from "./profile";
import { ProfileDialog } from "./EditProfile";
import { PERIOD_LABEL, type LeaderboardStats, type PeriodKey } from "./stats";
import { You } from "./You";
import "./leaderboard.css";

export { useProfile } from "./profile";

const PERIODS: PeriodKey[] = ["week", "30d", "all"];

/** The leaderboard page. Your own numbers read this machine's database; nothing here sends anything. */
export function Leaderboard(props: { now: number; version: number; projects: Project[] }) {
  const profile = useProfile();
  // Before you join the page opens on joining, after on the board; your own numbers are a tab away, joined or not.
  const [chosen, setTab] = useState<"join" | "board" | "you" | null>(null);
  const tab = chosen === "join" && profile ? "board" : (chosen ?? (profile ? "board" : "join"));
  const joining = profile === null && tab === "join";
  const [period, setPeriod] = useState<PeriodKey>("week");
  const [editing, setEditing] = useState(false);
  const [stats, setStats] = useState<LeaderboardStats | null>(null);
  const [error, setError] = useState<string | null>(null);
  const projects = profile?.projects ?? [];
  const projectsKey = projects.join(",");
  const ready = profile !== undefined;
  useEffect(() => { setStats(null); setError(null); }, [projectsKey]);
  useEffect(() => {
    if (!ready || tab !== "you") return;
    let live = true;
    invoke<LeaderboardStats>("leaderboard_stats", { projects }).then(
      (s) => live && (setStats(s), setError(null)),
      (e) => live && setError(String(e)),
    );
    return () => {
      live = false;
    };
  }, [ready, tab, projectsKey, props.version]);

  return (
    <div className="page lb">
      <div className="page-head lb-head">
        <h1>Leaderboard</h1>
        {profile ? (
          <button className={`lb-chip lb-chip-button${sharing(profile) ? " lb-chip-on" : ""}`} onClick={() => setEditing(true)} aria-haspopup="dialog" title="Edit your leaderboard profile">
            <span className="lb-chip-dot" aria-hidden="true" />
            {sharing(profile) ? "Sharing" : "Joined"} as {profile.display_name}
            <svg width="10" height="10" viewBox="0 0 12 12" aria-hidden="true">
              <path d="M3 4.5 6 7.5 9 4.5" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" />
            </svg>
          </button>
        ) : (
          <span className="lb-chip">
            <span className="lb-chip-dot" aria-hidden="true" />
            Not sharing
          </span>
        )}
        {profile !== undefined && (
          <Segmented
            aria-label="Page"
            value={tab}
            onChange={setTab}
            options={[...(profile ? [] : [{ value: "join" as const, label: "Join" }]), { value: "board" as const, label: "Board" }, { value: "you" as const, label: "You" }]}
          />
        )}
        {tab === "you" && <span className="lb-head-note">Your stats are always here, sharing or not</span>}
        {!joining && <Segmented aria-label="Period" value={period} onChange={setPeriod} options={PERIODS.map((p) => ({ value: p, label: PERIOD_LABEL[p] }))} />}
      </div>
      {profile === undefined ? (
        <Loading />
      ) : joining ? (
        <OptIn projects={props.projects} />
      ) : tab === "board" ? (
        <Board period={period} profile={profile} onEdit={() => (profile ? setEditing(true) : setTab("join"))} />
      ) : error ? (
        <EmptyState title="Could not read your numbers">{error}</EmptyState>
      ) : !stats ? (
        <Loading />
      ) : (
        <You
          stats={stats}
          period={period}
          profile={profile}
          now={props.now}
          onJoin={() => setTab("join")}
          onEdit={() => setEditing(true)}
        />
      )}
      {editing && profile && (
        <ProfileDialog
          profile={profile}
          projects={props.projects}
          onClose={() => setEditing(false)}
          onLeft={() => {
            setEditing(false);
            setTab("join");
          }}
        />
      )}
    </div>
  );
}
