// The browser preview's backend: realistic sample data for every command the app calls, so
// `npm run dev` in a plain browser shows every view. Only loaded when src/backend.ts finds no
// Tauri; never part of a production build. Times are relative to when the page loaded.
//
// Each issue is written as a short story (filed, claim, work, gate, review, changes, pass, ...)
// and its timeline, review records, pull request and worktree are generated from it, so the
// data stays consistent: an issue In Review has passed a gate and a review, a stuck one has a
// failed gate as its last gate event, and so on.

import * as phoneFixture from "./phone";
import * as rulesFixture from "./rules";
import { reviewMode, saveReviewMode, periodicReviews, retryPeriodic, effectiveModes, landsPipeline, landsDryRun } from "./reviewModes";
import type { ModeOptions, ReviewMode } from "../reviewModes";
import { performanceFixture, configurePerformance, clearPerformance, paceFixture } from "./performance";
import { leaderboardBoardFixture, leaderboardDeleteFixture, leaderboardSendFixture, leaderboardFixture, leaderboardJoinFixture, leaderboardReportFixture, leaderboardStatusFixture } from "./leaderboard";
import type {
  LaneRun,
  LanesOverview,
  BatchesOverview,
  Waits,
  IssueBatches,
  LandRun,
  ReviewBatch,
  ProjectLanes,
  Held,
  CrewEdit,
  CrewSaved,
  CrewStatus,
  CrewView,
  CrewWorker,
  Activity,
  ActivityLane,
  ActivityMark,
  ActivitySegment,
  AgentPlan,
  LinkedIssue,
  Applied,
  Ask,
  Board,
  Notice,
  AgentRow,
  BranchRow,
  BranchState,
  ConnectPlan,
  FlowIssue,
  Happening,
  IssueDetail,
  IssueRecord,
  IssueSpend,
  ProjectSpend,
  IssueMirror,
  MirrorPreview,
  PrRecord,
  PlaybookProposal,
  Project,
  ProjectFocus,
  Goal,
  GoalDetail,
  GoalFinish,
  GoalReport,
  GoalRisk,
  ReportNeeded,
  GoalPlanView,
  ProjectBrief,
  Pulse,
  VerificationHealth,
  VerificationAuditReport,
  RepoView,
  ReviewRecord,
  Session,
  SessionState,
  TaskRow,
  TimelineEntry,
} from "../types";
import { computeTrack } from "../ui/lifecycle";
import { isOwnWork } from "../decisions";

const NOW = Math.floor(Date.now() / 1000);
const MIN = 60;
const HOUR = 3600;
const iso = (ts: number) => new Date(ts * 1000).toISOString().replace(/\.\d+Z$/, "Z");

// A small seeded generator, so the preview looks the same on every load.
let seed = 24;
function rand() {
  seed = (seed * 1664525 + 1013904223) % 4294967296;
  return seed / 4294967296;
}

type Vendor = "claude" | "codex" | "cursor";

interface RepoSpec {
  project: string;
  name: string;
  root: string;
  integration: string;
  provider: "github" | "bitbucket";
  slug: string;
}

const REPOS: RepoSpec[] = [
  { project: "NIT", name: "Nitely", root: "C:\\Code\\Nitely", integration: "main", provider: "github", slug: "frontiermaxxing/Nitely" },
  { project: "ORB", name: "Orbit", root: "C:\\Code\\Orbit", integration: "develop", provider: "bitbucket", slug: "orbit-team/orbit" },
  { project: "HAR", name: "Harbor", root: "C:\\Code\\Harbor", integration: "main", provider: "github", slug: "harbor-app/harbor" },
];

// OLD has no issues, as a project left behind after a move, so removing and re-keying can be tried.
const PROJECT_NAMES: Record<string, string> = { NIT: "Nitely", ORB: "Orbit", HAR: "Harbor", OLD: "Orbit (old)" };

type Step =
  | "filed"
  | "claim"
  | "work"
  | "note"
  | "gate"
  | "gatefail"
  | "review"
  | "reviewfail"
  | "changes"
  | "pass"
  | "passhuman"
  | "disabledhuman"
  | "approve"
  | "land"
  | "landfail"
  | "ask"
  | "answer"
  | "cancel"
  | "dup";

interface Live {
  state: SessionState;
  /** Minutes since the session was last seen. */
  seen: number;
  activity: string;
}

interface IssueSpec {
  key: string;
  title: string;
  kind: string;
  priority: number;
  vendor?: Vendor;
  source?: string;
  /** Hours ago the issue was filed, and hours ago its last step happened. */
  from: number;
  to: number;
  story: Step[];
  /** Progress lines the agent reported while working. */
  notes?: string[];
  ask?: { question: string; options: string[]; finding?: { priority: number; text: string } };
  /** More questions on the same issue, asked after `ask`, still open (NIT-172). */
  moreAsks?: string[];
  /** Further requests to accept a finding, made after the first (the Inbox puts them on one card, NIT-571). */
  moreFindings?: { question: string; finding: { priority: number; text: string } }[];
  /** The agent session in the issue's worktree, if one is open. */
  live?: Live;
  branch?: Partial<BranchState>;
  checks?: PrRecord["checks"];
  body?: string;
  /** Reviewed by the author's own vendor, because the other one is not installed. */
  sameVendor?: boolean;
  /** Work only the human can do: filed into Needs You, and never handed to an agent. */
  owner?: "human";
  /** Its session was started by the project's crew (docs/crew.md). */
  crew?: boolean;
}

/** Areas and platforms on some issues, as agents label them. */
const LABELS: Record<string, string[]> = {
  "NIT-31": ["pulse", "home"],
  "NIT-30": ["review"],
  "HAR-14": ["booking", "mobile"],
  "HAR-16": ["mobile", "a11y"],
  "HAR-17": ["payments"],
  "HAR-8": ["checkout", "safari", "payments"],
  "HAR-15": ["api"],
};

const WORK = ["Reading the code around the change", "Writing the change", "Adding tests", "Fixing a failing test", "Tidying the diff before the gate"];

const ISSUES: IssueSpec[] = [
  // ----- Nitely -----
  {
    key: "NIT-31", title: "Pulse home: one glance at what needs you, what moves, what is stuck", kind: "feature", priority: 1, vendor: "claude",
    from: 20, to: 0.05, story: ["filed", "claim", "work", "work", "work", "work"],
    notes: ["Laying out the three bands: needs you, moving, stuck", "Wiring the bands to the snapshot", "Editing src/views/Pulse.tsx"],
    live: { state: "working", seen: 0.3, activity: "Editing src/views/Pulse.tsx" }, branch: { ahead: 4, dirty_files: 3 },
    body: "The first screen answers three questions before any detail: does anything need me, what is moving, what is stuck.",
  },
  {
    key: "NIT-30", title: "Show review rounds and findings on the issue track", kind: "feature", priority: 2, vendor: "codex", crew: true,
    from: 30, to: 0.2, story: ["filed", "claim", "work", "work", "gate", "review", "changes", "work", "work", "gate", "review"],
    notes: ["Mapping review records onto the track", "Fixing the P1 from review: the loop drew under the labels"],
    live: { state: "idle", seen: 6, activity: "Waiting for the review by claude" }, branch: { ahead: 6, dirty_files: 0 }, checks: "pending",
  },
  {
    key: "NIT-29", title: "Activity lanes per agent over the last 24 hours", kind: "feature", priority: 2, vendor: "claude",
    from: 46, to: 1.5, story: ["filed", "claim", "work", "work", "gate", "review", "changes", "work", "gate", "review", "changes", "work", "gate", "review", "pass"],
    notes: ["One lane per session, events as ticks", "Bucketing events per 15 minutes", "Fixing review: lanes overlapped at narrow widths"],
    live: { state: "idle", seen: 12, activity: "Submitted; review passed" }, branch: { ahead: 9, dirty_files: 0 }, checks: "success",
  },
  { key: "NIT-28", title: "Board and branch map for a project", kind: "feature", priority: 2, from: 22, to: 22, story: ["filed"] },
  {
    key: "NIT-27", title: "Land queue shows who is landing and what waits", kind: "feature", priority: 2, vendor: "claude",
    from: 70, to: 3, story: ["filed", "claim", "work", "gate", "review", "passhuman", "approve"],
    notes: ["Reading land.rs to see where the lock is held"], branch: { ahead: 5, dirty_files: 0 }, checks: "success", sameVendor: true,
  },
  {
    key: "NIT-33", title: "Decide where review findings below P1 go", kind: "improvement", priority: 3, vendor: "claude",
    from: 9, to: 0.7, story: ["filed", "claim", "work", "gate", "review", "passhuman", "ask"],
    notes: ["Listing where P2 and P3 findings end up today"],
    ask: { question: "Should P2 and P3 review findings become issues automatically, or stay on the pull request?", options: ["File them as issues", "Leave them on the PR"] },
    moreAsks: ["Should a P1 finding still stop the land after you approved?\nToday it does; the reviewer may be wrong about a file you know."],
    live: { state: "idle", seen: 48, activity: "Asked: where should P2 findings go?" }, branch: { ahead: 2, dirty_files: 0 }, checks: "success",
  },
  {
    key: "NIT-44", title: "Tray count of what needs you", kind: "feature", priority: 2, vendor: "claude",
    from: 50, to: 0.8, story: ["filed", "claim", "work", "gate", "review", "pass", "landfail"],
    notes: ["Counting asks, Needs You and stuck worktrees for the tray"],
    live: { state: "waiting", seen: 9, activity: "Land stopped on a conflict in src/App.tsx" },
    branch: { ahead: 3, behind: 5, dirty_files: 1, rebase_in_progress: true, rebase_started_at: NOW - 9 * MIN }, checks: "success",
  },
  {
    key: "NIT-45", title: "Keyboard shortcuts to move an issue between states", kind: "improvement", priority: 3, vendor: "cursor",
    from: 7, to: 0.1, story: ["filed", "claim", "work", "work"],
    notes: ["Mapping keys to states", "Editing src/IssuePanel.tsx"],
    live: { state: "working", seen: 1, activity: "Editing src/IssuePanel.tsx" }, branch: { ahead: 1, dirty_files: 5 },
  },
  {
    key: "NIT-46", title: "Export a project's backlog as JSON", kind: "feature", priority: 4, vendor: "codex",
    from: 30, to: 4, story: ["filed", "claim", "work", "gatefail"],
    notes: ["Writing the export command"],
    live: { state: "idle", seen: 240, activity: "Gate failed: npm run build" }, branch: { ahead: 2, behind: 3, dirty_files: 0 },
  },
  { key: "NIT-26", title: "Inbox groups asks by project", kind: "improvement", priority: 3, from: 26, to: 26, story: ["filed"] },
  { key: "NIT-25", title: "Command palette with instant search", kind: "feature", priority: 2, from: 120, to: 120, story: ["filed"] },
  {
    key: "NIT-24", title: "Visual foundation: design language, shared primitives, browser preview", kind: "feature", priority: 1, vendor: "claude",
    from: 60, to: 5, story: ["filed", "claim", "work", "work", "gate", "review", "changes", "work", "gate", "review", "pass", "land"],
  },
  {
    key: "NIT-23", title: "Push updates from the event store instead of polling", kind: "improvement", priority: 2, vendor: "codex",
    from: 140, to: 30, story: ["filed", "claim", "work", "gate", "review", "pass", "land"],
  },
  {
    key: "NIT-22", title: "Codex hooks miss the session end event", kind: "bug", priority: 2, vendor: "claude",
    from: 160, to: 52, story: ["filed", "claim", "work", "gatefail", "work", "gate", "review", "pass", "land"],
    // Landed, but its worktree was never cleaned up: sprawl for the branch map.
    branch: { ahead: 0, behind: 14, dirty_files: 0 },
  },
  {
    key: "NIT-21", title: "Gate output is cut at 4 KB in the timeline", kind: "bug", priority: 3, vendor: "codex",
    from: 34, to: 2.5, story: ["filed", "claim", "work", "gate", "review", "pass"], branch: { ahead: 2, dirty_files: 0 }, checks: "failure",
  },
  // Batched review and the land queue (docs/batching.md): NIT-50 and NIT-51 are in a review batch
  // with NIT-30; NIT-29, NIT-52 and NIT-54 land together while bisection runs; NIT-53 waits behind.
  {
    key: "NIT-50", title: "Say which review batch a change is in", kind: "improvement", priority: 3, vendor: "codex",
    from: 6, to: 0.25, story: ["filed", "claim", "work", "gate", "review"],
    live: { state: "idle", seen: 5, activity: "Submitted; in a review batch" }, branch: { ahead: 1, dirty_files: 0 }, checks: "pending",
  },
  {
    key: "NIT-51", title: "Typo in the help for nitely run", kind: "bug", priority: 4, vendor: "codex",
    from: 3, to: 0.2, story: ["filed", "claim", "work", "gate", "review"], branch: { ahead: 1, dirty_files: 0 },
  },
  {
    key: "NIT-52", title: "Keep the lanes strip on one line at narrow widths", kind: "bug", priority: 3, vendor: "claude",
    from: 9, to: 0.4, story: ["filed", "claim", "work", "gate", "review", "pass"],
    live: { state: "idle", seen: 4, activity: "Landing in a queue run" }, branch: { ahead: 2, dirty_files: 0 }, checks: "success",
  },
  {
    key: "NIT-53", title: "Name the failing gate step in a land's error", kind: "improvement", priority: 3, vendor: "cursor",
    from: 8, to: 0.3, story: ["filed", "claim", "work", "gate", "review", "pass"], branch: { ahead: 1, dirty_files: 0 }, checks: "success",
  },
  {
    key: "NIT-54", title: "Drop the unused sync retry setting", kind: "improvement", priority: 4, vendor: "codex",
    from: 12, to: 0.5, story: ["filed", "claim", "work", "gate", "review", "pass"], branch: { ahead: 1, dirty_files: 0 }, checks: "success",
  },
  { key: "NIT-20", title: "Approvals signed with a key in the OS keychain", kind: "feature", priority: 1, from: 200, to: 200, story: ["filed"] },
  { key: "NIT-19", title: "Retry a failed review once before failing submit", kind: "improvement", priority: 3, from: 180, to: 90, story: ["filed", "cancel"] },
  { key: "NIT-18", title: "Playbook editor with a dry run", kind: "feature", priority: 3, from: 230, to: 230, story: ["filed"] },
  { key: "NIT-17", title: "Linear import keeps duplicate labels", kind: "bug", priority: 4, from: 250, to: 240, story: ["filed", "dup"] },

  // ----- Orbit -----
  {
    key: "ORB-275", title: "Tab strip flickers when a profile switches", kind: "bug", priority: 1, vendor: "claude",
    from: 14, to: 0.15, story: ["filed", "claim", "work", "work", "gate", "work"],
    notes: ["Reproduced: the strip repaints twice on profile switch", "Rebasing on develop before submit"],
    live: { state: "idle", seen: 8, activity: "Rebasing on develop before submit" },
    branch: { ahead: 3, behind: 7, dirty_files: 2, rebase_in_progress: true, rebase_started_at: NOW - 48 * MIN },
  },
  {
    key: "ORB-271", title: "Sync settings across profiles", kind: "feature", priority: 2, vendor: "codex",
    from: 28, to: 0.1, story: ["filed", "claim", "work", "work", "ask", "work"],
    notes: ["Reading the sync service", "Handling the locked-profile case"],
    ask: { question: "The sync API returns 409 for a locked profile. Retry quietly, or show the person a banner?", options: [] },
    live: { state: "working", seen: 0.7, activity: "Running unit_tests --gtest_filter=Sync*" }, branch: { ahead: 5, dirty_files: 6 },
  },
  {
    key: "ORB-268", title: "Crash on startup with an empty bookmarks file", kind: "bug", priority: 1, vendor: "codex",
    from: 40, to: 4, story: ["filed", "claim", "work", "gate", "review", "passhuman"],
    branch: { ahead: 1, dirty_files: 0 }, checks: "success",
    body: "A zero-byte Bookmarks file makes the parser return null and startup dereferences it.",
  },
  {
    key: "ORB-266", title: "Download shelf ignores the new theme", kind: "bug", priority: 3, vendor: "codex",
    from: 50, to: 0.9, story: ["filed", "claim", "work", "work", "gatefail"],
    notes: ["Theme colors now come from the provider", "Gate failed in browser_tests, looking"],
    live: { state: "idle", seen: 52, activity: "Gate failed: browser_tests (3 failures)" }, branch: { ahead: 2, dirty_files: 1 },
  },
  { key: "ORB-270", title: "Reader mode for long articles", kind: "feature", priority: 3, from: 70, to: 70, story: ["filed"] },
  { key: "ORB-272", title: "Telemetry opt-out is not saved", kind: "bug", priority: 2, source: "human", from: 18, to: 18, story: ["filed"] },
  { key: "ORB-273", title: "Extensions page loads slowly with 40 or more extensions", kind: "improvement", priority: 3, from: 44, to: 44, story: ["filed"] },
  { key: "ORB-274", title: "Prefetch on hover for the new tab page", kind: "idea", priority: 4, from: 90, to: 90, story: ["filed"] },
  {
    key: "ORB-265", title: "Pinned tabs lose their favicons after restart", kind: "bug", priority: 2, vendor: "claude",
    from: 80, to: 1.2, story: ["filed", "claim", "work", "gate", "review", "passhuman", "approve", "landfail"],
    branch: { ahead: 2, behind: 4, dirty_files: 0 }, checks: "success",
  },
  {
    key: "ORB-260", title: "Live check: sign-in on a clean profile", kind: "live-check", priority: 2, vendor: "claude",
    from: 36, to: 2, story: ["filed", "claim", "work", "gate", "review", "pass"],
    live: { state: "idle", seen: 25, activity: "Review passed; waiting to land" }, branch: { ahead: 1, dirty_files: 0 }, checks: "success",
  },
  {
    // Five review rounds, four of them asking for changes: the issue track must read as a loop.
    key: "ORB-258", title: "Password autofill inside cross-origin iframes", kind: "feature", priority: 2, vendor: "codex",
    from: 96, to: 2.2,
    story: [
      "filed", "claim", "work", "gate", "review", "changes", "work", "gatefail", "work", "gate", "review", "changes",
      "work", "gate", "review", "changes", "work", "gate", "review", "changes", "work", "gate", "review", "passhuman",
    ],
    notes: ["Frame origin checks in the autofill agent", "Fixing review: the origin check ran after the fill", "Gate failed in components_unittests", "Fixing review: carried P1 on sandboxed frames", "Fixing review: test names", "Fixing review: last nit"],
    live: { state: "idle", seen: 130, activity: "Submitted; waits for a human" }, branch: { ahead: 11, dirty_files: 0 }, checks: "success",
  },
  { key: "ORB-263", title: "Update the Windows installer icon", kind: "improvement", priority: 4, vendor: "codex", from: 150, to: 100, story: ["filed", "claim", "work", "gate", "review", "pass", "land"] },
  { key: "ORB-262", title: "Memory saver for background tabs", kind: "feature", priority: 2, vendor: "claude", from: 300, to: 20, story: ["filed", "claim", "work", "gate", "review", "changes", "work", "gate", "review", "passhuman", "approve", "land"] },
  { key: "ORB-261", title: "Spellcheck underlines in the omnibox", kind: "bug", priority: 3, vendor: "codex", from: 110, to: 60, story: ["filed", "claim", "work", "gate", "review", "pass", "land"] },
  { key: "ORB-259", title: "Drag a tab into a new window on a second monitor", kind: "bug", priority: 3, vendor: "claude", from: 260, to: 150, story: ["filed", "claim", "work", "gatefail", "work", "gate", "review", "pass", "land"] },

  // ----- Harbor -----
  {
    key: "HAR-19", title: "Keep each booking site's settings apart", kind: "bug", priority: 1, vendor: "codex",
    from: 20, to: 0.6, story: ["filed", "claim", "work", "gate", "review", "changes", "work", "gate", "review", "changes", "ask"],
    ask: {
      question: "You decided on Tuesday not to migrate old site settings: there are no outside users yet. The reviewer keeps raising it as P1; fixing it means copying settings between sites, which is what this change stops.",
      options: [],
      finding: { priority: 1, text: "No migration for existing sites: src/settings.ts, load: a site set up before this starts with empty settings" },
    },
    moreFindings: [{
      question: "The old behaviour shared one cache across sites on purpose, to keep the first load fast; splitting it is a separate change.",
      finding: { priority: 1, text: "Shared cache still mixes sites: src/cache.ts, get: a site reads another site's cached settings until the cache expires" },
    }],
    live: { state: "waiting", seen: 0.6, activity: "Asked to accept a finding" }, branch: { ahead: 4, dirty_files: 0 },
  },
  {
    key: "HAR-14", title: "Booking form loses the date on back navigation", kind: "bug", priority: 2, vendor: "cursor",
    from: 16, to: 0.06, story: ["filed", "claim", "work", "work"],
    notes: ["The date lives in component state; moving it to the URL", "Needs the staging migration to test"],
    live: { state: "waiting", seen: 42, activity: "Waiting for input: run the migration against staging?" }, branch: { ahead: 2, dirty_files: 4 },
  },
  {
    key: "HAR-13", title: "Show availability for the next 14 days", kind: "feature", priority: 2, vendor: "cursor",
    from: 26, to: 0.4, story: ["filed", "claim", "work", "work", "gate", "review", "pass"],
    notes: ["Availability grid", "Writing tests for the availability window"],
    live: { state: "idle", seen: 18, activity: "Review passed; waiting to land" }, branch: { ahead: 7, dirty_files: 0 }, checks: "pending",
  },
  {
    key: "HAR-15", title: "Playbook change for harbor: the gate runs no tests", kind: "improvement", priority: 2, vendor: "claude",
    from: 1.6, to: 1.6, story: ["filed"], owner: "human",
    body: "claude proposes a change to the playbook Nitely keeps for harbor. Approving applies it; canceling it with a reason rejects it.",
  },
  { key: "HAR-12", title: "Email receipts in the guest's language", kind: "feature", priority: 3, from: 60, to: 60, story: ["filed"] },
  { key: "HAR-11", title: "Move image uploads to signed URLs", kind: "improvement", priority: 2, vendor: "cursor", from: 100, to: 3.5, story: ["filed", "claim", "work", "gate", "review", "pass", "land"] },
  { key: "HAR-10", title: "Calendar sync with Google", kind: "feature", priority: 3, from: 300, to: 300, story: ["filed"] },
  { key: "HAR-15", title: "Rate limit the public API", kind: "improvement", priority: 2, from: 12, to: 12, story: ["filed"] },
  { key: "HAR-16", title: "Mobile menu traps focus", kind: "bug", priority: 2, source: "human", from: 5, to: 5, story: ["filed"] },
  {
    key: "HAR-17", title: "Renew the payment provider's live API key", kind: "live-check", priority: 2, vendor: "claude", from: 4, to: 4, story: ["filed"], owner: "human",
    body: "The provider's dashboard shows the live key expires on the 30th. Rotate it there and paste the new one into the deploy secrets; no agent can sign in to that dashboard.\n\nFound while checking why the staging checkout warned about the key.",
  },
  {
    key: "HAR-18", title: "Plan: Guests can change a booking themselves", kind: "feature", priority: 2, vendor: "claude", from: 1.2, to: 1.2, story: ["filed"], owner: "human",
    body: "claude drafted this goal from what you asked for: \"let guests change their own bookings\". Approve to create the goal and file its issues; cancel it with a reason to file nothing.",
  },
  { key: "HAR-9", title: "Price shows twice on small screens", kind: "bug", priority: 3, vendor: "cursor", from: 130, to: 75, story: ["filed", "claim", "work", "gate", "review", "pass", "land"] },
  {
    key: "HAR-8", title: "Live check: checkout on Safari", kind: "live-check", priority: 3, vendor: "cursor",
    from: 20, to: 1, story: ["filed", "claim", "work", "gate", "disabledhuman"], branch: { ahead: 1, dirty_files: 0 }, checks: "success",
    body: "Pay with a test card on Safari 18 on macOS and iOS; confirm the receipt arrives.",
  },
  { key: "HAR-7", title: "Seasonal pricing rules", kind: "feature", priority: 3, vendor: "cursor", from: 330, to: 140, story: ["filed", "claim", "work", "gate", "review", "changes", "work", "gate", "review", "pass", "land"] },
];

