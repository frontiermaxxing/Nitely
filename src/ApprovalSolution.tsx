import { APPROVAL_NEXT, approvalSolution, type ApprovalEvidence } from "./approvalEvidence";
import "./styles/approval-solution.css";

/** Same explanation on desktop and phone; submitter text is plain text, never HTML. */
export function ApprovalSolution(props: { evidence?: ApprovalEvidence; issue: { key: string; title: string }; findings?: { priority: number; text: string; carried?: boolean }[]; showFindings?: boolean; showNext?: boolean }) {
  const { summary, gate, commit } = approvalSolution(props.evidence);
  return <section className="approval-solution">
    {summary && <><h3>What changed</h3><p className="solution-text">{summary}</p></>}
    <p className="solution-original"><strong>Original issue</strong> · {props.issue.key}: {props.issue.title}</p>
    <h3>Verification</h3>
    {gate ? <>
      <p>{gate.passed ? "Gate passed" : "Gate failed"} for <code>{commit?.slice(0, 8)}</code>.</p>
      {gate.results.length > 0 ? <ul>{gate.results.map((r, i) => <li key={i}>{r.skipped ? "Skipped" : r.ok ? "Passed" : "Failed"}: <code>{r.command}</code></li>)}</ul> : <p>No gate commands configured.</p>}
      {!!gate.flaked?.length && <p>Passed after retry: {gate.flaked.map((f) => f.command).join(", ")}</p>}
      {gate.warnings?.map((w, i) => <p key={i}>{w}</p>)}
    </> : <p>Verification evidence unavailable for this revision.</p>}
    {props.showFindings && !!props.findings?.length && <><h3>Remaining review findings</h3><ul>{props.findings.map((f, i) => <li key={i}>P{f.priority} · {f.text}{f.carried ? " (carried over)" : ""}</li>)}</ul></>}
    {props.showNext !== false && <p className="solution-next">{APPROVAL_NEXT}</p>}
  </section>;
}
