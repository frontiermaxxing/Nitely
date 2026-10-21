import { useState, type CSSProperties } from "react";
import { invoke } from "../backend";
import type { RulesEdit, RulesView } from "../types";
import { Button, TextField, vendorOf } from "../ui";
import { dryRunLine, sourceLabel, type PathDryRun, type Pipeline, type PipelineFact } from "./pipeline";
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
  /** The layer the value comes from: a preset, Custom, the playbook or a built-in default. */
  source: string | null;
}

const LAND_SHORT = { author: "The agent that wrote it", auto: "Nitely, at once", human: "Nitely, after you approve" } as const;

const listed = (items: string[]) => (items.length <= 2 ? items.join(" and ") : `${items.slice(0, -1).join(", ")} and ${items[items.length - 1]}`);
const capital = (s: string) => s.charAt(0).toUpperCase() + s.slice(1);
const lower = (s: string) => s.charAt(0).toLowerCase() + s.slice(1);
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

const fromPipeline = (f: PipelineFact, label: string, section: RulesSection | null): Station => ({ label, value: f.value, why: f.why, tone: f.tone, section, source: sourceLabel(f.source) });
const pending = (label: string, section: RulesSection | null): Station => ({ label, value: "…", why: "Not read yet", tone: "none", section, source: null });

/**
 * The six stations a change passes under the draft: what refuses it, the checks before landing,
 * who reviews it, what sends it to you, how it lands, and what checks main afterwards. The draft's
 * own rules (paths, reviewers, who lands) come from `RulesView`, as `waitsSummary()` reads them;
 * the rest from the backend's pipeline, which reads every layer (lands_pipeline.rs).
 */
export function landsStations(v: RulesView, d: RulesEdit, landBy: RulesDraft["landBy"], crossVendor: boolean, p: Pipeline | null): Station[] {
  const blocked = d.blocked_paths.filter((r) => r.pattern.trim());
  const refused: Station = blocked.length
    ? { label: "Refused if", value: `It touches ${count(blocked.length, "path")}`, why: capital(listed([...new Set(blocked.map((r) => r.reason.trim() || r.pattern.trim()))])), tone: "agent", section: "refused", source: "Playbook" }
    : { label: "Refused if", value: "Nothing", why: "No path is off limits", tone: "none", section: "refused", source: "Playbook" };

  const checks = p ? fromPipeline(p.checks, "Checks", null) : pending("Checks", null);

  const enabled = d.review_enabled ?? v.review_enabled.inherited;
  const review: Station = !enabled
    ? { label: "Reviewed by", value: "No one", why: `Review turned off${d.review_enabled !== draftOf(v).review_enabled ? " (unsaved)" : ""}`, tone: "off", section: "review", source: "Playbook" }
    : !p
      ? pending("Reviewed by", "review")
      : p.review.tone === "off"
        ? { ...fromPipeline(p.review, "Reviewed by", "review"), value: "No one" }
        : { label: "Reviewed by", ...reviewedBy(d.reviewer ?? v.reviewer.inherited, crossVendor), why: p.review.why, tone: "agent", section: "review", source: sourceLabel(p.review.source) };

  const paths = d.human_paths.filter((r) => r.pattern.trim());
  let waits: Station;
  if (d.review_enabled === false && (d.flags.length || crossVendor)) waits = { label: "Waits for you if", value: "Every change", why: "Reviewer questions go unanswered while review is off", tone: "you", section: "waits", source: "Playbook" };
  else if (d.always_human) waits = { label: "Waits for you if", value: "Every change", why: "Every change waits for you is on", tone: "you", section: "waits", source: "Playbook" };
  else if (landBy === "human") waits = { label: "Waits for you if", value: "Every change", why: "You land every change", tone: "you", section: "waits", source: "Playbook" };
  else {
    const value = [paths.length && count(paths.length, "path"), d.flags.length && count(d.flags.length, "question"), crossVendor && "own vendor"].filter(Boolean).join(" · ");
    const reasons = [...new Set(paths.map((r) => r.reason.trim() || r.pattern.trim()))];
    const why = [...reasons, ...(d.flags.length ? ["a yes from the reviewer"] : []), ...(crossVendor ? ["a same-vendor review"] : [])];
    waits = value
      ? { label: "Waits for you if", value, why: capital(why.join(", or ")), tone: "you", section: "waits", source: "Playbook" }
      : { label: "Waits for you if", value: "Never", why: "Nothing is set to need you", tone: "none", section: "waits", source: "Playbook" };
  }

  const landed: Station = p
    ? { label: "Landed by", value: LAND_SHORT[landBy], why: `${p.lands.value}: ${lower(p.lands.why)}`, tone: "ink", section: "landing", source: sourceLabel(p.lands.source) }
    : { ...pending("Landed by", "landing"), value: LAND_SHORT[landBy], tone: "ink" };
  const after = p ? fromPipeline(p.after, "After landing", null) : pending("After landing", null);
  return [refused, checks, review, waits, landed, after];
}