// ----- Sprawl: worktrees and branches no story accounts for -----

/** A worktree outside the stories: a spike, a bisect, a person's own experiment. */
interface LooseWorktree {
  project: string;
  /** Folder name under the repo's worktrees folder. */
  folder: string;
  /** Null for a detached HEAD. */
  branch: string | null;
  ahead: number;
  behind: number;
  dirty: number;
  /** Hours since its last commit. */
  idle: number;
  subject: string;
}

const LOOSE_WORKTREES: LooseWorktree[] = [
  { project: "NIT", folder: "spike-sqlite-wal", branch: "claude/spike-sqlite-wal", ahead: 3, behind: 22, dirty: 1, idle: 6 * 24, subject: "Try WAL checkpoints on every write" },
  { project: "NIT", folder: "bisect-snapshot-slowdown", branch: null, ahead: 0, behind: 31, dirty: 0, idle: 9 * 24, subject: "Read worktrees in parallel" },
  { project: "ORB", folder: "profile-perf-trace", branch: "sam/profile-perf-trace", ahead: 1, behind: 45, dirty: 5, idle: 4 * 24, subject: "Trace profile switch" },
];

/** A branch with no worktree: left after a land, parked, or made by hand. */
interface LooseBranch {
  project: string;
  name: string;
  ahead: number;
  behind: number;
  idle: number;
  subject: string;
}

const LOOSE_BRANCHES: LooseBranch[] = [
  { project: "NIT", name: "codex/nit-19-retry-a-failed-review-once", ahead: 2, behind: 38, idle: 90, subject: "Retry the review once on a timeout" },
  { project: "NIT", name: "claude/nit-23-push-updates-from-the-event", ahead: 0, behind: 12, idle: 30, subject: "Push updates from the event store" },
  { project: "NIT", name: "sam/tauri-2-upgrade", ahead: 5, behind: 61, idle: 12 * 24, subject: "Bump tauri to 2.2" },
  { project: "ORB", name: "claude/orb-262-memory-saver-for-background-tabs", ahead: 0, behind: 9, idle: 20, subject: "Discard background tabs after 10 minutes" },
  { project: "ORB", name: "codex/orb-251-crash-in-the-pdf-viewer", ahead: 4, behind: 210, idle: 21 * 24, subject: "Guard the PDF viewer against a null frame" },
  { project: "HAR", name: "cursor/har-11-move-image-uploads-to-signed", ahead: 0, behind: 6, idle: 3.5, subject: "Move image uploads to signed URLs" },
];

// ----- Generating the data from the stories -----

const STATE_TYPE: Record<string, string> = {
  Backlog: "backlog",
  Todo: "unstarted",
  "In Progress": "started",
  "In Review": "started",
  "Needs You": "started",
  Approved: "started",
  Done: "completed",
  Canceled: "canceled",
  Duplicate: "canceled",
};

const FINDINGS = [
  { priority: 1, text: "The cleanup in useEffect runs after the panel unmounts, so the listener leaks when it closes mid-load." },
  { priority: 1, text: "The rebase check reads the wrong worktree when two share a branch name prefix." },
  { priority: 2, text: "The sparkline divides by zero for an all-zero series." },
  { priority: 2, text: "Narrow widths wrap the station labels onto three lines." },
  { priority: 3, text: "The helper name says lanes but returns buckets." },
  { priority: 3, text: "Two tests share a fixture file and can race." },
];

interface World {
  /** What the person is told and nothing waits on (NIT-570). */
  notices: Notice[];
  issues: IssueRecord[];
  timelines: Map<string, TimelineEntry[]>;
  reviews: Map<string, ReviewRecord[]>;
  prs: Map<string, PrRecord[]>;
  asks: Ask[];
  projects: Project[];
  repos: RepoSpec[];
}

function slugOf(title: string) {
  return title.toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-|-$/g, "").split("-").slice(0, 5).join("-");
}

function repoOf(key: string): RepoSpec {
  return REPOS.find((r) => key.startsWith(`${r.project}-`))!;
}

function worktreeOf(spec: IssueSpec): string {
  const repo = repoOf(spec.key);
  return `C:\\Code\\${repo.name}-worktrees\\${spec.key.toLowerCase()}-${slugOf(spec.title)}`;
}

function branchOf(spec: IssueSpec): string {
  return `${spec.vendor ?? "claude"}/${spec.key.toLowerCase()}-${slugOf(spec.title)}`;
}

function prUrl(spec: IssueSpec, number: number) {
  const repo = repoOf(spec.key);
  return repo.provider === "github" ? `https://github.com/${repo.slug}/pull/${number}` : `https://bitbucket.org/${repo.slug}/pull-requests/${number}`;
}

function sha() {
  return Math.floor(rand() * 0xfffffff).toString(16).padStart(7, "0");
}

let askId = 1;
let prNumber = 40;

/** A session id for a fixture agent, made from its issue's key so it is the same on every load. */
function fakeSession(seed: string): string {
  let h = 2166136261;
  const hex = Array.from({ length: 32 }, (_, i) => {
    h = Math.imul(h ^ (seed.charCodeAt(i % seed.length) + i), 16777619);
    return ((h >>> 0) % 16).toString(16);
  }).join("");
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}

