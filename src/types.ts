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
  /** "setup" while its worktree is being set up, then "work". */
  phase: string;
}

/** Tokens one model used (crates/nitely-crew/src/usage.rs). Tokens are new input, cache writes and output. */
export interface ModelTokens {
  vendor: string;
  model: string;
  tokens: number;
  calls: number;
}

/** What an issue's agents used, from `issue_spend`: counts only. */
export interface IssueSpend {
  key: string;
  tokens: number;
  cache_read: number;
  calls: number;
  sessions: number;
  by_model: ModelTokens[];
  first: number | null;
  last: number | null;
}

/** A project's spend over the last days, from `project_spend`. */
export interface ProjectSpend {
  project: string;
  /** Oldest first, the last one today. */
  days: { day_start: number; tokens: number; landed: number }[];
  tokens: number;
  cache_read: number;
  by_agent: ModelTokens[];
  by_model: ModelTokens[];
  landed: number;
  per_landed: number | null;
  top_issues: { key: string; tokens: number }[];
}

/** What mirroring a project's issues to its repo's tracker would publish (crates/nitely-core/src/mirror.rs). */
export interface MirrorPreview {
  project: string;
  enabled: { project: string; enabled_at: number; backfill: string } | null;
  host: string | null;
  repo: string | null;
  /** Why it cannot mirror, when it cannot. */
  unavailable: string | null;
  counts: { backfill: string; publish: number; private: number; held: number }[];
  sample: { key: string; title: string; body: string; labels: string[] }[];
  /** What went wrong publishing just now. */
  last_error?: string | null;
}

/** An issue's place in the mirror. */
export interface IssueMirror {
  issue: string;
  /** mirrored, private, held or withdrawn. */
  status: string;
  reason: string | null;
  host: string | null;
  number: number | null;
  url: string | null;
  marked_private: boolean;
}

/** A project's crew, from `crew_status`; null when the project has none. */
export interface CrewStatus {
  project: string;
  cap: number;
  running: CrewWorker[];
  started_today: number;
  max_per_day: number;
  /** Tokens the project's agents used today, and the playbook's daily budget if it sets one. */
  tokens_today?: number;
  max_tokens_per_day?: number | null;
  paused: boolean;
  /** A `nitely crew run` heartbeat is fresh. */
  supervised: boolean;
  /** What the supervisor last said, e.g. "working" or "quiet hours until 07:00". */
  status: string | null;
  recent: CrewWorker[];
}

/** One repo's crew as the Crew section edits it (crates/nitely-crew/src/crew_settings.rs). */
export interface CrewView {
  repo: string;
  root: string;
  /** `repo`: saving commits to its .nitely/playbook.toml; `stored`: the copy Nitely keeps; `none`: no playbook yet. */
  source: "repo" | "stored" | "none";
  path: string;
  integration: string;
  workers: number;
  agents: string[];
  max_per_day: number | null;
  default_max_per_day: number;
  max_tokens_per_day: number | null;
  quiet_hours: string | null;
  choices: { name: string; installed: boolean }[];
  has_gate: boolean;
  pending_proposal: string | null;
}

export interface CrewEdit {
  workers: number;
  agents: string[];
  max_per_day: number | null;
  max_tokens_per_day: number | null;
  quiet_hours: string | null;
}

export interface CrewSaved {
  view: CrewView;
  done: string;
}

