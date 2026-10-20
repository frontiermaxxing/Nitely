import { useState } from "react";
import type { IssueDetail } from "./types";
import { issueDecision, type IssueDecision } from "./decisions";
import { automatedReviewEvidence } from "./approvalEvidence";
import { ChecksMark } from "./Pr";
import { ago, openExternal } from "./util";
import { AgentMark, FindingDots, ResultMark, SeverityMark, VENDOR_LABEL, journeyOf, vendorOf, type Round } from "./ui";
import "./issue.css";

/** What the person's cell in Who had it says, by what the issue waits on them for (decisions.ts). */
const WAITING_LABEL: Record<NonNullable<IssueDecision>, string> = { playbook: "Decide", "goal-plan": "Decide", own: "Yours to do", approve: "Approve?" };

const OUTCOME: Record<Round["outcome"], string> = {
  changes: "changes asked",
  passed: "passed",
  unreviewed: "no review",
  "review-failed": "did not finish",
  reviewing: "reviewing",
  open: "",
};

/**
 * What happened between Working and Done, under the issue's track: one row per round of gate and
 * review (a round that asked for changes loops into the next on a dashed rail), then the pull
 * request with its checks, then each land. A round opens to show its findings.
 */
export function IssueJourney(props: { detail: IssueDetail; now: number }) {
  const { detail, now } = props;
  const { rounds, lands } = journeyOf(detail.timeline, detail.reviews);
  // The latest round that was reviewed is open to start with: it is what the issue stands on.
  const latest = [...rounds].reverse().find((r) => r.review && (r.review.findings.length > 0 || r.review.reasons.length > 0));
  const [open, setOpen] = useState<number | null>(latest?.n ?? null);
  const prs = detail.prs;
  if (rounds.length === 0 && prs.length === 0 && lands.length === 0) return null;
  const waiting = detail.issue.state === "Needs You";

  return (
    <div className="journey" aria-label="Rounds, pull request and lands">
      {rounds.length > 0 && (
        <div className="journey-row journey-head" aria-hidden="true">
          <span />
          <span />
          <span>Gate</span>
          <span>Review</span>
          <span />
        </div>
      )}
      {rounds.map((r, i) => {
        const last = i === rounds.length - 1;
        const expandable = !!r.review && (r.review.findings.length > 0 || r.review.reasons.length > 0);
        const isOpen = expandable && open === r.n;
        const findings = r.review?.findings ?? [];
        const automated = automatedReviewEvidence(r.review);
        const head = (
          <>
            <span className={`journey-node journey-node-${r.outcome}${r.pushFailed ? " journey-node-stuck" : ""}`} />
            <span className="journey-label">Round {r.n}</span>
            <span className="journey-gates">
              {r.gates.map((g, k) => (
                g.skipped ? <span key={k} className="muted" title={g.summary ?? "Gate skipped"}>Skipped</span> : <ResultMark key={k} result={g.ok ? "pass" : "fail"} title={g.summary ?? (g.ok ? "Gate passed" : "Gate failed")} />
              ))}
              {r.gates.length === 0 && <span className="journey-none" />}
            </span>
            <span className="journey-review">
              {r.outcome !== "open" && (
                <span className={`journey-verdict journey-verdict-${r.outcome}`}>{r.reviewSkipped === "yolo" ? "skipped (YOLO)" : r.reviewSkipped === "disabled" ? "disabled" : OUTCOME[r.outcome]}</span>
              )}
              {automated.ran && <FindingDots findings={findings} />}
              {r.pushFailed && <span className="journey-stuck">push failed</span>}
              {last && waiting && r.waitsForHuman && <span className="attention-ink">waits for you</span>}
            </span>
            <span className="journey-time" title={when(r.end ?? r.start)}>
              {short(now, r.end ?? r.start)}
              {expandable && <Chevron open={isOpen} />}
            </span>
          </>
        );
        return (
          <div key={r.n} className={`journey-step${r.outcome === "changes" ? " journey-step-looped" : ""}${last && prs.length === 0 && lands.length === 0 ? " journey-step-last" : ""}`}>
            {expandable ? (
              <button
                type="button"
                className="journey-row journey-toggle"
                aria-expanded={isOpen}
                onClick={() => setOpen(isOpen ? null : r.n)}
              >
                {head}
              </button>
            ) : (
              <div className="journey-row">{head}</div>
            )}
            {isOpen && r.review && (
              <div className="journey-detail">
                <p className="journey-by">
                  {automated.ran ? `${VENDOR_LABEL[vendorOf(r.review.reviewer)]} reviewed` : automated.text}
                  {automated.ran && r.review.note ? ` (${r.review.note})` : ""}{" "}
                  <span className="mono">{r.review.commit_sha.slice(0, 7)}</span>
                  {r.review.approver === "human" ? "; it waits for a person." : "; no person needed."}
                </p>
                {r.review.reasons.length > 0 && (
                  <ul className={`journey-reasons${waiting && last ? " attention-ink" : ""}`}>
                    {r.review.reasons.map((x) => (
                      <li key={x}>{x}</li>
                    ))}
                  </ul>
                )}
                {findings.length > 0 && (
                  <ul className="journey-findings">
                    {[...findings]
                      .sort((a, b) => a.priority - b.priority)
                      .map((f, k) => (
                        <li key={k}>
                          <SeverityMark priority={f.priority} />
                          <span>
                            {f.text}
                            {f.carried && !f.now && <span className="muted"> Raised in an earlier round, not fixed since.</span>}
                            {f.now && <span className="muted"> Still open, the reviewer says: {f.now}</span>}
                          </span>
                        </li>
                      ))}
                  </ul>
                )}
              </div>
            )}
          </div>
        );
      })}
      {prs.map((pr, k) => (
        <div key={`${pr.provider}-${pr.repo}-${pr.number}`} className={`journey-step${k === prs.length - 1 && lands.length === 0 ? " journey-step-last" : ""}`}>
          <div className="journey-row">
            <span className={`journey-node journey-node-pr${pr.state === "MERGED" ? " journey-node-done" : ""}`} />
            <span className="journey-label">Pull request</span>
            <span className="journey-gates">
              <a href={pr.url} onClick={openExternal(pr.url)} className="mono">
                #{pr.number}
              </a>
            </span>
            <span className="journey-review">
              <span className={pr.state === "OPEN" ? "" : "muted"}>{pr.state.toLowerCase()}</span>
              {pr.checks && <ChecksMark checks={pr.checks} />}
              {pr.approved_by_me && <span className="muted">you approved</span>}
            </span>
            <span className="journey-time" title={when(Date.parse(pr.updated_at) / 1000)}>
              {short(now, Date.parse(pr.updated_at) / 1000)}
            </span>
          </div>
        </div>
      ))}
      {lands.map((l, i) => (
        <div key={i} className={`journey-step${i === lands.length - 1 ? " journey-step-last" : ""}`}>
          <div className="journey-row">
            <span className={`journey-node ${l.ok ? "journey-node-done" : "journey-node-stuck"}`} />
            <span className="journey-label">Land</span>
            <span className="journey-gates">
              <ResultMark result={l.ok ? "pass" : "fail"} title={l.ok ? "Landed" : "Land stopped"} />
            </span>
            <span className={`journey-review journey-land${l.ok ? "" : " journey-stuck"}`} title={l.summary ?? undefined}>
              {landLine(l)}
            </span>
            <span className="journey-time" title={when(l.ts)}>
              {short(now, l.ts)}
            </span>
          </div>
        </div>
      ))}
    </div>
  );
}