function build(): World {
  const world: World = {
    // A goal item whose review rounds stalled under `stalled_reviews = "fix"` (NIT-570).
    notices: [{ id: 9001, issue_key: "ORB-261", ts: NOW - 25 * 60, summary: "ORB-261: the review rounds are not converging (5 review rounds in a row asked for changes; 3 open findings (1 P0, 2 P1)). The agreed part lands; the open findings move to a follow-up issue in the same goal. Nothing waits on you." }],
    issues: [], timelines: new Map(), reviews: new Map(), prs: new Map(), asks: [], projects: [], repos: REPOS };
  for (const spec of ISSUES) {
    const repo = repoOf(spec.key);
    const vendor = spec.vendor ?? "claude";
    const other = vendor === "claude" ? "codex" : "claude";
    const reviewer = spec.sameVendor ? vendor : other;
    const reviewNote = spec.sameVendor ? `same vendor: ${other} not installed` : "";
    // Agents act as their session (NIT-194); every third issue's events are from before that, under
    // the agent's folder.
    const folder = `C:\\Code\\${repo.name}-worktrees\\${spec.key.toLowerCase()}`;
    const oldStyle = Number(spec.key.split("-")[1]) % 3 === 0;
    const agent = oldStyle ? `${vendor}:c:/code/${repo.name.toLowerCase()}-worktrees/${spec.key.toLowerCase()}` : `${vendor}:${fakeSession(spec.key)}`;
    const filer = spec.source === "human" ? "human" : `${spec.vendor ?? (rand() > 0.5 ? "claude" : "codex")}:c:/code/${repo.name.toLowerCase()}`;
    const start = NOW - spec.from * HOUR;
    const end = NOW - spec.to * HOUR;
    const steps = spec.story.length;
    const timeline: TimelineEntry[] = [];
    const reviews: ReviewRecord[] = [];
    const number = spec.story.some((s) => s === "changes" || s === "pass" || s === "passhuman" || s === "disabledhuman") ? prNumber++ : 0;
    const url = number ? prUrl(spec, number) : "";
    let state = spec.owner === "human" ? "Needs You" : spec.story.length === 1 && spec.from > 80 ? "Backlog" : "Todo";
    let notes = 0;
    let lastAsk: Ask | null = null;
    let claimed: string | null = null;
    const move = (ts: number, to: string, actor = agent) => {
      timeline.push({ ts, actor, kind: "issue_moved", summary: `${state} → ${to}` });
      state = to;
    };
    const review = (ts: number, verdict: "pass" | "changes" | "disabled", approver: "agent" | "human") => {
      const findings: ReviewRecord["findings"] =
        verdict === "changes"
          ? [FINDINGS[Math.floor(rand() * 2)], FINDINGS[2 + Math.floor(rand() * 2)]]
          : verdict !== "disabled" && rand() > 0.5
            ? [FINDINGS[4 + Math.floor(rand() * 2)]]
            : [];
      // A second round of changes carries a finding the first round raised and was not fixed.
      const before = reviews.find((r) => r.verdict === "changes");
      if (verdict === "changes" && before) findings.unshift({ ...before.findings[before.findings.length - 1], carried: true, now: "src/sync.rs:88 `retry(0)` still gives up after the first failure" });
      const reviewedCommit = sha();
      reviews.unshift({
        issue_key: spec.key,
        patch_id: sha(),
        commit_sha: reviewedCommit,
        branch: branchOf(spec),
        reviewer: verdict === "disabled" ? "" : reviewer,
        verdict,
        approver,
        reasons: approver === "human" ? [spec.kind === "live-check" ? "a live check needs a person" : "touches a path the playbook keeps for a human"] : [],
        findings,
        ts,
        note: verdict === "disabled" ? "Review disabled for this project" : reviewNote,
        ...(approver === "human" && spec.key === "ORB-268" ? {
          solution: { title: "Start normally with an empty bookmarks file", summary: "Treat a zero-byte bookmarks file as an empty collection before initializing the window. Existing bookmarks and malformed-file error reporting are preserved." },
          gate: { commit: reviewedCommit, passed: true, results: [{ command: "cargo test -p nitely-core", ok: true }, { command: "npm test", ok: true }] },
        } : {}),
      });
    };

    spec.story.forEach((step, n) => {
      // Spread the steps between filing and the last step, a little unevenly.
      const f = steps === 1 ? 0 : n / (steps - 1);
      const ts = Math.round(start + (end - start) * Math.min(1, f + (n > 0 && n < steps - 1 ? (rand() - 0.5) * 0.06 : 0)));
      switch (step) {
        case "filed":
          timeline.push({ ts, actor: filer, kind: "issue_created", summary: `Filed as ${state}${spec.owner === "human" ? ", the human's own work" : ""}` });
          break;
        case "claim":
          claimed = agent;
          timeline.push({ ts, actor: agent, kind: "claimed", summary: "Took the issue" });
          move(ts, "In Progress");
          timeline.push({ ts: ts + 4, actor: agent, kind: "worktree", summary: `Started ${branchOf(spec)} from ${repo.integration}` });
          if (spec.crew)
            timeline.push({
              ts: ts + 6, actor: "crew:NIT", kind: "crew_started",
              summary: `Crew worker started (${spec.vendor}), log: C:\\Users\\you\\AppData\\Roaming\\Nitely\\crew\\NIT\\${spec.key.toLowerCase()}-${spec.vendor}.log`,
            });
          break;
        case "work":
          timeline.push({ ts, actor: agent, kind: "status", summary: spec.notes?.[notes] ?? WORK[notes % WORK.length] });
          notes++;
          break;
        case "note":
          timeline.push({ ts, actor: agent, kind: "handoff", summary: "Done: the change and its tests. Left: the narrow layout. Unsure: whether the empty state needs an action." });
          break;
        case "gate":
          timeline.push({ ts, actor: agent, kind: "gate_passed", summary: "Gate passed (3 commands)" });
          break;
        case "gatefail":
          timeline.push({ ts, actor: agent, kind: "gate_failed", summary: repo.project === "ORB" ? "Gate failed at `autoninja -C out/Default browser_tests`: 3 failures" : "Gate failed at `npm run build`: 2 type errors" });
          break;
        case "review":
          timeline.push({ ts, actor: agent, kind: "review_started", summary: `Review by ${reviewer}${reviewNote ? ` (${reviewNote})` : ""} started` });
          break;
        case "reviewfail":
          timeline.push({ ts, actor: agent, kind: "review_failed", summary: `Review by ${reviewer} did not finish` });
          break;
        case "changes":
          review(ts, "changes", "agent");
          timeline.push({ ts, actor: agent, kind: "submit", summary: `Review by ${reviewer} requested changes. Pull request: ${url}` });
          break;
        case "pass":
          review(ts, "pass", "agent");
          timeline.push({ ts, actor: agent, kind: "submit", summary: `Review by ${reviewer} passed; ready to land. Pull request: ${url}` });
          move(ts + 2, "In Review");
          break;
        case "passhuman":
          review(ts, "pass", "human");
          timeline.push({ ts, actor: agent, kind: "submit", summary: `Review by ${reviewer} passed; waits for a human. Pull request: ${url}` });
          move(ts + 2, "Needs You");
          break;
        case "disabledhuman":
          review(ts, "disabled", "human");
          timeline.push({ ts, actor: agent, kind: "submit", summary: `Review disabled for this project; waits for a human. Pull request: ${url}` });
          move(ts + 2, "Needs You");
          break;
        case "approve":
          move(ts, "Approved", "human");
          break;
        case "landfail":
          timeline.push({ ts, actor: agent, kind: "land_failed", summary: `Land stopped: rebase conflict in chrome/browser/ui/tabs/pinned_tab_codec.cc` });
          break;
        case "land":
          timeline.push({ ts, actor: agent, kind: "landed", summary: `Landed ${branchOf(spec)} on ${repo.integration} at ${sha()}` });
          move(ts + 2, "Done");
          claimed = null;
          break;
        case "ask":
          lastAsk = { id: askId++, issue_key: spec.key, asked_by: agent, question: spec.ask!.question, options: spec.ask!.options, answer: null, asked_at: ts, answered_at: null, finding: spec.ask!.finding ?? null };
          timeline.push({ ts, actor: agent, kind: "ask", summary: spec.ask!.question });
          break;
        case "answer":
          timeline.push({ ts, actor: "human", kind: "answer", summary: spec.ask?.options[0] ?? "Yes" });
          lastAsk = null;
          break;
        case "cancel":
          timeline.push({ ts, actor: "human", kind: "issue_moved", summary: `${state} → Canceled` });
          state = "Canceled";
          break;
        case "dup":
          timeline.push({ ts, actor: "human", kind: "issue_moved", summary: `${state} → Duplicate` });
          state = "Duplicate";
          break;
      }
    });

    // Set inside the story's callback, which TypeScript does not follow.
    const open = lastAsk as Ask | null;
    if (open) {
      world.asks.push(open);
      for (const [n, question] of (spec.moreAsks ?? []).entries()) {
        const at = open.asked_at + 60 * (n + 1);
        world.asks.push({ ...open, id: askId++, question, options: [], asked_at: at });
        timeline.push({ ts: at, actor: open.asked_by, kind: "ask", summary: question });
      }
      for (const [n, more] of (spec.moreFindings ?? []).entries()) {
        const at = open.asked_at + 90 * (n + 1);
        world.asks.push({ ...open, id: askId++, question: more.question, options: [], asked_at: at, finding: more.finding });
        timeline.push({ ts: at, actor: open.asked_by, kind: "ask", summary: more.question });
      }
    }
    timeline.sort((a, b) => a.ts - b.ts);
    const updated = timeline[timeline.length - 1].ts;
    world.issues.push({
      key: spec.key,
      origin: "nitely",
      project: repo.project,
      title: spec.title,
      body: spec.body ?? "",
      // What the backend sends for a native issue: the app's own link.
      url: `http://127.0.0.1:7423/i/${spec.key}`,
      state,
      state_type: STATE_TYPE[state],
      kind: spec.kind,
      source: spec.source ?? vendor,
      priority: spec.priority,
      labels: LABELS[spec.key] ?? [],
      evidence: spec.kind === "bug" ? "Seen on the latest build; steps in the body." : "",
      claimed_by: STATE_TYPE[state] === "started" ? claimed : null,
      owner: spec.owner ?? "agent",
      created_at: iso(timeline[0].ts),
      updated_at: iso(updated),
      blocked_by: [],
      blocks: [],
      urgency: spec.priority,
    });
    world.timelines.set(spec.key, oldStyle ? timeline : timeline.map((t) => (t.actor === agent ? { ...t, place: folder } : t)));
    world.reviews.set(spec.key, reviews);
    if (number) {
      const done = state === "Done";
      const approved = spec.story.includes("approve");
      world.prs.set(spec.key, [
        {
          provider: repo.provider,
          repo: repo.slug,
          number,
          title: `${spec.key}: ${spec.title}`,
          url,
          state: done ? "MERGED" : "OPEN",
          branch: branchOf(spec),
          head_sha: reviews[0]?.commit_sha ?? sha(),
          updated_at: iso(updated),
          comment_count: reviews.length > 1 ? reviews.length : 0,
          approved_by: approved ? ["sam-lee"] : [],
          approved_by_me: approved,
          checks: done ? "success" : spec.checks ?? "success",
        },
      ]);
    }
  }
  link(world);
  for (const i of world.issues) retrack(world, i);
  world.projects = Object.entries(PROJECT_NAMES).map(([key, name]) => ({ key, name, open_issues: 0 }));
  recount(world);
  return world;
}

/** Blocked-by relations: [blocked, blocker]. Closed blockers no longer block. */
const BLOCKERS: [string, string][] = [
  ["NIT-26", "NIT-31"],
  ["NIT-28", "NIT-29"],
  ["NIT-18", "NIT-20"],
  ["ORB-270", "ORB-271"],
  ["HAR-12", "HAR-15"],
];

const isOpen = (i: IssueRecord) => i.state_type !== "completed" && i.state_type !== "canceled";
const byKey = (world: World, key: string) => world.issues.find((i) => i.key === key)!;

/** Fills blocked_by, blocks and urgency the way the backend does: an issue is as urgent as the most urgent open issue it blocks. */
function link(world: World) {
  for (const [blocked, blocker] of BLOCKERS) {
    const a = byKey(world, blocked);
    const b = byKey(world, blocker);
    if (!isOpen(a) || !isOpen(b)) continue;
    a.blocked_by.push(b.key);
    b.blocks.push(a.key);
    if (a.priority > 0 && (b.urgency === 0 || a.priority < b.urgency)) b.urgency = a.priority;
  }
}

function linked(world: World, key: string): LinkedIssue {
  const i = byKey(world, key);
  return { key: i.key, title: i.title, state: i.state, open: isOpen(i) };
}

/** The track the backend would send: computed from the issue's timeline and reviews. */
function retrack(world: World, issue: IssueRecord) {
  issue.track = undefined;
  issue.track = computeTrack(issue, world.timelines.get(issue.key), world.reviews.get(issue.key));
}

function recount(world: World) {
  for (const p of world.projects) {
    p.open_issues = world.issues.filter((i) => i.project === p.key && i.state_type !== "completed" && i.state_type !== "canceled").length;
  }
}

const world = build();

// ----- Build lanes (docs/build-lanes.md): running, idle, queued, not made yet, and unhealthy -----

function laneRun(id: number, project: string, state: string, lane: number | null, more: Partial<LaneRun> = {}): LaneRun {
  return {
    id, project, repo: `C:/Code/${project.toLowerCase()}`, issue: null, by: "claude", kind: "run", snapshot: "c0ffee1", tree: "t", base: "b",
    command: "cargo test -p nitely-core", state, lane, executor: "local", queued_at: NOW - 5 * MIN, started_at: lane ? NOW - 4 * MIN : null,
    ended_at: null, exit_code: null, timed_out: false, setup_ran: false, files_switched: null, log: null, output: "", error: null, ...more,
  };
}

function lanesOf(project: string, repo: string, count: number, lanes: ProjectLanes["lanes"], queue: LaneRun[], recent: LaneRun[]): ProjectLanes {
  return {
    project, repo: `C:\\Code\\${repo}`, dir: `C:\\Code\\worktrees\\.nitely-lanes\\${repo}`,
    rules: { count, keep: ["target", "node_modules"], timeout_minutes: 30 }, lanes, queue, recent,
  };
}

const BUILD_LANES: ProjectLanes[] = [
  lanesOf("NIT", "nitely", 2,
    [
      { lane: 1, path: "l1", made: true, busy: true, at: "9f3c2a1", size_bytes: 8.3 * 1024 ** 3, cap_bytes: 12.8 * 1024 ** 3 },
      { lane: 2, path: "l2", made: true, busy: false, at: "9f3c2a1", size_bytes: 6.4 * 1024 ** 3, cap_bytes: 12.8 * 1024 ** 3 },
    ],
    [
      laneRun(41, "NIT", "running", 1, { issue: "NIT-31", kind: "gate", started_at: NOW - 2 * MIN - 10 }),
      laneRun(42, "NIT", "queued", null, { issue: "NIT-44", command: "npm test" }),
      laneRun(43, "NIT", "queued", null, { issue: "NIT-45", command: "cargo clippy --workspace -- -D warnings" }),
    ],
    [laneRun(40, "NIT", "done", 2, { issue: "NIT-31", exit_code: 0, ended_at: NOW - 6 * MIN })]),
  lanesOf("HAR", "harbor", 1,
    [{ lane: 1, path: "l1", made: true, busy: false, at: "4e5d6c7" }],
    [],
    [laneRun(12, "HAR", "lane_error", 1, { issue: "HAR-14", command: "npm run build", error: "switching the lane failed twice: a file is still held open (node.exe)" })]),
  lanesOf("ORB", "orbit", 2,
    [{ lane: 1, path: "l1", made: true, busy: false, at: "a1b2c3d" }, { lane: 2, path: "l2", made: false, busy: false, at: null }],
    [],
    []),
  // Large-repo mode: the person's own checkout is the lane. `?checkout=waiting` shows a request
  // waiting for it to be clean.
  largeRepo(),
];

function largeRepo(): ProjectLanes {
  const waitingNow = new URLSearchParams(location.search).get("checkout") === "waiting";
  const queue = waitingNow
    ? [laneRun(77, "ATL", "queued", null, { issue: "ATL-9", command: "ninja -C out/debug tests", waiting_for: "your checkout has 3 changes, src/ui/frame.cc and more" })]
    : [laneRun(76, "ATL", "running", 1, { issue: "ATL-8", command: "ninja -C out/debug tests", started_at: NOW - 23 * MIN })];
  const p = lanesOf("ATL", "atlas", 1, [{ lane: 1, path: "C:\\Code\\atlas", made: true, busy: !waitingNow, at: "5e6f7a8" }], queue, []);
  return { ...p, dir: "C:\\Code\\worktrees\\.nitely-lanes\\atlas", rules: { ...p.rules, keep: ["out"], timeout_minutes: 240, large_repo: true, base: "every 4h" } };
}

function lanesOverview(): LanesOverview {
  // `?slots` shows the machine's build budget, which the scheduler adds.
  const slots = new URLSearchParams(location.search).has("slots") ? { used: 1, total: 4 } : null;
  // `?memory=low` shows a request waiting for the machine's memory (NIT-266).
  const low = new URLSearchParams(location.search).get("memory") === "low";
  const GB = 1024 ** 3;
  const machine = { memory: { total: 64 * GB, available: (low ? 6.2 : 38.4) * GB }, limits: { total: 64 * GB, floor: 12.8 * GB, cap: 16 * GB, floor_from: "default", cap_from: "default" }, below_normal: true, pace: paceFixture(location.search) };
  const waiting_for_memory = low ? "waiting for memory: 6.2 GB free, new work starts above 12.8 GB" : null;
  const projects = new URLSearchParams(location.search).has("executors") ? BUILD_LANES.map((p, index) => index ? p : {
    ...p, rules: { ...p.rules, executor: "machine:builder" },
    queue: [...p.queue,
      laneRun(101, "NIT", "running", null, { issue: "NIT-30", executor: "machine:builder", kind: "gate", started_at: NOW - 2 * MIN }),
      laneRun(102, "NIT", "running", null, { issue: "NIT-52", executor: "ci", kind: "land", started_at: NOW - 4 * MIN }),
    ],
    recent: [
      laneRun(103, "NIT", "done", null, { issue: "NIT-29", executor: "machine:builder", exit_code: 7, started_at: NOW - 6 * MIN, ended_at: NOW - 5 * MIN, output: JSON.stringify({ results: [{ output: "Expected the build to pass; one assertion failed." }] }) }),
      laneRun(104, "NIT", "abandoned", null, { issue: "NIT-53", executor: "machine:builder", started_at: NOW - 9 * MIN, ended_at: NOW - 6 * MIN }),
      laneRun(105, "NIT", "done", null, { issue: "NIT-31", executor: "ci", exit_code: 0, started_at: NOW - 10 * MIN, ended_at: NOW - 7 * MIN, output: JSON.stringify({ results: [{ command: "CI required checks", output: `https://github.com/example/acme/commit/${"a".repeat(40)}/checks` }] }) }),
      ...p.recent,
    ],
  }) : BUILD_LANES;
  return { projects: new URLSearchParams(location.search).has("nolanes") ? [] : projects, slots, machine, waiting_for_memory };
}

// ----- Review batches and the land queue (docs/batching.md); `?batches=none` shows none -----

const NIT_REPO = "C:/Code/nitely";

function reviewBatch(id: number, issues: string[], more: Partial<ReviewBatch> = {}): ReviewBatch {
  return {
    id, repo: NIT_REPO, repo_name: "nitely", reviewer: "claude", issues, state: "reviewing", shadow: false, started_at: NOW - 3 * MIN,
    ended_at: null, seconds: null, tokens: null, error: null, ...more,
  };
}

function landRun(id: number, issues: string[], more: Partial<LandRun> = {}): LandRun {
  return { id, repo: NIT_REPO, started_at: NOW - 6 * MIN, ended_at: null, issues, landed: [], sent_back: [], gates: 0, head: null, step: null, ...more };
}

// A review batch of three, a shadow batch that only records, and a queue run bisecting a stack of
// three with one more change waiting behind it.
const REVIEWING = reviewBatch(12, ["NIT-30", "NIT-50", "NIT-51"]);
const SHADOW = reviewBatch(13, ["ORB-261", "ORB-263"], { repo: "C:/Code/orbit", repo_name: "orbit", reviewer: "codex", shadow: true, started_at: NOW - 2 * MIN });
const RUN = landRun(7, ["NIT-29", "NIT-52", "NIT-54"], { step: "bisecting: gating 1 of 3", gates: 2 });
const EARLIER_RUN = landRun(6, ["NIT-24", "NIT-46"], { started_at: NOW - 5 * HOUR - 20 * MIN, ended_at: NOW - 5 * HOUR, landed: ["NIT-24"], sent_back: ["NIT-46"], gates: 3, head: "4b8e1f0" });

function batchesOverview(): BatchesOverview {
  if (new URLSearchParams(location.search).get("batches") === "none") return { reviewing: [], review_waiting: [], land: [] };
  return {
    reviewing: [REVIEWING, SHADOW],
    review_waiting: [],
    land: [
      {
        repo: NIT_REPO,
        repo_name: "nitely",
        entries: [
          { id: 30, issue: "NIT-29", state: "running", queued_at: NOW - 7 * MIN, run: 7 },
          { id: 31, issue: "NIT-52", state: "running", queued_at: NOW - 7 * MIN, run: 7 },
          { id: 32, issue: "NIT-54", state: "running", queued_at: NOW - 6 * MIN - 30, run: 7 },
          { id: 33, issue: "NIT-53", state: "queued", queued_at: NOW - 2 * MIN, run: null },
        ],
        runs: [RUN],
      },
    ],
  };
}

type BatchReview = IssueBatches["reviews"][number];
type BatchLand = IssueBatches["lands"][number];
const inBatch = (b: ReviewBatch, more: Partial<BatchReview> = {}): BatchReview => ({ batch: b, state: "reviewing", verdict: null, blocking: 0, minor: 0, why: null, sent_back: null, ...more });
const trip = (id: number, state: string, run: LandRun | null, more: Partial<BatchLand> = {}): BatchLand => ({
  id, state, queued_at: (run?.started_at ?? NOW) - 60, ended_at: null, place: null, run, lane_run: null, head: null, error: null, sent_back: null, ...more,
});

