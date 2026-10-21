import { useCallback, useState, type ReactNode } from "react";
import { AgentMark, VENDOR_LABEL, vendorInfo, vendorOf } from "../ui";
import { REPLY_BUCKETS, ROUND_LABELS, percent, shortDate, type DayCount, type VendorShare } from "./stats";

// The You page's charts. They follow docs/visualization.md: marks in the series colour, text in the
// text tokens, thin bars with a 2px surface gap, and each mark's value on hover and focus.

interface Tip {
  x: number;
  y: number;
  value: ReactNode;
  label: ReactNode;
}

/** One tooltip per chart: the mark under the pointer or the focus says its value, then what it is. */
function useTip() {
  const [tip, setTip] = useState<Tip | null>(null);
  const show = useCallback((el: Element, value: ReactNode, label: ReactNode) => {
    const r = el.getBoundingClientRect();
    setTip({ x: r.left + r.width / 2, y: r.top, value, label });
  }, []);
  const hide = useCallback(() => setTip(null), []);
  const node = tip && (
    <div className="lb-tip" role="tooltip" style={{ left: tip.x, top: tip.y }}>
      <strong>{tip.value}</strong>
      <span>{tip.label}</span>
    </div>
  );
  return { show, hide, node };
}

/** Props that make a mark show its tip on hover and focus. */
function tipProps(t: ReturnType<typeof useTip>, value: ReactNode, label: ReactNode) {
  return {
    onPointerEnter: (e: React.PointerEvent) => t.show(e.currentTarget, value, label),
    onPointerLeave: t.hide,
    onFocus: (e: React.FocusEvent) => t.show(e.currentTarget, value, label),
    onBlur: t.hide,
  };
}

/** Five steps of the live hue for a day's landings, 0 being none. */
function heatLevel(n: number, max: number): number {
  if (n <= 0 || max <= 0) return 0;
  return Math.min(4, Math.max(1, Math.ceil((n / max) * 4)));
}

const DAY_NAMES = ["Mon", "", "Wed", "", "Fri", "", "Sun"];

/** Landed per day, a column per week (Monday on top), the last 26 weeks. */
export function Heatmap(props: { daily: DayCount[]; now: number }) {
  const t = useTip();
  const max = Math.max(0, ...props.daily.map((d) => d.landed));
  const weeks = Math.ceil(props.daily.length / 7);
  const total = props.daily.reduce((n, d) => n + d.landed, 0);
  return (
    <div className="lb-heat-wrap">
      <div className="lb-heat-days" aria-hidden="true">
        {DAY_NAMES.map((d, i) => (
          <span key={i}>{d}</span>
        ))}
      </div>
      <div className="lb-heat" style={{ gridTemplateColumns: `repeat(${weeks}, minmax(0, 1fr))` }} role="img" aria-label={`${total} landed over the last ${weeks} weeks; the busiest day had ${max}`}>
        {props.daily.map((d) => {
          const future = d.day_start > props.now;
          const level = heatLevel(d.landed, max);
          return (
            <span
              key={d.day_start}
              className={`lb-heat-cell lb-heat-${future ? "future" : level}`}
              {...(future ? {} : tipProps(t, `${d.landed} landed`, new Date(d.day_start * 1000).toLocaleDateString("en-US", { weekday: "short", month: "short", day: "numeric" })))}
            />
          );
        })}
      </div>
      <div className="lb-heat-key" aria-hidden="true">
        less
        {[0, 1, 2, 3, 4].map((l) => (
          <span key={l} className={`lb-heat-cell lb-heat-${l}`} />
        ))}
        more
      </div>
      {t.node}
    </div>
  );
}

/** A column per week, this week last and in full; the others a step back. */
export function WeeklyBars(props: { values: number[]; weekStarts: number[]; format: (v: number) => string; label: string }) {
  const t = useTip();
  const max = Math.max(1, ...props.values);
  const last = props.values.length - 1;
  return (
    <div className="lb-weekly" role="img" aria-label={props.label}>
      {props.values.map((v, i) => (
        <span key={props.weekStarts[i]} className="lb-weekly-slot" {...tipProps(t, props.format(v), `Week of ${shortDate(props.weekStarts[i])}`)}>
          <span className={`lb-weekly-bar${i === last ? " lb-now" : ""}${v === 0 ? " lb-zero" : ""}`} style={{ height: v === 0 ? 1 : `${Math.max(4, (v / max) * 100)}%` }} />
        </span>
      ))}
      {t.node}
    </div>
  );
}

