import type { ButtonHTMLAttributes, ReactNode } from "react";

export type ButtonVariant = "primary" | "secondary" | "quiet" | "danger";

/**
 * A button of the settings kit. `primary` is the one action that saves (ink, not a brand color:
 * the attention color stays free for what needs you), `secondary` the bordered alternative,
 * `quiet` (no border) adds or resets, and `danger` is an action that loosens or removes for good.
 * `shortcut` shows the key that does the same, e.g. "Ctrl S".
 */
export function Button(props: ButtonHTMLAttributes<HTMLButtonElement> & { variant?: ButtonVariant; shortcut?: string; children: ReactNode }) {
  const { variant = "secondary", shortcut, className, children, type, ...rest } = props;
  return (
    <button type={type ?? "button"} className={`ui-button ui-button-${variant}${className ? ` ${className}` : ""}`} {...rest}>
      {children}
      {shortcut && <kbd className="ui-button-kbd">{shortcut}</kbd>}
    </button>
  );
}

/**
 * The × at a row's end that removes it, in place of the word "Remove". `label` names what goes,
 * for screen readers and the tooltip ("Remove ^src/billing/").
 */
export function RemoveButton(props: Omit<ButtonHTMLAttributes<HTMLButtonElement>, "children"> & { label: string }) {
  const { label, className, type, ...rest } = props;
  return (
    <button type={type ?? "button"} className={`ui-icon-button${className ? ` ${className}` : ""}`} aria-label={label} title={label} {...rest}>
      <svg width="14" height="14" viewBox="0 0 14 14" aria-hidden="true">
        <path d="M3.5 3.5l7 7M10.5 3.5l-7 7" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" />
      </svg>
    </button>
  );
}
