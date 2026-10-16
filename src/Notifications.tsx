// Notifications for decisions that wait on the person (the rules are in src/notify.ts): batched,
// never twice, not in quiet hours or while the Inbox is in view, and the count on the taskbar.
// What was already told and the settings live in this machine's app storage.

import { useEffect, useRef, useState } from "react";
import { invoke } from "./backend";
import type { Decision } from "./decisions";
import { DEFAULT_SETTINGS, NOTIFY_KINDS, freshDecisions, messageOf, type Fresh, type NotifySettings, type Seen } from "./notify";
import type { IssueDetail } from "./types";

/** Decisions that arrive within this long of each other become one notification. */
const BATCH_MS = 4000;
const SETTINGS_KEY = "nitely.notify.settings";
const SEEN_KEY = "nitely.notify.seen";
const SETTINGS_EVENT = "nitely:notify-settings";

function read<T>(key: string): T | null {
  try {
    const raw = localStorage.getItem(key);
    return raw ? (JSON.parse(raw) as T) : null;
  } catch {
    return null;
  }
}

function write(key: string, value: unknown) {
  try {
    localStorage.setItem(key, JSON.stringify(value));
  } catch {
    // Storage can be off (a private window); notifications then start fresh each run.
  }
}

export function loadSettings(): NotifySettings {
  const s = read<Partial<NotifySettings>>(SETTINGS_KEY);
  return { kinds: { ...DEFAULT_SETTINGS.kinds, ...s?.kinds }, quiet: { ...DEFAULT_SETTINGS.quiet, ...s?.quiet } };
}

/** The settings, kept in step across every place that shows or uses them. */
export function useNotifySettings(): [NotifySettings, (s: NotifySettings) => void] {
  const [settings, setSettings] = useState(loadSettings);
  useEffect(() => {
    const on = () => setSettings(loadSettings());
    window.addEventListener(SETTINGS_EVENT, on);
    return () => window.removeEventListener(SETTINGS_EVENT, on);
  }, []);
  const save = (s: NotifySettings) => {
    write(SETTINGS_KEY, s);
    setSettings(s);
    window.dispatchEvent(new Event(SETTINGS_EVENT));
  };
  return [settings, save];
}

/**
 * Notifies about new decisions and keeps the taskbar count. `decisions` is null until the board
 * has loaded; `inboxInView` is true while the window has focus on the Inbox, which lists them all.
 */
export function useNotifications(decisions: Decision[] | null, details: Map<string, IssueDetail>, inboxInView: boolean) {
  const [settings] = useNotifySettings();
  const pending = useRef<Fresh[]>([]);
  const timer = useRef<number | null>(null);
  const latest = useRef({ decisions, inboxInView });
  latest.current = { decisions, inboxInView };

  const ids = decisions?.map((d) => d.id).join(" ") ?? null;
  const detailIds = [...details.keys()].join(" ");
  useEffect(() => {
    if (!decisions) return;
    const now = Math.floor(Date.now() / 1000);
    const { fresh, seen } = freshDecisions(decisions, details, read<Seen>(SEEN_KEY), settings, now, { inboxInView });
    write(SEEN_KEY, seen);
    if (fresh.length === 0) return;
    pending.current.push(...fresh);
    if (timer.current !== null) return;
    timer.current = window.setTimeout(() => {
      timer.current = null;
      // Only what still waits, and nothing if the person has the Inbox in front of them by now.
      const still = new Set(latest.current.decisions?.map((d) => d.id));
      const batch = pending.current.filter((f) => still.has(f.decision.id));
      pending.current = [];
      if (batch.length === 0 || latest.current.inboxInView) return;
      invoke("notify", messageOf(batch)).catch(() => {});
    }, BATCH_MS);
    // Runs when the list, its details or the settings change, not on every render.
  }, [ids, detailIds, settings, inboxInView]);
  useEffect(() => () => void (timer.current !== null && clearTimeout(timer.current)), []);

  const count = decisions?.length ?? 0;
  useEffect(() => {
    if (decisions === null) return;
    const size = 32;
    invoke("set_badge", { count, rgba: count > 0 ? badgePixels(count, size) : null, size }).catch(() => {});
    // Only the count matters.
  }, [count, decisions === null]);
}

/** The taskbar overlay: the count in a filled attention circle, as RGBA for the backend. */
function badgePixels(count: number, size: number): number[] | null {
  const canvas = document.createElement("canvas");
  canvas.width = canvas.height = size;
  const g = canvas.getContext("2d");
  if (!g) return null;
  const css = getComputedStyle(document.documentElement);
  g.fillStyle = css.getPropertyValue("--attention").trim() || "#d9902b";
  g.beginPath();
  g.arc(size / 2, size / 2, size / 2, 0, Math.PI * 2);
  g.fill();
  const text = count > 99 ? "99+" : String(count);
  g.fillStyle = css.getPropertyValue("--on-attention").trim() || "#1b1b1a";
  g.font = `700 ${text.length > 2 ? 12 : text.length > 1 ? 16 : 20}px sans-serif`;
  g.textAlign = "center";
  g.textBaseline = "middle";
  g.fillText(text, size / 2, size / 2 + 1);
  return Array.from(g.getImageData(0, 0, size, size).data);
}

/** Which decisions notify, and quiet hours. */
export function NotifySettingsPanel(props: { onClose: () => void }) {
  const [settings, save] = useNotifySettings();
  return (
    <section className="notify-settings" aria-label="Notifications">
      <div className="notify-settings-head">
        <h2>Notifications</h2>
        <button className="link" onClick={props.onClose}>
          Done
        </button>
      </div>
      <p className="muted small">When a decision starts waiting on you, Nitely tells you once, outside the window. Several at once come as one.</p>
      {NOTIFY_KINDS.map(({ kind, label }) => (
        <label key={kind} className="notify-kind">
          <input type="checkbox" checked={settings.kinds[kind]} onChange={(e) => save({ ...settings, kinds: { ...settings.kinds, [kind]: e.target.checked } })} />
          {label}
        </label>
      ))}
      <div className="notify-quiet">
        <label className="notify-kind">
          <input type="checkbox" checked={settings.quiet.on} onChange={(e) => save({ ...settings, quiet: { ...settings.quiet, on: e.target.checked } })} />
          Quiet hours, from
        </label>
        <input type="time" value={settings.quiet.from} disabled={!settings.quiet.on} onChange={(e) => save({ ...settings, quiet: { ...settings.quiet, from: e.target.value } })} aria-label="Quiet from" />
        <span className="muted small">to</span>
        <input type="time" value={settings.quiet.to} disabled={!settings.quiet.on} onChange={(e) => save({ ...settings, quiet: { ...settings.quiet, to: e.target.value } })} aria-label="Quiet until" />
      </div>
      <p className="muted small">In quiet hours nothing notifies; the Inbox and its count still update.</p>
    </section>
  );
}