export interface BranchState {
  ahead: number;
  behind: number;
  dirty_files: number;
  last_commit_subject: string | null;
  last_commit_at: number | null;
  rebase_in_progress: boolean;
  /** When the stopped rebase began (unix seconds); null when there is none or it is not known. */
  rebase_started_at?: number | null;
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

/** An issue in flight and the files it holds: its branch's changes, then paths its body names. */
export interface Held {
  key: string;
  branch: string | null;
  files: string[];
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
  /** The goal it belongs to, if any (crates/nitely-core/src/goals.rs). */
  goal?: number | null;
  /** Declared review level: inert, contained, standard or sensitive. Absent is undeclared. */
  review_level?: string | null;
  /** Who declared it. */
  review_level_by?: string | null;
  /** author, planner, coordinator, human, or implementer (a hint). */
  review_level_source?: string | null;
  review_level_at?: number | null;
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
  /** Every agent session on this machine, one each, newest first (snapshot.rs AgentRow). */
  agents: AgentRow[];
  /** The latest land whose `[[land.after]]` steps failed, while no later land's ran clean (NIT-469). */
  after_land_failed?: { issue: string | null; ts: number; summary: string } | null;
  /** What the person is told and nothing waits on, until they dismiss it (snapshot.rs Notice, NIT-570). */
  notices?: Notice[];
}

/** A notice in the Inbox: the event that says it, its issue, when, and what it says. */
export interface Notice {
  id: number;
  issue_key: string | null;
  ts: number;
  summary: string;
}

/** One agent session, wherever it runs: several often share a folder, so a folder is not an agent. */
export interface AgentRow {
  session: Session;
  repo: string | null;
  project: string | null;
  /** The task worktree it runs in; null in a main checkout. */
  worktree: string | null;
  /** What its own claims, start_task, submit and recent reports name, else its worktree's issue. */
  issue_key: string | null;
  issue: IssueRecord | null;
  /** Its latest report line. */
  report: string | null;
  needs_me: boolean;
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
  /** Why it closed unanswered: its issue closed first. */
  closed?: string | null;
  /** The review finding the agent asks the person to accept as it is (crates/nitely-core/src/accept.rs). */
  finding?: { priority: number; text: string } | null;
  /** When the answer reached an agent, and which (NIT-547). */
  delivered_at?: number | null;
  delivered_to?: string | null;
}

export interface Project {
  key: string;
  name: string;
  open_issues: number;
  archived?: boolean;
}

/** What agents learned about a project, and when the setup sweep read it (crates/nitely-core/src/brief.rs). */
/** An outcome a project works toward (crates/nitely-core/src/goals.rs). */
export interface Goal {
  id: number;
  project: string;
  title: string;
  /** One paragraph: what is true once it is reached. */
  outcome: string;
  /** YYYY-MM-DD. */
  target_date: string | null;
  state: "active" | "done" | "dropped";
  created_by: string;
  created_at: number;
  updated_at: number;
  progress: { total: number; done: number; dropped: number; open: number };
  /** The person's order among the project's active goals, lower first; Crew works goals in it. */
  rank?: number | null;
  /** How many crew workers may work on it at once; null leaves it to the playbook. */
  workers?: number | null;
  /** Crew workers on its issues now. */
  working?: number;
  /** Its issues that landed since local midnight. */
  landed_today?: number;
  /** Its goal branch, when its items land into one (crates/nitely-core/src/goal_branch.rs). */
  branch?: GoalBranchSummary;
}

// Mirrors goal_branch::Summary.
export interface GoalBranchSummary {
  branch: string;
  /** Made yet: its first item started. */
  created: boolean;
  state: "green" | "red";
  red_reason?: string | null;
  red_issue?: string | null;
  /** Items landed on it since the last checkpoint. */
  items: number;
  /** "at 10 items (4 to go)", "due now", or "NIT-612 is landing it". */
  next_checkpoint: string;
  checkpoint_issue?: string | null;
  checkpoint_at?: number | null;
  landed_items: number;
  checkpoints: number;
  full_runs: number;
  reviews: number;
}

/** Which goal new agent work follows, without changing issue priority. */
export interface ProjectFocus {
  project: string;
  goal: number | null;
  goal_title: string | null;
  allow_urgent: boolean;
  active: boolean;
}

/** A goal and its issues, open and closed: what a goal row draws. */
export interface GoalDetail {
  goal: Goal;
  issues: IssueRecord[];
  /** Its open issues that may hold it up. */
  risks?: GoalRisk[];
  /** When it will be done at its recent pace; null when nothing of it landed lately. */
  finish?: GoalFinish | null;
}

// Mirrors crates/nitely-core/src/goal_report.rs (NIT-247).

/** An open issue of a goal that may hold it up: stuck, waiting on you, or through many review rounds. */
export interface GoalRisk {
  key: string;
  title: string;
  /** "stuck: …", "waits on you", "asked you something", "4 review rounds". */
  why: string;
}

export interface GoalFinish {
  /** Unix seconds. */
  at: number;
  /** Its issues landed per day over the last three days. */
  per_day: number;
}

export interface ReportLanded {
  key: string;
  title: string;
  ts: number;
}

/** A place the person was needed: an agent's question (or a finding to accept), or a change that waited for approval. */
export interface ReportNeeded {
  ts: number;
  kind: "asked" | "approval";
  issue_key: string | null;
  summary: string;
  /** Still waiting on them. */
  open: boolean;
}

/** What happened since `since`, per active goal, and every place the person was needed. */
export interface GoalReport {
  since: number;
  now: number;
  goals: { goal: Goal; landed: ReportLanded[]; risks: GoalRisk[]; finish: GoalFinish | null }[];
  /** Issues in no active goal that landed since `since`. */
  other_landed: ReportLanded[];
  needed: ReportNeeded[];
  /** Questions triage answered so you were not needed. */
  triaged: { ts: number; issue_key: string | null; question: string; answer: string }[];
}

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
  /** The session that wrote it (`claude:<session id>`), or for an older event its agent and folder. */
  actor: string;
  kind: string;
  summary: string | null;
  /** The folder its session works in, where known. */
  place?: string;
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
  /** The goal plan the issue carries, when an agent drafted one (crates/nitely-core/src/goal_plans.rs). */
  goal_plan?: GoalPlanView | null;
  /** Its review batches and trips through a land queue (crates/nitely-core/src/batches.rs). */
  batches?: IssueBatches;
  /** The asks about it, answered or not, so its timeline can link each one (NIT-546). */
  asks?: Ask[];
}

