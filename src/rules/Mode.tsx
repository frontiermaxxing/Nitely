import { useEffect, useRef, useState } from "react";
import { revealItemInDir } from "@tauri-apps/plugin-opener";
import { invoke, isPreview } from "../backend";
import type { ReviewModeControl } from "../ProjectReviewMode";
import { MODE_DESCRIPTION, MODE_LABEL, optionsFor, type ModeOptions } from "../reviewModes";
import type { RulesView } from "../types";
import { Button } from "../ui";

type Tone = "off" | "on" | "repo";

const TESTS: Record<NonNullable<ModeOptions["test_depth"]>, string> = { off: "off", basic: "basic checks", risk: "match the risk", full: "full suite" };
const REVIEW: Record<NonNullable<ModeOptions["pre_review"]>, string> = { off: "off", individual: "each change", batched: "in batches" };

/** Tests and review before merging under a mode: on, off, or left to each repo's playbook. */
export function modeChecks(options: ModeOptions): { label: string; tone: Tone }[] {
  const one = (name: string, value: string | null, words: Record<string, string>) => ({
    label: `${name}: ${value === null ? "repo settings" : words[value]}`,
    tone: (value === null ? "repo" : value === "off" ? "off" : "on") as Tone,
  });
  return [one("Tests", options.test_depth, TESTS), one("Review", options.pre_review, REVIEW)];
}

const SOURCE: Record<RulesView["source"], string> = { repo: "Committed in", stored: "Kept by Nitely in", none: "Nitely's defaults; saving writes" };

/**
 * The project's review mode as a chip in its header, next to the crew. It opens a popover with what
 * the mode means, whether tests and review run, where each repo's playbook lives, and the way to
 * change the mode (the project's Settings) or see the file.
 */
export function ModeChip(props: { projectKey: string; control: ReviewModeControl; onChangeMode: () => void }) {
  const mode = props.control.value;
  const [open, setOpen] = useState(false);
  const [views, setViews] = useState<RulesView[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [note, setNote] = useState<string | null>(null);
  const box = useRef<HTMLSpanElement>(null);
  const chip = useRef<HTMLButtonElement>(null);

  useEffect(() => {
    setOpen(false);
    setViews(null);
  }, [props.projectKey]);

  useEffect(() => {
    if (!open) return;
    let live = true;
    setError(null);
    setNote(null);
    invoke<RulesView[]>("rules", { project: props.projectKey })
      .then((v) => live && setViews(v))
      .catch((e) => live && setError(String(e)));
    const away = (e: PointerEvent) => {
      if (!box.current?.contains(e.target as Node)) setOpen(false);
    };
    const esc = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      setOpen(false);
      chip.current?.focus();
    };
    document.addEventListener("pointerdown", away);
    document.addEventListener("keydown", esc);
    return () => {
      live = false;
      document.removeEventListener("pointerdown", away);
      document.removeEventListener("keydown", esc);
    };
  }, [open, props.projectKey]);

  if (!mode) return null;
  let checks: ReturnType<typeof modeChecks> = [];
  try {
    checks = modeChecks(optionsFor(mode.mode, mode.presets, mode.overrides));
  } catch {
    // A preset the backend did not send: the chip still names the mode.
  }
  const tone = checks.some((c) => c.tone === "off") ? "off" : "plain";
  const label = `${MODE_LABEL[mode.mode]} mode`;

  const show = async (path: string) => {
    if (isPreview) {
      setNote("In the app this shows the file in Explorer.");
      return;
    }
    try {
      await revealItemInDir(path);
    } catch (e) {
      setError(String(e));
    }
  };

  return (
    <span className="mode" ref={box}>
      <button
        ref={chip}
        className={`mode-chip mode-chip-${tone}`}
        aria-expanded={open}
        aria-haspopup="dialog"
        onClick={() => setOpen(!open)}
        title="What this mode does, and where the rules live"
      >
        <span className="mode-dot" aria-hidden="true" />
        {label}
        <svg width="10" height="10" viewBox="0 0 12 12" aria-hidden="true">
          <path d="M3 4.5 6 7.5 9 4.5" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" />
        </svg>
      </button>
      {open && (
        <div className="mode-pop" role="dialog" aria-label={label}>
          <div className={`mode-pop-head mode-chip-${tone}`}>
            <span className="mode-dot" aria-hidden="true" />
            <strong>{label}</strong>
          </div>
          <p className="mode-pop-why">{MODE_DESCRIPTION[mode.mode]} The rules on the Rules tab apply in every mode.</p>
          {checks.length > 0 && (
            <div className="mode-pop-checks">
              {checks.map((c) => (
                <span key={c.label} className={`mode-check mode-check-${c.tone}`}>
                  <span className="mode-dot" aria-hidden="true" />
                  {c.label}
                </span>
              ))}
            </div>
          )}
          {views?.map((v) => (
            <div key={v.repo} className="mode-pop-file">
              <span className="mode-pop-source">
                {SOURCE[v.source]}
                {views.length > 1 && <> ({v.repo})</>}
              </span>
              <span className="mono">{v.path}</span>
            </div>
          ))}
          {error && <p className="error small">{error}</p>}
          {note && <p className="muted small">{note}</p>}
          <div className="mode-pop-actions">
            <Button
              variant="primary"
              onClick={() => {
                setOpen(false);
                props.onChangeMode();
              }}
            >
              Change mode
            </Button>
            {views?.length === 1 && views[0].source !== "none" && <Button onClick={() => show(views[0].path)}>Show file</Button>}
          </div>
        </div>
      )}
    </span>
  );
}

/** Why the rules cannot be edited here, when they cannot. */
export function Mode({ view: v }: { view: RulesView }) {
  return v.read_only ? <p className="rules-readonly">{v.read_only}</p> : null;
}
