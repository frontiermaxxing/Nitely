import type { Batching, LandBy } from "../types";
import type { RulesDraft } from "./shared";
import { From, LAND_BY } from "./shared";

const REVIEW_BATCH: Record<Batching["review"], string> = {
  off: "Each change alone",
  small: "Small changes together",
  all: "Changes of any size together",
  shadow: "Each change alone, and measured in batches",
};
const LAND_BATCH: Record<Batching["land"], string> = {
  off: "Each change alone",
  waiting: "What waits lands together",
  all: "What waits, and what comes within a window, lands together",
};

/** "5 min", "30 s", "until the reviewer is free": a playbook window in words. */
function windowWords(text: string): string {
  const t = text.trim().toLowerCase();
  if (t === "busy") return "until the reviewer is free";
  const m = /^(\d+)\s*(s|sec|secs|m|min|mins|h)?$/.exec(t);
  if (!m) return text;
  const n = Number(m[1]);
  const unit = m[2] ?? "s";
  const secs = unit.startsWith("h") ? n * 3600 : unit.startsWith("m") ? n * 60 : n;
  if (secs === 0) return "no wait";
  return secs % 3600 === 0 ? `${secs / 3600} h` : secs % 60 === 0 ? `${secs / 60} min` : `${secs} s`;
}

/**
 * How the project batches reviews and lands (docs/batching.md): read here, changed in the playbook's
 * `[review]` and `[land]` tables, since a batch changes what a reviewer reads and what one gate covers.
 */
export function BatchingCard({ batching: b, reviewEnabled }: { batching: Batching; reviewEnabled: boolean }) {
  return (
    <section className="settings-card">
      <h2>Batching</h2>
      <p className="settings-why">
        Set in the playbook's <span className="mono">[review]</span> and <span className="mono">[land]</span> tables. A change that needs your approval never batches:
        you approve each one on its own.
      </p>
      <dl className="lanes-facts">
        <dt>Reviews</dt>
        <dd>
          {reviewEnabled ? REVIEW_BATCH[b.review] ?? b.review : "Disabled; configuration retained"} <span className="muted mono">batch = "{b.review}"</span>
          {b.review !== "off" && (
            <div className="muted small">
              Up to {b.review_max} per review call, waiting {windowWords(b.review_window)} for others
              {b.review === "all" ? `; up to ${b.max_lines} changed lines a batch` : `; small is at most ${b.small_lines} lines and ${b.small_files} files`}.
              {b.review === "shadow" && " The batch's reviews are only recorded, to compare with the single reviews that decided."}
            </div>
          )}
        </dd>
        <dt>Lands</dt>
        <dd>
          {LAND_BATCH[b.land] ?? b.land} <span className="muted mono">batch = "{b.land}"</span>
          {b.land !== "off" && (
            <div className="muted small">
              Up to {b.land_max} a run, gated once at the stack's tip
              {b.land === "all" && b.land_window_seconds > 0 && `, waiting ${windowWords(String(b.land_window_seconds))} for more`}. A change that breaks the stack's gate is found
              by bisection and sent back; the others land.
            </div>
          )}
          {b.land_alone && <div className="small attention-ink">Each change lands alone all the same: {b.land_alone}.</div>}
        </dd>
      </dl>
    </section>
  );
}

/** Who lands a change, then how reviews and lands batch. */
export function Landing({ view: v, draft, ro, landBy, patch }: RulesDraft) {
  return (
    <>
      <section className="settings-card">
        <div className="rules-field-head">
          <h2>Who lands a change</h2>
          <From from={draft.land_by !== null ? "project" : v.land_by.inherited_from} />
        </div>
        <select value={draft.land_by ?? "inherit"} disabled={ro} onChange={(e) => patch({ land_by: e.target.value === "inherit" ? null : (e.target.value as LandBy) })}>
          <option value="inherit">Inherit ({LAND_BY[v.land_by.inherited].label})</option>
          {(Object.keys(LAND_BY) as LandBy[]).map((b) => (
            <option key={b} value={b}>
              {LAND_BY[b].label}
            </option>
          ))}
        </select>
        <p className="settings-why rules-land-why">{draft.review_enabled === false ? (landBy === "author" ? "After the gate and any required approval, the author agent calls land." : "After the gate and any required approval, Nitely lands the change.") : LAND_BY[landBy].why}</p>
      </section>

      {v.batching && <BatchingCard batching={v.batching} reviewEnabled={draft.review_enabled ?? v.review_enabled.inherited} />}
    </>
  );
}
