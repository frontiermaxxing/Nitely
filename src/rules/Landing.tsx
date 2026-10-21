import type { ReactNode } from "react";
import type { Batching, LandBy } from "../types";
import { RadioCards, SettingsRow, SettingsSection, SourceTag, type ChoiceOption } from "../ui";
import type { RulesDraft } from "./shared";
import { LAND_BY, sourceOf } from "./shared";

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

/** Three changes, apart when each goes alone and joined when they go together. */
function BatchGlyph(props: { together: boolean; muted?: boolean }) {
  return (
    <span className={`rules-batch-glyph${props.together ? " is-together" : ""}${props.muted ? " is-muted" : ""}`} aria-hidden="true">
      <span />
      <span />
      <span />
    </span>
  );
}

/** One read-only batching fact: what it does, the playbook key that sets it, and its details. */
function BatchTile(props: { title: string; together: boolean; muted?: boolean; value: string; toml: string; children?: ReactNode }) {
  return (
    <div className="rules-batch-tile">
      <span className="rules-batch-title">{props.title}</span>
      <span className="rules-batch-value">
        <BatchGlyph together={props.together} muted={props.muted} />
        {props.value}
      </span>
      <span className="rules-batch-toml mono">{props.toml}</span>
      {props.children}
    </div>
  );
}

/**
 * How the project batches reviews and lands (docs/batching.md): read here, changed in the playbook's
 * `[review]` and `[land]` tables, since a batch changes what a reviewer reads and what one gate covers.
 */
export function BatchingRow({ batching: b, reviewEnabled }: { batching: Batching; reviewEnabled: boolean }) {
  return (
    <SettingsRow label="Batching" why="Set in the playbook's [review] and [land] tables. A change that needs your approval never batches: you approve each one on its own." source={<SourceTag source="playbook" />}>
      <div className="rules-batch">
        <BatchTile
          title="Reviews"
          together={reviewEnabled && b.review !== "off" && b.review !== "shadow"}
          muted={!reviewEnabled}
          value={reviewEnabled ? REVIEW_BATCH[b.review] ?? b.review : "Disabled; configuration retained"}
          toml={`[review] batch = "${b.review}"`}
        >
          {b.review !== "off" && (
            <span className="rules-batch-detail">
              Up to {b.review_max} per review call, waiting {windowWords(b.review_window)} for others
              {b.review === "all" ? `; up to ${b.max_lines} changed lines a batch` : `; small is at most ${b.small_lines} lines and ${b.small_files} files`}.
              {b.review === "shadow" && " The batch's reviews are only recorded, to compare with the single reviews that decided."}
            </span>
          )}
        </BatchTile>
        <BatchTile title="Lands" together={b.land !== "off" && !b.land_alone} value={LAND_BATCH[b.land] ?? b.land} toml={`[land] batch = "${b.land}"`}>
          {b.land !== "off" && (
            <span className="rules-batch-detail">
              Up to {b.land_max} a run, gated once at the stack's tip
              {b.land === "all" && b.land_window_seconds > 0 && `, waiting ${windowWords(String(b.land_window_seconds))} for more`}. A change that breaks the stack's gate is found by
              bisection and sent back; the others land.
            </span>
          )}
          {b.land_alone && <span className="rules-batch-detail attention-ink">Each change lands alone all the same: {b.land_alone}.</span>}
        </BatchTile>
      </div>
    </SettingsRow>
  );
}

/** Who lands a change, then how reviews and lands batch. */
export function Landing({ view: v, draft, ro, landBy, patch }: RulesDraft) {
  const reviewOff = draft.review_enabled === false;
  const options: ChoiceOption<LandBy>[] = (Object.keys(LAND_BY) as LandBy[]).map((b) => ({
    value: b,
    label: LAND_BY[b].label,
    // With review off, the cards say what happens without one.
    why: reviewOff ? (b === "author" ? "After the gate and any required approval, the author agent calls land." : b === "auto" ? "After the gate and any required approval, Nitely lands the change." : LAND_BY[b].why) : LAND_BY[b].why,
    isDefault: b === v.land_by.inherited,
  }));
  return (
    <SettingsSection title="Landing" description="Who puts a reviewed change on the integration branch, and whether changes go together.">
      <SettingsRow
        label="Who lands a change"
        source={<SourceTag source={sourceOf(draft.land_by !== null ? "project" : v.land_by.inherited_from)} onReset={ro ? undefined : () => patch({ land_by: null })} />}
      >
        <div className="rules-land-by">
          <RadioCards
            aria-label="Who lands a change"
            options={options}
            value={landBy}
            disabled={ro}
            onChange={(b) => patch({ land_by: b === v.land_by.inherited ? null : b })}
          />
        </div>
      </SettingsRow>
      {v.batching && <BatchingRow batching={v.batching} reviewEnabled={draft.review_enabled ?? v.review_enabled.inherited} />}
    </SettingsSection>
  );
}
