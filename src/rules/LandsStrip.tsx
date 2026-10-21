import type { CSSProperties } from "react";
import { MODE_LABEL, type ModeOptions, type ReviewMode } from "../reviewModes";
import type { RulesEdit, RulesView } from "../types";
import { vendorOf } from "../ui";
import type { RulesDraft } from "./shared";
import { draftOf, reviewerName } from "./shared";
import "./lands.css";

/** Where a station's section is on the page, for the station to scroll to. */
export type RulesSection = "refused" | "waits" | "review" | "landing";

/** teal: an agent or Nitely handles it; amber: it waits for you; red: off or skipped; none: nothing set. */
type Tone = "agent" | "you" | "off" | "none" | "ink";

interface Station {
  label: string;
  value: string;
  why: string;
  tone: Tone;
  section: RulesSection | null;
}

/** The project's review mode, as far as the strip needs it: its name and the options it resolves to. */
export interface ModeFacts {
  mode: ReviewMode;
  options: ModeOptions;
}

const LAND_SHORT = { author: "The agent that wrote it", auto: "Nitely, at once", human: "Nitely, after you approve" } as const;

const listed = (items: string[]) => (items.length <= 2 ? items.join(" and ") : `${items.slice(0, -1).join(", ")} and ${items[items.length - 1]}`);
const capital = (s: string) => s.charAt(0).toUpperCase() + s.slice(1);
const count = (n: number, one: string) => `${n} ${one}${n === 1 ? "" : "s"}`;

function reviewedBy(map: Record<string, string>, crossVendor: boolean): Pick<Station, "value" | "why"> {
  const rows = Object.entries(map);
  const authors = rows.filter(([a]) => a !== "*");
  const names = [...new Set(rows.map(([, r]) => reviewerName(r)))];
  const mutual =
    names.length === 2 &&
    authors.length >= 2 &&
    authors.every(([a, r]) => vendorOf(a) !== vendorOf(r)) &&
    authors.some(([a, r]) => map[r] && vendorOf(map[r]) === vendorOf(a));
  const own = authors.some(([a, r]) => vendorOf(a) === vendorOf(r) && vendorOf(a) !== "other");
  const value = mutual ? names.join(" ↔ ") : names.length > 3 ? `${names.slice(0, 2).join(", ")} +${names.length - 2}` : names.join(", ");
  const why = mutual ? "Each vendor reviews the other" : own ? (crossVendor ? "A vendor reviewing its own work waits for you" : "Some vendors review their own work") : "Another vendor reviews each change";
  return { value: value || "No one", why };
}

/**
 * The stations a change passes under the draft, from the same data as `waitsSummary()`: what
 * refuses it, the gate, who reviews it, what sends it to you, who lands it, and, with a review
 * mode, the full suite that checks main afterwards. That last one is red only when Custom turns
 * it off, since every preset keeps it on (NIT-496).
 */
