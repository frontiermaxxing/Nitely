import { useRef } from "react";
import type { ReviewFlag } from "../types";
import { RemoveButton } from "../ui";
import { AddRow, EmptyRules } from "./Paths";

/** The yes/no questions the reviewer answers, as a table of name and question, with × and "+ Add a question". */
export function Questions(props: { flags: ReviewFlag[]; onChange: (flags: ReviewFlag[]) => void }) {
  const set = (i: number, f: Partial<ReviewFlag>) => props.onChange(props.flags.map((x, j) => (j === i ? { ...x, ...f } : x)));
  // A row just added takes the focus, since the button that added it may be gone.
  const added = useRef(false);
  const add = (
    <AddRow
      label="Add a question"
      onAdd={() => {
        added.current = true;
        props.onChange([...props.flags, { name: "", question: "", reason: "" }]);
      }}
    />
  );
  if (props.flags.length === 0) return <EmptyRules text="None. The reviewer answers no questions of yours." add={add} />;
  return (
    <div className="rules-table" role="group" aria-label="Questions the reviewer answers">
      <div className="rules-table-head rules-table-flags">
        <span aria-hidden="true">Name</span>
        <span aria-hidden="true">Yes when</span>
        <span />
      </div>
      {props.flags.map((f, i) => (
        <div className="rules-table-row rules-table-flags" key={i}>
          <input
            className="rules-cell rules-cell-mono rules-cell-flag"
            ref={(el) => {
              if (el && added.current && i === props.flags.length - 1) {
                added.current = false;
                el.focus();
              }
            }}
            value={f.name}
            title={f.name}
            placeholder="MONEY"
            aria-label="Flag name"
            spellCheck={false}
            onChange={(e) => set(i, { name: e.target.value.toUpperCase().replace(/[^A-Z0-9_]/g, "_") })}
          />
          <input className="rules-cell" value={f.question} title={f.question} placeholder="the change alters billing or pricing" aria-label="Yes when" onChange={(e) => set(i, { question: e.target.value })} />
          <RemoveButton label={`Remove ${f.name || "this question"}`} onClick={() => props.onChange(props.flags.filter((_, j) => j !== i))} />
        </div>
      ))}
      <div className="rules-table-foot">{add}</div>
    </div>
  );
}
