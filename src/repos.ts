import type { Board, RepoView } from "./types";

/** The repos holding a project's work: the ones whose config names it, and the unconfigured ones whose branches carry its keys. */
export function reposOf(board: Board, projectKey: string): RepoView[] {
  const key = projectKey.toUpperCase();
  const ours = (k: string | null) => !!k && k.toUpperCase().startsWith(`${key}-`);
  return board.repos.filter((r) =>
    r.project ? r.project.toUpperCase() === key : r.rows.some((x) => ours(x.issue_key)) || r.branches.some((b) => ours(b.issue_key)),
  );
}