/** A review batch: changes of one repository reviewed in one reviewer call (batches.rs `ReviewBatch`). */
export interface ReviewBatch {
  id: number;
  repo: string;
  repo_name: string | null;
  reviewer: string;
  /** Its changes, in the order the reviewer got them. */
  issues: string[];
  /** reviewing | done | failed */
  state: string;
  /** Recorded only: each change's single review decided it (`[review] batch = "shadow"`). */
  shadow: boolean;
  started_at: number;
  ended_at: number | null;
  seconds: number | null;
  tokens: number | null;
  error: string | null;
}

/** A land queue run: its changes in stack order, what became of them, and what it is doing (land_queue.rs `Run`). */
export interface LandRun {
  id: number;
  repo: string;
  started_at: number;
  ended_at: number | null;
  issues: string[];
  landed: string[];
  sent_back: string[];
  gates: number;
  head: string | null;
  /** While it runs: "stacking", "gating the stack", "bisecting: gating 2 of 3", "landing". */
  step: string | null;
}

/** Something a waiting request waits behind (waits.rs `Behind`). */
export interface WaitBehind {
  /** "run 412: NIT-290 gate", "submit job 31", "NIT-288 (land), holding the land lock". */
  what: string;
  issue: string | null;
  /** Since when it holds what the request waits for. */
  since: number | null;
}

/** A request waiting now, why, and behind what (waits.rs `Waiting`, NIT-322). */
export interface Waiting {
  /** run | gate | land | submit | review | land_queue */
  kind: string;
  issue: string | null;
  project: string | null;
  since: number;
  why: string;
  /** Its place in its queue, 1 for the next. */
  place: number | null;
  behind: WaitBehind[];
}

/** Every request waiting now, longest waiting first (src-tauri `waits_overview`). */
export interface Waits {
  waiting: Waiting[];
}

/** The batches going now, for the board (batches.rs `Overview`). */
export interface BatchesOverview {
  reviewing: ReviewBatch[];
  review_waiting: { issue: string; repo_name: string | null; reviewer: string; shadow: boolean; queued_at: number; lines: number }[];
  land: {
    repo: string;
    repo_name: string | null;
    /** Oldest first: their place in the queue. */
    entries: { id: number; issue: string; state: "queued" | "running" | string; queued_at: number; run: number | null }[];
    runs: LandRun[];
  }[];
}

/** Why a change went back from a batch (batches.rs `SentBack`). */
export interface SentBack {
  why: "own" | "interacts" | "bisected" | "gate" | "conflict" | "moved" | "stopped" | "other" | string;
  /** The changes it names: the one it interacts or conflicts with. */
  with: string[];
}

/** An issue's review batches and land queue trips, newest first (batches.rs `IssueBatches`). */
export interface IssueBatches {
  reviews: {
    batch: ReviewBatch;
    /** reviewing | done | alone */
    state: string;
    verdict: string | null;
    blocking: number;
    minor: number;
    why: string | null;
    sent_back: SentBack | null;
  }[];
  lands: {
    id: number;
    /** queued | running | landed | sent_back */
    state: string;
    queued_at: number;
    ended_at: number | null;
    place: number | null;
    run: LandRun | null;
    lane_run: number | null;
    head: string | null;
    error: string | null;
    sent_back: SentBack | null;
  }[];
}

