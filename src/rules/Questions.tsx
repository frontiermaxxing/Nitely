import type { ReviewFlag } from "../types";

/** The yes/no questions the reviewer answers, with add and remove. */
export function Questions(props: { flags: ReviewFlag[]; disabled: boolean; onChange: (flags: ReviewFlag[]) => void }) {
  const set = (i: number, f: Partial<ReviewFlag>) => props.onChange(props.flags.map((x, j) => (j === i ? { ...x, ...f } : x)));
  return (
    <div className="rules-list">
      {props.disabled && props.flags.length === 0 && <p className="muted small rules-none">None.</p>}
      {props.flags.map((f, i) => (
        <div className="rules-item rules-flag" key={i}>
          <input
            className="mono"
            value={f.name}
            disabled={props.disabled}
            placeholder="MONEY"
            aria-label="Flag name"
            onChange={(e) => set(i, { name: e.target.value.toUpperCase().replace(/[^A-Z0-9_]/g, "_") })}
          />
          <input value={f.question} disabled={props.disabled} placeholder="the change alters billing or pricing" aria-label="Yes when" onChange={(e) => set(i, { question: e.target.value })} />
          {!props.disabled && (
            <button className="btn btn-quiet" aria-label="Remove this question" onClick={() => props.onChange(props.flags.filter((_, j) => j !== i))}>
              Remove
            </button>
          )}
        </div>
      ))}
      {!props.disabled && (
        <button className="btn btn-quiet rules-add" onClick={() => props.onChange([...props.flags, { name: "", question: "", reason: "" }])}>
          Add a question
        </button>
      )}
    </div>
  );
}
