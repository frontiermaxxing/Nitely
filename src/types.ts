// Mirrors nitely-core's snapshot types (crates/nitely-core/src/snapshot.rs).

import type { Track } from "./ui/lifecycle";

export type SessionState = "working" | "idle" | "waiting" | "ended";

export interface Session {
  id: string;
  vendor: string;
  cwd: string;
  started_at: number;
  last_seen_at: number;
  last_activity: string | null;
  state: SessionState;
  /** The crew started it (docs/crew.md); absent otherwise. */
  crew?: boolean;
}

/** One agent process a project's crew started. */
export interface CrewWorker {
  id: number;
  project: string;
  issue_key: string;
  agent: string;
  pid: number | null;
  worktree: string;
  log: string;
  started_at: number;
  ended_at: number | null;
  exit_code: number | null;
  /** landed, submitted, asked, failed, stopped or launch_failed, once it ended. */
  outcome: string | null;
  reason: string | null;
}

/** A project's crew, from `crew_status`; null when the project has none. */
export interface CrewStatus {
  project: string;
  cap: number;
  running: CrewWorker[];
  started_today: number;
  max_per_day: number;
  paused: boolean;
  /** A `nitely crew run` heartbeat is fresh. */
  supervised: boolean;
  /** What the supervisor last said, e.g. "working" or "quiet hours until 07:00". */
  status: string | null;
  recent: CrewWorker[];
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

/** A local branch that no worktree has checked out: left after a land, parked, or made by hand. */
export interface BranchRow {
  name: string;
  vendor: string | null;
  issue_key: string | null;
  /** Null when git could not count them against the integration branch. */
  ahead: number | null;
  behind: number | null;
  last_commit_subject: string | null;
  last_commit_at: number | null;
  issue: IssueRecord | null;
  pr: PrRecord | null;
}

export interface RepoView {
  name: string;
  root: string;
  integration_branch: string;
  /** The project (issue prefix) the repo's work belongs to, when its config names one. */
  project: string | null;
  rows: TaskRow[];
  /** Branches with no worktree, the integration branch aside, newest first. */
  branches: BranchRow[];
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
  /** Where a native issue came from, e.g. its Linear address after an import. */
  external_url?: string;
  state: string;
  state_type: string;
  kind: string | null;
  source: string | null;
  priority: number;
  labels: string[];
  evidence: string;
  claimed_by: string | null;
  /** "human" for work only the human can do (next_task never hands it out); "agent" otherwise. */
  owner?: "agent" | "human" | string;
  created_at: string;
  updated_at: string;
  /** Open issues that must close before this one can start; next_task skips it until then. */
  blocked_by: string[];
  /** Open issues waiting on this one. */
  blocks: string[];
  /** Its priority, raised to that of the most urgent open issue it blocks (same 0-4 scale). */
  urgency: number;
  /** Where it is on the lifecycle track, computed by the backend (crates/nitely-core/src/track.rs). */
  track?: Track;
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
  checks: "success" | "failure" | "pending" | "";
}

export interface SyncStatus {
  source: string;
  last_attempt: number;
  last_ok: number | null;
  last_error: string | null;
}

/** A machine allowed to reach this backlog through `nitely serve` (never the token itself). */
export interface MachineToken {
  name: string;
  project: string | null;
  created_at: number;
}

export interface NetworkInfo {
  tokens: MachineToken[];
  urls: string[];
  port: number;
}

/** What adding a machine returns: its token, once, inside the commands to run there. */
export interface NewMachine {
  name: string;
  token: string;
  commands: string[];
}

export interface Board {
  repos: RepoView[];
  needs_me_issues: IssueRecord[];
  asks: Ask[];
  sync: SyncStatus[];
  /** Agents on other machines on the local network, reaching this one through `nitely serve`. */
  remote_sessions: Session[];
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

/** What agents learned about a project, and when the setup sweep read it (crates/nitely-core/src/brief.rs). */
export interface ProjectBrief {
  project: string;
  name: string;
  open_issues: number;
  brief: string;
  /** Bumped on every write; a save passes the one it read and is refused if someone wrote since. */
  revision: number;
  updated_at: number | null;
  updated_by: string | null;
  setup: { sha: string; at: number; by: string } | null;
}

export interface TimelineEntry {
  ts: number;
  actor: string;
  kind: string;
  summary: string | null;
}

/** An issue on the other end of a blocked-by relation, open or closed. */
export interface LinkedIssue {
  key: string;
  title: string;
  state: string;
  open: boolean;
}

export interface IssueDetail {
  issue: IssueRecord;
  relations: { blocked_by: LinkedIssue[]; blocks: LinkedIssue[] };
  timeline: TimelineEntry[];
  prs: PrRecord[];
  reviews: ReviewRecord[];
  /** The playbook change the issue carries, when an agent proposed one (crates/nitely-core/src/proposals.rs). */
  proposal?: PlaybookProposal | null;
}

/** A playbook change an agent proposed; approving its issue applies it, canceling rejects it. */
export interface PlaybookProposal {
  id: number;
  issue_key: string;
  repo: string;
  path: string;
  /** The repo commits its own .nitely/playbook.toml: approving commits the change on its integration branch. */
  in_repo: boolean;
  why: string;
  status: "pending" | "applied" | "rejected" | "superseded";
  created_by: string;
  created_at: number;
  decided_by: string | null;
  decided_at: number | null;
  outcome: string | null;
  /** While pending, against the playbook as it is now. `@` stands for unchanged lines left out. */
  diff: { op: " " | "+" | "-" | "@"; text: string }[];
  /** While pending: why it no longer applies to the playbook as it is now. */
  stale: string | null;
}

export interface ReviewRecord {
  issue_key: string;
  patch_id: string;
  commit_sha: string;
  /** Empty when no review ran. */
  reviewer: string;
  verdict: "pass" | "changes" | "none";
  approver: "agent" | "human";
  reasons: string[];
  /** carried: raised in an earlier round and not fixed since. */
  findings: { priority: number; text: string; carried?: boolean }[];
  ts: number;
  /** How independent the review was when it was not another vendor by choice, e.g. "same vendor: codex not installed". */
  note?: string;
}

export interface ConnectChange {
  target: string;
  action: "create" | "update" | "unchanged" | "run" | "manual";
  why: string;
  before?: string;
  after: string;
}

export interface ConnectPlan {
  root: string;
  repo_name: string;
  project_key: string;
  project_name: string;
  project_exists: boolean;
  integration_branch: string;
  playbook: { source: string; path: string; text: string };
  nitely: string;
  agents: AgentPlan[];
  warnings: string[];
}

export interface AgentPlan {
  agent: string;
  found: boolean;
  changes: ConnectChange[];
  error?: string;
}

export interface AgentsUpdated {
  done: string[];
  failed: string[];
}

export interface Applied {
  project: string;
  repo: string;
  done: string[];
  failed: string[];
}

// The Pulse home view (crates/nitely-core/src/pulse.rs).

export interface GateRun {
  ts: number;
  passed: boolean;
  summary: string | null;
}

export interface FlowIssue {
  issue: IssueRecord;
  /** Who works on it: an actor id like `claude:c:/...`, or a vendor. */
  agent: string | null;
  session: Session | null;
  pr: PrRecord | null;
  review: ReviewRecord | null;
  gate: GateRun | null;
  worktree: string | null;
  last_activity: number;
  /** At an agent's station, not stuck, and nothing happened for an hour. */
  quiet: boolean;
}

export interface ProjectFlow {
  key: string;
  name: string;
  issues: FlowIssue[];
  done_today: FlowIssue[];
}

export interface Happening {
  ts: number;
  kind: "landed" | "filed" | "stuck" | "review" | "asked";
  issue_key: string | null;
  actor: string;
  summary: string | null;
}

export interface PulseHour {
  start: number;
  landed: number;
  submitted: number;
}

export interface Pulse {
  now: number;
  since: number;
  projects: ProjectFlow[];
  asks: Ask[];
  approvals: IssueRecord[];
  /** At most 300, newest first. */
  happened: Happening[];
  /** Everything since `since`, counted per kind, past the list's limit. */
  happened_counts: Record<Happening["kind"], number>;
  throughput: PulseHour[];
  landed_today: number;
}

// Mirrors crates/nitely-core/src/activity.rs: what every agent session did over a window.

/** "seen": active before session states were recorded, so the state is unknown. */
export type SegmentState = "working" | "waiting" | "idle" | "stale" | "seen";

export interface ActivitySegment {
  from: number;
  to: number;
  state: SegmentState;
}

export type MarkKind =
  | "start"
  | "commit"
  | "gate_pass"
  | "gate_fail"
  | "submit"
  | "review_pass"
  | "review_changes"
  | "review_fail"
  | "land"
  | "land_fail"
  | "ask"
  | "handoff";

export interface ActivityMark {
  ts: number;
  kind: MarkKind;
  issue_key: string | null;
  summary: string;
}

export interface ActivityLane {
  session_id: string;
  vendor: string;
  cwd: string;
  project: string | null;
  place: string;
  /** Issues it worked on in the window, the most recent last. */
  issues: string[];
  started_at: number;
  last_seen_at: number;
  state: SessionState;
  segments: ActivitySegment[];
  marks: ActivityMark[];
}

export interface Activity {
  from: number;
  to: number;
  /** When session states started being recorded; null while nothing is. */
  recorded_since: number | null;
  lanes: ActivityLane[];
  issues: IssueRecord[];
}

/** Who lands a reviewed change (`[land] by` in the playbook). */
export type LandBy = "author" | "auto" | "human";

export interface PathRule {
  pattern: string;
  reason: string;
}

/** A yes/no question the reviewer answers; a yes sends the change to a person. */
export interface ReviewFlag {
  name: string;
  question: string;
  reason: string;
}

/** A value and where it comes from: the project's playbook, the app-wide default, or Nitely's own. */
export interface RuleField<T> {
  value: T;
  from: "project" | "app" | "built-in";
  /** What it would be if the project did not set it, and where that comes from. */
  inherited: T;
  inherited_from: "app" | "built-in";
}

export interface ReviewerInfo {
  name: string;
  installed: boolean;
}

/** One repo's review and landing rules (crates/nitely-core/src/rules.rs). */
export interface RulesView {
  repo: string;
  root: string;
  source: "repo" | "stored" | "none";
  path: string;
  editable: boolean;
  read_only: string | null;
  integration: string;
  always_human: boolean;
  human_paths: PathRule[];
  blocked_paths: PathRule[];
  flags: ReviewFlag[];
  reviewer: RuleField<Record<string, string>>;
  require_cross_vendor: RuleField<boolean>;
  land_by: RuleField<LandBy>;
  focus: RuleField<string>;
  reviewers: ReviewerInfo[];
}

/** A save from the Rules view; a null default-able value inherits again. */
export interface RulesEdit {
  always_human: boolean;
  human_paths: PathRule[];
  blocked_paths: PathRule[];
  flags: ReviewFlag[];
  reviewer: Record<string, string> | null;
  require_cross_vendor: boolean | null;
  land_by: LandBy | null;
  focus: string | null;
}

/** App-wide defaults for what a playbook leaves unset; a missing key keeps Nitely's own. */
export interface RuleDefaults {
  reviewer?: Record<string, string>;
  require_cross_vendor?: boolean;
  land_by?: LandBy;
  focus?: string;
}

export interface DefaultsView {
  defaults: RuleDefaults;
  built_in_reviewer: Record<string, string>;
  reviewers: ReviewerInfo[];
}

/** What submit would decide for a branch under the rules as they are now. */
export interface DryRun {
  branch: string;
  base: string;
  files: string[];
  reviewer: string | null;
  reviewer_note: string;
  reviewer_error: string | null;
  reasons: string[];
  flags: string[];
  blocked: string | null;
  outcome: string;
}
