import type { Liveness } from "./agents";
import { LIVENESS_LABEL, VENDOR_LABEL, vendorInfo, vendorOf } from "./agents";

// Every agent is a rounded square in its vendor's colour with its mark from VENDORS (agents.ts):
// Cl Claude, Cx Codex, Cu Cursor. A person is a ring, so the human never reads as one more agent.

/** An agent: vendor colour and mark, with its live state as a small notched dot. */
export function AgentMark(props: { vendor: string | null | undefined; state?: Liveness | null; size?: number; title?: string }) {
  const vendor = vendorOf(props.vendor);
  const size = props.size ?? 18;
  const state = props.state ?? null;
  const label = props.title ?? `${VENDOR_LABEL[vendor]}${state ? ` · ${LIVENESS_LABEL[state]}` : ""}`;
  const info = vendorInfo(vendor);
  const fill = info?.color ?? "var(--vendor-human)";
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
      {!info ? (
        <circle cx="8" cy="8" r="6.1" fill="none" stroke={fill} strokeWidth="1.8" />
      ) : (
        <>
          <rect x="0.5" y="0.5" width="15" height="15" rx="3.6" fill={fill} />
          {"path" in info.mark ? (
            <path d={info.mark.path} className="ui-mark-glyph" />
          ) : (
            <text x="8" y="8.4" textAnchor="middle" dominantBaseline="central" className={`ui-mark-letter${info.mark.letters.length > 1 ? " ui-mark-letters" : ""}`}>
              {info.mark.letters}
            </text>
          )}
        </>
      )}
      {(state === "working" || state === "waiting" || state === "idle") && (
        <circle className={`ui-mark-dot ui-mark-dot-${state}`} cx="14.6" cy="14.6" r="3" strokeWidth="1.6" />
      )}
    </svg>
  );
}