/** Each issue's batches, for its timeline and station. */
function issueBatches(key: string): IssueBatches | undefined {
  if (new URLSearchParams(location.search).get("batches") === "none") return undefined;
  const done = { ...SHADOW, state: "done", ended_at: NOW - 1 * MIN, seconds: 96 };
  const map: Record<string, IssueBatches> = {
    "NIT-30": { reviews: [inBatch(REVIEWING)], lands: [] },
    "NIT-50": { reviews: [inBatch(REVIEWING)], lands: [] },
    "NIT-51": { reviews: [inBatch(REVIEWING)], lands: [] },
    // An earlier batch sent NIT-29 back for its own P1; its fix now lands in the run that bisects.
    "NIT-29": {
      reviews: [
        inBatch(reviewBatch(9, ["NIT-29", "NIT-21"], { reviewer: "codex", state: "done", started_at: NOW - 2 * HOUR - 8 * MIN, ended_at: NOW - 2 * HOUR }), {
          state: "done", verdict: "changes", blocking: 1, minor: 1, sent_back: { why: "own", with: [] },
        }),
      ],
      lands: [trip(30, "running", RUN)],
    },
    "NIT-52": { reviews: [], lands: [trip(31, "running", RUN)] },
    "NIT-54": { reviews: [], lands: [trip(32, "running", RUN)] },
    "NIT-53": { reviews: [], lands: [trip(33, "queued", null, { queued_at: NOW - 2 * MIN, place: 4 })] },
    "NIT-24": { reviews: [], lands: [trip(20, "landed", EARLIER_RUN, { ended_at: NOW - 5 * HOUR, lane_run: 38, head: "4b8e1f0" })] },
    "NIT-46": {
      reviews: [],
      lands: [trip(21, "sent_back", EARLIER_RUN, { ended_at: NOW - 5 * HOUR, lane_run: 37, error: "the gate failed at `npm run build`: bisecting the stack found that its change is the one that fails the gate.", sent_back: { why: "bisected", with: [] } })],
    },
    "ORB-261": { reviews: [inBatch(done, { state: "done", verdict: "pass", minor: 1 })], lands: [] },
    "ORB-263": { reviews: [inBatch(done, { state: "done", verdict: "changes", blocking: 1, sent_back: { why: "own", with: [] } })], lands: [] },
  };
  return map[key];
}

// ----- What waiting requests wait for (NIT-322); `?waits=none` shows none -----

function waitsOverview(): Waits {
  if (new URLSearchParams(location.search).get("waits") === "none") return { waiting: [] };
  const run = (id: number, issue: string, kind: string, mins: number) => ({ what: `run ${id}: ${issue} ${kind}`, issue, since: NOW - mins * MIN });
  return {
    waiting: [
      {
        kind: "run", issue: "NIT-45", project: "NIT", since: NOW - 14 * MIN, place: 2, why: "3 of 3 build slots in use",
        behind: [run(41, "NIT-31", "gate", 2), run(36, "HAR-14", "gate", 24), run(38, "ORB-271", "gate", 19)],
      },
      { kind: "gate", issue: "NIT-44", project: "NIT", since: NOW - 9 * MIN, place: 1, why: "waiting for memory: 3.1 GB free, a gate needs 6 GB", behind: [] },
      { kind: "landing", issue: "NIT-24", project: "NIT", since: NOW - 3 * MIN, place: null, why: "landing now: it holds the land lock, so other lands of this repo wait", behind: [{ what: "run 44: its gate in lane 2", issue: "NIT-24", since: NOW - 3 * MIN }] },
      { kind: "land_refused", issue: "NIT-29", project: "NIT", since: NOW - 2 * MIN, place: null, why: "its land was refused: another land holds the lock; its agent lands again once it is free", behind: [{ what: "NIT-24, holding the land lock", issue: "NIT-24", since: NOW - 3 * MIN }] },
      { kind: "land_after", issue: "NIT-31", project: "NIT", since: NOW - 9 * MIN, place: null, why: "lands by itself once NIT-24 has landed", behind: [{ what: "NIT-24's land", issue: "NIT-24", since: null }] },
      { kind: "land_queue", issue: "ORB-260", project: "ORB", since: NOW - 6 * MIN, place: 1, why: "1st in the land queue, behind a land holding the lock", behind: [{ what: "ORB-265 (land), holding the land lock", issue: "ORB-265", since: NOW - 7 * MIN }] },
      { kind: "submit", issue: "NIT-30", project: "NIT", since: NOW - 4 * MIN, place: null, why: "waits for the issue's previous submit", behind: [{ what: "submit job 31", issue: "NIT-30", since: NOW - 11 * MIN }] },
      { kind: "land_queue", issue: "NIT-53", project: "NIT", since: NOW - 2 * MIN, place: 4, why: "4th in the land queue, behind the run landing now", behind: [{ what: "land queue run 7: NIT-29, NIT-52, NIT-54", issue: null, since: NOW - 6 * MIN }] },
      { kind: "review", issue: "NIT-21", project: "NIT", since: NOW - 40, place: 1, why: "waits for a review batch by codex (1 other change waiting)", behind: [{ what: "review batch 12: NIT-30, NIT-50, NIT-51", issue: null, since: NOW - 3 * MIN }] },
    ],
  };
}

// Its gate ran in a lane before: the timeline names the run, and Open log asks the app for it.
world.timelines.get("NIT-31")?.push(
  { ts: NOW - 7 * MIN, actor: "claude", kind: "lane_run", summary: "Ran `cargo test -p nitely-core` in lane 2: exit 0 after 63 s (run 40)" },
);

// ----- Commands -----

function session(spec: IssueSpec, live: Live): Session {
  return {
    id: `${spec.vendor}:${fakeSession(spec.key)}`,
    vendor: spec.vendor ?? "claude",
    cwd: worktreeOf(spec),
    started_at: NOW - spec.from * HOUR + 10 * MIN,
    last_seen_at: Math.round(NOW - live.seen * MIN),
    last_activity: live.activity,
    state: live.state,
    ...(spec.crew ? { crew: true } : {}),
  };
}

// ----- Files held by work in flight (overlap.rs), for the branch map's hover -----

const HELD: Record<string, string[]> = {
  "NIT-31": ["src/Pulse.tsx", "src/pulse.css", "src/types.ts"],
  "NIT-30": ["src/IssueJourney.tsx", "crates/nitely-core/src/review.rs", "src/types.ts"],
  "NIT-29": ["src/Activity.tsx", "crates/nitely-core/src/activity.rs"],
};

function heldFiles(project: string): Held[] {
  return Object.entries(HELD)
    .filter(([key]) => key.startsWith(`${project.toUpperCase()}-`))
    .map(([key, files]) => ({ key, branch: null, files }));
}

// ----- The NIT crew: the specs marked crew are its running workers -----

let crewPaused = false;

// Each repo's [crew] as the Crew section edits it (crates/nitely-crew/src/crew_settings.rs): Nitely
// commits its own playbook and runs a crew, Orbit's playbook is kept by Nitely with the crew off,
// and Harbor has no playbook (so no gate) yet.
const CREW_CHOICES = [{ name: "claude", installed: true }, { name: "codex", installed: true }];
const crewViews: CrewView[] = [
  {
    repo: "Nitely", root: "C:\\Code\\Nitely", source: "repo", path: "C:\\Code\\Nitely\\.nitely\\playbook.toml", integration: "main", workers: 2,
    agents: ["claude", "codex"], max_per_day: null, default_max_per_day: 20, max_tokens_per_day: 8_000_000, quiet_hours: "01:00-07:00",
    choices: CREW_CHOICES, has_gate: true, pending_proposal: null,
  },
  {
    repo: "Orbit", root: "C:\\Code\\Orbit", source: "stored", path: "C:\\Users\\you\\AppData\\Roaming\\Nitely\\playbooks\\Orbit.toml", integration: "develop", workers: 0,
    agents: ["claude", "codex"], max_per_day: null, default_max_per_day: 20, max_tokens_per_day: null, quiet_hours: null,
    choices: [{ name: "claude", installed: true }, { name: "codex", installed: false }], has_gate: true, pending_proposal: "ORB-12",
  },
  {
    repo: "Harbor", root: "C:\\Code\\Harbor", source: "none", path: "C:\\Users\\you\\AppData\\Roaming\\Nitely\\playbooks\\Harbor.toml", integration: "main", workers: 0,
    agents: ["claude", "codex"], max_per_day: null, default_max_per_day: 20, max_tokens_per_day: null, quiet_hours: null,
    choices: CREW_CHOICES, has_gate: false, pending_proposal: null,
  },
];

function crewSettings(project: string): CrewView[] {
  const names = REPOS.filter((r) => r.project === project.toUpperCase()).map((r) => r.name);
  return crewViews.filter((v) => names.includes(v.repo));
}

function crewSave(args: Record<string, unknown>): CrewSaved {
  const v = crewViews.find((x) => x.repo === args.repo);
  if (!v) throw `no repo ${String(args.repo)} in Nitely's config`;
  const edit = args.edit as CrewEdit;
  if (edit.workers > 0 && edit.agents.length === 0) throw "pick at least one agent for the crew to start";
  Object.assign(v, { workers: edit.workers, agents: edit.agents, max_per_day: edit.max_per_day, max_tokens_per_day: edit.max_tokens_per_day, quiet_hours: edit.quiet_hours });
  if (v.source === "none") v.source = "stored";
  const done = v.source === "repo" ? "Committed 4f2a9c1e0 to .nitely/playbook.toml on main; it is pushed with the next land" : `Saved to ${v.path}`;
  return { view: { ...v }, done };
}

function crewStatus(project: string): CrewStatus | null {
  const views = crewSettings(project);
  const on = views.find((v) => v.workers > 0);
  if (!on) return null;
  if (project.toUpperCase() !== "NIT") {
    return {
      project: project.toUpperCase(), cap: on.workers, running: [], started_today: 0, max_per_day: on.max_per_day ?? on.default_max_per_day,
      tokens_today: 0, max_tokens_per_day: on.max_tokens_per_day, paused: crewPaused, supervised: on.has_gate,
      status: on.has_gate ? "working; nothing else ready" : "no gate: the playbook has no gate commands", recent: [],
    };
  }
  const running: CrewWorker[] = ISSUES.filter((s) => s.crew && s.live).map((s, i) => ({
    id: 40 + i, project: "NIT", issue_key: s.key, agent: s.vendor ?? "claude", pid: 7100 + i, worktree: worktreeOf(s),
    log: `C:\Users\you\AppData\Roaming\Nitely\crew\NIT\${s.key.toLowerCase()}-${s.vendor}.log`,
    started_at: NOW - s.from * HOUR + 10 * MIN, ended_at: null, exit_code: null, outcome: null, reason: null, phase: "work",
  }));
  return {
    project: "NIT", cap: on.workers, running, started_today: 3, max_per_day: on.max_per_day ?? 20, tokens_today: 3_400_000, max_tokens_per_day: on.max_tokens_per_day, paused: crewPaused, supervised: true,
    status: crewPaused ? "paused" : "working; nothing else ready", recent: [],
  };
}

/** A steady made-up number from a string, so an issue's spend is the same every time. */
function hashOf(s: string): number {
  let h = 2166136261;
  for (const c of s) h = Math.imul(h ^ c.charCodeAt(0), 16777619);
  return h >>> 0;
}

const MODEL: Record<string, string> = { claude: "claude-opus-5-5", codex: "gpt-6", cursor: "cursor-fast" };

/** Tokens an issue's agents used: none before work started, then by its agent's model and the reviewer's. */
function issueSpend(args: Record<string, unknown>): IssueSpend {
  const key = String(args.key);
  const spec = ISSUES.find((s) => s.key === key);
  const worked = spec ? spec.story.some((s) => s === "claim" || s === "work") : false;
  const base = { key, tokens: 0, cache_read: 0, calls: 0, sessions: 0, by_model: [] as IssueSpend["by_model"], first: null, last: null };
  if (!spec || !worked) return base;
  const vendor = spec.vendor ?? "claude";
  const main = 120_000 + (hashOf(key) % 900_000);
  const by_model = [{ vendor, model: MODEL[vendor] ?? vendor, tokens: main, calls: 20 + (hashOf(key + "c") % 140) }];
  // The other vendor reviews.
  const reviewer = vendor === "codex" ? "claude" : "codex";
  if (spec.story.includes("review")) by_model.push({ vendor: reviewer, model: MODEL[reviewer], tokens: 30_000 + (hashOf(key + "r") % 60_000), calls: 6 });
  const tokens = by_model.reduce((a, m) => a + m.tokens, 0);
  return { ...base, tokens, cache_read: tokens * 40, calls: by_model.reduce((a, m) => a + m.calls, 0), sessions: by_model.length, by_model, first: NOW - spec.from * HOUR, last: NOW - spec.to * HOUR };
}

/** A project's tokens per day: its issues' spend spread over the days they were worked. */
function projectSpend(args: Record<string, unknown>): ProjectSpend {
  const project = String(args.project).toUpperCase();
  const days = Number(args.days ?? 14);
  const dayStart = Number(args.dayStart);
  const from = dayStart - (days - 1) * 86_400;
  const series = Array.from({ length: days }, (_, i) => ({ day_start: from + i * 86_400, tokens: 0, landed: 0 }));
  const dayOf = (ts: number) => Math.min(days - 1, Math.max(0, Math.floor((ts - from) / 86_400)));
  const repo = REPOS.find((r) => r.project === project);
  const mine = ISSUES.filter((s) => repo && repoOf(s.key) === repo);
  const spends = mine.map((s) => ({ spec: s, spend: issueSpend({ key: s.key }) })).filter((x) => x.spend.tokens > 0 && NOW - x.spec.from * HOUR >= from - 86_400);
  for (const { spec, spend } of spends) series[dayOf(NOW - ((spec.from + spec.to) / 2) * HOUR)].tokens += spend.tokens;
  const landed = world.issues.filter((i) => i.project === project && i.state === "Done");
  for (const i of landed) series[dayOf(Date.parse(i.updated_at) / 1000)].landed += 1;
  const models = new Map<string, IssueSpend["by_model"][number]>();
  for (const { spend } of spends) for (const m of spend.by_model) {
    const e = models.get(m.model) ?? { ...m, tokens: 0, calls: 0 };
    e.tokens += m.tokens;
    e.calls += m.calls;
    models.set(m.model, e);
  }
  const by_model = [...models.values()].sort((a, b) => b.tokens - a.tokens);
  const vendors = new Map<string, IssueSpend["by_model"][number]>();
  for (const m of by_model) {
    const e = vendors.get(m.vendor) ?? { vendor: m.vendor, model: "", tokens: 0, calls: 0 };
    e.tokens += m.tokens;
    e.calls += m.calls;
    vendors.set(m.vendor, e);
  }
  const tokens = series.reduce((a, d) => a + d.tokens, 0);
  const landedSpend = spends.filter((x) => landed.some((i) => i.key === x.spec.key)).reduce((a, x) => a + x.spend.tokens, 0);
  return {
    project,
    days: series,
    tokens,
    cache_read: tokens * 40,
    by_agent: [...vendors.values()].sort((a, b) => b.tokens - a.tokens),
    by_model,
    landed: landed.length,
    per_landed: landed.length && landedSpend ? Math.round(landedSpend / landed.length) : null,
    top_issues: spends.map((x) => ({ key: x.spec.key, tokens: x.spend.tokens })).sort((a, b) => b.tokens - a.tokens).slice(0, 5),
  };
}

/** The mirror in the preview: Nitely's repo is on GitHub; Orbit's Bitbucket repo has no issue tracker. */
const mirrorOn = new Map<string, string | null>([["NIT", "open"]]);
const privateIssues = new Set<string>(["NIT-20"]);

function mirrorPreview(project: string): MirrorPreview {
  const key = project.toUpperCase();
  const repo = REPOS.find((r) => r.project === key);
  const issues = world.issues.filter((i) => i.project === key);
  const sensitive = (i: IssueRecord) => privateIssues.has(i.key) || i.owner === "human" || i.labels.includes("security") || (i.priority === 1 && i.kind === "bug");
  const count = (backfill: string, pick: (i: IssueRecord) => boolean) => {
    const chosen = issues.filter(pick);
    return { backfill, publish: chosen.filter((i) => !sensitive(i)).length, private: chosen.filter(sensitive).length, held: 0 };
  };
  const open = (i: IssueRecord) => i.state_type !== "completed" && i.state_type !== "canceled";
  const sample = issues.filter((i) => open(i) && !sensitive(i)).slice(0, 1);
  const backfill = mirrorOn.get(key) ?? null;
  return {
    project: key,
    enabled: backfill ? { project: key, enabled_at: NOW - 2 * 24 * HOUR, backfill } : null,
    host: repo?.provider === "github" ? "GitHub" : repo?.provider === "bitbucket" ? "Bitbucket" : null,
    repo: repo?.slug ?? null,
    unavailable: repo?.provider === "bitbucket" ? `${repo.slug} has no issue tracker on Bitbucket (many repos have it off); turn it on in the repo's settings to mirror` : null,
    counts: [count("none", () => false), count("open", open), count("all", () => true)],
    sample: sample.map((i) => ({
      key: i.key,
      title: i.title,
      body: `${i.body || "What is wrong or wanted, and why."}\n\n---\nTracked in Nitely as ${i.key} (${i.state}, ${i.kind}). Nitely is where it is decided; comments here are not read.`,
      labels: [i.kind ?? "", ...i.labels].filter(Boolean),
    })),
  };
}

