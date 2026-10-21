import { EmptyState } from "../ui";

/**
 * Whether you joined the leaderboard. Nobody can join yet (the opt-in page is NIT-445), so the
 * sidebar's "opt in" tag always shows; joining will set this, and nothing is sent before it does.
 */
export function leaderboardJoined(): boolean {
  return false;
}

/** The leaderboard page: a placeholder until its views land (goal 15). Reads and sends nothing. */
export function Leaderboard() {
  return (
    <div className="page">
      <div className="page-head">
        <h1>Leaderboard</h1>
      </div>
      <EmptyState title="The leaderboard is on its way">
        It is opt-in: nothing about your work leaves this machine unless you join, and you choose what counts.
      </EmptyState>
    </div>
  );
}
