import type { PathRule } from "../types";
import { LandsStrip } from "./LandsStrip";
import { Questions } from "./Questions";
import type { RulesDraft } from "./shared";

/** Path rules as rows of pattern and reason, with add and remove. */
export function PathRules(props: { rules: PathRule[]; disabled: boolean; onChange: (rules: PathRule[]) => void; patternHint: string; reasonHint: string }) {
  const set = (i: number, r: Partial<PathRule>) => props.onChange(props.rules.map((x, j) => (j === i ? { ...x, ...r } : x)));
  return (
    <div className="rules-list">
      {props.disabled && props.rules.length === 0 && <p className="muted small rules-none">None.</p>}
      {props.rules.map((r, i) => (
        <div className="rules-item" key={i}>
          <input className="mono" value={r.pattern} disabled={props.disabled} placeholder={props.patternHint} aria-label="Path pattern" onChange={(e) => set(i, { pattern: e.target.value })} />
          <input value={r.reason} disabled={props.disabled} placeholder={props.reasonHint} aria-label="Reason" onChange={(e) => set(i, { reason: e.target.value })} />
          {!props.disabled && (
            <button className="btn btn-quiet" aria-label="Remove this rule" onClick={() => props.onChange(props.rules.filter((_, j) => j !== i))}>
              Remove
            </button>
          )}
        </div>
      ))}
      {!props.disabled && (
        <button className="btn btn-quiet rules-add" onClick={() => props.onChange([...props.rules, { pattern: "", reason: "" }])}>
          Add a path
        </button>
      )}
    </div>
  );
}

/** What waits for you: the summary, every change, the paths and questions that send one to you, and paths no task may change. */
export function Paths({ draft, ro, landBy, crossVendor, patch }: RulesDraft) {
  return (
    <section className="settings-card">
      <h2>What waits for you</h2>
      <LandsStrip draft={draft} landBy={landBy} crossVendor={crossVendor} />
      <label className="rules-check">
        <input type="checkbox" checked={draft.always_human} disabled={ro} onChange={(e) => patch({ always_human: e.target.checked })} />
        Every change waits for me
      </label>
      <h3>Paths that need you</h3>
      <p className="settings-why">A change that touches a matching path waits for you. Patterns are regular expressions over repo paths.</p>
      <PathRules rules={draft.human_paths} disabled={ro} onChange={(human_paths) => patch({ human_paths })} patternHint="^src/billing/" reasonHint="billing and money" />
      <h3>Questions the reviewer answers</h3>
      <p className="settings-why">For what a path cannot show. The reviewer ends with a yes or no for each; a yes sends the change to you.</p>
      <Questions flags={draft.flags} disabled={ro} onChange={(flags) => patch({ flags })} />
      <h3>Paths no task may change</h3>
      <p className="settings-why">Submit and land refuse a branch that touches these.</p>
      <PathRules rules={draft.blocked_paths} disabled={ro} onChange={(blocked_paths) => patch({ blocked_paths })} patternHint="^vendor/" reasonHint="updated by its own process" />
    </section>
  );
}
