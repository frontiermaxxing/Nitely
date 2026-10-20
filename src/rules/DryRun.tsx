import { useEffect, useState } from "react";
import { invoke } from "../backend";
import type { DryRun as DryRunResult } from "../types";
import { Loading } from "../ui";
import { reviewerName } from "./shared";

/** Pick a branch and see what submit would decide for it under the saved rules. */
export function DryRun(props: { repo: string; stale: boolean }) {
  const [branches, setBranches] = useState<string[] | null>(null);
  const [branch, setBranch] = useState("");
  const [result, setResult] = useState<DryRunResult | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    let current = true;
    setBranches(null);
    setResult(null);
    invoke<string[]>("rules_branches", { repo: props.repo })
      .then((b) => {
        if (!current) return;
        setBranches(b);
        setBranch(b[0] ?? "");
      })
      .catch((e) => current && setError(String(e)));
    return () => {
      current = false;
    };
  }, [props.repo]);

  const run = async () => {
    setError(null);
    try {
      setResult(await invoke<DryRunResult>("rules_dry_run", { repo: props.repo, branch }));
    } catch (e) {
      setError(String(e));
    }
  };

  return (
    <section className="settings-card">
      <h2>Dry run</h2>
      <p className="settings-why">What submit would decide for a branch under the saved rules{props.stale ? " (save your changes first to try them)" : ""}. The review does not run, so its findings and answers are not known.</p>
      {branches === null && !error && <Loading />}
      {branches && branches.length === 0 && <p className="muted small">No branches besides the integration branch.</p>}
      {branches && branches.length > 0 && (
        <div className="settings-row">
          <select className="mono" value={branch} onChange={(e) => setBranch(e.target.value)} aria-label="Branch">
            {branches.map((b) => (
              <option key={b} value={b}>
                {b}
              </option>
            ))}
          </select>
          <button className="btn" disabled={!branch} onClick={run}>
            Try it
          </button>
        </div>
      )}
      {result && (
        <div className="rules-dry">
          <p className="rules-dry-outcome">{result.blocked ?? result.outcome}</p>
          <dl>
            <dt>Changes</dt>
            <dd>
              {result.files.length} file{result.files.length === 1 ? "" : "s"} against <span className="mono">{result.base}</span>
            </dd>
            <dt>Reviewer</dt>
            <dd>{result.reviewer ? `${reviewerName(result.reviewer)}${result.reviewer_note ? ` (${result.reviewer_note})` : ""}` : result.reviewer_note || result.reviewer_error}</dd>
            <dt>Waits for you</dt>
            <dd>{result.reasons.length ? result.reasons.join("; ") : "No rule sends it to you"}</dd>
            {result.flags.length > 0 && (
              <>
                <dt>Also if yes</dt>
                <dd>{result.flags.join("; ")}</dd>
              </>
            )}
          </dl>
        </div>
      )}
      {error && <p className="error small">{error}</p>}
    </section>
  );
}
