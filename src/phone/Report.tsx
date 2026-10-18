import { useEffect, useState } from "react";
import { signedGet, unreachable } from "./api";
import type { Device } from "./keys";
import type { GoalReport } from "../types";
import { finishLine, neededLine, riskTone, triagedLine } from "../goals";

/**
 * The last day on the PC, by goal (NIT-247): what landed in each active goal, what is at risk and
 * when it will be done at this pace, and every place you were needed. Folded until opened, except
 * when nothing waits to be decided.
 */
export function LastDay(props: { device: Device; open: boolean }) {
  const [open, setOpen] = useState(props.open);
  const [report, setReport] = useState<GoalReport | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!open) return;
    let live = true;
    signedGet<GoalReport & { error?: string }>(props.device, "/phone/api/report")
      .then((r) => live && (r.status === 200 ? (setReport(r.body), setError(null)) : setError(r.body.error ?? `The PC answered ${r.status}.`)))
      .catch((e) => live && setError(unreachable(e)));
    return () => {
      live = false;
    };
  }, [open, props.device]);

  const today = new Date();
  const needed = report ? neededLine(report.needed) : null;
  const triaged = report ? triagedLine(report.triaged?.length ?? 0) : null;
  return (
    <details className="ph-report" open={open} onToggle={(e) => setOpen((e.target as HTMLDetailsElement).open)}>
      <summary>The last day, by goal</summary>
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
                {g.risks.length > 0 && (
                  <ul className="ph-report-risks">
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
          {report.other_landed.length > 0 && <p>{report.other_landed.length} landed outside any goal.</p>}
          <p className="ph-report-needed">{needed ?? "You were not needed."}</p>
          {triaged && <p>{triaged}.</p>}
        </>
      )}
    </details>
  );
}
