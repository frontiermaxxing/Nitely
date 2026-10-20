import { test } from "node:test";
import assert from "node:assert/strict";
import { pollPerformance, type PollClock } from "./performancePolling.ts";

function clockFixture() {
  let time = 0, hidden = false;
  const listeners = new Set<() => void>();
  const timers = new Map<number, { at: number; callback: () => void }>();
  let next = 0;
  const clock: PollClock = {
    now: () => time, hidden: () => hidden,
    later(callback, delay) { const id = ++next; timers.set(id, { at: time + delay, callback }); return id; },
    cancel(id) { timers.delete(id as number); },
    visibility(callback) { listeners.add(callback); return () => { listeners.delete(callback); }; },
  };
  return {
    clock,
    visibility(value: boolean) { hidden = value; for (const listener of listeners) listener(); },
    async advance(ms: number) {
      const end = time + ms;
      for (;;) {
        const due = [...timers.entries()].sort((a, b) => a[1].at - b[1].at)[0];
        if (!due || due[1].at > end) break;
        time = due[1].at; timers.delete(due[0]); due[1].callback();
        await Promise.resolve();
      }
      time = end;
      await Promise.resolve();
    },
    timers,
  };
}

test("visibility and frequent wakeups cannot bypass history cadence; fixed history reads once", async () => {
  const env = clockFixture();
  let reads = 0, fixedReads = 0;
  const stop = pollPerformance(async () => { reads++; }, 60_000, env.clock);
  const stopFixed = pollPerformance(async () => { fixedReads++; }, null, env.clock);
  await env.advance(0);
  for (let i = 0; i < 59; i++) {
    await env.advance(1000);
    env.visibility(true); env.visibility(false);
  }
  assert.equal(reads, 1);
  assert.equal(fixedReads, 1);
  await env.advance(1000);
  assert.equal(reads, 2);
  assert.equal(fixedReads, 1);
  stop(); stopFixed();
  assert.equal(env.timers.size, 0);
});

test("hidden mounting, long hidden periods and in-flight completion never initiate background reads", async () => {
  const env = clockFixture();
  env.visibility(true);
  let reads = 0;
  let finish: (() => void) | undefined;
  const stop = pollPerformance(() => { reads++; return new Promise<void>((resolve) => { finish = resolve; }); }, 5000, env.clock);
  await env.advance(60_000);
  assert.equal(reads, 0);
  env.visibility(false);
  assert.equal(reads, 1);
  for (let i = 0; i < 5; i++) { env.visibility(true); env.visibility(false); }
  assert.equal(reads, 1, "only one read may be in flight");
  env.visibility(true); finish!();
  await env.advance(0);
  await env.advance(60_000);
  assert.equal(reads, 1);
  env.visibility(false);
  assert.equal(reads, 2);
  stop(); finish!(); await env.advance(60_000);
  assert.equal(reads, 2, "completion after unmount cannot restart the poller");
});