/** "3d": the time column is narrow, and its header-less position already says "when". */
const short = (now: number, ts: number) => ago(now, ts).replace(" ago", "");
const when = (ts: number) => new Date(ts * 1000).toLocaleString();

/** "Land stopped: rebase conflict in x" → "stopped: rebase conflict in x"; "Landed <branch> on main at abc" → "on main at abc". */
function landLine(l: { ok: boolean; summary: string | null }): string {
  if (!l.summary) return l.ok ? "landed" : "stopped";
  return l.ok ? l.summary.replace(/^Landed \S+ /, "") : l.summary.replace(/^Land stopped: /, "stopped: ");
}

function Chevron({ open }: { open: boolean }) {
  return (
    <svg className={`journey-chevron${open ? " open" : ""}`} width="10" height="10" viewBox="0 0 10 10" aria-hidden="true">
      <path d="M3.5 2 6.5 5 3.5 8" fill="none" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round" strokeLinejoin="round" />
    </svg>
  );
}

type CellTone = "step" | "ok" | "fail" | "changes" | "pass" | "needs";
interface Cell {
  lane: string;
  label: string;
  sub: string;
  tone: CellTone;
}

/** The most recent steps the swimlanes show; older ones are on the timeline. */
const MAX_CELLS = 12;

/**
 * Who had the issue, read left to right: the agent that wrote it, the gate that checked it, the
 * agent that reviewed it, and you. Each step is a cell in its owner's lane, in time order, so a
 * hand-off reads as a step down and a round of changes as the work coming back up.
 */
