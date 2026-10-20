/** Save, and Discard while there is something to discard. */
export function SaveBar(props: { label: string; busy: boolean; changed: boolean; onSave: () => void; onDiscard?: () => void }) {
  return (
    <div className="settings-row">
      <button className="btn primary" disabled={props.busy || !props.changed} onClick={props.onSave}>
        {props.label}
      </button>
      {props.changed && props.onDiscard && (
        <button className="btn" disabled={props.busy} onClick={props.onDiscard}>
          Discard changes
        </button>
      )}
    </div>
  );
}
