// Motion: every change is shown moving, not swapped in (docs/visualization.md, "Motion").
// CSS transitions and the Web Animations API only; animations run after the state changed and
// never hold anything up.

import { useLayoutEffect, useRef, type RefObject } from "react";

/** True when the person asked for less motion: changes are then instant, with the same mark. */
export function reducedMotion(): boolean {
  return typeof matchMedia === "function" && matchMedia("(prefers-reduced-motion: reduce)").matches;
}

const timings = new Map<string, number>();
/** A duration token from tokens.css in milliseconds (`--dur`, `--dur-travel`, …). */
export function duration(token: string, fallback: number): number {
  let ms = timings.get(token);
  if (ms === undefined) {
    const raw = getComputedStyle(document.documentElement).getPropertyValue(token).trim();
    ms = raw.endsWith("ms") ? parseFloat(raw) : raw.endsWith("s") ? parseFloat(raw) * 1000 : NaN;
    if (Number.isNaN(ms)) ms = fallback;
    timings.set(token, ms);
  }
  return ms;
}
export const EASE = "cubic-bezier(0.2, 0.7, 0.2, 1)";

/**
 * Mark something that just changed where it landed: a brief ring in `--arrive` (teal unless the
 * element sets it, amber for what needs the human) that fades. The same with reduced motion.
 */
export function markArrived(el: Element) {
  el.classList.remove("ui-arrived");
  // Restart the animation when it is marked again before the last one ended.
  void (el as HTMLElement).offsetWidth;
  el.classList.add("ui-arrived");
  el.addEventListener("animationend", () => el.classList.remove("ui-arrived"), { once: true });
}

/** Fade something new in from just below where it sits, and mark it. */
export function enter(el: HTMLElement) {
  if (!reducedMotion()) {
    el.animate([{ opacity: 0, transform: "translateY(4px)" }, { opacity: 1, transform: "none" }], { duration: duration("--dur", 200), easing: EASE });
  }
  markArrived(el);
}

interface Place {
  x: number;
  y: number;
  group: string | null;
}

/**
 * FLIP for a list or a board: every element under `ref` with `data-flip="<stable key>"` glides
 * from where it was to where it is whenever a render moves it, and one that arrives after the
 * first render fades in, marked. Moving to another `data-flip-group` (a board column, a station
 * box) marks it where it lands. Give items stable keys so they move instead of remounting.
 */
export function useFlip(ref: RefObject<HTMLElement | null>) {
  const last = useRef<Map<string, Place> | null>(null);
  useLayoutEffect(() => {
    const root = ref.current;
    if (!root) return;
    const reduce = reducedMotion();
    const travel = duration("--dur-travel", 420);
    const box = root.getBoundingClientRect();
    const next = new Map<string, Place>();
    for (const el of root.querySelectorAll<HTMLElement>("[data-flip]")) {
      const key = el.dataset.flip!;
      // Where it sits in the layout, without the offset of a glide still running.
      const gliding = el.getAnimations().filter((a) => a.id === "ui-flip");
      const off = gliding.length ? offsetOf(el) : { x: 0, y: 0 };
      const r = el.getBoundingClientRect();
      const place = {
        x: r.left - box.left + root.scrollLeft - off.x,
        y: r.top - box.top + root.scrollTop - off.y,
        group: el.closest<HTMLElement>("[data-flip-group]")?.dataset.flipGroup ?? null,
      };
      next.set(key, place);
      if (!last.current) continue;
      const was = last.current.get(key);
      if (!was) {
        enter(el);
        continue;
      }
      const dx = was.x - place.x;
      const dy = was.y - place.y;
      const moved = Math.abs(dx) >= 1 || Math.abs(dy) >= 1;
      if (moved && !reduce) {
        // From where it shows now: a glide cut short carries on from mid-way.
        for (const a of gliding) a.cancel();
        const a = el.animate([{ transform: `translate(${dx + off.x}px, ${dy + off.y}px)` }, { transform: "none" }], { duration: travel, easing: EASE });
        a.id = "ui-flip";
      }
      if (was.group !== place.group) markArrived(el);
    }
    last.current = next;
  });
}

/** The translation a running animation gives the element right now. */
function offsetOf(el: HTMLElement): { x: number; y: number } {
  const m = getComputedStyle(el).transform;
  if (!m || m === "none") return { x: 0, y: 0 };
  const t = new DOMMatrixReadOnly(m);
  return { x: t.m41, y: t.m42 };
}

/**
 * A number that ticks instead of jumping: when it changes, the new figure slides in from below
 * when it went up and from above when it went down. With reduced motion it is marked instead.
 */
export function Ticker(props: { value: number; className?: string }) {
  const ref = useRef<HTMLSpanElement>(null);
  const prev = useRef(props.value);
  useLayoutEffect(() => {
    const was = prev.current;
    prev.current = props.value;
    const el = ref.current;
    if (!el || was === props.value) return;
    if (reducedMotion()) {
      markArrived(el);
      return;
    }
    const from = props.value > was ? "0.5em" : "-0.5em";
    el.animate([{ opacity: 0, transform: `translateY(${from})` }, { opacity: 1, transform: "none" }], { duration: duration("--dur", 200), easing: EASE });
  }, [props.value]);
  return (
    <span ref={ref} className={`ui-ticker${props.className ? ` ${props.className}` : ""}`}>
      {props.value}
    </span>
  );
}
