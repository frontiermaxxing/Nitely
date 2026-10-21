import { useRef, type ReactNode } from "react";
import type { PathRule } from "../types";
import { Button, Checkbox, RemoveButton, SettingsRow, SettingsSection, SourceTag } from "../ui";
import { Questions } from "./Questions";
import type { RulesDraft } from "./shared";
import { waitsSummary } from "./shared";
import "./tables.css";

/** The "+ Add …" button that ends a table or sits in its empty box. */
export function AddRow(props: { label: string; onAdd: () => void }) {
  return (
    <Button variant="quiet" className="rules-table-add" onClick={props.onAdd}>
      <span aria-hidden="true">+</span>
      {props.label}
    </Button>
  );
}

/** An empty list: a dashed box saying so, with the add button inside when it can be edited. */
export function EmptyRules(props: { text: string; add?: ReactNode }) {
  return (
    <div className="rules-empty">
      <span>{props.text}</span>
      {props.add}
    </div>
  );
}

/** A rule's name with its count beside it: amber for what sends a change to you, grey otherwise. */
function Counted(props: { title: string; count: number; quiet?: boolean }) {
  return (
    <span className="rules-counted">
      {props.title}
      {props.count > 0 && <span className={`rules-count${props.quiet ? " rules-count-quiet" : ""}`}>{props.count}</span>}
    </span>
  );
}

/**
 * Path rules as a table of pattern and reason, with × to remove and "+ Add a path" in its last row.
 * Patterns are cut off with … until the field has the focus; `tone` colours the reason's dot.
 */
export function PathRules(props: { label: string; rules: PathRule[]; onChange: (rules: PathRule[]) => void; patternHint: string; reasonHint: string; reasonHead: string; tone: "you" | "refused"; empty: string }) {
  const set = (i: number, r: Partial<PathRule>) => props.onChange(props.rules.map((x, j) => (j === i ? { ...x, ...r } : x)));
  // A row just added takes the focus, since the button that added it may be gone.
  const added = useRef(false);
  const add = (
    <AddRow
      label="Add a path"
      onAdd={() => {
        added.current = true;
        props.onChange([...props.rules, { pattern: "", reason: "" }]);
      }}
    />
  );
  if (props.rules.length === 0) return <EmptyRules text={props.empty} add={add} />;
  return (
    <div className="rules-table" role="group" aria-label={props.label}>
      <div className="rules-table-head rules-table-paths">
        <span aria-hidden="true">Pattern</span>
        <span aria-hidden="true">{props.reasonHead}</span>
        <span />
      </div>
      {props.rules.map((r, i) => (
        <div className="rules-table-row rules-table-paths" key={i}>
          <input
            className="rules-cell rules-cell-mono"
            ref={(el) => {
              if (el && added.current && i === props.rules.length - 1) {
                added.current = false;
                el.focus();
              }
            }}
            value={r.pattern}
            title={r.pattern} placeholder={props.patternHint} aria-label="Path pattern" spellCheck={false} onChange={(e) => set(i, { pattern: e.target.value })} />
          <span className="rules-cell-dotted">
            <span className={`rules-dot rules-dot-${props.tone}`} aria-hidden="true" />
            <input className="rules-cell" value={r.reason} title={r.reason} placeholder={props.reasonHint} aria-label={props.reasonHead} onChange={(e) => set(i, { reason: e.target.value })} />
          </span>
          <RemoveButton label={`Remove ${r.pattern || "this path"}`} onClick={() => props.onChange(props.rules.filter((_, j) => j !== i))} />
        </div>
      ))}
      <div className="rules-table-foot">{add}</div>
    </div>
  );
}

