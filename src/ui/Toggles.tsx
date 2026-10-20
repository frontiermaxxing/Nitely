import { useId, type ReactNode } from "react";
import "./toggles.css";

// The kit's on/off and choice controls (docs/visualization.md, "On/off and choices"). A Switch
// for a setting that takes effect on its own; a Checkbox only inside a form saved as a whole; a Segmented control
// for two or three short options; Radio cards for options that each need a sentence. The choices
// are native radios underneath, so arrow keys, Tab and forms work as they always do.

/** On or off, applied at once: notifications, review on or off. */
export function Switch(props: {
  checked: boolean;
  onChange: (checked: boolean) => void;
  label?: ReactNode;
  disabled?: boolean;
  "aria-label"?: string;
}) {
  return (
    <button
      type="button"
      role="switch"
      className="ui-switch"
      aria-checked={props.checked}
      aria-label={props["aria-label"]}
      disabled={props.disabled}
      onClick={() => props.onChange(!props.checked)}
    >
      <span className="ui-switch-track" aria-hidden="true">
        <span className="ui-switch-thumb" />
      </span>
      {props.label != null && <span className="ui-toggle-label">{props.label}</span>}
    </button>
  );
}

/** A choice inside a form that is saved together. */
export function Checkbox(props: {
  checked: boolean;
  onChange: (checked: boolean) => void;
  label?: ReactNode;
  disabled?: boolean;
  name?: string;
  "aria-label"?: string;
}) {
  return (
    <label className={`ui-check${props.disabled ? " ui-disabled" : ""}`}>
      <input
        type="checkbox"
        className="ui-check-box"
        name={props.name}
        checked={props.checked}
        disabled={props.disabled}
        aria-label={props["aria-label"]}
        onChange={(e) => props.onChange(e.target.checked)}
      />
      {props.label != null && <span className="ui-toggle-label">{props.label}</span>}
    </label>
  );
}

/** One option of a Segmented control or Radio cards. `isDefault` marks the one that equals the
 * default: choosing it clears the project's own value, so no "Inherit (…)" entry is needed. */
export interface ChoiceOption<T extends string> {
  value: T;
  label: ReactNode;
  /** Radio cards: the sentence on what it does. */
  why?: ReactNode;
  isDefault?: boolean;
  disabled?: boolean;
}

function DefaultMark() {
  return <span className="ui-default-mark">default</span>;
}

/** Two or three short options. */
export function Segmented<T extends string>(props: {
  options: ChoiceOption<T>[];
  value: T | null;
  onChange: (value: T) => void;
  "aria-label": string;
  disabled?: boolean;
}) {
  const name = useId();
  return (
    <div className="ui-segmented" role="radiogroup" aria-label={props["aria-label"]}>
      {props.options.map((o) => (
        <label key={o.value} className={`ui-segment${o.value === props.value ? " ui-on" : ""}`}>
          <input
            type="radio"
            className="ui-hidden-radio"
            name={name}
            value={o.value}
            checked={o.value === props.value}
            disabled={props.disabled || o.disabled}
            onChange={() => props.onChange(o.value)}
          />
          {o.label}
          {o.isDefault && <DefaultMark />}
        </label>
      ))}
    </div>
  );
}

/** Options that each need a sentence. */
export function RadioCards<T extends string>(props: {
  options: ChoiceOption<T>[];
  value: T | null;
  onChange: (value: T) => void;
  "aria-label": string;
  disabled?: boolean;
}) {
  const name = useId();
  return (
    <div className="ui-radio-cards" role="radiogroup" aria-label={props["aria-label"]}>
      {props.options.map((o) => {
        const off = props.disabled || o.disabled;
        return (
          <label key={o.value} className={`ui-radio-card${o.value === props.value ? " ui-on" : ""}${off ? " ui-disabled" : ""}`}>
            <input
              type="radio"
              className="ui-radio"
              name={name}
              value={o.value}
              checked={o.value === props.value}
              disabled={off}
              onChange={() => props.onChange(o.value)}
            />
            <span className="ui-radio-card-text">
              <span className="ui-radio-card-title">
                {o.label}
                {o.isDefault && <DefaultMark />}
              </span>
              {o.why != null && <span className="ui-radio-card-why">{o.why}</span>}
            </span>
          </label>
        );
      })}
    </div>
  );
}
