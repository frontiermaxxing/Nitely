import { test } from "node:test";
import assert from "node:assert/strict";
import { editMode, modeSave, modeValidation, optionsFor, INHERITED_OPTIONS, type ModeOptions } from "./reviewModes.ts";

test("editing a preset makes Custom without mutating preset policy or losing saved custom choices", () => {
  const custom: ModeOptions = { test_depth: null, pre_review: "off", land_batch: null, shared_lanes: true, full_backstop: null, audit_percent: null, periodic: { enabled: true, every_landings: 12, max_delay_seconds: 3600, reviewer: "codex", findings_strategy: "create" }, goal_branches: null, item_review: null, checkpoint: null, goal_plans: null };
  const fast = { ...structuredClone(custom), test_depth: "basic" as const };
  const presets = [{ mode: "fast" as const, options: fast }];
  const chosen = optionsFor("fast", presets, custom);
  const edited = editMode({ mode: "fast", overrides: chosen }, { periodic: { ...chosen.periodic!, every_landings: 7 } });
  assert.equal(edited.mode, "custom");
  assert.equal(edited.overrides.test_depth, "basic");
  assert.equal(fast.periodic!.every_landings, 12);
  assert.deepEqual(optionsFor("custom", presets, custom), custom);
  assert.deepEqual(modeSave({ mode: "fast", overrides: chosen }).overrides, INHERITED_OPTIONS);
  assert.deepEqual(modeSave(edited).overrides, edited.overrides);
  assert.throws(() => optionsFor("release", presets, custom), /unavailable/);
  assert.equal(modeValidation(edited.overrides), null);
  assert.match(modeValidation({ ...edited.overrides, periodic: { ...edited.overrides.periodic!, every_landings: NaN } })!, /at least one merge/);
  assert.match(modeValidation({ ...edited.overrides, audit_percent: 1 })!, /10%/);
  assert.match(modeValidation({ ...edited.overrides, checkpoint: { items: 0, daily: true } })!, /checkpoint/);
});