function issueMirror(key: string): IssueMirror | null {
  const issue = world.issues.find((i) => i.key === key);
  if (!issue || !mirrorOn.get(issue.project ?? "")) return null;
  const repo = REPOS.find((r) => r.project === issue.project);
  const base = { issue: key, host: "GitHub", number: null, url: null, marked_private: privateIssues.has(key) };
  if (privateIssues.has(key)) return { ...base, status: "private", reason: "marked private" };
  if (issue.owner === "human") return { ...base, status: "private", reason: "the human's own work stays private" };
  if (issue.priority === 1 && issue.kind === "bug") return { ...base, status: "private", reason: "an urgent bug may be a security problem; mark it public to publish it" };
  const n = Number(key.split("-")[1]) + 3;
  return { ...base, status: "mirrored", reason: null, number: n, url: `https://github.com/${repo?.slug}/issues/${n}` };
}

function crewPause(args: Record<string, unknown>): CrewStatus | null {
  crewPaused = Boolean(args.paused);
  return crewStatus(String(args.project));
}

/** `claude/nit-12-slug` → ["claude", "NIT-12"], as git::parse_branch does. */
function parseBranch(branch: string): [string | null, string | null] {
  const slash = branch.indexOf("/");
  const vendor = slash < 0 ? null : branch.slice(0, slash);
  const m = /^([a-z]+)-(\d+)(-|$)/i.exec(branch.slice(slash + 1));
  return [vendor, m ? `${m[1].toUpperCase()}-${m[2]}` : null];
}

function snapshot(): Board {
  const repos: RepoView[] = REPOS.map((repo) => {
    const main: TaskRow = {
      worktree: repo.root,
      branch: repo.integration,
      is_main: true,
      vendor: null,
      issue_key: null,
      state: { ahead: 0, behind: 0, dirty_files: 0, last_commit_subject: "Land the last task", last_commit_at: NOW - 3 * HOUR, rebase_in_progress: false },
      error: null,
      session: null,
      issue: null,
      pr: null,
      needs_me: false,
    };
    const rows = ISSUES.filter((s) => s.key.startsWith(`${repo.project}-`) && s.branch).map((spec): TaskRow => {
      const issue = world.issues.find((i) => i.key === spec.key)!;
      const s = spec.live ? session(spec, spec.live) : null;
      const timeline = world.timelines.get(spec.key)!;
      const lastStatus = [...timeline].reverse().find((t) => t.kind === "status" || t.kind === "gate_passed" || t.kind === "gate_failed");
      const state: BranchState = {
        ahead: 0,
        behind: 0,
        dirty_files: 0,
        last_commit_subject: lastStatus?.summary ?? spec.title,
        last_commit_at: lastStatus?.ts ?? null,
        rebase_in_progress: false,
        ...spec.branch,
      };
      return {
        worktree: worktreeOf(spec),
        branch: branchOf(spec),
        is_main: false,
        vendor: spec.vendor ?? null,
        issue_key: spec.key,
        state,
        error: null,
        session: s,
        issue,
        pr: world.prs.get(spec.key)?.[0] ?? null,
        // As the backend decides it: a stopped rebase, an agent waiting, or the issue waiting on the human.
        needs_me: state.rebase_in_progress || s?.state === "waiting" || issue.state === "Needs You",
      };
    });
    for (const loose of LOOSE_WORKTREES.filter((l) => l.project === repo.project)) {
      const [vendor, key] = loose.branch ? parseBranch(loose.branch) : [null, null];
      rows.push({
        worktree: `C:\\Code\\${repo.name}-worktrees\\${loose.folder}`,
        branch: loose.branch,
        is_main: false,
        vendor,
        issue_key: key,
        state: {
          ahead: loose.ahead,
          behind: loose.behind,
          dirty_files: loose.dirty,
          last_commit_subject: loose.subject,
          last_commit_at: NOW - loose.idle * HOUR,
          rebase_in_progress: false,
        },
        error: null,
        session: null,
        issue: null,
        pr: null,
        needs_me: false,
      });
    }
    const branches: BranchRow[] = LOOSE_BRANCHES.filter((b) => b.project === repo.project).map((b) => {
      const [vendor, key] = parseBranch(b.name);
      return {
        name: b.name,
        vendor,
        issue_key: key,
        ahead: b.ahead,
        behind: b.behind,
        last_commit_subject: b.subject,
        last_commit_at: NOW - b.idle * HOUR,
        issue: world.issues.find((i) => i.key === key) ?? null,
        pr: [...world.prs.values()].flat().find((p) => p.branch === b.name) ?? null,
      };
    });
    const unmatched: Session[] =
      repo.project === "HAR"
        ? [{ id: "cursor-harbor", vendor: "cursor", cwd: repo.root, started_at: NOW - 5 * HOUR, last_seen_at: NOW - 3 * HOUR, last_activity: "Read README.md", state: "ended" }]
        : [];
    return {
      name: repo.name,
      root: repo.root,
      integration_branch: repo.integration,
      project: repo.project,
      rows: [main, ...rows],
      branches,
      error: null,
      unmatched_sessions: unmatched,
    };
  });
  // One row per session, as snapshot.rs makes them: the sessions in task worktrees, and three that
  // run from the Nitely main checkout on issues of their own (a coordinator's lanes do).
  const agents: AgentRow[] = repos.flatMap((r) =>
    r.rows
      .filter((row) => row.session && !row.is_main)
      .map((row) => ({ session: row.session!, repo: r.name, project: r.project, worktree: row.worktree, issue_key: row.issue_key, issue: row.issue, report: row.session!.last_activity, needs_me: row.needs_me })),
  );
  const nitely = repos.find((r) => r.project === "NIT");
  const started = world.issues.filter((i) => i.project === "NIT" && i.state_type === "started" && !agents.some((a) => a.issue_key === i.key));
  if (nitely) {
    const lanes: [string, number, string, Session["state"]][] = [
      ["5f1c9a2e-0d4b-4c7e-9b1a-3e6d2f8a7b11", 0, "Wiring the per-session rows into Now", "working"],
      ["9b7e4d10-6a2c-4f3e-8d5b-1c0a9e7f6d22", 1, "Waiting for the lane to free up", "working"],
      ["c3a8f6e2-7b1d-4e9a-a2c5-8f4b0d6e3a33", 2, "Asked whether to keep the old layout", "waiting"],
    ];
    for (const [id, n, report, state] of lanes) {
      const issue = started[n] ?? null;
      const session: Session = { id: `claude:${id}`, vendor: "claude", cwd: nitely.root, started_at: NOW - (n + 1) * 40 * 60, last_seen_at: NOW - 30 - n * 60, last_activity: "Bash: cargo test", state };
      agents.push({ session, repo: nitely.name, project: "NIT", worktree: null, issue_key: issue?.key ?? null, issue, report, needs_me: state === "waiting" });
    }
  }
  agents.sort((a, b) => b.session.last_seen_at - a.session.last_seen_at);
  return {
    repos,
    agents,
    // Only those without a worktree row, as in nitely-core: the others are flagged on their row.
    needs_me_issues: world.issues.filter((i) => (i.state === "Needs You" || isOwnWork(i)) && !repos.some((r) => r.rows.some((row) => row.issue_key === i.key))),
    asks: world.asks.filter((a) => a.answer === null),
    notices: world.notices,
    sync: [
      { source: "github", last_attempt: NOW - 95, last_ok: NOW - 95, last_error: null },
      { source: "bitbucket", last_attempt: NOW - 95, last_ok: NOW - 95, last_error: null },
    ],
    // An agent on the Mac, reaching this machine's backlog through `nitely serve`.
    remote_sessions: [
      { id: "claude@mac", vendor: "claude", cwd: "remote:mac", started_at: NOW - 2 * HOUR, last_seen_at: NOW - 140, last_activity: "Filed ORB-281: Mac sidebar flickers on resize", state: "working" },
    ],
  };
}

// ----- Pulse: the same shape crates/nitely-crew/src/pulse.rs returns -----

const QUIET_SECS = 3600;
const HAPPENING: Record<string, Happening["kind"]> = {
  landed: "landed",
  issue_created: "filed",
  gate_failed: "stuck",
  land_failed: "stuck",
  review_failed: "stuck",
  submit: "review",
  ask: "asked",
};

function flowIssue(issue: IssueRecord, now: number): FlowIssue {
  const spec = ISSUES.find((s) => s.key === issue.key)!;
  const timeline = world.timelines.get(issue.key) ?? [];
  const gate = [...timeline].reverse().find((t) => t.kind === "gate_passed" || t.kind === "gate_failed" || t.kind === "gate_skipped");
  const s = spec.live ? session(spec, spec.live) : null;
  const last = Math.max(timeline[timeline.length - 1]?.ts ?? 0, s?.last_seen_at ?? 0);
  const track = issue.track!;
  const atAgent = ["started", "working", "gate", "review", "landing"].includes(track.station);
  const open = issue.state_type !== "completed" && issue.state_type !== "canceled";
  return {
    issue,
    agent: issue.claimed_by ?? (spec.vendor ? `${spec.vendor}:c:/code/${repoOf(spec.key).name.toLowerCase()}-worktrees/${spec.key.toLowerCase()}` : null),
    session: s,
    pr: world.prs.get(issue.key)?.[0] ?? null,
    review: world.reviews.get(issue.key)?.[0] ?? null,
    gate: gate ? { ts: gate.ts, passed: gate.kind === "gate_passed", skipped: gate.kind === "gate_skipped" ? gate.summary ?? "YOLO mode" : null, summary: gate.summary } : null,
    worktree: spec.branch ? worktreeOf(spec) : null,
    last_activity: last,
    quiet: open && atAgent && !track.stuck && last > 0 && now - last > QUIET_SECS,
  };
}

function pulse(sinceArg: number, dayStart: number): Pulse {
  const now = Math.floor(Date.now() / 1000);
  const since = Math.max(sinceArg || 0, now - 7 * 24 * HOUR);
  const today = iso(dayStart);
  const projects = world.projects.map((p) => {
    const mine = issues({ project: p.key, includeClosed: true });
    return {
      key: p.key,
      name: p.name,
      issues: mine.filter((i) => i.state_type !== "completed" && i.state_type !== "canceled").map((i) => flowIssue(i, now)),
      done_today: mine.filter((i) => i.state === "Done" && i.updated_at >= today).map((i) => flowIssue(i, now)),
      verification_health: verificationHealth(p.key, now),
      verification_audits: verificationAudit(p.key),
    };
  });
  const events = [...world.timelines.entries()].flatMap(([key, tl]) => tl.map((t) => ({ ...t, key })));
  const all: Happening[] = events
    .filter((e) => e.ts >= since && HAPPENING[e.kind])
    .sort((a, b) => b.ts - a.ts)
    .map((e) => ({ ts: e.ts, kind: HAPPENING[e.kind], issue_key: e.key, actor: e.actor, summary: e.summary }));
  const happened_counts = { landed: 0, filed: 0, stuck: 0, review: 0, asked: 0 };
  for (const h of all) happened_counts[h.kind]++;
  // As pulse.rs counts them: the issues behind the failures.
  const gotStuck = new Set(all.filter((h) => h.kind === "stuck" && h.issue_key).map((h) => h.issue_key!.toUpperCase()));
  const end = (Math.floor(now / HOUR) + 1) * HOUR;
  const start = end - 24 * HOUR;
  const throughput = Array.from({ length: 24 }, (_, h) => ({ start: start + h * HOUR, landed: 0, submitted: 0 }));
  for (const e of events) {
    if (e.ts < start || (e.kind !== "landed" && e.kind !== "submit")) continue;
    const h = throughput[Math.min(23, Math.floor((e.ts - start) / HOUR))];
    if (e.kind === "landed") h.landed++;
    else h.submitted++;
  }
  return {
    now,
    since,
    projects,
    asks: world.asks.filter((a) => a.answer === null),
    approvals: world.issues.filter((i) => i.state === "Needs You"),
    happened: all.slice(0, 300),
    happened_counts,
    stuck_issues: { got: gotStuck.size, keys: [...gotStuck].sort() },
    throughput,
    landed_today: events.filter((e) => e.kind === "landed" && e.ts >= dayStart).length,
    report: goalReport(since),
  };
}

/** Preview only: use ?verification=failed|paused|recovering|pending|green|off for each state. */
function verificationHealth(project: string, now: number): VerificationHealth[] {
  if (!["NIT", "ORB", "HAR"].includes(project)) return [];
  const requested = new URLSearchParams(window.location.search).get("verification");
  if (requested === "off") return [];
  const status = requested && ["failed", "paused", "recovering", "pending", "green"].includes(requested)
    ? requested : project === "NIT" ? "pending" : project === "ORB" ? "green" : "recovering";
  return [{ repo: `C:/Code/${project.toLowerCase()}/.git`, project, observed_head: "b7c281d".padEnd(40, "0"),
    verified_head: (status === "green" ? "b7c281d" : "a632d90").padEnd(40, "0"),
    pending_since: now - 18 * MIN, reduced_count: status === "green" ? 0 : 3,
    checked_at: now - (status === "green" ? 3 : 24) * MIN, status, message: "Sample verification state",
    recovery_issue: ["failed", "paused", "recovering"].includes(status) ? (project === "NIT" ? "NIT-29" : project === "HAR" ? "HAR-14" : null) : null }];
}

const auditReports = new Map<string, VerificationAuditReport>();
function verificationAudit(project: string): VerificationAuditReport | null {
  if (project !== "NIT" || new URLSearchParams(window.location.search).get("verification") === "off") return null;
  const saved = auditReports.get(project);
  if (saved) return saved;
  const common = { landed: 12, audited: 4, confirmed_escape_lands: 1, declared_lands: 12, matching_declarations: 10, audited_escape_rate: 0.25 };
  const result: VerificationAuditReport = { jobs: [
    { id: 1, project, issue_key: "NIT-29", head: "b7c281d".padEnd(40, "0"), state: "findings", rules: ["ordinary-comments"], result: { reviewer: "Independent reviewer", summary: "An executable documentation example may no longer match its contract.", findings: [{ priority: 2, text: "The example uses a removed option. Reproduce the example before confirming an escape." }] }, error: null },
    { id: 2, project, issue_key: "NIT-29", head: "a632d90".padEnd(40, "0"), state: "failed", rules: ["display-copy"], result: null, error: "Reviewer unavailable. No audit result was recorded." },
  ], rules: [{ name: "display-copy", ...common }, { name: "ordinary-comments", landed: 6, audited: 1, confirmed_escape_lands: 0, declared_lands: 6, matching_declarations: 6, audited_escape_rate: 0 }],
    declarers: [{ name: "codex:example-session", ...common, landed: 18, audited: 5, declared_lands: 18, matching_declarations: 16, audited_escape_rate: 0.2 }], disabled_rules: ["display-copy"], escapes: [], jobs_truncated: false, rules_truncated: false, declarers_truncated: false, disabled_rules_truncated: false, escapes_truncated: false, note: "Sample audit results" };
  auditReports.set(project, result);
  return result;
}

const PRIORITY_ORDER = (p: number) => (p === 0 ? 9 : p);

function issues(args: Record<string, unknown>): IssueRecord[] {
  const closed = !!args.includeClosed;
  return world.issues
    .filter((i) => i.project === args.project && (closed || (i.state_type !== "completed" && i.state_type !== "canceled")))
    .sort((a, b) => PRIORITY_ORDER(a.priority) - PRIORITY_ORDER(b.priority) || b.updated_at.localeCompare(a.updated_at));
}

function detail(key: string): IssueDetail {
  placeGoals();
  const issue = world.issues.find((i) => i.key.toLowerCase() === key.toLowerCase());
  if (!issue) throw `no issue ${key}`;
  const relations = {
    blocked_by: BLOCKERS.filter(([a]) => a === issue.key).map(([, b]) => linked(world, b)),
    blocks: BLOCKERS.filter(([, b]) => b === issue.key).map(([a]) => linked(world, a)),
  };
  return {
    issue,
    relations,
    timeline: world.timelines.get(issue.key) ?? [],
    prs: world.prs.get(issue.key) ?? [],
    reviews: world.reviews.get(issue.key) ?? [],
    proposal: PROPOSALS.get(issue.key) ?? null,
    goal_plan: PLANS.get(issue.key) ?? null,
    batches: issueBatches(issue.key),
    asks: world.asks.filter((a) => a.issue_key === issue.key),
  };
}