/** A read-only setting of a committed playbook: its name and value, as a tile. */
function Tile(props: { title: string; value: string; items?: { key: string; label: string; detail: string }[] }) {
  return (
    <div className="rules-tile">
      <span className="rules-tile-title">{props.title}</span>
      <span className="rules-tile-value">{props.value}</span>
      {props.items && props.items.length > 0 && (
        <ul>
          {props.items.map((it, i) => (
            <li key={i} title={it.label}>
              <span className="mono">{it.label}</span>
              {it.detail && <span className="muted"> {it.detail}</span>}
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

const count = (n: number, one: string) => (n === 0 ? "None" : `${n} ${one}${n === 1 ? "" : "s"}`);
const paths = (rules: PathRule[]) => rules.map((r) => ({ key: r.pattern, label: r.pattern, detail: r.reason }));

const LOCK = (
  <svg width="12" height="12" viewBox="0 0 12 12" aria-hidden="true">
    <rect x="2.5" y="5.5" width="7" height="5" rx="1" fill="none" stroke="currentColor" strokeWidth="1.2" />
    <path d="M4 5.5V4a2 2 0 0 1 4 0v1.5" fill="none" stroke="currentColor" strokeWidth="1.2" />
  </svg>
);

/** A list's own value is this project's; an empty one is Nitely's default of none. */
const listSource = (n: number) => <SourceTag source={n ? "project" : "nitely"} />;

/** What waits for you: every change, the paths and questions that send one to you, and paths no task may change. */
export function Paths({ draft, ro, landBy, crossVendor, patch }: RulesDraft) {
  const summary = waitsSummary(draft, landBy, crossVendor);
  if (ro)
    return (
      <SettingsSection title="What waits for you" description={summary}>
        <p className="rules-lock">
          {LOCK}
          Set in the committed playbook. An agent can propose a change for you to approve.
        </p>
        <div className="rules-tiles">
          <Tile title="Every change waits for me" value={draft.always_human ? "On" : "Off"} />
          <Tile title="Paths that need you" value={count(draft.human_paths.length, "path")} items={paths(draft.human_paths)} />
          <Tile title="Questions the reviewer answers" value={count(draft.flags.length, "question")} items={draft.flags.map((f) => ({ key: f.name, label: f.name, detail: f.question }))} />
          <div data-rules-section="refused">
            <Tile title="Paths no task may change" value={count(draft.blocked_paths.length, "path")} items={paths(draft.blocked_paths)} />
          </div>
        </div>
      </SettingsSection>
    );
  return (
    <SettingsSection title="What waits for you" description={summary}>
      <SettingsRow label="Every change waits for me" why="Overrides the lists below." source={draft.always_human ? <SourceTag source="project" /> : undefined}>
        <Checkbox checked={draft.always_human} onChange={(always_human) => patch({ always_human })} label={draft.always_human ? "On" : "Off"} aria-label="Every change waits for me" />
      </SettingsRow>
      <SettingsRow
        label={<Counted title="Paths that need you" count={draft.human_paths.length} />}
        why="A change that touches a matching path waits for you. Patterns are regular expressions over repo paths."
        source={listSource(draft.human_paths.length)}
      >
        <PathRules
          label="Paths that need you"
          rules={draft.human_paths}
          onChange={(human_paths) => patch({ human_paths })}
          patternHint="^src/billing/"
          reasonHint="billing and money"
          reasonHead="Shown to you as"
          tone="you"
          empty="None. No path sends a change to you."
        />
      </SettingsRow>
      <SettingsRow
        label={<Counted title="Questions the reviewer answers" count={draft.flags.length} />}
        why="For what a path cannot show. The reviewer ends with a yes or no for each; a yes sends the change to you."
        source={listSource(draft.flags.length)}
      >
        <Questions flags={draft.flags} onChange={(flags) => patch({ flags })} />
      </SettingsRow>
      <div data-rules-section="refused">
        <SettingsRow label={<Counted title="Paths no task may change" count={draft.blocked_paths.length} quiet />} why="Submit and land refuse a branch that touches these." source={listSource(draft.blocked_paths.length)}>
          <PathRules
            label="Paths no task may change"
            rules={draft.blocked_paths}
            onChange={(blocked_paths) => patch({ blocked_paths })}
            patternHint="^vendor/"
            reasonHint="updated by its own process"
            reasonHead="Why"
            tone="refused"
            empty="None. Every path can be changed."
          />
        </SettingsRow>
      </div>
    </SettingsSection>
  );
}