/** "A change to src/X would run …": what the gate would pick for one path, read from the committed test map. */
function DryRunLine(props: { project: string; repo: string }) {
  const [path, setPath] = useState("");
  const [result, setResult] = useState<{ text: string; error: boolean } | null>(null);
  return (
    <form
      className="lands-dry"
      onSubmit={(e) => {
        e.preventDefault();
        const asked = path.trim();
        if (!asked) return;
        invoke<PathDryRun>("lands_dry_run", { project: props.project, repo: props.repo, path: asked })
          .then((d) => setResult({ text: dryRunLine(d), error: false }))
          .catch((err) => setResult({ text: String(err), error: true }));
      }}
    >
      <span className="lands-dry-ask">A change to</span>
      <TextField mono value={path} placeholder="src/App.tsx" onChange={(e) => setPath(e.target.value)} aria-label="A path to try" />
      <Button type="submit" disabled={!path.trim()}>
        Try it
      </Button>
      {result && (
        <p className={result.error ? "lands-dry-out error" : "lands-dry-out"} role="status">
          {result.text}
        </p>
      )}
    </form>
  );
}

/** How a change lands under these rules, left to right; each station scrolls to its section. */
export function LandsStrip(props: { project: string; view: RulesView; draft: RulesEdit; landBy: RulesDraft["landBy"]; crossVendor: boolean; pipeline: Pipeline | null; onGo: (section: RulesSection) => void }) {
  const p = props.pipeline;
  const stations = landsStations(props.view, props.draft, props.landBy, props.crossVendor, p);
  const facts: [string, PipelineFact][] = p ? [["Goals", p.goals], ["Full suites run", p.suites]] : [];
  return (
    <section className="lands" aria-label="How a change lands">
      <div className="lands-head">
        <h2>How a change lands in {props.view.repo}</h2>
        <span className="muted">left to right; each value says where it comes from</span>
      </div>
      <ol className="lands-track" style={{ "--stations": stations.length } as CSSProperties}>
        {stations.map((s) => {
          const body = (
            <>
              <span className={`lands-dot lands-dot-${s.tone}`} aria-hidden="true" />
              <span className="lands-label">{s.label}</span>
              <span className={`lands-value lands-value-${s.tone}`}>{s.value}</span>
              <span className="lands-why">{s.why}</span>
              {s.source && <span className="lands-source">{s.source}</span>}
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
      {facts.length > 0 && (
        <dl className="lands-facts">
          {facts.map(([label, f]) => (
            <div key={label} className="lands-fact">
              <dt>{label}</dt>
              <dd>
                <span className={`lands-value lands-value-${f.tone}`}>{f.value}</span>
                <span className="lands-why">{f.why}</span>
                <span className="lands-source">{sourceLabel(f.source)}</span>
              </dd>
            </div>
          ))}
        </dl>
      )}
      {p && p.warnings.length > 0 && (
        <ul className="lands-warnings">
          {p.warnings.map((w) => (
            <li key={w}>{w}</li>
          ))}
        </ul>
      )}
      <DryRunLine project={props.project} repo={props.view.repo} />
    </section>
  );
}
