// Runs with `npm test` (scripts/test.mjs).
import test from "node:test";
import assert from "node:assert/strict";
import { findings, findingsIn } from "./check-controls.mjs";

test("src has no native select, no native checkbox and no Inherit option", () => {
  assert.deepEqual(findings(), [], "the shared Select and Checkbox replace them (docs/visualization.md)");
});

test("each native control is caught, in code but not in a comment or the shared control", () => {
  const text = [
    `<select value={v}>`,
    `<input type="checkbox" checked />`,
    `<option value="">Inherit (Codex)</option>`,
    `// no "Inherit (…)" entry, and no <select> either`,
    `/* type="checkbox" */`,
  ].join("\n");
  assert.deepEqual(
    findingsIn("src/Page.tsx", text).map((f) => f.line),
    [1, 2, 3],
  );
  assert.deepEqual(findingsIn("src/ui/Toggles.tsx", `<input type="checkbox" />`), []);
});
