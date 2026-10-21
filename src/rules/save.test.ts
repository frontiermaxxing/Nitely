import { test } from "node:test";
import assert from "node:assert/strict";
import { confirmLoosening } from "./save.ts";

test("a loosening is never saved until the person confirms the same reasons they were shown", async () => {
  // A stand-in backend: `rules_check` answers what loosens now, `rules_save` records each save.
  let loosens = ["Changes to `^src/billing/` (billing and money) no longer wait for a person."];
  const saves: boolean[] = [];
  const check = async () => loosens;
  const save = async (confirm: boolean) => {
    saves.push(confirm);
    return "saved";
  };

  // Save: nothing is written, the reasons come back for the confirmation panel.
  assert.deepEqual(await confirmLoosening(check, save, null), { loosens });
  assert.deepEqual(saves, []);

  // The edit changed after the panel showed (another rule dropped too): confirm again, still no save.
  const shown = [...loosens];
  loosens = [...shown, "The reviewer no longer answers MONEY_OR_RELEASE (pricing)."];
  assert.deepEqual(await confirmLoosening(check, save, shown), { loosens });
  assert.deepEqual(saves, []);

  // Loosen and save on exactly what was shown: one confirmed save.
  assert.deepEqual(await confirmLoosening(check, save, [...loosens]), { saved: "saved" });
  assert.deepEqual(saves, [true]);

  // Nothing loosens: it saves unconfirmed, with no panel.
  loosens = [];
  assert.deepEqual(await confirmLoosening(check, save, null), { saved: "saved" });
  assert.deepEqual(saves, [true, false]);
});
