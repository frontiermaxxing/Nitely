import { useEffect, useId, useRef, useState, type KeyboardEvent } from "react";
import { CLOSED, chosenValue, onKey, openAt, type MenuState, type SelectOption } from "./selectKeys";
import "./select.css";

export type { SelectOption } from "./selectKeys";

/**
 * The shared select (NIT-425): a 32px trigger that opens Nitely's own menu, not the system's list.
 * Each option can carry a second line, a disabled one says why, the current one has a check, and
 * the one equal to `defaultValue` a small "default" mark. Picking that one reports `null`, so a
 * caller clears its own value and follows the default. Keyboard: arrows, Home/End, Enter or Space,
 * Esc, and type-ahead; focus comes back to the trigger.
 */
export function Select<T extends string>(props: {
  /** The value set here, or null to follow `defaultValue`. */
  value: T | null;
  options: SelectOption<T>[];
  onChange: (value: T | null) => void;
  /** What applies when nothing is set here (a playbook's value, Nitely's own). */
  defaultValue?: T | null;
  "aria-label"?: string;
  /** The id of the label element that names it. */
  labelledBy?: string;
  disabled?: boolean;
  /** Shown on the trigger when nothing is set and there is no default. */
  placeholder?: string;
  className?: string;
}) {
  const { options, defaultValue } = props;
  const shown = props.value ?? defaultValue ?? null;
  const [menu, setMenu] = useState<MenuState>(CLOSED);
  const wrap = useRef<HTMLDivElement>(null);
  const trigger = useRef<HTMLButtonElement>(null);
  const list = useRef<HTMLUListElement>(null);
  const id = useId();
  const current = options.find((o) => o.value === shown);

  // A press anywhere outside closes it, as the system's list does.
  useEffect(() => {
    if (!menu.open) return;
    const away = (e: PointerEvent) => {
      if (!wrap.current?.contains(e.target as Node)) setMenu(CLOSED);
    };
    document.addEventListener("pointerdown", away);
    return () => document.removeEventListener("pointerdown", away);
  }, [menu.open]);

  // The keyboard's option stays in view in a long list.
  useEffect(() => {
    if (menu.open && menu.active >= 0) list.current?.children[menu.active]?.scrollIntoView({ block: "nearest" });
  }, [menu.open, menu.active]);

  const pick = (at: number) => {
    setMenu(CLOSED);
    trigger.current?.focus();
    const o = options[at];
    if (o && !o.disabled) props.onChange(chosenValue(o.value, defaultValue));
  };

  const key = (e: KeyboardEvent) => {
    const r = onKey(menu, options, e.key, Date.now(), shown);
    if (r.handled) e.preventDefault();
    if (r.picked !== undefined) return pick(r.picked);
    if (menu.open && !r.state.open) trigger.current?.focus();
    setMenu(r.state);
  };

  const activeId = menu.open && menu.active >= 0 ? `${id}-${menu.active}` : undefined;
  return (
    <div className={`select${props.className ? ` ${props.className}` : ""}`} ref={wrap}>
      <button
        type="button"
        ref={trigger}
        className="select-trigger"
        disabled={props.disabled}
        aria-haspopup="listbox"
        aria-expanded={menu.open}
        aria-controls={`${id}-list`}
        aria-activedescendant={activeId}
        aria-label={props["aria-label"]}
        aria-labelledby={props.labelledBy}
        onClick={() => setMenu(menu.open ? CLOSED : openAt(options, shown))}
        onKeyDown={key}
      >
        <span className={`select-value${current ? "" : " select-placeholder"}`}>{current?.label ?? props.placeholder ?? "Choose…"}</span>
        <svg className="select-chevron" viewBox="0 0 12 12" width="12" height="12" aria-hidden="true">
          <path d="M3 4.5 6 7.5 9 4.5" fill="none" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round" strokeLinejoin="round" />
        </svg>
      </button>
      {menu.open && (
        <ul className="select-menu" role="listbox" id={`${id}-list`} ref={list} aria-label={props["aria-label"]} aria-labelledby={props.labelledBy}>
          {options.map((o, i) => {
            const selected = o.value === shown;
            const isDefault = defaultValue !== undefined && defaultValue !== null && o.value === defaultValue;
            return (
              <li
                key={o.value}
                id={`${id}-${i}`}
                role="option"
                aria-selected={selected}
                aria-disabled={o.disabled ? true : undefined}
                className={`select-option${i === menu.active ? " is-active" : ""}${o.disabled ? " is-disabled" : ""}`}
                onPointerMove={() => !o.disabled && i !== menu.active && setMenu({ ...menu, active: i })}
                // Picked on press, before the trigger loses focus to the menu.
                onPointerDown={(e) => e.preventDefault()}
                onClick={() => !o.disabled && pick(i)}
                title={o.disabled}
              >
                <span className="select-check" aria-hidden="true">
                  {selected && (
                    <svg viewBox="0 0 12 12" width="12" height="12">
                      <path d="M2.5 6.2 5 8.6 9.5 3.6" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" />
                    </svg>
                  )}
                </span>
                <span className="select-text">
                  <span className="select-label">
                    {o.label}
                    {isDefault && <span className="select-default">default</span>}
                  </span>
                  {(o.disabled || o.detail) && <span className="select-detail">{o.disabled ?? o.detail}</span>}
                </span>
              </li>
            );
          })}
        </ul>
      )}
    </div>
  );
}
