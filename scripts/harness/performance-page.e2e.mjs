// Cached Performance page, recorder persistence and human controls on isolated harness data.
// CI only by default; app.mjs owns the native harness guard and one-window lock.
import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdirSync } from "node:fs";
import { join } from "node:path";
import { launch } from "./app.mjs";
import { installPerformanceProbe } from "./performance-probe.mjs";
const SCREENS = join("target", "harness", "screens");
mkdirSync(SCREENS, { recursive: true });
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

test("Performance history survives an app restart and only clears through paused human controls", { timeout: 180000 }, async () => {
  const app = await launch();
  const showPage = async () => {
    // Emulate renderer focus/visibility without touching the runner's native desktop focus.
    await app.page.send("Emulation.setFocusEmulationEnabled", { enabled: true });
    assert.equal(await app.page.eval(`document.dispatchEvent(new Event('visibilitychange')); return document.hidden;`), false, "the Performance test page is visible");
  };
  const query = () => app.page.invoke("performance_query", { query: { view: "history", limit: 100 } });
  const clickControl = async (text) => {
    await app.page.eval(
      `const button = [...document.querySelectorAll('button')].find(b => b.textContent.trim() === ${JSON.stringify(text)}); button?.scrollIntoView({ block: 'center' });`,
    );
    await app.page.click({ css: "button", text });
  };
  try {
    await showPage();
    await app.page.open("#performance");
    await app.page.waitFor(`return document.querySelector('[data-testid="performance-page"]') !== null`, { what: "Performance route" });
    let recorded;
    for (let i = 0; i < 40; i++) {
      recorded = await query();
      if (recorded.latest) break;
      await sleep(500);
    }
    assert.ok(recorded.latest, "the isolated recorder produced a sample");
    assert.ok(recorded.points.length, "history reads the saved sample");
    await app.page.waitFor(`return document.querySelector('.perf-freshness')?.textContent.includes('Recording')`, { what: "recording freshness" });
    await app.page.screenshot(join(SCREENS, "performance-recording.png"));
    // Board changes must not restart the page's independent cadence. Instrument only this
    // throwaway page, leaving the real backend response and recorder untouched.
    await app.page.eval(`(${installPerformanceProbe.toString()})(window);`);
    // Fail at setup if Tauri changes transport, rather than misdiagnosing a real refresh.
    await app.page.invoke("performance_query", { query: { view: "now", limit: 200 } });
    assert.ok(await app.page.eval(`return window.__perfChecks.reads.some(q => q.view === 'now');`), "probe observes an actual native invoke");
    await clickControl("Refresh readings");
    await app.page.waitFor(`return window.__perfChecks.reads.some(q => q.view === 'history')`, { what: "explicit refresh" });
    const histories = () => app.page.eval(`return window.__perfChecks.reads.filter(q => q.view === 'history').length`);
    const initialHistoryReads = await histories();
    for (let i = 0; i < 3; i++) {
      const snapshots = await app.page.eval(`return window.__perfChecks.snapshots`);
      await app.page.invoke("plugin:event|emit", { event: "nitely://changed", payload: null });
      await app.page.waitFor(`return window.__perfChecks.snapshots > ${snapshots}`, { what: "board version update" });
    }
    assert.equal(await histories(), initialHistoryReads, "board changes do not reread history before its cadence");
    await app.page.eval(`Object.defineProperty(document, 'hidden', { configurable: true, value: true }); document.dispatchEvent(new Event('visibilitychange'));`);
    const hiddenReads = await app.page.eval(`return window.__perfChecks.reads.length`);
    await app.page.invoke("plugin:event|emit", { event: "nitely://changed", payload: null });
    await sleep(31_000); // Includes App's ordinary 30-second board refresh and several Now deadlines.
    assert.equal(await app.page.eval(`return window.__perfChecks.reads.length`), hiddenReads, "hidden page does not issue Now or history reads");
    await app.page.eval(`delete document.hidden; document.dispatchEvent(new Event('visibilitychange'));`);
    assert.equal(await histories(), initialHistoryReads, "becoming visible early does not bypass history cadence");

    // Deterministic older incident response for the native page wiring. The separate model
    // regression exercises a full two-hour fixture with the unchanged 200-observation cap.
    const incidentFrom = recorded.latest.wall_ms - 3_600_000;
    const incidentTo = incidentFrom + 120_000;
    await app.page.eval(`
      window.__perfChecks.incident = { from_ms: ${incidentFrom}, to_ms: ${incidentTo} };
      window.__perfChecks.transform = (report, query) => {
        if (query.view !== "history") return report;
        const incident = window.__perfChecks.incident;
        report.episodes = report.from_ms <= incident.to_ms && report.to_ms >= incident.from_ms ? [{ ...incident, observations: 25, observed: ["cpu_at_least_90_percent"], inferred_causes: [],
          peaks: { ...report.latest.metrics, cpu_percent: 99 }, min_available_bytes: null, min_available_percent: null, max_commit_percent: null }] : [];
        report.attribution = [];
        if (query.from_ms === incident.from_ms - 30000 && query.to_ms === incident.to_ms) {
          const usage = { cpu_percent: 50, private_commit_bytes: 1024, working_set_sum_bytes: 2048, process_read_bytes: 0, process_write_bytes: 0, processes: 1 };
          report.attribution = [{ wall_ms: incident.to_ms, monotonic_ms: 1000, boot: "fixture-boot", session: "older-incident", interval_ms: 5000, collection_ms: 1,
            resolution: "sample", observations: 1, coverage: { enumerated: 1, measured: 1, denied: 0, missing_identity: 0, partial_metrics: 0, truncated: false, first_sample: false, supported: true, short_lived_partial: true },
            nitely: usage, related: usage, other: usage, unattributed_cpu_percent: null, top_other: [],
            workloads: [{ id: "old-build", kind: "build", context: { project: "APP", issue: "APP-17", run_id: 17 }, usage }] }];
          report.events = [{ wall_ms: incident.to_ms, kind: "build_finished", project: "APP", issue: "APP-17", run: 17 }];
        }
        return report;
      };
    `);
    await clickControl("Refresh readings");
    await app.page.waitFor(`return document.querySelector('.perf-inspect') !== null`, { what: "older incident" });
    await clickControl("Inspect incident");
    await app.page.waitFor(`return document.body.textContent.includes('APP-17') && document.body.textContent.includes('Fixed window')`, { what: "older incident workload detail" });
    assert.equal(await app.page.eval(`return document.querySelector('button[aria-label="Workload sample"] .select-value')?.textContent`), "Last observation in this window");
    const incidentQueries = await app.page.eval(`return window.__perfChecks.reads.filter(q => q.view === 'history')`);
    assert.equal(incidentQueries.at(-1).from_ms, incidentFrom - 30_000);
    assert.equal(incidentQueries.at(-1).to_ms, incidentTo);
    await clickControl("Previous window");
    await app.page.waitFor(`return document.querySelector('[aria-label="Workload sample"]') !== null && !document.body.textContent.includes('APP-17')`, { what: "old workload is removed outside its window" });
    // Custom windows use native datetime controls and the exact query bounds, not an end of now.
    await app.page.eval(`
      const inputs = document.querySelectorAll('.perf-window-form input');
      const set = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value').set;
      for (const [index, value] of ['2026-10-01T10:00:00', '2026-10-01T10:10:00'].entries()) {
        set.call(inputs[index], value); inputs[index].dispatchEvent(new Event('input', { bubbles: true }));
      }
    `);
    await clickControl("Open time window");
    await app.page.waitFor(`return window.__perfChecks.reads.some(q => q.from_ms === new Date('2026-10-01T10:00:00').getTime() && q.to_ms === new Date('2026-10-01T10:10:00').getTime())`, { what: "custom historical range" });
    await app.page.waitFor(`return document.querySelector('[aria-label="Workload sample"]') !== null && document.querySelector('#performance-history')?.textContent.includes(new Date('2026-10-01T10:00:00').toLocaleString())`, { what: "custom history response rendered" });
    await app.page.screenshot(join(SCREENS, "performance-fixed-window.png"));
    await app.page.eval(`window.__perfChecks.restore(); delete window.__perfChecks;`);
    await clickControl("1h");

    assert.equal(await app.page.eval(`return [...document.querySelectorAll('button')].find(b => b.textContent === 'Clear history…')?.disabled`), true);
    await clickControl("Pause collection");
    await app.page.waitFor(`return document.querySelector('.perf-freshness')?.textContent.includes('paused')`, { what: "paused collection" });
    assert.equal((await query()).settings.enabled, false);
    await app.restart();
    await showPage();
    await app.page.open("#performance");
    const restored = await query();
    assert.equal(restored.settings.enabled, false, "pause is durable");
    assert.ok(
      restored.points.some((p) => p.to_ms === recorded.latest.wall_ms),
      "recorded history survives app/recorder restart",
    );
    // Restarting the app is not evidence of a machine reboot.
    // Capture the backend rejection before CDP reduces an uncaught promise to a generic error.
    const beforeRestart = await app.page.eval(`
      try {
        await window.__TAURI_INTERNALS__.invoke("performance_query", { query: { view: "history", before_last_restart: true } });
        return { rejected: false, message: "" };
      } catch (error) {
        return { rejected: true, message: typeof error === "string" ? error : error instanceof Error ? error.message : JSON.stringify(error) };
      }
    `);
    assert.equal(beforeRestart.rejected, true, "an app restart must not establish a machine boot transition");
    assert.match(beforeRestart.message, /boot transition/);
    await app.page.waitFor(`return document.querySelector('.perf-freshness')?.textContent.includes('paused')`, { what: "restored paused state" });
    await clickControl("Clear history…");
    await clickControl("Cancel");
    assert.ok((await query()).latest, "cancelling preserves history");
    await clickControl("Clear history…");
    await clickControl("Clear recorded history");
    await app.page.waitFor(`return document.body.textContent.includes('Recorded history cleared.')`, { what: "clear completion" });
    const cleared = await query();
    assert.equal(cleared.latest, null);
    assert.equal(cleared.points.length, 0);
    await app.page.waitFor(
      `return document.querySelector('.perf-metrics')?.textContent.includes('Unavailable') && document.body.textContent.includes('Process attribution is unavailable')`,
      { what: "cleared readings removed from the page" },
    );
    await app.page.screenshot(join(SCREENS, "performance-paused-empty.png"));
    await clickControl("Resume collection");
    await app.page.waitFor(`return document.body.textContent.includes('Collection enabled. Settings saved.')`, { what: "resume completion" });
    assert.equal((await query()).settings.enabled, true);
  } finally {
    await app.close();
  }
});