/** Your agents' hours against one developer's, on one scale. */
export function CompareBars(props: { agents: number; developer: number; format: (h: number) => string }) {
  const max = Math.max(props.agents, props.developer, 1);
  const row = (label: string, v: number, cls: string) => (
    <div className="lb-compare-row">
      <span className="lb-compare-label">{label}</span>
      <span className="lb-compare-track">
        <span className={`lb-compare-bar ${cls}`} style={{ width: `${Math.max(v > 0 ? 0.6 : 0, (v / max) * 100)}%` }} />
      </span>
      <span className="lb-num">{props.format(v)}</span>
    </div>
  );
  return (
    <div className="lb-compare">
      {row("Your agents", props.agents, "lb-agents")}
      {row("One developer", props.developer, "lb-developer")}
    </div>
  );
}

/** The crew by vendor: a split bar of what landed, then a row per vendor. */
export function VendorSplit(props: { vendors: VendorShare[] }) {
  const t = useTip();
  const color = (v: string) => vendorInfo(vendorOf(v))?.color ?? "var(--vendor-other)";
  const name = (v: string) => vendorInfo(vendorOf(v))?.product ?? (vendorOf(v) === "other" ? v : VENDOR_LABEL[vendorOf(v)]);
  return (
    <div className="lb-crew">
      <div className="lb-split" role="img" aria-label={props.vendors.map((v) => `${name(v.vendor)} ${v.landed}`).join(", ")}>
        {props.vendors.map((v) => (
          <span key={v.vendor} className="lb-split-part" style={{ flex: v.landed, background: color(v.vendor) }} {...tipProps(t, `${v.landed} landed · ${percent(v.share)}`, name(v.vendor))} />
        ))}
      </div>
      <div className="lb-crew-rows">
        <div className="lb-crew-row lb-crew-head">
          <span />
          <span />
          <span>landed</span>
          <span>first pass</span>
        </div>
        {props.vendors.map((v) => (
          <div key={v.vendor} className="lb-crew-row">
            <AgentMark vendor={v.vendor} size={16} />
            <span>{name(v.vendor)}</span>
            <span className="lb-num">{v.landed}</span>
            <span className="lb-num lb-quiet">{percent(v.first_pass_rate)}</span>
          </div>
        ))}
      </div>
      {t.node}
    </div>
  );
}

/** How many review rounds your changes took to land: 1, 2, 3, 4 or more. */
export function RoundsBars(props: { counts: number[] }) {
  const max = Math.max(1, ...props.counts);
  return (
    <div className="lb-rounds">
      {ROUND_LABELS.map((label, i) => (
        <div key={label} className="lb-rounds-row">
          <span className="lb-quiet">{label}</span>
          <span className="lb-rounds-track">
            {(props.counts[i] ?? 0) > 0 && <span className="lb-rounds-bar" style={{ width: `${((props.counts[i] ?? 0) / max) * 100}%` }} />}
          </span>
          <span className="lb-num">{props.counts[i] ?? 0}</span>
        </div>
      ))}
    </div>
  );
}

/** Inbox replies by how long they waited; a day or more waits in the attention colour. */
export function ReplyHistogram(props: { buckets: number[] }) {
  const t = useTip();
  const max = Math.max(1, ...props.buckets);
  const last = REPLY_BUCKETS.length - 1;
  return (
    <div className="lb-reply">
      <div className="lb-reply-bars" role="img" aria-label={REPLY_BUCKETS.map((b, i) => `${b.label}: ${props.buckets[i] ?? 0}`).join(", ")}>
        {REPLY_BUCKETS.map((b, i) => {
          const n = props.buckets[i] ?? 0;
          return (
            <span key={b.label} className="lb-reply-slot" {...tipProps(t, `${n} answered`, b.label)}>
              <span className={`lb-reply-bar${i === last ? " lb-late" : ""}${n === 0 ? " lb-zero" : ""}`} style={{ height: n === 0 ? 1 : `${Math.max(4, (n / max) * 100)}%` }} />
            </span>
          );
        })}
      </div>
      <div className="lb-reply-axis" aria-hidden="true">
        <span>&lt;5m</span>
        <span>30m</span>
        <span>2h</span>
        <span>8h</span>
        <span>1d</span>
        <span>1d+</span>
      </div>
      {t.node}
    </div>
  );
}
