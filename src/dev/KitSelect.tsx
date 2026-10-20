// The kit page's Select row (NIT-425): a default with its mark, a second line, a disabled option
// with its reason, a value set over the default, and a disabled trigger.

import { useState } from "react";
import { Select, type SelectOption } from "../ui";

const REVIEWERS: SelectOption[] = [
  { value: "claude", label: "Claude", detail: "Reviews changes made by Codex and Cursor" },
  { value: "codex", label: "Codex", detail: "Reviews changes made by Claude" },
  { value: "cursor", label: "Cursor", disabled: "Not installed on this machine" },
  { value: "none", label: "No review", detail: "Changes land on the gate alone" },
];

export function KitSelect() {
  const [reviewer, setReviewer] = useState<string | null>(null);
  const [branch, setBranch] = useState<string | null>("release");
  return (
    <section>
      <h2>Select</h2>
      <div className="kit-row" style={{ display: "flex", gap: 24, flexWrap: "wrap", alignItems: "flex-start" }}>
        <label style={{ display: "grid", gap: 4 }}>
          <span className="muted small">Reviewer (following the default)</span>
          <Select aria-label="Reviewer" value={reviewer} defaultValue="claude" options={REVIEWERS} onChange={setReviewer} />
          <span className="muted small mono">value: {String(reviewer)}</span>
        </label>
        <label style={{ display: "grid", gap: 4 }}>
          <span className="muted small">Integration branch (set here)</span>
          <Select
            aria-label="Integration branch"
            value={branch}
            defaultValue="main"
            options={[{ value: "main", label: "main" }, { value: "release", label: "release" }, { value: "develop", label: "develop" }]}
            onChange={setBranch}
          />
          <span className="muted small mono">value: {String(branch)}</span>
        </label>
        <label style={{ display: "grid", gap: 4 }}>
          <span className="muted small">Disabled</span>
          <Select aria-label="Disabled" value={null} defaultValue="claude" options={REVIEWERS} onChange={() => {}} disabled />
        </label>
      </div>
    </section>
  );
}
