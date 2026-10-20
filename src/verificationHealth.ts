import type { VerificationAuditRate, VerificationHealth } from "./types";

/** A stale green row must never label a newer integration revision as verified. */
export function verificationPresentation(health: VerificationHealth) {
  const verified = health.status === "green" && !!health.verified_head && health.verified_head === health.observed_head;
  const state = verified ? "verified" : health.status === "green" ? "pending" : health.status;
  switch (state) {
    case "verified": return { label: "Verified", tone: "quiet", detail: "The current revision passed its full check." };
    case "pending": return { label: "Verification pending", tone: "quiet", detail: health.reduced_count > 0
      ? `${health.reduced_count} ${health.reduced_count === 1 ? "change awaits" : "changes await"} a full check.`
      : "The current revision is awaiting a full check." };
    case "failed": return { label: "Verification failed", tone: "failed", detail: "The full check failed. Recovery is required before reduced verification resumes." };
    case "paused": return { label: "Verification paused", tone: "paused", detail: "Reduced verification is paused until a new full baseline passes." };
    case "recovering": return { label: "Recovering", tone: "live", detail: "Recovery is underway. A full check must pass before this revision is verified." };
    case "running": return { label: "Verifying", tone: "live", detail: "The full check is running." };
    default: return { label: "Verification unavailable", tone: "quiet", detail: "No current verification result is available." };
  }
}

/** Show a repository name, never its machine-specific directory or common Git path. */
export function verificationRepoName(path: string): string {
  const parts = path.replaceAll("\\", "/").replace(/\/+$/, "").split("/").filter(Boolean);
  if (parts.at(-1) === ".git") parts.pop();
  return parts.at(-1) || "Repository";
}

export function verificationRevision(head: string | null): string | null {
  return head && /^[0-9a-f]{40,64}$/i.test(head) ? head.slice(0, 7) : null;
}

export function auditedEscapeRate(rate: VerificationAuditRate): string {
  if (rate.audited <= 0) return "Unknown — no completed audits";
  return rate.audited_escape_rate !== null && Number.isFinite(rate.audited_escape_rate) && rate.audited_escape_rate >= 0 && rate.audited_escape_rate <= 1
    ? `${(rate.audited_escape_rate * 100).toFixed(1)}% of audited`
    : "Unknown — rate unavailable";
}
