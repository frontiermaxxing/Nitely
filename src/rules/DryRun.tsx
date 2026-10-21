import { useEffect, useState } from "react";
import { invoke } from "../backend";
import type { DryRun as DryRunResult, DryRunBranch } from "../types";
import { AgentMark, Button, Loading, Select, SettingsSection, StationGlyph, type SelectOption, type Track } from "../ui";
import { reviewerName } from "./shared";

/** The station a dry run ends at, drawn as the issue glyph: refused, waits for you, or lands. */
function outcomeOf(r: DryRunResult): { label: string; tone: "fail" | "needs" | "live"; track: Track } {
  const base: Track = { station: "landing", reached: {}, rounds: 0, stuck: null, asking: false, closed: null, backlog: false };
  if (r.blocked) return { label: "Would be refused", tone: "fail", track: { ...base, stuck: r.blocked } };
  if (r.reasons.length) return { label: "Would wait for you", tone: "needs", track: { ...base, station: "needs" } };
  return { label: "Would land", tone: "live", track: base };
}

/** "NIT-31 · 3 ahead": what the branch name and git say about a branch, beside it. */
function branchDetail(b: DryRunBranch): string | undefined {
  const parts = [b.key, b.ahead == null ? null : `${b.ahead} ahead`].filter(Boolean);
  return parts.length ? parts.join(" · ") : undefined;
}

/** Pick a branch and see what submit would decide for it under the saved rules. */
export function DryRun(props: { repo: string; stale: boolean }) {
  const [branches, setBranches] = useState<DryRunBranch[] | null>(null);
  const [branch, setBranch] = useState("");
  const [result, setResult] = useState<DryRunResult | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    let current = true;
    setBranches(null);
    setResult(null);
    invoke<DryRunBranch[]>("rules_branches", { repo: props.repo })
      .then((b) => {
        if (!current) return;
        setBranches(b);
        setBranch(b[0]?.name ?? "");
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

  const options: SelectOption[] = (branches ?? []).map((b) => ({
    value: b.name,
    label: b.name,
    icon: <AgentMark vendor={b.vendor ?? "other"} size={16} />,
    detail: branchDetail(b),
  }));
  const picked = branches?.find((b) => b.name === branch);
  const outcome = result && outcomeOf(result);
  return (
    <SettingsSection
      title="Dry run"
      description={`What submit would decide for a branch under the saved rules${props.stale ? " (save your changes first to try them)" : ""}. The review does not run, so its answers are not known.`}
    >
      {branches === null && !error && <Loading />}
      {branches && branches.length === 0 && <p className="muted small rules-dry-none">No branches besides the integration branch.</p>}
      {branches && branches.length > 0 && (
        <div className="rules-dry-pick">
          <Select
            className="rules-dry-branch mono"
            aria-label="Branch"
            value={branch}
            options={options}
            onChange={(b) => b && (setBranch(b), setResult(null))}
          />
          {picked && branchDetail(picked) && <span className="rules-dry-detail">{branchDetail(picked)}</span>}
          <Button disabled={!branch} onClick={run}>
            Try it
          </Button>
        </div>
      )}
      {result && outcome && (
        <div className="rules-dry" role="region" aria-label={`Dry run of ${result.branch}`}>
          <div className="rules-dry-cell">
            <span className="rules-dry-label">Outcome</span>
            <span className={`rules-dry-outcome is-${outcome.tone}`}>
              <StationGlyph track={outcome.track} size={14} />
              {outcome.label}
            </span>
            <span className="rules-dry-note">{result.blocked ?? result.outcome}</span>
          </div>
          <div className="rules-dry-cell">
            <span className="rules-dry-label">Changes</span>
            <span title={result.files.join("\n")}>
              {result.files.length} file{result.files.length === 1 ? "" : "s"} against <span className="mono">{result.base}</span>
            </span>
          </div>
          <div className="rules-dry-cell">
            <span className="rules-dry-label">Reviewer</span>
            {result.reviewer ? (
              <span className="rules-dry-reviewer">
                <AgentMark vendor={result.reviewer} size={16} />
                {reviewerName(result.reviewer)}
              </span>
            ) : (
              <span className={result.reviewer_error ? "rules-dry-error" : undefined}>{result.reviewer_error ?? "No review"}</span>
            )}
            {result.reviewer_note && <span className="rules-dry-note">{result.reviewer_note}</span>}
          </div>
          <div className="rules-dry-cell">
            <span className="rules-dry-label">Because</span>
            {result.reasons.length ? (
              <ul className="rules-dry-reasons">
                {result.reasons.map((r) => (
                  <li key={r}>{r}</li>
                ))}
              </ul>
            ) : (
              <span>No rule sends it to you</span>
            )}
            {result.flags.length > 0 && <span className="rules-dry-note">Also if yes: {result.flags.join("; ")}</span>}
          </div>
        </div>
      )}
      {error && <p className="error small">{error}</p>}
    </SettingsSection>
  );
}