export function WhoHadIt(props: { detail: IssueDetail; now: number }) {
  const { detail, now } = props;
  const { rounds } = journeyOf(detail.timeline, detail.reviews);
  const cells: Cell[] = [];
  const author = detail.issue.claimed_by ?? detail.timeline.find((e) => e.kind === "claimed" || e.kind === "worktree")?.actor ?? null;
  const authorLane = author ? vendorOf(author) : "other";
  let started = false;
  let submits = 0;
  for (const e of detail.timeline) {
    const at = ago(now, e.ts).replace(" ago", "");
    switch (e.kind) {
      case "claimed":
      case "worktree":
        if (!started) cells.push({ lane: authorLane, label: "Started", sub: at, tone: "step" });
        started = true;
        break;
      case "gate_passed":
      case "gate_failed": {
        // Runs back to back are one cell: "✕ ✓ Pass" reads as failed, then fixed.
        const ok = e.kind === "gate_passed";
        const prev = cells[cells.length - 1];
        const marks = prev?.lane === "gate" && prev.tone !== "step" ? `${prev.label.replace(/ (Pass|Failed)$/, "")} ` : "";
        if (prev?.lane === "gate") cells.pop();
        cells.push({ lane: "gate", label: `${marks}${ok ? "✓ Pass" : "✕ Failed"}`, sub: at, tone: ok ? "ok" : "fail" });
        break;
      }
      case "gate_skipped":
        if (cells[cells.length - 1]?.lane === "gate") cells.pop();
        cells.push({ lane: "gate", label: "Skipped (YOLO)", sub: at, tone: "step" });
        break;
      case "submit": {
        submits += 1;
        cells.push({ lane: authorLane, label: submits === 1 ? "Submitted" : "Fixed", sub: at, tone: "step" });
        const r = rounds.find((x) => x.end === e.ts);
        const reviewer = r?.reviewer ?? r?.review?.reviewer;
        if (r && reviewer && r.outcome !== "unreviewed") {
          const found = r.review?.findings.length ?? 0;
          const lane = vendorOf(reviewer) === authorLane ? `${vendorOf(reviewer)}-review` : vendorOf(reviewer);
          if (r.outcome === "changes") cells.push({ lane, label: "Changes", sub: `${found} found`, tone: "changes" });
          else if (r.outcome === "passed") cells.push({ lane, label: "Passed", sub: found ? `${found} found` : "clean", tone: "pass" });
          else if (r.outcome === "review-failed") cells.push({ lane, label: "Didn't finish", sub: at, tone: "fail" });
        }
        break;
      }
      case "ask":
        cells.push({ lane: "human", label: "Asked", sub: at, tone: "needs" });
        break;
      case "answer":
        cells.push({ lane: "human", label: "Answered", sub: at, tone: "step" });
        break;
      case "ask_closed":
        // The issue closed before the question was answered: nothing is left to decide.
        cells.push({ lane: "human", label: "No answer needed", sub: at, tone: "step" });
        break;
      case "issue_moved":
        if (e.actor === "human" && /→ Approved/.test(e.summary ?? "")) cells.push({ lane: "human", label: "Approved", sub: at, tone: "pass" });
        break;
      case "landed":
        cells.push({ lane: authorLane, label: "Landed", sub: at, tone: "pass" });
        break;
      case "land_failed":
        cells.push({ lane: authorLane, label: "Land stopped", sub: at, tone: "fail" });
        break;
    }
  }
  if (detail.issue.state === "Needs You") {
    const since = [...detail.timeline].reverse().find((e) => e.kind === "submit" || (e.kind === "issue_moved" && /→ Needs You/.test(e.summary ?? "")));
    cells.push({ lane: "human", label: WAITING_LABEL[issueDecision(detail.issue, detail) ?? "approve"], sub: since ? ago(now, since.ts).replace(" ago", "") : "now", tone: "needs" });
  }
  if (cells.length === 0) return null;
  const shown = cells.slice(-MAX_CELLS);
  const lanes = [authorLane, "gate", ...new Set(shown.map((c) => c.lane).filter((l) => l !== authorLane && l !== "gate" && l !== "human")), "human"].filter(
    (l, i, all) => all.indexOf(l) === i && (l === authorLane || shown.some((c) => c.lane === l)),
  );
  const name = (lane: string) => (lane === "gate" ? "Gate" : lane === "human" ? "You" : VENDOR_LABEL[vendorOf(lane.replace(/-review$/, ""))]);
  const reviewerNote = lanes.find((l) => l !== authorLane && l !== "gate" && l !== "human");
  return (
    <section className="who-had">
      <h2>
        Who had it{" "}
        <span className="head-note">
          {[author && `${VENDOR_LABEL[vendorOf(author)]} wrote it`, shown.some((c) => c.lane === "gate" && c.tone !== "step") && "the gate checked it", reviewerNote && `${name(reviewerNote)} reviewed it`]
            .filter(Boolean)
            .join(", ")}
          . Read left to right.
        </span>
      </h2>
      <div className="who-grid" style={{ gridTemplateColumns: `84px repeat(${shown.length}, minmax(76px, 1fr))` }}>
        {lanes.map((lane, row) => (
          <div key={lane} className="who-lane" style={{ gridRow: row + 1 }}>
            {lane !== "gate" && lane !== "human" ? <AgentMark vendor={lane.replace(/-review$/, "")} size={16} /> : <span className="who-lane-blank" />}
            <span>{name(lane)}</span>
          </div>
        ))}
        {lanes.map((lane, row) => (
          <div key={`rule-${lane}`} className="who-rule" style={{ gridRow: row + 1 }} />
        ))}
        {shown.map((c, i) => (
          <div key={i} className={`who-cell who-${c.tone}`} style={{ gridRow: lanes.indexOf(c.lane) + 1, gridColumn: i + 2 }} title={`${name(c.lane)}: ${c.label} · ${c.sub}`}>
            <span className="who-label">{c.label}</span>
            <span className="who-sub">{c.sub}</span>
          </div>
        ))}
      </div>
    </section>
  );
}
