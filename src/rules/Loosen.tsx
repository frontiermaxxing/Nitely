import type { ReactNode } from "react";
import { Button } from "../ui";

/** The exact edit a save makes to a playbook file: where it goes, and its lines with - and +. */
export interface PlaybookDiff {
  /** "Commit to C:\Code\app\.nitely\playbook.toml on main; pushed with the next land." */
  destination: string;
  text: string;
}

function Diff({ diff }: { diff: PlaybookDiff }) {
  const lines = diff.text.replace(/\n$/, "").split("\n");
  const changed = lines.filter((l) => l.startsWith("+") || l.startsWith("-")).length;
  return (
    <div className="rules-diff" role="region" aria-label="Playbook change">
      <div className="rules-diff-head">
        <span>{diff.destination}</span>
        <span>
          {changed} line{changed === 1 ? "" : "s"}
        </span>
      </div>
      <pre className="rules-diff-body">
        {lines.map((l, i) => (
          <span key={i} className={l.startsWith("+") ? "is-add" : l.startsWith("-") ? "is-del" : undefined}>
            {l}
            {"\n"}
          </span>
        ))}
      </pre>
    </div>
  );
}

/**
 * The one panel before a save that needs a second look: what loosens (red, when the save sends fewer
 * changes to you), the exact playbook edit when there is one, and confirm or keep editing. A save
 * that loosens goes through only from here.
 */
export function Loosen(props: {
  loosens: string[];
  /** Said after the title, e.g. "in every project that sets none of these". */
  lead?: ReactNode;
  diff?: PlaybookDiff | null;
  /** "Loosen and save", "Loosen and commit", "Commit". */
  confirmLabel: string;
  busy: boolean;
  onConfirm: () => void;
  onKeepEditing: () => void;
}) {
  const loosening = props.loosens.length > 0;
  return (
    <div className={`rules-loosen rules-panel${loosening ? "" : " is-neutral"}`} role={loosening ? "alert" : "region"} aria-label={loosening ? undefined : "Review the change"}>
      <div className="rules-loosen-title">
        <span className="rules-loosen-dot" aria-hidden="true" />
        <span>
          <strong>{loosening ? "This sends fewer changes to you" : "Review the change before it is committed"}</strong>
          {props.lead && <> {props.lead}</>}
        </span>
      </div>
      {loosening && (
        <ul className="rules-loosen-reasons">
          {props.loosens.map((l) => (
            <li key={l}>{l}</li>
          ))}
        </ul>
      )}
      {props.diff && <Diff diff={props.diff} />}
      <div className="rules-loosen-actions">
        <Button variant={loosening ? "danger" : "primary"} disabled={props.busy} onClick={props.onConfirm}>
          {props.confirmLabel}
        </Button>
        <Button disabled={props.busy} onClick={props.onKeepEditing}>
          Keep editing
        </Button>
      </div>
    </div>
  );
}