export const REVIEW_LEVELS: { id: string; label: string }[] = [
  { id: "inert", label: "Basic" },
  { id: "contained", label: "Focused" },
  { id: "standard", label: "Integration" },
  { id: "sensitive", label: "Full" },
];

export function reviewLevelLabel(level: string | null | undefined): string {
  return REVIEW_LEVELS.find((l) => l.id === level)?.label ?? "Undeclared";
}

export interface PlanItem {
  ref: string;
  title: string;
  kind: string;
  body: string;
  evidence: string;
  priority?: number;
  labels?: string[];
  blocked_by?: string[];
  owner?: string;
  state?: string;
  /** The files it will change, which Crew uses to keep parallel workers apart. */
  touches?: string[];
  /** Why the work changes no file in the repo (a design in an outside tool), in place of touches. */
  no_repo_changes?: string;
  /** S, M or L. */
  size?: string;
  /** inert, contained, standard or sensitive. */
  review_level?: string;
}

/** A goal and its issues an agent drafted from the person's sentence, decided once in the Inbox. */
export interface GoalPlanView {
  id: number;
  issue_key: string;
  project: string;
  plan: { sentence: string; title: string; outcome: string; target_date: string | null; issues: PlanItem[]; existing: string[]; existing_blocked_by?: Record<string, string[]> };
  /** `started`: the review mode started it by itself (NIT-550); its issue is the person's notice. */
  status: "pending" | "approved" | "started" | "rejected";
  created_by: string;
  created_at: number;
  outcome: string | null;
  goal_id: number | null;
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

export interface SolutionSummary {
  title: string;
  summary: string;
}
export interface ApprovalGate {
  plan?: { depth: string; effective: string; head: string; policy_commit: string; commands: string[]; reasons: string[] } | null;
  commit: string;
  passed: boolean;
  /** An intentional policy skip is unverified, never a passing gate. */
  skipped?: string;
  results: { command: string; ok: boolean; skipped?: boolean }[];
  flaked?: { command: string }[];
  warnings?: string[];
}

export interface ReviewRecord {
  /** Branch handed in; absent on legacy reviews. */
  branch?: string | null;
  solution?: SolutionSummary | null;
  gate?: ApprovalGate | null;
  issue_key: string;
  patch_id: string;
  commit_sha: string;
  /** Empty when no review ran. */
  reviewer: string;
  verdict: "pass" | "changes" | "none" | "disabled" | "yolo" | "light" | "quick";
  approver: "agent" | "human";
  reasons: string[];
  /** carried: raised in an earlier round and not fixed since; now: how the reviewer says it is still open. */
  findings: { priority: number; text: string; carried?: boolean; now?: string }[];
  ts: number;
  /** How independent the review was when it was not another vendor by choice, e.g. "same vendor: codex not installed". */
  note?: string;
  /** The review batch it was reviewed in, with other changes in one reviewer call (NIT-216). */
  batch_id?: number | null;
}

/** A tool Nitely works with, as `tools_check` found it (crates/nitely-core/src/tools.rs). */
export interface Tool {
  command: string;
  name: string;
  /** Where its command is; null when it is not installed. */
  path: string | null;
  version: string | null;
  /** Signed in, signed out, or null: the tool can't say, or has no sign-in. */
  signed_in: boolean | null;
}

/** How a missing tool installs (crates/nitely-core/src/tools_install.rs, NIT-688): shown before it runs. */
export interface InstallPlan {
  tool: string;
  command: string[];
  /** The command as the person reads it. */
  shown: string;
  via: "Winget" | "Brew" | "Npm" | "Vendor" | "System";
  docs: string;
  /** The installer asks for administrator rights; the OS shows that prompt. */
  elevates: boolean;
  /** Nitely runs it when the person says so; false where it only shows it. */
  runs: boolean;
  note?: string;
}

export interface ConnectChange {
  target: string;
  action: "create" | "update" | "unchanged" | "run" | "manual" | "remove";
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
  /** The integration branch has no commit yet: the page offers the first commit (NIT-685). */
  first_commit_needed: boolean;
}

/** What a repo's first commit would hold (first_commit.rs). */
export interface FirstCommitPreview {
  files: number;
  bytes: number;
  largest: [string, number][];
  flagged: { path: string; reason: "build" | "large" | "secret"; bytes: number; line: string }[];
  gitignore: string[] | null;
  needs_identity: boolean;
  branch: string;
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

// The Pulse home view (crates/nitely-crew/src/pulse.rs).

export interface GateRun {
  ts: number;
  passed: boolean;
  skipped?: string | null;
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
  /** Absent on older servers; empty when post-merge verification is disabled. */
  verification_health?: VerificationHealth[];
  verification_audits?: VerificationAuditReport | null;
}

export interface VerificationHealth {
  repo: string;
  project: string;
  observed_head: string;
  verified_head: string | null;
  pending_since: number;
  reduced_count: number;
  checked_at: number;
  status: string;
  message: string;
  recovery_issue: string | null;
}

export interface VerificationAuditRate {
  name: string;
  landed: number;
  audited: number;
  confirmed_escape_lands: number;
  declared_lands: number;
  matching_declarations: number;
  audited_escape_rate: number | null;
}

export interface VerificationAuditJob {
  id: number;
  project: string;
  issue_key: string;
  head: string;
  state: string;
  rules?: string[];
  result: { reviewer: string; summary: string; findings: { priority: number; text: string }[] } | null;
  error: string | null;
}

export interface VerificationAuditReport {
  jobs: VerificationAuditJob[];
  rules: VerificationAuditRate[];
  declarers: VerificationAuditRate[];
  disabled_rules: string[];
  escapes: { audit_id: number; rule: string; evidence: string }[];
  jobs_truncated: boolean;
  rules_truncated: boolean;
  declarers_truncated: boolean;
  disabled_rules_truncated: boolean;
  escapes_truncated: boolean;
  note: string;
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
  /** The issues behind the `stuck` count, which counts every failed gate, stopped land and unfinished review (NIT-356). */
  stuck_issues?: { got: number; keys: string[] };
  throughput: PulseHour[];
  landed_today: number;
  /** Since `since`, by goal (NIT-247). */
  report?: GoalReport;
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
  reviewer_editable: boolean;
  review_enabled: RuleField<boolean>;
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
  /** How it batches reviews and lands; set in the playbook (rules.rs `Batching`). */
  batching?: Batching;
}

export interface ReviewerPreview {
  before: string;
  after: string;
  diff: string;
  destination: string;
}

export interface ReviewerSaved {
  view: RulesView;
  done: string;
}

/** The playbook's `[review] batch` and `[land] batch` settings (docs/batching.md). */
export interface Batching {
  review: "off" | "small" | "all" | "shadow";
  land: "off" | "waiting" | "all";
  land_window_seconds: number;
  land_max: number;
  /** Why changes land alone all the same, when `land` batches but the repo cannot. */
  land_alone: string | null;
}

/** A save from the Rules view; a null default-able value inherits again. */
export interface RulesEdit {
  review_enabled: boolean | null;
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

/** A branch the dry run can try (rules.rs `DryRunBranch`). */
export interface DryRunBranch {
  name: string;
  /** From a task branch name, `claude/nit-12-...`. */
  vendor: string | null;
  key: string | null;
  /** Commits ahead of the integration branch; null when git cannot count them. */
  ahead: number | null;
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

/** One of this machine's addresses, as the Phone screen offers it (crates/nitely-core/src/phone.rs). */
export interface PhoneAddress {
  ip: string;
  interface: string;
  /** null: an ordinary network address, which cannot serve the phone page. */
  kind: "loopback" | "tunnel" | null;
}

export interface PhoneDevice {
  id: string;
  name: string;
  public_key: string;
  paired_at: number;
}

export interface PairRequest {
  id: string;
  name: string;
  public_key: string;
  code: string;
  at: number;
  status: "waiting" | "paired" | "declined";
}

/** Tailscale on this machine (crates/nitely-core/src/phone.rs `TailscaleState`). */
export interface TailscaleState {
  installed: boolean;
  running: boolean;
  backend: string;
  name: string | null;
  ips: string[];
  phones: { name: string; online: boolean }[];
}

export interface PhoneStatus {
  /** The phone page's address on each address the server listens on; empty when it is off. */
  listening: string[];
  /** The phone server keeps this PC from sleeping. */
  awake: boolean;
  /** The addresses the server is bound to. */
  bound: string[];
  addresses: PhoneAddress[];
  devices: PhoneDevice[];
  requests: PairRequest[];
  devices_error: string | null;
  /** The paired phones that asked for notifications, by id. */
  notified: string[];
}

export interface PhoneOffer {
  urls: string[];
  qr_svg: string;
  expires: number;
}

/** A build lane request and what came of it: a `lane_run` row (crates/nitely-core/src/lanes.rs `Run`). */
export interface LaneRun {
  id: number;
  project: string;
  repo: string;
  issue: string | null;
  by: string;
  kind: "run" | "gate" | "land" | string;
  snapshot: string;
  tree: string;
  base: string;
  command: string;
  /** queued | running | done | lane_error | abandoned */
  state: string;
  lane: number | null;
  executor: string;
  queued_at: number;
  started_at: number | null;
  ended_at: number | null;
  exit_code: number | null;
  timed_out: boolean;
  setup_ran: boolean;
  files_switched: number | null;
  log: string | null;
  output: string;
  error: string | null;
  /** Large-repo mode: why it waits for the person's main checkout (a change there, another branch). */
  waiting_for?: string | null;
  /** Why it waits for the machine's memory: "waiting for memory: 6.2 GB free, …" (NIT-266). */
  waiting_memory?: string | null;
  /** The most memory it held at once, in bytes, where the system tells it. */
  peak_memory?: number | null;
  /** What admission set aside for it, its command's expected peak (NIT-363). */
  memory_reserved?: number | null;
  /** The cap it is held to. */
  memory_cap?: number | null;
  /** Why a queued request waits, in words: "waiting: 3 of 3 machine slots in use (…)" (NIT-291). */
  waiting_why?: string | null;
}

/** The machine's memory and how builds share it (machine.rs `Now`, NIT-266); bytes. */
/** When Nitely may empty idle programs' working memory while a run waits for memory (NIT-657),
 * and whether this system can: on macOS and Linux it cannot, and `why` says so. */
export interface MemoryTrimSetting {
  mode: "off" | "away" | "always";
  available: boolean;
  why: string | null;
}

export interface MachineNow {
  memory: { total: number; available: number } | null;
  /** Nothing new starts below `floor` free; one run and all it starts may hold `cap`. `*_from`:
   * `default`, `settings` or the environment variable that set it. */
  limits: { total: number; floor: number; cap: number; floor_from: string; cap_from: string } | null;
  /** The cap one run is held to now: the limits' cap, or less when the machine's slots at it would
   * not fit above the floor. */
  run_cap?: number | null;
  /** Builds, gates, reviews and crew workers run at below-normal priority. */
  below_normal: boolean;
  /** How hard builds and tests may push, from the machine's measured process starts and memory
   * (NIT-519); absent from an older backend. */
  pace?: MachinePace;
}

/** How hard Nitely lets builds and tests push, and why (pace.rs `Pace`). */
export interface MachinePace {
  starts: "normal" | "slow" | "very_slow";
  /** Median milliseconds to start a trivial program and see it end. */
  start_ms: number | null;
  /** Less memory free than the floor and one run's cap: builds start by live use. */
  memory_short: boolean;
  available: number | null;
  /** The most test threads one suite gets, and heavy runs at once; null when not held back. */
  test_threads: number | null;
  suites: number | null;
  prefer_ci: boolean;
  reasons: string[];
}

/** One change of pace, with what was said about it. */
export interface PaceAdjustment {
  wall_ms: number;
  pace: MachinePace;
  said: string[];
}

/** One repo's build lanes, as `nitely lanes status` shows them (lanes.rs `Lanes`). */
export interface ProjectLanes {
  project: string;
  repo: string;
  dir: string;
  /** `large_repo`: the person's main checkout is the one lane; `base`: `tip` or `every <n>h`. */
  rules: { count: number; keep: string[]; timeout_minutes: number; size_cap_gb?: number | null; large_repo?: boolean; base?: string; quick?: boolean; executor?: string };
  /** `size_bytes` and `cap_bytes`: its build directories after its last request, and the cap
   * hygiene keeps them under; absent until it has run one. */
  lanes: { lane: number; quick?: boolean; path: string; made: boolean; busy: boolean; at: string | null; size_bytes?: number | null; cap_bytes?: number | null; kept?: string[] }[];
  /** Waiting and running requests, oldest first. */
  queue: LaneRun[];
  /** The last finished runs, newest first. */
  recent: LaneRun[];
  /** The last finished run of each lane that has one, however long ago (`lanes::overview`). */
  last?: LaneRun[];
}

/** Every repo's lanes (src-tauri `lanes_overview`); `slots` once the machine keeps a build budget. */
export interface LanesOverview {
  projects: ProjectLanes[];
  slots: { used: number; total: number } | null;
  machine?: MachineNow;
  /** Why the first request waiting on the machine's memory waits, when one does. */
  waiting_for_memory?: string | null;
}
