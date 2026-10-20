// How the shared Select's menu answers the keyboard (NIT-425), apart from React so node:test runs
// it (selectKeys.test.ts): arrows move over the options a person may pick, Enter or Space picks, Esc
// closes, and typing jumps to the option whose label starts with what was typed.

import type { ReactNode } from "react";

export interface SelectOption<T extends string = string> {
  value: T;
  label: string;
  /** A mark before the label, on the trigger too (an agent's vendor mark). */
  icon?: ReactNode;
  /** A second, quieter line under the label. */
  detail?: string;
  /** "warn" draws the second line in amber: picking it makes a change wait for you. */
  tone?: "warn";
  /** Why it cannot be picked ("Not installed on this machine"); set, the option is disabled. */
  disabled?: string;
}

export interface MenuState {
  open: boolean;
  /** The option the keyboard is on, or -1. */
  active: number;
  /** What was typed for type-ahead, and when the last key came. */
  typed: string;
  typedAt: number;
}

export const CLOSED: MenuState = { open: false, active: -1, typed: "", typedAt: 0 };

/** Keys typed within this many milliseconds of each other make one type-ahead word. */
export const TYPE_AHEAD_MS = 700;

const pickable = (o: SelectOption) => !o.disabled;

/** The next option one may pick from `from`, `step` forward or back, wrapping; -1 when none can be. */
export function nextEnabled(options: SelectOption[], from: number, step: 1 | -1): number {
  const n = options.length;
  for (let i = 1; i <= n; i++) {
    const at = (((from + step * i) % n) + n) % n;
    if (pickable(options[at])) return at;
  }
  return -1;
}

/** The first option from `from` on whose label starts with `typed`, ignoring case; -1 when none. */
export function typeAhead(options: SelectOption[], typed: string, from: number): number {
  const want = typed.toLowerCase();
  const n = options.length;
  for (let i = 0; i < n; i++) {
    const at = (Math.max(from, 0) + i) % n;
    if (pickable(options[at]) && options[at].label.toLowerCase().startsWith(want)) return at;
  }
  return -1;
}

/** Where the menu opens: on the current option when one may pick it, else on the first one may. */
export function openAt(options: SelectOption[], current: string | null | undefined): MenuState {
  const at = options.findIndex((o) => o.value === current && pickable(o));
  return { ...CLOSED, open: true, active: at >= 0 ? at : nextEnabled(options, -1, 1) };
}

export interface KeyResult {
  state: MenuState;
  /** The option picked, by index, when the key picked one. */
  picked?: number;
  /** The key was the menu's; the caller stops it there (no scrolling, no form submit). */
  handled: boolean;
}

/** What `key` does with the menu in `state`. `current` is the value shown on the trigger. */
export function onKey(state: MenuState, options: SelectOption[], key: string, now: number, current: string | null | undefined): KeyResult {
  if (!state.open) {
    if (key === "ArrowDown" || key === "ArrowUp" || key === "Enter" || key === " ") return { state: openAt(options, current), handled: true };
    return { state, handled: false };
  }
  switch (key) {
    case "ArrowDown":
      return { state: { ...state, active: nextEnabled(options, state.active, 1) }, handled: true };
    case "ArrowUp":
      return { state: { ...state, active: nextEnabled(options, state.active < 0 ? 0 : state.active, -1) }, handled: true };
    case "Home":
      return { state: { ...state, active: nextEnabled(options, -1, 1) }, handled: true };
    case "End":
      return { state: { ...state, active: nextEnabled(options, 0, -1) }, handled: true };
    case "Enter":
    case " ":
      // A space while typing a word is part of the word.
      if (key === " " && state.typed && now - state.typedAt < TYPE_AHEAD_MS) break;
      return state.active >= 0 && pickable(options[state.active]) ? { state: CLOSED, picked: state.active, handled: true } : { state, handled: true };
    case "Escape":
      return { state: CLOSED, handled: true };
    case "Tab":
      return { state: CLOSED, handled: false };
  }
  if (key.length !== 1) return { state, handled: false };
  const typed = (now - state.typedAt < TYPE_AHEAD_MS ? state.typed : "") + key;
  // One key again and again steps through the options it starts; a word looks from the current one.
  const repeat = typed.length > 1 && [...typed].every((c) => c === key);
  const at = repeat ? typeAhead(options, key, state.active + 1) : typeAhead(options, typed, state.active < 0 ? 0 : typed.length > 1 ? state.active : state.active + 1);
  return { state: { ...state, typed, typedAt: now, active: at >= 0 ? at : state.active }, handled: true };
}

/** What picking `value` reports: null when it is the default, so the caller clears its own value. */
export function chosenValue<T extends string>(value: T, defaultValue: T | null | undefined): T | null {
  return defaultValue !== undefined && defaultValue !== null && value === defaultValue ? null : value;
}
