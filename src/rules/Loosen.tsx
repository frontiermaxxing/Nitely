import type { ReactNode } from "react";

/** The warning before a save that sends fewer changes to you: what loosens, and loosen or keep editing. */
export function Loosen(props: { lead: ReactNode; loosens: string[]; busy: boolean; onLoosen: () => void; onKeepEditing: () => void }) {
  return (
    <div className="rules-loosen" role="alert">
      <p>{props.lead}</p>
      <ul>
        {props.loosens.map((l) => (
          <li key={l}>{l}</li>
        ))}
      </ul>
      <div className="settings-row">
        <button className="btn danger" disabled={props.busy} onClick={props.onLoosen}>
          Loosen and save
        </button>
        <button className="btn" disabled={props.busy} onClick={props.onKeepEditing}>
          Keep editing
        </button>
      </div>
    </div>
  );
}