// A goal plan an agent drafted (see HAR-18's story); the person decides it in the Inbox.
const PLANS = new Map<string, GoalPlanView>([
  [
    "HAR-18",
    {
      id: 1,
      issue_key: "HAR-18",
      project: "HAR",
      plan: {
        sentence: "let guests change their own bookings",
        title: "Guests can change a booking themselves",
        outcome: "A guest moves or cancels their own booking from the confirmation email, within the house rules, and the front desk only hears about it.",
        target_date: null,
        issues: [
          { ref: "screens", title: "Design the manage-booking screens", kind: "feature", body: "Move, cancel and the confirmation, on a phone first.", evidence: "none yet", priority: 2, size: "S", no_repo_changes: "the screens are drawn in the design tool" },
          { ref: "link", title: "Signed manage-booking link in the confirmation email", kind: "feature", body: "Protect the confirmation link contract: a valid signed link opens only its booking; expired or changed tokens fail safely. Reuse the existing booking scenario where practical.", evidence: "The guest flow currently stops at the confirmation email.", priority: 2, size: "M", touches: ["src/email/confirmation.ts", "src/bookings/links.ts"] },
          { ref: "page", title: "Manage-booking page: move to another free date", kind: "feature", body: "b", evidence: "none yet", priority: 2, size: "L", blocked_by: ["link"], touches: ["src/pages/ManageBooking.tsx", "src/bookings/availability.ts"] },
          { ref: "cancel", title: "Cancel within the free-cancellation window", kind: "feature", body: "b", evidence: "none yet", priority: 2, size: "S", blocked_by: ["link"], touches: ["src/bookings/cancel.ts"] },
          { ref: "notify", title: "Tell the front desk about every change", kind: "feature", body: "b", evidence: "none yet", priority: 3, size: "S", blocked_by: ["page", "cancel"], touches: ["src/desk/notify.ts"] },
        ],
        existing: ["HAR-14"],
      },
      status: "pending",
      created_by: "claude:c:/development/harbor",
      created_at: NOW - 1.2 * HOUR,
      outcome: null,
      goal_id: null,
    },
  ],
]);

/** As nitely-core's approve_goal_plan: the plan is decided and its issue closed (the preview files nothing). */
function approveGoalPlan(args: Record<string, unknown>) {
  const plan = PLANS.get(String(args.key));
  if (!plan || plan.status !== "pending") throw `${String(args.key)} carries no pending goal plan`;
  const edit = (args.edit ?? {}) as { title?: string; drop?: string[]; bodies?: Record<string, string> };
  const kept = plan.plan.issues.filter((i) => !(edit.drop ?? []).includes(i.ref)).length + plan.plan.existing.length;
  for (const item of plan.plan.issues) { if (edit.bodies?.[item.ref]) item.body = edit.bodies[item.ref]; }
  plan.status = "approved";
  plan.outcome = `Goal "${edit.title || plan.plan.title}" with ${kept} issues`;
  moveIssue({ key: plan.issue_key, state: "Done", evidence: `Approved: ${plan.outcome}.` });
  return { outcome: plan.outcome };
}

/** As nitely-core's start_goal: the planning issue an agent picks up (the preview only says so). */
function startGoal(args: Record<string, unknown>) {
  const sentence = String(args.sentence ?? "").trim();
  if (!sentence) throw "say in a sentence what the goal is";
  const n = world.issues.filter((i) => i.project === String(args.project)).length + 100;
  return { ...world.issues[0], key: `${String(args.project)}-${n}`, title: `Plan a goal: ${sentence}`, state: "Todo", state_type: "unstarted" };
}

// Playbook changes agents proposed, by the issue that carries them (see HAR-15's story).
const PROPOSALS = new Map<string, PlaybookProposal>([
  [
    "HAR-15",
    {
      id: 1,
      issue_key: "HAR-15",
      repo: "harbor",
      path: "C:\\Users\\you\\AppData\\Roaming\\nitely\\playbooks\\harbor.toml",
      in_repo: false,
      why: "The gate has no commands, so every change lands untested. package.json has test and build scripts, and CI runs both (.github/workflows/ci.yml:18). Billing code under api/billing/ should wait for a person.",
      status: "pending",
      created_by: "claude:c:/development/harbor",
      created_at: NOW - 1.6 * HOUR,
      decided_by: null,
      decided_at: null,
      outcome: null,
      diff: [
        { op: " ", text: "[worktree]" },
        { op: " ", text: 'setup = ["npm ci"]' },
        { op: " ", text: "" },
        { op: " ", text: "[gate]" },
        { op: " ", text: "# Must pass before a change can land." },
        { op: "-", text: "commands = []" },
        { op: "+", text: 'commands = ["npm test", "npm run build"]' },
        { op: "+", text: "" },
        { op: "+", text: "[approval]" },
        { op: "+", text: 'human_paths = [{ path = "^api/billing/", why = "money" }]' },
      ],
      stale: null,
    },
  ],
]);

/** Adds or removes a blocker, the way set_blocker does, and returns the issue's new relations. */
function setBlocker(args: Record<string, unknown>) {
  const key = String(args.key).toUpperCase();
  const blocker = String(args.blocker).toUpperCase();
  if (!world.issues.some((i) => i.key === key)) throw `no issue ${key}`;
  if (!world.issues.some((i) => i.key === blocker)) throw `no issue ${blocker}`;
  if (key === blocker) throw "an issue cannot wait on itself";
  const at = BLOCKERS.findIndex(([a, b]) => a === key && b === blocker);
  if (args.blocked && at < 0) BLOCKERS.push([key, blocker]);
  if (!args.blocked && at >= 0) BLOCKERS.splice(at, 1);
  for (const i of world.issues) {
    i.blocked_by = [];
    i.blocks = [];
    i.urgency = i.priority;
  }
  link(world);
  return detail(key).relations;
}

// ----- projects: the same rules as crates/nitely-core/src/projects.rs -----

function projectOf(key: unknown): Project {
  const p = world.projects.find((x) => x.key === String(key).toUpperCase());
  if (!p) throw `no project ${String(key)}`;
  return p;
}

// What submit and land keep out of each project's repos (crates/nitely-core/src/private_terms.rs).
let PRIVATE_TERMS: { all: string[]; projects: Record<string, string[]> } = { all: ["C:\\Users\\sam\\notes"], projects: { NIT: ["Bluebird", "re:CLIENT-\\d+"] } };

function setPrivateTerms(args: Record<string, unknown>) {
  const t = args.terms as typeof PRIVATE_TERMS;
  const tidy = (l: string[]) => [...new Set(l.map((x) => x.trim()).filter(Boolean))];
  const bad = [t.all, ...Object.values(t.projects)].flat().find((x) => {
    if (!x.startsWith("re:")) return false;
    try {
      new RegExp(x.slice(3));
      return false;
    } catch {
      return true;
    }
  });
  if (bad !== undefined) throw new Error("a private term is not a valid regular expression");
  const projects = Object.fromEntries(Object.entries(t.projects).map(([k, l]) => [k.toUpperCase(), tidy(l)]).filter(([, l]) => l.length));
  PRIVATE_TERMS = { all: tidy(t.all), projects };
  return PRIVATE_TERMS;
}

// A beta build with the next beta downloaded and ready (src-tauri/src/update.rs).
let UPDATE = {
  version: "0.2.0-beta.1",
  build_channel: "beta",
  channel: "beta",
  channels: [
    { name: "stable", warning: "You run a beta build. The stable channel's newest version can be older than this one; Nitely updates to it only once it is newer." },
    { name: "beta", warning: null },
    { name: "nightly", warning: null },
  ],
  disabled: null as string | null,
  checking: false,
  checked_at: NOW - 2 * HOUR as number | null,
  available: { version: "0.2.0-beta.2", notes_url: "https://github.com/frontiermaxxing/Nitely/releases/tag/v0.2.0-beta.2", ready: true } as { version: string; notes_url: string; ready: boolean } | null,
  waiting_for_lands: [] as string[],
  error: null as string | null,
};

function renameProject(args: Record<string, unknown>) {
  const p = projectOf(args.key);
  const name = String(args.name ?? "").trim();
  if (!name) throw "a project needs a name";
  p.name = name;
  return p;
}

function rekeyProject(args: Record<string, unknown>) {
  const p = projectOf(args.key);
  const to = String(args.newKey ?? "").trim().toUpperCase();
  if (!/^[A-Z][A-Z0-9]{1,9}$/.test(to)) throw "project key must be 2-10 letters or digits, starting with a letter (e.g. NIT)";
  if (to === p.key) throw `${p.key} already has that key`;
  if (world.projects.some((x) => x.key === to) || world.issues.some((i) => i.key.startsWith(`${to}-`))) throw `${to} is already a project or an issue prefix`;
  const busy = world.issues.filter((i) => i.project === p.key && i.state_type === "started").map((i) => i.key);
  if (busy.length) throw `${busy.join(", ")} ${busy.length === 1 ? "is" : "are"} checked out in a worktree whose branch names ${p.key}; land or close ${busy.length === 1 ? "it" : "them"} first`;
  const from = p.key;
  const move = (k: string) => (k.startsWith(`${from}-`) ? `${to}${k.slice(from.length)}` : k);
  let issues = 0;
  for (const i of world.issues) {
    if (i.project !== from) continue;
    issues += 1;
    i.key = move(i.key);
    i.project = to;
    i.blocked_by = i.blocked_by.map(move);
    i.blocks = i.blocks.map(move);
  }
  for (const m of [world.timelines, world.reviews, world.prs] as Map<string, unknown>[]) {
    for (const k of [...m.keys()].filter((k) => k.startsWith(`${from}-`))) {
      m.set(move(k), m.get(k));
      m.delete(k);
    }
  }
  p.key = to;
  return { from, to, issues, repos: 0, machines: MACHINES.filter((m) => m.project === from).map((m) => (m.project = to)).length, warnings: [] };
}

function removeProject(args: Record<string, unknown>) {
  const p = projectOf(args.key);
  const own = world.issues.filter((i) => i.project === p.key);
  const confirmed = String(args.confirm ?? "").trim().toUpperCase() === p.key;
  if (own.length && !confirmed) throw `${p.key} has ${own.length} issue(s); remove it with its issues, or move them first`;
  world.issues = world.issues.filter((i) => i.project !== p.key);
  world.projects = world.projects.filter((x) => x !== p);
  return { key: p.key, issues: own.length, repos: 0, machines: 0, warnings: [] };
}

function moveIssue(args: Record<string, unknown>) {
  const issue = world.issues.find((i) => i.key === args.key);
  if (!issue) throw `no issue ${String(args.key)}`;
  const to = String(args.state);
  if (to === "Done" && !args.evidence) throw "Done needs evidence: what ran and what it showed";
  if ((to === "Canceled" || to === "Duplicate") && !args.reason) throw `${to} needs a reason`;
  if (to === "Done") {
    // As nitely-core's check_move (NIT-220): a change submitted and not landed since is not on the
    // integration branch, and an open question is answered first.
    const events = world.timelines.get(issue.key) ?? [];
    const lastOf = (kind: string) => events.map((e) => e.kind).lastIndexOf(kind);
    const landed = lastOf("landed");
    if (lastOf("submit") > landed) {
      throw `${issue.key}'s change was submitted and never landed, so it is not on the integration branch: land it, or Cancel the issue with a reason to close it without landing`;
    }
    const open = world.asks.filter((a) => a.issue_key === issue.key && a.answer === null && !a.closed).length;
    if (open > 0 && landed < 0) throw `${issue.key} has ${open} open question${open === 1 ? "" : "s"}: answer ${open === 1 ? "it" : "them"} first (Cancel the issue with a reason to drop ${open === 1 ? "it" : "them"} with it)`;
  }
  const ts = Math.floor(Date.now() / 1000);
  const why = (args.evidence ?? args.reason) as string | null;
  world.timelines.get(issue.key)!.push({ ts, actor: "human", kind: "issue_moved", summary: `${issue.state} → ${to}${why ? `: ${why}` : ""}` });
  // Approving a proposed playbook change applies it (the playbook Nitely keeps) and closes the issue.
  const plan = PLANS.get(issue.key);
  if (plan?.status === "pending" && to === "Canceled") {
    plan.status = "rejected";
    plan.outcome = String(args.reason);
  }
  const proposal = PROPOSALS.get(issue.key);
  const applies = proposal?.status === "pending" && to === "Approved";
  if (proposal?.status === "pending" && (applies || to === "Canceled")) {
    proposal.status = applies ? "applied" : "rejected";
    proposal.outcome = applies ? `Applied to ${proposal.path}` : String(args.reason);
  }
  const landed = applies ? "Done" : to;
  issue.state = landed;
  issue.state_type = STATE_TYPE[landed] ?? issue.state_type;
  issue.updated_at = iso(ts);
  if (issue.state_type !== "started") issue.claimed_by = null;
  retrack(world, issue);
  recount(world);
}

/** As nitely-core's set_owner, done by the human: handing work back from Needs You puts it in Todo. */
function setOwner(args: Record<string, unknown>) {
  const issue = world.issues.find((i) => i.key === args.key);
  if (!issue) throw `no issue ${String(args.key)}`;
  const owner = String(args.owner);
  if (owner === issue.owner) return issue;
  const ts = Math.floor(Date.now() / 1000);
  const before = issue.state;
  if (owner === "human" && issue.state !== "Backlog" && issue.state_type !== "completed" && issue.state_type !== "canceled") issue.state = "Needs You";
  if (owner === "agent" && issue.state !== "Backlog" && issue.state_type !== "completed" && issue.state_type !== "canceled") issue.state = "Todo";
  issue.state_type = STATE_TYPE[issue.state] ?? issue.state_type;
  issue.owner = owner;
  issue.claimed_by = null;
  issue.updated_at = iso(ts);
  world.timelines.get(issue.key)!.push({ ts, actor: "human", kind: "owner_changed", summary: owner === "human" ? "Made the human's own work" : "Handed back to agents" });
  if (issue.state !== before) world.timelines.get(issue.key)!.push({ ts, actor: "human", kind: "issue_moved", summary: `${before} → ${issue.state}` });
  retrack(world, issue);
  recount(world);
  return issue;
}

/** An ask as its link shows it (NIT-546): one the preview has, or ask 69, answered and taken up. */
function askDetail(id: number): Ask {
  const ask = world.asks.find((a) => a.id === id);
  if (ask) return ask;
  if (id !== 69) throw `no ask ${id}`;
  const at = Math.floor(Date.now() / 1000) - 3 * 3600;
  return {
    id: 69,
    issue_key: "NIT-29",
    asked_by: "claude:c:/code/worktrees/nit-29",
    question: "Delete the three groups of slow tests their pure replacements now cover?",
    options: ["Approve all three groups", "Keep them for now"],
    answer: "Approve all three groups",
    asked_at: at,
    answered_at: at + 1800,
    delivered_to: "codex:c:/code/worktrees/nit-29",
    delivered_at: at + 2400,
  };
}

function answerAsk(args: Record<string, unknown>) {
  const ask = world.asks.find((a) => a.id === args.id);
  if (!ask) throw "no such question";
  const ts = Math.floor(Date.now() / 1000);
  ask.answer = String(args.answer);
  ask.answered_at = ts;
  if (ask.issue_key) world.timelines.get(ask.issue_key)?.push({ ts, actor: "human", kind: "answer", summary: ask.answer });
  const issue = world.issues.find((i) => i.key === ask.issue_key);
  if (issue) retrack(world, issue);
}

/**
 * For checking live changes in the preview from the console: `nitelyDev.ask("NIT-28", "Keep the
 * flag?")` adds an open question as an agent would, and the app hears of it the way it hears the
 * backend's change event.
 */
(window as unknown as { nitelyDev: unknown }).nitelyDev = {
  ask(issueKey: string, question: string, options: string[] = []) {
    const id = Math.max(0, ...world.asks.map((a) => a.id)) + 1;
    world.asks.push({ id, issue_key: issueKey, asked_by: "claude:c:/dev/preview", question, options, answer: null, asked_at: Math.floor(Date.now() / 1000), answered_at: null });
    window.dispatchEvent(new CustomEvent("nitely://changed"));
    return id;
  },
};

const AGENTS: AgentPlan[] = ["claude", "codex", "cursor"].map((agent) => ({
  agent,
  found: true,
  changes: [
    { target: `~/.${agent}/skills/nitely/SKILL.md`, action: "unchanged", why: "The nitely skill: how to take, report, submit and land work", after: "" },
    { target: agent === "codex" ? "~/.codex/config.toml" : `~/.${agent}/settings.json`, action: "unchanged", why: "Hooks that report sessions to Nitely", after: "" },
    { target: agent === "codex" ? "~/.codex/config.toml" : `~/.${agent}.json`, action: "unchanged", why: "The nitely MCP server", after: "" },
  ],
}));

function connectPlan(args: Record<string, unknown>): ConnectPlan {
  const path = String(args.path ?? "");
  const name = (args.name as string | null) || path.split(/[\\/]/).filter(Boolean).pop() || "Project";
  const key = (args.key as string | null) || name.replace(/[^A-Za-z]/g, "").slice(0, 3).toUpperCase() || "NEW";
  return {
    root: path,
    repo_name: name,
    project_key: key,
    project_name: name,
    project_exists: world.projects.some((p) => p.key === key),
    integration_branch: "main",
    playbook: {
      source: "generated",
      path: `%APPDATA%\\nitely\\playbooks\\${key.toLowerCase()}.toml`,
      text: `[branch]\nintegration = "main"\n\n[worktree]\nsetup = ["npm ci"]\n\n[gate]\n# Must pass before a change can land.\ncommands = ["npm test", "npm run build"]\n\n[approval]\n# Every change waits for you until this project's review rules are approved. The setup\n# sweep drafts them; approving its proposal puts them in place of this line.\nalways_human = true\n`,
    },
    nitely: "C:\\Users\\you\\.cargo\\bin\\nitely.exe",
    agents: AGENTS.map((a) => ({
      ...a,
      changes: [{ target: `${path}\\AGENTS.md`, action: "update" as const, why: "Adds how this repo's agents take work from Nitely", after: "## Working with Nitely\n\nTake work with next_task, report as you go, submit when done.\n" }],
    })),
    warnings: [],
  };
}

