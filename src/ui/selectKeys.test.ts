import { test } from "node:test";
import assert from "node:assert/strict";
import { CLOSED, chosenValue, onKey, type MenuState, type SelectOption } from "./selectKeys.ts";

const options: SelectOption[] = [
  { value: "claude", label: "Claude", detail: "Reviews by default" },
  { value: "codex", label: "Codex" },
  { value: "cursor", label: "Cursor", disabled: "Not installed on this machine" },
  { value: "copilot", label: "Copilot" },
];

test("a person picks with the keyboard, past a disabled option, and the default reports null", () => {
  let state: MenuState = CLOSED;
  let now = 0;
  const press = (key: string, current: string | null = "claude") => {
    now += 1000;
    const r = onKey(state, options, key, now, current);
    state = r.state;
    return r;
  };
  // Down opens it on the current value.
  assert.equal(press("ArrowDown").handled, true);
  assert.deepEqual([state.open, state.active], [true, 0]);
  // Down twice steps over Cursor, which says why it cannot be picked.
  press("ArrowDown");
  press("ArrowDown");
  assert.equal(state.active, 3);
  // Down again wraps to the top; Up from there wraps to the bottom.
  press("ArrowDown");
  assert.equal(state.active, 0);
  press("ArrowUp");
  assert.equal(state.active, 3);
  // Type-ahead: "co" lands on Codex.
  now += 1000;
  state = onKey(state, options, "c", now, "claude").state;
  state = onKey(state, options, "o", now + 100, "claude").state;
  assert.equal(options[state.active].value, "codex");
  // Esc closes without picking.
  const esc = press("Escape");
  assert.equal(esc.picked, undefined);
  assert.equal(state.open, false);
  // Enter on Claude, the default, reports null: the caller clears its own value.
  press("Enter", "codex");
  assert.equal(options[state.active].value, "codex");
  press("ArrowUp", "codex");
  const enter = press("Enter", "codex");
  assert.equal(enter.picked, 0);
  assert.equal(chosenValue(options[enter.picked!].value, "claude"), null);
  assert.equal(chosenValue("codex", "claude"), "codex");
  assert.equal(chosenValue("codex", null), "codex");
  // Enter on a disabled option does nothing.
  state = { ...CLOSED, open: true, active: 2 };
  assert.equal(press("Enter").picked, undefined);
  assert.equal(state.open, true);
});
