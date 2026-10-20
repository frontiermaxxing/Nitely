import { useState } from "react";
import { invoke, isLive } from "./backend";
import type { VerificationAuditJob, VerificationAuditReport } from "./types";

export function VerificationAuditActions({ report, onChange, onOpenIssue }: {
  report: VerificationAuditReport;
  onChange: (report: VerificationAuditReport) => void;
  onOpenIssue: (key: string) => void;
}) {
  const actionable = report.jobs.filter((job) => job.state === "findings" || job.state === "failed");
  const pending = report.jobs.filter((job) => job.state === "pending" || job.state === "running").length;
  return <div className="verification-audit-actions">
    {pending > 0 && <p className="verification-audit-note">{pending} {pending === 1 ? "audit awaits" : "audits await"} a result.</p>}
    {actionable.map((job) => <AuditAction key={job.id} job={job} report={report} onChange={onChange} onOpenIssue={onOpenIssue} />)}
    {report.jobs_truncated && <p className="verification-audit-note">Showing recent audit jobs only.</p>}
  </div>;
}

function AuditAction({ job, report, onChange, onOpenIssue }: {
  job: VerificationAuditJob;
  report: VerificationAuditReport;
  onChange: (report: VerificationAuditReport) => void;
  onOpenIssue: (key: string) => void;
}) {
  const [rule, setRule] = useState(job.rules?.[0] ?? "");
  const [evidence, setEvidence] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const confirmed = report.escapes.some((escape) => escape.audit_id === job.id && escape.rule === rule);
  const allConfirmed = !!job.rules?.length && job.rules.every((name) => report.escapes.some((escape) => escape.audit_id === job.id && escape.rule === name));
  async function act(command: string) {
    setBusy(true);
    setError("");
    try {
      onChange(await invoke<VerificationAuditReport>(command, { id: job.id, rule, evidence }));
    } catch (err) {
      setError(String(err));
    } finally {
      setBusy(false);
    }
  }
  return <section className="verification-audit-action" aria-label={`Audit of ${job.issue_key}`}>
    <div className="verification-audit-job-heading">
      <button className="btn quiet" onClick={() => onOpenIssue(job.issue_key)}>{job.issue_key}</button>
      <code title={job.head}>{job.head.slice(0, 7)}</code>
      <strong>{job.state === "failed" ? "Audit failed" : allConfirmed ? "Escape confirmed" : "Findings need confirmation"}</strong>
    </div>
    {job.result && <><p>{job.result.summary}</p><ul>{job.result.findings.map((finding, index) => <li key={index}>P{finding.priority}: {finding.text}</li>)}</ul></>}
    {job.error && <p className="stuck-ink">{job.error}</p>}
    {isLive && <p className="verification-audit-note">Open the desktop app to act on this audit. This preview is read-only.</p>}
    {job.state === "failed" ? <button className="btn" disabled={busy || isLive} onClick={() => void act("retry_verification_audit")}>{busy ? "Retrying…" : "Retry audit"}</button> :
      <form onSubmit={(event) => { event.preventDefault(); if (rule && evidence.trim() && !confirmed && !isLive && !busy) void act("confirm_verification_escape"); }}>
        <p className="verification-audit-note">Confirm a reproduced defect. This disables the selected lowering rule for this project and requires Full verification for that scope.</p>
        <label>Rule that lowered this change
          <select value={rule} onChange={(event) => setRule(event.target.value)} disabled={busy || isLive}>
            {(job.rules ?? []).map((name) => <option key={name} value={name}>{name}</option>)}
          </select>
        </label>
        {confirmed ? <p role="status">Escape confirmed. This rule now requires Full verification.</p> :
          <label>Reproduction evidence
            <textarea value={evidence} onChange={(event) => setEvidence(event.target.value)} maxLength={16384} required disabled={busy || isLive} placeholder="What failed, how you reproduced it, and the expected result" rows={3} />
          </label>}
        {!confirmed && <button className="btn" disabled={busy || isLive || !rule || !evidence.trim()}>{busy ? "Confirming…" : "Confirm escape and require Full"}</button>}
        {!job.rules?.length && <p className="verification-audit-note">Rule details are unavailable. Refresh after updating the app.</p>}
      </form>}
    {error && <p role="alert" className="stuck-ink">{error}</p>}
  </section>;
}
