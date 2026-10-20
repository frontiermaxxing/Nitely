export interface PollClock {
  now(): number;
  hidden(): boolean;
  later(callback: () => void, delay: number): unknown;
  cancel(timer: unknown): void;
  visibility(callback: () => void): () => void;
}

/** One in-flight read, cadence measured after completion, and no new reads while hidden.
 * Fixed history uses a null interval: visibility changes never refetch a completed window.
 * Only replacing the controller for an explicit range/refresh/control change resets its cadence.
 */
export function pollPerformance(read: () => Promise<void>, interval: number | null, clock: PollClock): () => void {
  let stopped = false;
  let running = false;
  let due = -Infinity;
  let timer: unknown;
  const wake = () => {
    clock.cancel(timer);
    timer = undefined;
    if (stopped || running || clock.hidden() || due === Infinity) return;
    const delay = due - clock.now();
    if (delay > 0) {
      timer = clock.later(wake, delay);
      return;
    }
    running = true;
    void read().finally(() => {
      running = false;
      due = interval == null ? Infinity : clock.now() + interval;
      wake();
    });
  };
  const unlisten = clock.visibility(wake);
  wake();
  return () => {
    stopped = true;
    clock.cancel(timer);
    unlisten();
  };
}

export const performanceClock: PollClock = {
  now: () => performance.now(),
  hidden: () => document.hidden,
  later: (callback, delay) => setTimeout(callback, delay),
  cancel: (timer) => clearTimeout(timer as ReturnType<typeof setTimeout>),
  visibility: (callback) => {
    document.addEventListener("visibilitychange", callback);
    return () => document.removeEventListener("visibilitychange", callback);
  },
};
