import type { ReactNode } from "react";

/**
 * What a view shows when it has nothing: an empty track (the house glyph for "nothing moving"),
 * one plain sentence, and what would fill it.
 */
export function EmptyState(props: { title: string; children?: ReactNode; action?: ReactNode }) {
  return (
    <div className="ui-empty">
      <svg className="ui-empty-glyph" width="62" height="10" viewBox="0 0 62 10" aria-hidden="true">
        <path d="M5 5H57" stroke="var(--line-strong)" strokeWidth="1.5" strokeLinecap="round" strokeDasharray="0.1 4" />
        {[5, 31, 57].map((x) => (
          <circle key={x} cx={x} cy="5" r="3.2" fill="var(--surface)" stroke="var(--line-strong)" strokeWidth="1.5" />
        ))}
      </svg>
      <p className="ui-empty-title">{props.title}</p>
      {props.children && <div className="ui-empty-detail">{props.children}</div>}
      {props.action && <div className="ui-empty-action">{props.action}</div>}
    </div>
  );
}