export function landsStations(v: RulesView, d: RulesEdit, landBy: RulesDraft["landBy"], crossVendor: boolean, mode: ModeFacts | null): Station[] {
  const blocked = d.blocked_paths.filter((r) => r.pattern.trim());
  const refused: Station = blocked.length
    ? { label: "Refused if", value: `It touches ${count(blocked.length, "path")}`, why: capital(listed([...new Set(blocked.map((r) => r.reason.trim() || r.pattern.trim()))])), tone: "agent", section: "refused" }
    : { label: "Refused if", value: "Nothing", why: "No path is off limits", tone: "none", section: "refused" };

  const tests = mode?.options.test_depth ?? null;
  const gate: Station =
    tests === "off"
      ? { label: "Gate", value: "Skipped", why: `${MODE_LABEL[mode!.mode]} mode runs no tests`, tone: "off", section: null }
      : tests === "basic"
        ? { label: "Gate", value: "Basic checks", why: "The playbook's basic commands", tone: "agent", section: null }
        : tests === "risk"
          ? { label: "Gate", value: "Matched to risk", why: "Light changes run fewer checks", tone: "agent", section: null }
          : { label: "Gate", value: "Build lane checks", why: "Every command in the playbook's gate", tone: "agent", section: null };

  const enabled = d.review_enabled ?? v.review_enabled.inherited;
  const modeSkipsReview = mode?.options.pre_review === "off";
  const review: Station = !enabled
    ? { label: "Reviewed by", value: "No one", why: `Review turned off${d.review_enabled !== draftOf(v).review_enabled ? " (unsaved)" : ""}`, tone: "off", section: "review" }
    : modeSkipsReview
      ? { label: "Reviewed by", value: "No one", why: `${MODE_LABEL[mode!.mode]} mode runs no review`, tone: "off", section: "review" }
      : { label: "Reviewed by", ...reviewedBy(d.reviewer ?? v.reviewer.inherited, crossVendor), tone: "agent", section: "review" };

  const paths = d.human_paths.filter((r) => r.pattern.trim());
  let waits: Station;
  if (d.review_enabled === false && (d.flags.length || crossVendor)) waits = { label: "Waits for you if", value: "Every change", why: "Reviewer questions go unanswered while review is off", tone: "you", section: "waits" };
  else if (d.always_human) waits = { label: "Waits for you if", value: "Every change", why: "Every change waits for you is on", tone: "you", section: "waits" };
  else if (landBy === "human") waits = { label: "Waits for you if", value: "Every change", why: "You land every change", tone: "you", section: "waits" };
  else {
    const value = [paths.length && count(paths.length, "path"), d.flags.length && count(d.flags.length, "question"), crossVendor && "own vendor"].filter(Boolean).join(" · ");
    const reasons = [...new Set(paths.map((r) => r.reason.trim() || r.pattern.trim()))];
    const why = [...reasons, ...(d.flags.length ? ["a yes from the reviewer"] : []), ...(crossVendor ? ["a same-vendor review"] : [])];
    waits = value
      ? { label: "Waits for you if", value, why: capital(why.join(", or ")), tone: "you", section: "waits" }
      : { label: "Waits for you if", value: "Never", why: "Nothing is set to need you", tone: "none", section: "waits" };
  }

  const b = v.batching;
  const batched = b && b.land !== "off" && !b.land_alone && !(mode?.options.land_batch === false);
  const landed: Station = { label: "Landed by", value: LAND_SHORT[landBy], why: batched ? `In batches of up to ${b.land_max}` : "Each change alone", tone: "ink", section: "landing" };
  const backstop = mode?.options.full_backstop;
  const minutes = backstop ? Math.round(backstop.delay_seconds / 60) : 0;
  const checked: Station | null = !mode || !backstop
    ? null
    : !backstop.enabled
      ? { label: "Checked on main", value: "Off", why: `${MODE_LABEL[mode.mode]} turns off the full suite on main`, tone: "off", section: null }
      : { label: "Checked on main", value: "Full suite", why: backstop.landings <= 1 ? "After every land, reverting a culprit" : `After ${backstop.landings} lands or ${minutes} min, reverting a culprit`, tone: "agent", section: null };
  return checked ? [refused, gate, review, waits, landed, checked] : [refused, gate, review, waits, landed];
}

/** How a change lands under these rules, left to right; each station scrolls to its section. */
export function LandsStrip(props: { view: RulesView; draft: RulesEdit; landBy: RulesDraft["landBy"]; crossVendor: boolean; mode: ModeFacts | null; onGo: (section: RulesSection) => void }) {
  const stations = landsStations(props.view, props.draft, props.landBy, props.crossVendor, props.mode);
  return (
    <section className="lands" aria-label="How a change lands">
      <div className="lands-head">
        <h2>How a change lands in {props.view.repo}</h2>
        <span className="muted">from these rules, left to right</span>
      </div>
      <ol className="lands-track" style={{ "--stations": stations.length } as CSSProperties}>
        {stations.map((s) => {
          const body = (
            <>
              <span className={`lands-dot lands-dot-${s.tone}`} aria-hidden="true" />
              <span className="lands-label">{s.label}</span>
              <span className={`lands-value lands-value-${s.tone}`}>{s.value}</span>
              <span className="lands-why">{s.why}</span>
            </>
          );
          return (
            <li key={s.label} className="lands-station">
              {s.section ? (
                <button type="button" className="lands-go" onClick={() => props.onGo(s.section!)} title={`Go to ${s.label.toLowerCase()}`}>
                  {body}
                </button>
              ) : (
                <div className="lands-go">{body}</div>
              )}
            </li>
          );
        })}
      </ol>
    </section>
  );
}
