import { test } from "node:test";
import assert from "node:assert/strict";
import { installPerformanceProbe } from "./harness/performance-probe.mjs";

test("native Performance probe observes immutable Tauri invoke and transforms only successful selected queries", async () => {
  let reject = false;
  const target = { Response, fetch: async (input) => new Response(JSON.stringify({ command: input, original: true }), {
    headers: { "Content-Type": "application/json", "Tauri-Response": reject ? "error" : "ok" },
  }) };
  const originalFetch = target.fetch;
  const internals = {};
  Object.defineProperty(target, "__TAURI_INTERNALS__", { value: internals });
  Object.defineProperty(internals, "convertFileSrc", { value: (command) => `http://ipc.localhost/${command}` });
  Object.defineProperty(internals, "invoke", { value: async (command, args) => {
    const response = await target.fetch(internals.convertFileSrc(command, "ipc"), { body: JSON.stringify(args) });
    return response.json();
  } });
  const originalInvoke = internals.invoke;
  assert.equal(Reflect.set(internals, "invoke", () => {}), false, "old test silently failed to replace this property");
  installPerformanceProbe(target);
  assert.equal(internals.invoke, originalInvoke);
  const query = { view: "history", from_ms: 100, to_ms: 200 };
  const response = await internals.invoke("performance_query", { query });
  assert.equal(response.original, true);
  assert.deepEqual(target.__perfChecks.reads, [query]);
  await internals.invoke("snapshot", {});
  assert.equal(target.__perfChecks.snapshots, 1);
  target.__perfChecks.transform = (report, request) => request.view === "history" ? { ...report, incident: request.from_ms } : report;
  assert.equal((await internals.invoke("performance_query", { query })).incident, 100);
  assert.equal((await internals.invoke("performance_query", { query: { view: "now" } })).incident, undefined);
  assert.equal((await internals.invoke("snapshot", {})).incident, undefined);
  reject = true;
  assert.equal((await internals.invoke("performance_query", { query })).incident, undefined, "backend rejections are not changed into incident fixtures");
  reject = false;
  target.__perfChecks.restore();
  assert.equal(target.fetch, originalFetch);
  assert.equal((await internals.invoke("performance_query", { query })).incident, undefined);
});