function connectApply(args: Record<string, unknown>): Applied {
  const plan = connectPlan(args);
  if (!world.projects.some((p) => p.key === plan.project_key)) world.projects.push({ key: plan.project_key, name: plan.project_name, open_issues: 0 });
  const done = [`created project ${plan.project_key} (${plan.project_name}) on the Fast review mode`, `watching ${plan.root} for project ${plan.project_key}`];
  if (!plan.project_exists) done.push(`filed ${plan.project_key}-1: an agent sweeps the repo, seeds the board and drafts the review rules`);
  return { project: plan.project_key, repo: plan.root, done, failed: [] };
}

// ----- Activity: the night each agent had, from the same stories -----

/** States were recorded from here on; earlier the preview shows only that a session was active. */
const RECORDED_SINCE = NOW - 19 * HOUR;

const MARK_OF: Record<string, ActivityMark["kind"] | undefined> = {
  claimed: "start",
  gate_passed: "gate_pass",
  gate_failed: "gate_fail",
  review_failed: "review_fail",
  landed: "land",
  land_failed: "land_fail",
  ask: "ask",
  handoff: "handoff",
};

function markKind(t: TimelineEntry): ActivityMark["kind"] | undefined {
  if (t.kind === "submit") return t.summary?.includes("requested changes") ? "review_changes" : "review_pass";
  return MARK_OF[t.kind];
}

/** What the agent is doing after an event, until its next one. */
function stateAfter(t: TimelineEntry): ActivitySegment["state"] | "ended" {
  if (t.kind === "ask") return "waiting";
  if (t.kind === "landed") return "ended";
  if (t.kind === "land_failed") return "idle";
  if (t.kind === "submit" && !t.summary?.includes("requested changes")) return "idle";
  return "working";
}

/** One lane per issue an agent worked on, built from its timeline so marks and the track agree. */
function laneOf(spec: IssueSpec): ActivityLane | null {
  if (!spec.vendor) return null;
  const agent = `${spec.vendor}:c:/code/${repoOf(spec.key).name.toLowerCase()}-worktrees/${spec.key.toLowerCase()}`;
  const own = world.timelines.get(spec.key)!.filter((t) => t.actor === agent);
  const start = own.find((t) => t.kind === "claimed");
  if (!start) return null;
  const events = own.filter((t) => t.ts >= start.ts && t.kind !== "worktree" && t.kind !== "issue_moved");
  const segments: ActivitySegment[] = [];
  const span = (from: number, to: number, state: ActivitySegment["state"]) => {
    // Work that runs on for hours with nothing recorded goes quiet, as it does in the app.
    const quietAfter = state === "working" ? 80 * MIN : state === "idle" ? 30 * MIN : Infinity;
    const busyTo = Math.min(to, from + quietAfter);
    if (busyTo > from) segments.push({ from, to: busyTo, state });
    if (to > busyTo) segments.push({ from: busyTo, to, state: "stale" });
  };
  let state = "working" as ActivitySegment["state"] | "ended";
  for (const [n, t] of events.entries()) {
    state = stateAfter(t);
    const next = events[n + 1]?.ts;
    if (state !== "ended" && next !== undefined) span(t.ts, next, state);
  }
  const last = events[events.length - 1];
  const live = spec.live;
  const lastSeen = live ? Math.round(NOW - live.seen * MIN) : last.ts + (state === "ended" ? 4 * MIN : 30 * MIN);
  if (live && live.state !== "ended") span(last.ts, NOW, live.state);
  else if (state !== "ended") span(last.ts, lastSeen, state);

  const marks: ActivityMark[] = [];
  for (const t of events) {
    const kind = markKind(t);
    if (!kind) continue;
    // A commit or two before every gate.
    if (kind === "gate_pass" || kind === "gate_fail") {
      for (const back of rand() > 0.5 ? [7, 31] : [9]) {
        if (t.ts - back * MIN > start.ts) marks.push({ ts: t.ts - back * MIN, kind: "commit", issue_key: spec.key, summary: "git commit" });
      }
    }
    marks.push({ ts: t.ts, kind, issue_key: spec.key, summary: t.summary ?? "" });
  }
  marks.sort((a, b) => a.ts - b.ts);
  return {
    session_id: `${spec.vendor}:${fakeSession(spec.key)}`,
    vendor: spec.vendor,
    cwd: worktreeOf(spec),
    project: repoOf(spec.key).project,
    place: worktreeOf(spec).split("\\").pop()!,
    issues: [spec.key],
    started_at: start.ts,
    last_seen_at: lastSeen,
    state: live?.state ?? (state === "ended" || NOW - lastSeen > 30 * MIN ? "ended" : (state as SessionState)),
    segments,
    marks,
  };
}

const LANES = ISSUES.map(laneOf).filter((l): l is ActivityLane => l !== null);

/** Before RECORDED_SINCE only activity is known, so those stretches read "seen". */
function recordedOnly(segments: ActivitySegment[]): ActivitySegment[] {
  return segments.flatMap((s): ActivitySegment[] => {
    if (s.to <= RECORDED_SINCE) return s.state === "stale" ? [] : [{ ...s, state: "seen" }];
    if (s.from >= RECORDED_SINCE) return [s];
    return [...(s.state === "stale" ? [] : [{ from: s.from, to: RECORDED_SINCE, state: "seen" as const }]), { ...s, from: RECORDED_SINCE }];
  });
}

const STATE_RANK: Record<string, number> = { waiting: 0, working: 1, idle: 2, ended: 3 };
const PROJECT_RANK = (p: string | null) => REPOS.findIndex((r) => r.project === p);

function activity(args: Record<string, unknown>): Activity {
  const hours = Number(args.hours ?? 12);
  const from = NOW - hours * HOUR;
  const lanes = LANES.map((l): ActivityLane => ({
    ...l,
    segments: recordedOnly(l.segments)
      .map((s) => ({ ...s, from: Math.max(s.from, from), to: Math.min(s.to, NOW) }))
      .filter((s) => s.to > s.from),
    marks: l.marks.filter((m) => m.ts >= from && m.ts <= NOW),
  }))
    .filter((l) => l.segments.length > 0 || l.marks.length > 0)
    .sort(
      (a, b) =>
        PROJECT_RANK(a.project) - PROJECT_RANK(b.project) || STATE_RANK[a.state] - STATE_RANK[b.state] || b.last_seen_at - a.last_seen_at,
    );
  const keys = new Set(lanes.flatMap((l) => l.issues));
  return { from, to: NOW, recorded_since: RECORDED_SINCE, lanes, issues: world.issues.filter((i) => keys.has(i.key)) };
}

const delay = (ms: number) => new Promise((r) => setTimeout(r, ms));

/** Machines allowed through `nitely serve` in the preview; the Mac is the one on the Now board. */
const MACHINES: { name: string; project: string | null; created_at: number }[] = [
  { name: "mac", project: "ORB", created_at: NOW - 3 * 24 * HOUR },
  { name: "cloud-box", project: null, created_at: NOW - 9 * 24 * HOUR },
];

// Briefs: NIT has had its setup sweep and a brief an agent wrote and you edited; HAR has a brief
// you started by hand but no sweep; ORB has neither, so the empty state shows.
const BRIEFS: Record<string, Pick<ProjectBrief, "brief" | "revision" | "updated_at" | "updated_by" | "setup">> = {
  NIT: {
    brief: [
      "# Vision",
      "",
      "*Inferred from the code and docs/vision.md, not confirmed by the owner.* One person runs a team of coding agents from a desktop app: agents take issues, work in their own worktrees, get reviewed by another vendor and land one at a time, and the human sees at a glance what needs them.",
      "",
      "# State of each area",
      "",
      "| Area | State |",
      "|---|---|",
      "| Core rules (`crates/nitely-core`) | Solid: issues, blockers, gate, review, land. Well tested. |",
      "| MCP server | Complete for the loop. |",
      "| App views | Overview, Inbox, Now and Activity; the board and branch map are next ([NIT-28](nitely://issue/NIT-28)). |",
      "| Onboarding | Connect works; the board starts empty. |",
      "",
      "# Target order",
      "",
      "1. Onboarding a new repo: the setup sweep and drafted review rules.",
      "2. Per-project review and landing settings.",
      "3. Agents on other machines.",
      "",
      "# Questions for the owner",
      "",
      "- [x] Should agents ever approve their own playbook changes? **No.**",
      "- [ ] Is Bitbucket still needed once the last mirrored project moves?",
      "- [ ] Which agents should review which by default?",
    ].join("\n"),
    revision: 3,
    updated_at: NOW - 5 * HOUR,
    updated_by: "human",
    setup: { sha: "4f0a3ec9b2d1e7a8c6f5", at: NOW - 2 * 24 * HOUR, by: "claude:c:/code/nitely" },
  },
  HAR: {
    brief: "# Harbor\n\nPhoto sharing for small teams. Uploads move to signed URLs before anything else ships.\n",
    revision: 1,
    updated_at: NOW - 26 * HOUR,
    updated_by: "human",
    setup: null,
  },
};

// Goals: two in Nitely (one due soon, one late) and one in Harbor; Orbit has none, so its page
// shows no goal row.
const dateIn = (days: number) => {
  const d = new Date(Date.now() + days * 86400000);
  return `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, "0")}-${String(d.getDate()).padStart(2, "0")}`;
};
const GOALS: { id: number; project: string; title: string; outcome: string; target_date: string | null; state?: Goal["state"]; keys: string[] }[] = [
  {
    id: 1,
    project: "NIT",
    title: "One glance says what needs you",
    outcome: "You open Nitely and know in a few seconds what waits on you, what moves and what is stuck, on any view.",
    target_date: dateIn(12),
    keys: ["NIT-24", "NIT-25", "NIT-31", "NIT-30", "NIT-29", "NIT-44", "NIT-26", "NIT-28"],
  },
  {
    id: 2,
    project: "NIT",
    title: "Reviews and lands need no babysitting",
    outcome: "A submitted change is reviewed, retried when the reviewer stalls, and landed in order without anyone watching.",
    target_date: dateIn(-3),
    keys: ["NIT-19", "NIT-27", "NIT-33", "NIT-21", "NIT-20"],
  },
  {
    id: 3,
    project: "HAR",
    title: "Guests book without calling",
    outcome: "A guest finds a free date, books it and gets a receipt in their language, on a phone, without phoning the front desk.",
    target_date: null,
    keys: ["HAR-14", "HAR-13", "HAR-12", "HAR-16", "HAR-9", "HAR-8"],
  },
  {
    // Reached: it has no row any more, but an issue still in it names it in the issue panel.
    id: 4,
    project: "NIT",
    title: "Every agent's session is on the record",
    outcome: "Each session of every agent shows from its start to its end, whichever vendor ran it.",
    target_date: null,
    state: "done",
    keys: ["NIT-22"],
  },
  {
    id: 5,
    project: "HAR",
    title: "Publish the guest guide",
    outcome: "Guests can find arrival instructions and the house rules before their stay.",
    target_date: null,
    state: "done",
    keys: [],
  },
];

/** Puts the fixture issues in their goals; the world is built before GOALS is defined. */
function placeGoals() {
  for (const g of GOALS) for (const key of g.keys) {
    const i = world.issues.find((x) => x.key === key);
    if (i && i.goal === undefined) i.goal = g.id;
  }
}

function goalRecord(g: (typeof GOALS)[number]): Goal {
  const issues = world.issues.filter((i) => i.goal === g.id);
  const done = issues.filter((i) => i.state_type === "completed").length;
  const dropped = issues.filter((i) => i.state_type === "canceled").length;
  return {
    id: g.id,
    project: g.project,
    title: g.title,
    outcome: g.outcome,
    target_date: g.target_date,
    state: g.state ?? "active",
    created_by: "human",
    created_at: NOW - 5 * 24 * HOUR,
    updated_at: NOW - 5 * 24 * HOUR,
    progress: { total: issues.length, done, dropped, open: issues.length - done - dropped },
    // The crew on it: one worker per issue an agent moves, and what of it landed today.
    working: issues.filter((i) => i.state_type === "started").length,
    rank: goalRanks.get(g.id) ?? null,
    landed_today: Math.min(done, 2),
    branch: goalBranches.get(g.id),
  };
}

/** Goal branches (goal_branch.rs): one green with its next checkpoint, one red waiting on its fix. */
const goalBranches = new Map<number, Goal["branch"]>([
  [2, { branch: "goal-2", created: true, state: "green", items: 4, next_checkpoint: "at 10 items (6 to go)", checkpoint_at: NOW - 26 * HOUR, landed_items: 11, checkpoints: 1, full_runs: 1, reviews: 1 }],
  [3, { branch: "goal-3", created: true, state: "red", red_reason: "main at 8c41f0e conflicts in src/booking/slots.ts", red_issue: "HAR-31", items: 2, next_checkpoint: "at 10 items (8 to go)", landed_items: 2, checkpoints: 0, full_runs: 0, reviews: 0 }],
]);

/** The person's order of goals (goal_order): rank by goal id. */
const goalRanks = new Map<number, number>();

// Nitely is concentrating on review automation; Harbor still has a completed focus selected.
const projectFocuses = new Map<string, { goal: number | null; allow_urgent: boolean }>([
  ["NIT", { goal: 2, allow_urgent: true }],
  ["HAR", { goal: 5, allow_urgent: false }],
]);

function projectFocus(project: string): ProjectFocus {
  const selected = projectFocuses.get(project);
  const goal = GOALS.find((g) => g.id === selected?.goal && g.project === project);
  return {
    project,
    goal: selected?.goal ?? null,
    goal_title: goal?.title ?? null,
    allow_urgent: selected?.allow_urgent ?? true,
    active: !!goal && (goal.state ?? "active") === "active",
  };
}

function setProjectFocus(args: Record<string, unknown>): ProjectFocus {
  const project = String(args.project).toUpperCase();
  const goal = args.goal == null ? null : Number(args.goal);
  if (goal !== null && !GOALS.some((g) => g.id === goal && g.project === project && (g.state ?? "active") === "active")) {
    throw new Error("Choose an active goal in this project.");
  }
  projectFocuses.set(project, { goal, allow_urgent: Boolean(args.allowUrgent) });
  return projectFocus(project);
}

/** As the goals command: the active goals, with their issues, ranked ones first. */
function goals(args: Record<string, unknown>): GoalDetail[] {
  placeGoals();
  const project = args.project ? String(args.project).toUpperCase() : null;
  const rank = (id: number) => goalRanks.get(id) ?? Number.MAX_SAFE_INTEGER;
  const closed = args.includeClosed === true;
  return GOALS.filter((g) => (!project || g.project === project) && (closed || (g.state ?? "active") === "active"))
    .sort((a, b) => rank(a.id) - rank(b.id))
    .map((g) => {
      const goal = goalRecord(g);
      const mine = world.issues.filter((i) => i.goal === g.id);
      return { goal, issues: mine, risks: goalRisks(mine), finish: goalFinish(g.id, goal.progress.open) };
    });
}

// As crates/nitely-core/src/goal_report.rs (NIT-247).

/** A goal's open issues that may hold it up, the most pressing reason each. */
function goalRisks(mine: IssueRecord[]): GoalRisk[] {
  return mine
    .filter((i) => i.state_type !== "completed" && i.state_type !== "canceled")
    .flatMap((i) => {
      const t = i.track;
      const why = t?.stuck ? `stuck: ${t.stuck}` : i.state === "Needs You" ? "waits on you" : t?.asking ? "asked you something" : (t?.rounds ?? 0) >= 3 ? `${t!.rounds} review rounds` : null;
      return why ? [{ key: i.key, title: i.title, why }] : [];
    });
}

const landedEvents = () => [...world.timelines.entries()].flatMap(([key, tl]) => tl.filter((t) => t.kind === "landed").map((t) => ({ key, ts: t.ts })));

/** When a goal will be done at its last three days' pace. */
function goalFinish(id: number, open: number): GoalFinish | null {
  if (open <= 0) return null;
  const now = Math.floor(Date.now() / 1000);
  const keys = new Set(world.issues.filter((i) => i.goal === id).map((i) => i.key));
  const landed = new Set(landedEvents().filter((e) => keys.has(e.key) && e.ts >= now - 3 * 24 * HOUR).map((e) => e.key)).size;
  if (landed === 0) return null;
  const per_day = landed / 3;
  return { at: now + Math.round((open / per_day) * 24 * HOUR), per_day };
}

