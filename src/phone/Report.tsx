import { useEffect, useRef, useState } from "react";
import { signedPost, unreachable } from "./api";
import type { Device } from "./keys";
import type { GoalReport } from "../types";
import { finishLine, neededLine, riskTone, triagedLine } from "../goals";

/** When the person last read the report on this phone. Storage can be missing or refuse. */
const SEEN_KEY = "nitely.phone.reportSeen";

function readSeen(): number | null {
  try {
    const v = Number(localStorage.getItem(SEEN_KEY));
    return Number.isFinite(v) && v > 0 ? v : null;
  } catch {
    return null;
  }
}

function writeSeen(ts: number) {
  try {
    localStorage.setItem(SEEN_KEY, String(ts));
  } catch {
    // Without storage the next visit covers the last day.
  }
}

const clock = (ts: number) => new Date(ts * 1000).toLocaleTimeString([], { hour: "numeric", minute: "2-digit" });

/** "since 9:40 PM" today, "since Fri 9:40 PM" before. */
function sinceLabel(since: number, now: number): string {
  const d = new Date(since * 1000);
  const sameDay = new Date(now * 1000).toDateString() === d.toDateString();
  return sameDay ? `since ${clock(since)}` : `since ${d.toLocaleDateString([], { weekday: "short" })} ${clock(since)}`;
}

/**
 * What happened since you last read this on the phone (the last day the first time), by goal
 * (NIT-247): what landed in each active goal, what is at risk and when it will be done at this
 * pace, and every place you were needed. Folded until opened, except when nothing waits to be
 * decided. It reads again whenever the Inbox does (`version`), from the same start, and that
 * start moves on once you leave the page having read it.
 */
export function LastDay(props: { device: Device; open: boolean; version: number }) {
  const [open, setOpen] = useState(props.open);
  const [since] = useState(() => readSeen());
  const [report, setReport] = useState<GoalReport | null>(null);
  const [error, setError] = useState<string | null>(null);
  // The newest report read while open: where the next visit starts.
  const read = useRef<number | null>(null);

  useEffect(() => {
    if (!open) return;
    let live = true;
    signedPost<GoalReport & { error?: string }>(props.device, "/phone/api/report", since === null ? {} : { since })
      .then((r) => {
        if (!live) return;
        if (r.status !== 200) return setError(r.body.error ?? `The PC answered ${r.status}.`);
        setReport(r.body);
        setError(null);
        read.current = r.body.now;
      })
      .catch((e) => live && setError(unreachable(e)));
    return () => {
      live = false;
    };
  }, [open, props.device, props.version, since]);

  useEffect(() => {
    const save = () => read.current !== null && writeSeen(read.current);
    const onHide = () => document.visibilityState === "hidden" && save();
    document.addEventListener("visibilitychange", onHide);
    window.addEventListener("pagehide", save);
    return () => {
      document.removeEventListener("visibilitychange", onHide);
      window.removeEventListener("pagehide", save);
    };
  }, []);

  const today = new Date();
  const needed = report ? neededLine(report.needed) : null;
  const triaged = report ? triagedLine(report.triaged?.length ?? 0) : null;
  return (
    <details className="ph-report" open={open} onToggle={(e) => setOpen((e.target as HTMLDetailsElement).open)}>
      <summary>
        By goal, {report ? sinceLabel(report.since, report.now) : since === null ? "the last day" : sinceLabel(since, Math.floor(Date.now() / 1000))}
      </summary>
      {error && <p className="ph-error">{error}</p>}
      {open && !report && !error && <p className="ph-quiet">Loading…</p>}
      {report && (
        <>
          {report.goals.length === 0 && <p className="ph-quiet">No active goals.</p>}
          {report.goals.map((g) => {
            const pace = finishLine(g.finish, g.goal.target_date, today);
            return (
              <div key={g.goal.id} className="ph-report-goal">
                <div className="ph-report-title">{g.goal.title}</div>
                <p>
                  <strong>{g.landed.length}</strong> landed · {g.goal.progress.done}/{g.goal.progress.total - g.goal.progress.dropped} done
                  {pace && <span className={pace.late ? " ph-report-late" : undefined}> · {pace.text}</span>}
                </p>
                {g.landed.length > 0 && (
                  <ul className="ph-report-list">
                    {g.landed.map((l) => (
                      <li key={l.key}>
                        <span className="ph-issue-key">{l.key}</span> {l.title}
                      </li>
                    ))}
                  </ul>
                )}
                {g.risks.length > 0 && (
                  <ul className="ph-report-list">
                    {g.risks.map((r) => (
                      <li key={r.key} className={`ph-risk-${riskTone(r.why)}`}>
                        <span className="ph-issue-key">{r.key}</span> {r.why}
                      </li>
                    ))}
                  </ul>
                )}
              </div>
            );
          })}
          {report.other_landed.length > 0 && (
            <div className="ph-report-goal">
              <div className="ph-report-title">Outside any goal</div>
              <ul className="ph-report-list">
                {report.other_landed.map((l) => (
                  <li key={l.key}>
                    <span className="ph-issue-key">{l.key}</span> {l.title} landed
                  </li>
                ))}
              </ul>
            </div>
          )}
          <p className="ph-report-needed">{needed ?? "You were not needed."}</p>
          {report.needed.length > 0 && (
            <ul className="ph-report-list ph-report-needs">
              {report.needed.map((n, i) => (
                <li key={`${n.ts}-${i}`} className={n.open ? "ph-report-open" : undefined}>
                  <span className="ph-quiet">{clock(n.ts)}</span> {n.issue_key && <span className="ph-issue-key">{n.issue_key}</span>} {n.kind === "asked" ? "asked:" : "approval:"} {n.summary}
                  {n.open && <span className="ph-report-waits"> · waits</span>}
                </li>
              ))}
            </ul>
          )}
          {triaged && <p>{triaged}.</p>}
        </>
      )}
    </details>
  );
}
