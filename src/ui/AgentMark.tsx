import type { Liveness, Vendor } from "./agents";
import { LIVENESS_LABEL, VENDOR_LABEL, vendorOf } from "./agents";

// Each vendor has a shape as well as a color, so marks stay apart in grayscale and at a glance:
// Claude a disc, Codex a rounded square, Cursor a hexagon with a caret, a human a ring.
const LETTER: Partial<Record<Vendor, string>> = { claude: "C", codex: "X", other: "?" };

/** An agent: vendor shape and color, with its live state as a small notched dot. */
export function AgentMark(props: { vendor: string | null | undefined; state?: Liveness | null; size?: number; title?: string }) {
  const vendor = vendorOf(props.vendor);
  const size = props.size ?? 16;
  const state = props.state ?? null;
  const label = props.title ?? `${VENDOR_LABEL[vendor]}${state ? ` · ${LIVENESS_LABEL[state]}` : ""}`;
  const fill = `var(--vendor-${vendor})`;
  const letter = LETTER[vendor];
  return (
    <svg
      className={`ui-mark${state === "stale" || state === "ended" ? ` ui-mark-${state}` : ""}`}
      width={size}
      height={size}
      viewBox="0 0 16 16"
      role="img"
      aria-label={label}
    >
      <title>{label}</title>
      {vendor === "codex" && <rect x="1.25" y="1.25" width="13.5" height="13.5" rx="3.5" fill={fill} />}
      {vendor === "cursor" && (
        <>
          <path d="M8 0.6 14.4 4.3v7.4L8 15.4 1.6 11.7V4.3Z" fill={fill} strokeLinejoin="round" stroke={fill} strokeWidth="1" />
          <rect x="7.1" y="4.4" width="1.8" height="7.2" rx="0.9" fill="var(--on-vendor)" />
        </>
      )}
      {vendor === "human" && <circle cx="8" cy="8" r="6.1" fill="none" stroke={fill} strokeWidth="1.8" />}
      {(vendor === "claude" || vendor === "other") && <circle cx="8" cy="8" r="7" fill={fill} />}
      {letter && (
        <text x="8" y="8.35" textAnchor="middle" dominantBaseline="central" className="ui-mark-letter">
          {letter}
        </text>
      )}
      {(state === "working" || state === "waiting" || state === "idle") && (
        <circle
          className={`ui-mark-dot ui-mark-dot-${state}`}
          cx="13.4"
          cy="13.4"
          r="3"
          strokeWidth="1.6"
        />
      )}
    </svg>
  );
}
