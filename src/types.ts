// Mirrors nitely-core's snapshot types (crates/nitely-core/src/snapshot.rs).

export type SessionState = "working" | "idle" | "waiting" | "ended";

export interface Session {
  id: string;
  vendor: string;
  cwd: string;
  started_at: number;
  last_seen_at: number;
  last_activity: string | null;
  state: SessionState;
}

export interface BranchState {
  ahead: number;
  behind: number;
  dirty_files: number;
  last_commit_subject: string | null;
  last_commit_at: number | null;
  rebase_in_progress: boolean;
}

export interface TaskRow {
  worktree: string;
  branch: string | null;
  is_main: boolean;
  vendor: string | null;
  issue_key: string | null;
  state: BranchState | null;
  error: string | null;
  session: Session | null;
  issue: IssueRecord | null;
  pr: PrRecord | null;
  needs_me: boolean;
}

export interface RepoView {
  name: string;
  root: string;
  integration_branch: string;
  rows: TaskRow[];
  error: string | null;
  unmatched_sessions: Session[];
}

export interface IssueRecord {
  key: string;
  origin: "nitely" | "linear" | string;
  project: string | null;
  title: string;
  body: string;
  url: string;
  state: string;
  state_type: string;
  kind: string | null;
  source: string | null;
  priority: number;
  labels: string[];
  evidence: string;
  claimed_by: string | null;
  created_at: string;
  updated_at: string;
}

export interface PrRecord {
  provider: string;
  repo: string;
  number: number;
  title: string;
  url: string;
  state: string;
  branch: string;
  head_sha: string;
  updated_at: string;
  comment_count: number;
  approved_by: string[];
  approved_by_me: boolean;
}

export interface SyncStatus {
  source: string;
  last_attempt: number;
  last_ok: number | null;
  last_error: string | null;
}

export interface Board {
  repos: RepoView[];
  needs_me_issues: IssueRecord[];
  asks: Ask[];
  sync: SyncStatus[];
}

export interface Ask {
  id: number;
  issue_key: string | null;
  asked_by: string;
  question: string;
  options: string[];
  answer: string | null;
  asked_at: number;
  answered_at: number | null;
}

export interface Project {
  key: string;
  name: string;
  open_issues: number;
}

export interface TimelineEntry {
  ts: number;
  actor: string;
  kind: string;
  summary: string | null;
}

export interface IssueDetail {
  issue: IssueRecord;
  timeline: TimelineEntry[];
}
