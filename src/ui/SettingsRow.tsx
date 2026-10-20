import { useId, type ReactNode } from "react";
import "./settings-layout.css";

/**
 * A group of settings: a heading with a one-line description beside it and a hairline under it,
 * then its rows. Sections follow each other on the page with no card around them, and the page
 * keeps no max-width column: rows fill the width they are given.
 */
export function SettingsSection(props: { title: ReactNode; description?: ReactNode; id?: string; children: ReactNode }) {
  const heading = useId();
  return (
    <section className="setting-group" id={props.id} aria-labelledby={heading}>
      <header className="setting-group-head">
        <h2 id={heading}>{props.title}</h2>
        {props.description && <p>{props.description}</p>}
      </header>
      <div className="setting-group-rows">{props.children}</div>
    </section>
  );
}

/**
 * One setting: what it is and why on the left (320px), the control filling the middle, and where
 * its value comes from on the right (150px). In a section narrower than about 760px (a window of
 * about 1000px with the sidebar) the control drops under the label and the source stays beside it.
 * `htmlFor` names the control's input, so the label focuses it; without one the row is a group
 * labelled by its label.
 */
export function SettingsRow(props: { label: ReactNode; why?: ReactNode; source?: ReactNode; htmlFor?: string; id?: string; children: ReactNode }) {
  const label = useId();
  const why = useId();
  const Name = props.htmlFor ? "label" : "span";
  return (
    <div className="setting" id={props.id} role={props.htmlFor ? undefined : "group"} aria-labelledby={props.htmlFor ? undefined : label} aria-describedby={props.why ? why : undefined}>
      <div className="setting-label">
        <Name id={label} className="setting-name" htmlFor={props.htmlFor}>
          {props.label}
        </Name>
        {props.why && (
          <span id={why} className="setting-why">
            {props.why}
          </span>
        )}
      </div>
      <div className="setting-control">{props.children}</div>
      <div className="setting-source">{props.source}</div>
    </div>
  );
}
