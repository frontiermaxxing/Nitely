import type { ReactNode } from "react";

/**
 * A notice in a page: `danger` (red) for what is off or unsafe, `warning` (amber) for what waits
 * on you, `neutral` for how things are. A dot (or, for `locked`, a lock) and an optional bold
 * `lead` sentence come first; the rest says why or what to do.
 */
export function Notice(props: { tone: "danger" | "warning" | "neutral"; lead?: ReactNode; locked?: boolean; children?: ReactNode }) {
  const { tone, lead, locked, children } = props;
  return (
    <div className={`ui-notice ui-notice-${tone}`} role={tone === "danger" ? "alert" : "status"}>
      {locked ? (
        <svg className="ui-notice-lock" width="14" height="14" viewBox="0 0 12 12" aria-hidden="true">
          <rect x="2.5" y="5.5" width="7" height="5" rx="1" fill="none" stroke="currentColor" strokeWidth="1.2" />
          <path d="M4 5.5V4a2 2 0 0 1 4 0v1.5" fill="none" stroke="currentColor" strokeWidth="1.2" />
        </svg>
      ) : (
        <span className="ui-notice-dot" aria-hidden="true" />
      )}
      <span className="ui-notice-text">
        {lead && <strong className="ui-notice-lead">{lead}</strong>}
        {lead && children ? " " : null}
        {children}
      </span>
    </div>
  );
}