/** As the goal_report command: what happened since `since`, by goal. */
function goalReport(since: number): GoalReport {
  const active = goals({});
  const byKey = new Map<string, { key: string; title: string; ts: number }>();
  for (const e of landedEvents()) {
    if (e.ts < since) continue;
    const prev = byKey.get(e.key);
    if (!prev || prev.ts < e.ts) byKey.set(e.key, { key: e.key, title: world.issues.find((i) => i.key === e.key)?.title ?? e.key, ts: e.ts });
  }
  const landed = [...byKey.values()].sort((a, b) => a.ts - b.ts);
  const goalOf = (key: string) => world.issues.find((i) => i.key === key)?.goal;
  const inGoal = new Set(active.map((g) => g.goal.id));
  const needed: ReportNeeded[] = [
    ...world.asks.filter((a) => a.asked_at >= since).map((a) => ({
      ts: a.asked_at,
      kind: "asked" as const,
      issue_key: a.issue_key,
      summary: a.finding ? `accept a finding? ${a.question.split("\n")[0]}` : a.question.split("\n")[0],
      open: a.answer === null && !a.closed,
    })),
    ...[...world.timelines.entries()].flatMap(([key, tl]) =>
      tl
        .filter((t) => t.ts >= since && t.kind === "issue_moved" && t.actor !== "human" && (t.summary ?? "").includes("→ Needs You"))
        .map((t) => ({ ts: t.ts, kind: "approval" as const, issue_key: key, summary: (t.summary ?? "").split(": ")[1] ?? "waited for your approval", open: world.issues.find((i) => i.key === key)?.state === "Needs You" })),
    ),
  ].sort((a, b) => a.ts - b.ts || (a.issue_key ?? "").localeCompare(b.issue_key ?? ""));
  return {
    since,
    now: Math.floor(Date.now() / 1000),
    goals: active.map((g) => ({ goal: g.goal, landed: landed.filter((l) => goalOf(l.key) === g.goal.id), risks: g.risks ?? [], finish: g.finish ?? null })),
    other_landed: landed.filter((l) => !inGoal.has(goalOf(l.key) ?? -1)),
    needed,
    triaged: [
      { ts: Math.max(since, Math.floor(Date.now() / 1000) - 2 * HOUR), issue_key: "NIT-30", question: "Does NIT-29 already change the review track?", answer: "No: NIT-29 only adds the lanes; the track is NIT-30's." },
      { ts: Math.max(since, Math.floor(Date.now() / 1000) - HOUR), issue_key: "HAR-14", question: "Is the date picker shared with HAR-13?", answer: "Yes, src/booking/DatePicker.tsx." },
    ],
  };
}

/** As goal_order: the given goals first, the project's others after in their order. */
function goalOrder(args: Record<string, unknown>): Goal[] {
  const project = String(args.project).toUpperCase();
  const ids = (args.ids as number[]).map(Number);
  const rest = goals({ project }).map((g) => g.goal.id).filter((id) => !ids.includes(id));
  [...ids, ...rest].forEach((id, n) => goalRanks.set(id, n + 1));
  return goals({ project }).map((g) => g.goal);
}

/** As goal_choices: every goal of a project, closed ones too. */
function goalChoices(args: Record<string, unknown>): Goal[] {
  placeGoals();
  const project = String(args.project ?? "").toUpperCase();
  return GOALS.filter((g) => g.project === project).map(goalRecord);
}

/** As nitely-core's set_issue_goal: an active goal of the issue's own project, or none. */
function setIssueGoal(args: Record<string, unknown>): IssueRecord {
  placeGoals();
  const issue = world.issues.find((i) => i.key === String(args.key).toUpperCase());
  if (!issue) throw `no issue ${String(args.key)}`;
  const id = args.goal == null ? null : Number(args.goal);
  if (id != null) {
    const g = GOALS.find((x) => x.id === id);
    if (!g || g.project !== issue.project) throw `no goal ${id} in ${issue.project}`;
    if ((g.state ?? "active") !== "active") throw `goal ${id} is ${g.state}; reopen it first`;
  }
  issue.goal = id;
  return issue;
}

const REVIEW_LEVELS = ["inert", "contained", "standard", "sensitive"];

/** As nitely-core's set_review_level for the person: any level, or none. */
function setReviewLevel(args: Record<string, unknown>): IssueRecord {
  const issue = world.issues.find((i) => i.key === String(args.key).toUpperCase());
  if (!issue) throw `no issue ${String(args.key)}`;
  if (issue.origin !== "nitely") throw `${issue.key} is mirrored from ${issue.origin}; change it there`;
  const raw = args.level == null ? "" : String(args.level).trim();
  const level = raw === "" || raw === "undeclared" ? null : raw;
  if (level && !REVIEW_LEVELS.includes(level)) throw `review level is one of ${REVIEW_LEVELS.join(", ")}, not ${level}`;
  issue.review_level = level;
  issue.review_level_by = level ? "human" : null;
  issue.review_level_source = level ? "human" : null;
  issue.review_level_at = level ? Math.floor(Date.now() / 1000) : null;
  return issue;
}

function projectBrief(key: string): ProjectBrief {
  const p = world.projects.find((x) => x.key === key.toUpperCase());
  if (!p) throw `no project ${key}`;
  const b = BRIEFS[p.key] ?? { brief: "", revision: 0, updated_at: null, updated_by: null, setup: null };
  return { project: p.key, name: p.name, open_issues: world.issues.filter((i) => i.project === p.key && !["completed", "canceled"].includes(i.state_type)).length, ...b };
}

function setProjectBrief(args: Record<string, unknown>): ProjectBrief {
  const current = projectBrief(String(args.project));
  if (Number(args.revision) !== current.revision)
    throw `the brief changed since you read it (you read revision ${args.revision}; it is now ${current.revision} by ${current.updated_by ?? "someone"}). Read it again with get_project and merge your changes into it`;
  BRIEFS[current.project] = { brief: String(args.brief), revision: current.revision + 1, updated_at: Math.floor(Date.now() / 1000), updated_by: "human", setup: current.setup };
  return projectBrief(current.project);
}

/** The preview's stand-in for Tauri's `invoke`: same command names and argument names. */
export async function fixtureInvoke<T>(cmd: string, args: Record<string, unknown>): Promise<T> {
  // Each command the page sends, counted, for measuring how much a view reads (NIT-337):
  // `nitelyDev.calls` in the console. `?snapshotMs=3800` makes a snapshot take as long as a busy
  // machine's.
  const calls = ((window as unknown as { nitelyCalls?: Record<string, number> }).nitelyCalls ??= {});
  calls[cmd] = (calls[cmd] ?? 0) + 1;
  const snapshotMs = Number(new URLSearchParams(location.search).get("snapshotMs") ?? 0);
  await delay(cmd === "sync_now" ? 700 : cmd === "snapshot" && snapshotMs > 0 ? snapshotMs : 40);
  const result: unknown = (() => {
    switch (cmd) {
      case "snapshot":
        return snapshot();
      case "pulse":
        return pulse(Number(args.since), Number(args.dayStart));
      case "activity":
        return activity(args);
      case "opened_link":
        return null;
      case "projects":
        return world.projects;
      case "approve_goal_plan":
        return approveGoalPlan(args);
      case "start_goal":
        return startGoal(args);
      case "goals":
        return goals(args);
      case "goal_state": {
        const g = GOALS.find((x) => x.id === Number(args.id));
        if (g) g.state = String(args.state) as Goal["state"];
        return g ? goalRecord(g) : null;
      }
      case "project_focus":
        return projectFocus(String(args.project).toUpperCase());
      case "project_review_mode":
        return reviewMode(String(args.project).toUpperCase());
      case "project_review_mode_effective":
        return effectiveModes(String(args.project).toUpperCase());
      case "lands_pipeline":
        return landsPipeline(String(args.project).toUpperCase(), rulesFixture.rules(String(args.project)).map((v) => v.repo));
      case "lands_dry_run":
        return landsDryRun(String(args.project).toUpperCase(), String(args.path));
      case "set_project_review_mode":
        return saveReviewMode(String(args.project).toUpperCase(), args.mode as ReviewMode, args.overrides as ModeOptions, Number(args.expectedRevision));
      case "periodic_review_status":
        return periodicReviews(String(args.project).toUpperCase());
      case "retry_periodic_review":
        retryPeriodic(String(args.project).toUpperCase(), String(args.repo), Number(args.id)); return null;
      case "confirm_verification_escape":
      case "retry_verification_audit": {
        const report = structuredClone(verificationAudit("NIT")!);
        const job = report.jobs.find((job) => job.id === Number(args.id));
        if (!job) throw new Error("Audit not found");
        if (cmd === "retry_verification_audit") {
          if (job.state !== "failed") throw new Error("Only failed audits may be retried");
          job.state = "pending";
          job.error = null;
        } else {
          const rule = String(args.rule);
          const evidence = String(args.evidence).trim();
          if (job.state !== "findings" || !job.rules?.includes(rule) || !evidence || evidence.length > 16384) throw new Error("Choose an attributed rule and enter reproduction evidence");
          if (!report.escapes.some((escape) => escape.audit_id === job.id && escape.rule === rule)) {
            const alreadyConfirmed = report.escapes.some((escape) => escape.audit_id === job.id);
            report.escapes.push({ audit_id: job.id, rule, evidence });
            if (!report.disabled_rules.includes(rule)) report.disabled_rules.push(rule);
            for (const rate of report.rules.filter((rate) => rate.name === rule)) {
              rate.confirmed_escape_lands++;
              rate.audited_escape_rate = rate.confirmed_escape_lands / rate.audited;
            }
            if (!alreadyConfirmed) for (const rate of report.declarers) {
              rate.confirmed_escape_lands++;
              rate.audited_escape_rate = rate.confirmed_escape_lands / rate.audited;
            }
          }
        }
        auditReports.set("NIT", report);
        return report;
      }
      case "set_project_focus":
        return setProjectFocus(args);
      case "goal_report":
        return goalReport(Number(args.since));
      case "goal_order":
        return goalOrder(args);
      case "goal_choices":
        return goalChoices(args);
      case "set_issue_goal":
        return setIssueGoal(args);
      case "set_review_level":
        return setReviewLevel(args);
      case "project_brief":
        return projectBrief(String(args.project));
      case "mirror_preview":
        return mirrorPreview(String(args.project));
      case "set_mirror":
        mirrorOn.set(String(args.project).toUpperCase(), (args.backfill as string | null) ?? null);
        return mirrorPreview(String(args.project));
      case "issue_mirror":
        return issueMirror(String(args.key));
      case "set_issue_private":
        if (args.private) privateIssues.add(String(args.key));
        else privateIssues.delete(String(args.key));
        return issueMirror(String(args.key));
      case "held_files":
        return heldFiles(String(args.project));
      case "crew_status":
        return crewStatus(String(args.project));
      case "crew_settings":
        return crewSettings(String(args.project));
      case "crew_save":
        return crewSave(args);
      case "issue_spend":
        return issueSpend(args);
      case "project_spend":
        return projectSpend(args);
      case "crew_pause":
        return crewPause(args);
      case "open_crew_log":
        throw "The preview has no logs to open; the app opens them.";
      case "lanes_overview":
        return lanesOverview();
      case "leaderboard_stats":
        return leaderboardFixture();
      case "leaderboard_status":
        return leaderboardStatusFixture();
      case "leaderboard_join":
        return leaderboardJoinFixture(args);
      case "leaderboard_board":
        return leaderboardBoardFixture(args);
      case "leaderboard_send":
        return leaderboardSendFixture();
      case "leaderboard_delete":
        return leaderboardDeleteFixture(args);
      case "leaderboard_report":
        return leaderboardReportFixture(args);
      case "performance_query":
        return performanceFixture(args);
      case "set_performance_settings":
        return configurePerformance(args);
      case "clear_performance_history":
        return clearPerformance();
      case "set_machine_limits":
        return lanesOverview().machine;
      case "batches_overview":
        return batchesOverview();
      case "waits_overview":
        return waitsOverview();
      case "open_lane_log":
        throw "The preview has no logs to open; the app opens them.";
      case "set_project_brief":
        return setProjectBrief(args);
      case "issues":
        return issues(args);
      case "issue_detail":
        return detail(String(args.key));
      case "ask_detail":
        return askDetail(Number(args.id));
      case "move_issue":
        return moveIssue(args);
      case "set_blocker":
        return setBlocker(args);
      case "set_owner":
        return setOwner(args);
      case "answer_ask":
        return answerAsk(args);
      case "dismiss_notice":
        world.notices = world.notices.filter((n) => n.id !== args.id);
        return null;
      case "accept_finding":
        return answerAsk({ id: args.id, answer: `Accepted: ${String(args.reason)}` });
      case "decline_finding":
        return answerAsk({ id: args.id, answer: `Not accepted: ${String(args.why)}. Fix it.` });
      case "notify":
        // The preview's notifier: src/dev/FakeToasts.tsx shows it on the page.
        window.dispatchEvent(new CustomEvent("nitely:preview-notification", { detail: args }));
        return null;
      case "set_badge":
        (window as unknown as { nitelyBadge?: unknown }).nitelyBadge = args.count;
        return null;
      case "sync_now":
        return null;
      case "create_project":
        return null;
      case "rename_project":
        return renameProject(args);
      case "private_terms":
        return PRIVATE_TERMS;
      case "set_private_terms":
        return setPrivateTerms(args);
      case "rekey_project":
        return rekeyProject(args);
      case "remove_project":
        return removeProject(args);
      case "connect_plan":
        return connectPlan(args);
      case "connect_apply":
        return connectApply(args);
      case "update_status":
        return UPDATE;
      case "dismiss_approval_notice":
        return null;
      case "approval_notice":
        // `?approval-notice` shows the line a profile on its own data folder gets once.
        return new URLSearchParams(location.search).has("approval-notice")
          ? { text: "This profile has its own approval key now: approve waiting changes and pair phones again.", at: NOW }
          : null;
      case "check_for_update":
        UPDATE = { ...UPDATE, checked_at: NOW };
        return UPDATE;
      case "set_update_channel":
        UPDATE = { ...UPDATE, channel: String(args.channel), available: null, checked_at: NOW };
        return UPDATE;
      case "restart_to_update":
        UPDATE = { ...UPDATE, waiting_for_lands: ["Nitely"] };
        return UPDATE;
      case "agent_setup":
        return AGENTS;
      case "rules":
        return rulesFixture.rules(String(args.project));
      case "rules_check":
        return rulesFixture.rulesCheck(args);
      case "rules_save":
        return rulesFixture.rulesSave(args);
      case "reviewer_settings_preview":
        return rulesFixture.reviewerPreview(args);
      case "reviewer_settings_save":
        return rulesFixture.reviewerSave(args);
      case "rules_branches":
        return rulesFixture.rulesBranches(args);
      case "rules_dry_run":
        return rulesFixture.rulesDryRun(args);
      case "rule_defaults":
        return rulesFixture.ruleDefaults();
      case "rule_defaults_check":
        return rulesFixture.ruleDefaultsCheck(args);
      case "rule_defaults_save":
        return rulesFixture.ruleDefaultsSave(args);
      case "phone_status":
        return phoneFixture.phoneStatus();
      case "phone_tailscale":
        return phoneFixture.phoneTailscale();
      case "phone_tailscale_name":
        return phoneFixture.phoneTailscaleName();
      case "phone_start":
        return phoneFixture.phoneStart(args);
      case "phone_stop":
        return phoneFixture.phoneStop();
      case "phone_pair":
        return phoneFixture.phonePair();
      case "phone_decide":
        return phoneFixture.phoneDecide(args);
      case "phone_revoke":
        return phoneFixture.phoneRevoke(args);
      case "agent_setup_apply":
        return { done: [], failed: [] };
      case "links_service":
        return linksStatus();
      case "set_links_service":
        linksOn = Boolean(args.on);
        return linksStatus();
      case "network":
        return { tokens: MACHINES, urls: ["http://192.168.1.68:7421/mcp"], port: 7421 };
      case "network_add": {
        const name = String(args.name);
        MACHINES.splice(0, MACHINES.length, ...MACHINES.filter((m) => m.name !== name), { name, project: (args.project as string | null) ?? null, created_at: NOW });
        // A made-up token: the preview never makes real ones.
        const token = "nly_preview0000000000000000000000000000000000000000000000000000000";
        return { name, token, commands: [`claude mcp add --transport http nitely "http://192.168.1.68:7421/mcp" --header "Authorization: Bearer ${token}"`] };
      }
      case "network_remove": {
        const before = MACHINES.length;
        MACHINES.splice(0, MACHINES.length, ...MACHINES.filter((m) => m.name !== String(args.name)));
        return MACHINES.length < before;
      }
      case "agent_icons":
        // Nothing is installed in the preview: every agent keeps its letters.
        return {};
      default:
        throw `The preview has no fixture for "${cmd}". Add one in src/dev/fixtures.ts.`;
    }
  })();
  // Callers own what they get back, as they would a fresh copy from Tauri.
  return structuredClone(result) as T;
}

// The listener for issue links at login (Settings, Issue links): on until turned off.
let linksOn = true;
function linksStatus() {
  return {
    allowed: true,
    off: !linksOn,
    registered: linksOn,
    current: linksOn,
    answering: linksOn ? "service" : "app",
    place: "HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Run\\NitelyLinks",
    command: 'conhost.exe --headless "C:\\Users\\you\\.cargo\\bin\\nitely.exe" links serve',
    port: 7423,
  };
}
