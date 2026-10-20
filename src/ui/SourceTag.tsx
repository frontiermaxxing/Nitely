/** Where a setting's value comes from. */
export type ValueSource = "project" | "app" | "nitely" | "playbook";

const LABEL: Record<ValueSource, string> = {
  project: "This project",
  app: "App default",
  nitely: "Nitely’s default",
  playbook: "In playbook.toml",
};

const WHY: Record<ValueSource, string> = {
  project: "Set for this project",
  app: "Not set for this project: the app's setting applies",
  nitely: "Not set anywhere: Nitely's own default applies",
  playbook: "Set only in the committed playbook file",
};

/**
 * The tag at the end of a settings row that says where its value comes from: this project's own
 * value (filled, with a Reset that clears it), the app's default (outlined), Nitely's own default
 * (dashed), or the committed playbook file (a lock: change it there, or have an agent propose it).
 */
export function SourceTag(props: { source: ValueSource; onReset?: () => void; resetLabel?: string }) {
  const { source, onReset } = props;
  return (
    <span className={`ui-source ui-source-${source}`} title={WHY[source]}>
      {source === "playbook" ? (
        <svg className="ui-source-lock" width="11" height="11" viewBox="0 0 12 12" aria-hidden="true">
          <rect x="2.5" y="5.5" width="7" height="5" rx="1" fill="none" stroke="currentColor" strokeWidth="1.2" />
          <path d="M4 5.5V4a2 2 0 0 1 4 0v1.5" fill="none" stroke="currentColor" strokeWidth="1.2" />
        </svg>
      ) : (
        <span className="ui-source-dot" aria-hidden="true" />
      )}
      {LABEL[source]}
      {source === "project" && onReset && (
        <button type="button" className="ui-source-reset" onClick={onReset} aria-label={props.resetLabel ?? "Reset to the default"}>
          Reset
        </button>
      )}
    </span>
  );
}
