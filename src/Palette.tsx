import { useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "./backend";
import type { View } from "./App";
import { SETTINGS_SECTIONS } from "./Settings";
import type { UpdateStatus } from "./Update";
import { HOME_LABEL } from "./util";
import type { CrewStatus, IssueRecord, Project } from "./types";
import { IssueToken, ResultMark } from "./ui";
import "./styles/palette.css";

type Group = "Go to" | "Issues" | "Actions";

interface Item {
  id: string;
  label: string;
  hint?: string;
  issue?: IssueRecord;
  group: Group;
  /** Goes somewhere. */
  run?: () => void;
  /** Changes something; resolves to what happened, said in the palette before it closes. */
  act?: () => Promise<string>;
}

/** How long the palette says what an action did before it closes. */
const SAID_MS = 900;

const VIEWS: { label: string; view: View }[] = [
  { label: HOME_LABEL, view: { kind: "pulse" } },
  { label: "Inbox", view: { kind: "inbox" } },
  { label: "Now", view: { kind: "now" } },
  { label: "Activity", view: { kind: "activity" } },
  { label: "Leaderboard", view: { kind: "leaderboard" } },
  { label: "Performance", view: { kind: "performance" } },
  { label: "Settings", view: { kind: "settings" } },
  // Each section of Settings by its own name too ("phone", "crew"), as the sidebar once had some.
  ...SETTINGS_SECTIONS.map((s) => ({ label: `${s.label} settings`, view: { kind: "settings", section: s.id } as View })),
  { label: "Agent setup", view: { kind: "agents" } },
  { label: "Add project", view: { kind: "add" } },
];

/**
 * Starting a goal: "Start a goal" with nothing more typed, then, for "goal <sentence>", one
 * action per project that files the issue an agent picks up to draft the plan.
 */
function goalItems(query: string, projects: Project[], begin: () => void): Item[] {
  const sentence = query.trim().match(/^goal:?\s+(.+)$/i)?.[1]?.trim();
  if (!sentence) return [{ id: "goal-start", label: "Start a goal", hint: "type goal and a sentence", group: "Actions", run: begin }];
  return projects.map((p) => ({
    id: `goal-${p.key}`,
    label: `Start the goal “${sentence}” in ${p.name}`,
    hint: p.key,
    group: "Actions" as Group,
    act: async () => {
      const issue = await invoke<IssueRecord>("start_goal", { project: p.key, sentence });
      return `Filed ${issue.key}: an agent drafts the plan for you to approve.`;
    },
  }));
}

/** Pausing or resuming each project's crew (docs/crew.md), for the projects that have one. */
function crewItems(crews: { project: Project; crew: CrewStatus }[]): Item[] {
  return crews.map(({ project: p, crew }) => ({
    id: `crew-${p.key}`,
    label: `${crew.paused ? "Resume" : "Pause"} the crew in ${p.name}`,
    hint: p.key,
    group: "Actions" as Group,
    act: async () => {
      await invoke<CrewStatus | null>("crew_pause", { project: p.key, paused: !crew.paused });
      return crew.paused ? `Resumed the crew in ${p.name}.` : `Paused the crew in ${p.name}: running workers finish, no new ones start.`;
    },
  }));
}

/** Checking for a newer Nitely now, as Settings' Updates "Check now" does. */
const UPDATE_ITEM: Item = {
  id: "update-check",
  label: "Check for updates",
  group: "Actions",
  act: async () => {
    const s = await invoke<UpdateStatus>("check_for_update");
    if (s.disabled) return `Not checking: ${s.disabled}.`;
    if (s.error) throw s.error;
    if (s.available) return s.available.ready ? `${s.available.version} is ready; restart to update.` : `Downloading ${s.available.version}.`;
    return s.checking ? "Checking now; Settings, Updates says what it finds." : `Nitely ${s.version} is up to date.`;
  },
};

/** An issue key: a project key (letters, then letters or digits), a dash, a number. */
const KEY = /^[A-Za-z][A-Za-z0-9]*-\d+$/;

/**
 * Ctrl+K (Cmd+K on a Mac): go to any view, project tab or open issue by typing, or act on an issue
 * ("Approve NIT-12", "Move NIT-12 to Todo"). Arrow keys pick, Enter goes, Esc closes. An exact key
 * like "nit-12" opens that issue even if no list shows it. Actions show only for what you typed.
 */
export function Palette(props: {
  projects: Project[];
  onGo: (view: View) => void;
  onOpenIssue: (key: string) => void;
  onChanged: () => void;
  onClose: () => void;
}) {
  const [query, setQuery] = useState("");
  const [picked, setPicked] = useState(0);
  const [busy, setBusy] = useState(false);
  const [said, setSaid] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [issues, setIssues] = useState<IssueRecord[]>([]);
  const [crews, setCrews] = useState<{ project: Project; crew: CrewStatus }[]>([]);
  const list = useRef<HTMLUListElement>(null);
  const input = useRef<HTMLInputElement>(null);

  useEffect(() => {
    let live = true;
    Promise.all(props.projects.map((p) => invoke<IssueRecord[]>("issues", { project: p.key, includeClosed: false }).catch(() => [])))
      .then((all) => live && setIssues(all.flat()));
    Promise.all(props.projects.map((p) => invoke<CrewStatus | null>("crew_status", { project: p.key }).then((crew) => (crew ? [{ project: p, crew }] : []), () => [])))
      .then((all) => live && setCrews(all.flat()));
    return () => {
      live = false;
    };
  }, [props.projects]);

  const items = useMemo<Item[]>(() => {
    const q = query.trim().toLowerCase();
    const go = (view: View) => () => {
      props.onGo(view);
      props.onClose();
    };
    const open = (key: string) => () => {
      props.onOpenIssue(key);
      props.onClose();
    };
    const all: Item[] = [
      ...VIEWS.map((v): Item => ({ id: `view-${v.label}`, label: v.label, hint: "view", group: "Go to", run: go({ ...v.view }) })),
      // The Inbox filtered to one project, or to none (NIT-236).
      { id: "inbox-all", label: "Inbox, every project", hint: "view", group: "Go to", run: go({ kind: "inbox", project: null }) },
      ...props.projects.map((p): Item => ({ id: `inbox-${p.key}`, label: `${p.name} inbox`, hint: p.key, group: "Go to", run: go({ kind: "inbox", project: p.key }) })),
      ...props.projects.map((p): Item => ({
        id: `focus-${p.key}`,
        label: `Change current focus in ${p.name}`,
        hint: p.key,
        group: "Actions",
        run: go({ kind: "project", key: p.key, tab: "list" }),
      })),
      ...props.projects.flatMap((p) =>
        (["list", "board", "branches", "brief", "rules", "settings"] as const).map(
          (tab): Item => ({
            id: `project-${p.key}-${tab}`,
            label: `${p.name} ${tab}`,
            hint: p.key,
            group: "Go to",
            run: go({ kind: "project", key: p.key, tab }),
          }),
        ),
      ),
      ...issues.map((i): Item => ({ id: `issue-${i.key}`, label: `${i.key} ${i.title}`, issue: i, group: "Issues", run: open(i.key) })),
      ...goalItems(query, props.projects, () => setQuery("goal ")),
      ...crewItems(crews),
      UPDATE_ITEM,
      ...issues.flatMap(actionsOf),
    ];
    // With nothing typed, only where to go; issues and actions need a word of what you mean. A
    // key in what you typed ("approve nit-1") gives the actions of that issue alone, never NIT-10's.
    const named = q.split(/\s+/).find((w) => KEY.test(w));
    const found = q
      ? all.filter(
          (it) =>
            // Goal actions answer "goal <sentence>", which no label holds.
            (it.id.startsWith("goal-") && q.startsWith("goal")) ||
            ((it.label.toLowerCase().includes(q) || it.hint?.toLowerCase() === q) && (!named || it.group !== "Actions" || it.issue?.key.toLowerCase() === named)),
        )
      : all.filter((it) => it.group === "Go to");
    // An exact key always comes first, so "nit-1" never opens NIT-100; one that no list shows
    // (a closed issue) still opens. Its actions follow it.
    const exact = KEY.test(query.trim());
    if (exact) {
      const at = found.findIndex((it) => it.issue?.key.toLowerCase() === q && it.group === "Issues");
      if (at >= 0) found.unshift(...found.splice(at, 1));
      else {
        const key = query.trim().toUpperCase();
        found.unshift({ id: `key-${key}`, label: `Open ${key}`, hint: "issue", group: "Issues", run: open(key) });
      }
    }
    const order: Group[] = exact ? ["Issues", "Actions", "Go to"] : ["Go to", "Issues", "Actions"];
    return order.flatMap((g) => found.filter((it) => it.group === g)).slice(0, 12);
  }, [query, issues, crews, props]);

  useEffect(() => setPicked(0), [query]);

  const run = async (it: Item | undefined) => {
    if (!it || busy) return;
    if (it.run) return it.run();
    if (!it.act) return;
    setBusy(true);
    setError(null);
    try {
      setSaid(await it.act());
      props.onChanged();
      setTimeout(props.onClose, SAID_MS);
    } catch (e) {
      setError(String(e));
      setBusy(false);
    }
  };
  useEffect(() => {
    list.current?.children[picked]?.scrollIntoView({ block: "nearest" });
  }, [picked]);

  const onKey = (e: React.KeyboardEvent) => {
    if (e.key === "ArrowDown") {
      e.preventDefault();
      setPicked((p) => Math.min(items.length - 1, p + 1));
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      setPicked((p) => Math.max(0, p - 1));
    } else if (e.key === "Enter") {
      e.preventDefault();
      run(items[picked]);
    } else if (e.key === "Escape") {
      e.preventDefault();
      e.stopPropagation();
      props.onClose();
    } else if (e.key === "Tab") {
      // The palette is modal: focus stays in its input, so Esc and Enter always reach it.
      e.preventDefault();
      input.current?.focus();
    }
  };

  return (
    <div className="palette-veil" onMouseDown={props.onClose}>
      <div
        className="palette"
        role="dialog"
        aria-modal="true"
        aria-label="Go to"
        onKeyDown={onKey}
        onMouseDown={(e) => {
          e.stopPropagation();
          // A click anywhere but the input keeps the focus in the input, so the keys keep working.
          if (e.target !== input.current) e.preventDefault();
        }}
      >
        <div className="palette-search">
          <input
            ref={input}
            autoFocus
            className="palette-input"
            placeholder="Go to a view, project or issue"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            aria-controls="palette-list"
            aria-activedescendant={items[picked] ? `palette-${items[picked].id}` : undefined}
          />
          {query.trim() && !said && (
            <span className="palette-count">
              {items.length} match{items.length === 1 ? "" : "es"}
            </span>
          )}
        </div>
        {said ? (
          <p className="palette-said" role="status">
            <ResultMark result="pass" size={16} />
            {said}
          </p>
        ) : (
          <ul className="palette-list" id="palette-list" role="listbox" ref={list}>
            {items.map((it, n) => {
              const first = n === 0 || it.group !== items[n - 1].group;
              return (
                <li
                  key={it.id}
                  id={`palette-${it.id}`}
                  role="option"
                  aria-selected={n === picked}
                  aria-disabled={busy || undefined}
                  className={`${n === picked ? "on" : ""}${first ? " palette-first" : ""}`}
                  data-group={first ? it.group : undefined}
                  onMouseEnter={() => setPicked(n)}
                  onClick={() => run(it)}
                >
                  {it.issue && it.group === "Issues" ? <IssueToken issue={it.issue} title /> : <span className="palette-label">{it.label}</span>}
                  {it.hint && <span className="palette-hint">{it.hint}</span>}
                </li>
              );
            })}
            {items.length === 0 && <li className="palette-none">Nothing matches.</li>}
          </ul>
        )}
        {error && <p className="palette-error error small">{error}</p>}
        <div className="palette-foot">
          <span>
            <kbd>↑↓</kbd> pick
          </span>
          <span>
            <kbd>↵</kbd> go
          </span>
          <span>
            <kbd>esc</kbd> close
          </span>
          <span>
            <kbd>j k</kbd> move between items on a page
          </span>
          <span>
            <kbd>?</kbd> all keys
          </span>
        </div>
      </div>
    </div>
  );
}

/**
 * What you can do to an issue from the palette, with the same commands the Inbox and the issue
 * sheet use: approve one that waits on you, or send one back to Todo or the Backlog. Done and
 * Canceled need words of why, so they stay on the issue sheet.
 */
function actionsOf(i: IssueRecord): Item[] {
  if (i.origin !== "nitely") return [];
  const move = (state: string, said: string) => async () => {
    await invoke("move_issue", { key: i.key, state, evidence: null, reason: null });
    return said;
  };
  const out: Item[] = [];
  if (i.state === "Needs You") {
    out.push({ id: `approve-${i.key}`, label: `Approve ${i.key} ${i.title}`, hint: "approve", group: "Actions", issue: i, act: move("Approved", `Approved ${i.key}. Landing next.`) });
  }
  for (const state of ["Todo", "Backlog"]) {
    if (i.state !== state) {
      out.push({ id: `move-${i.key}-${state}`, label: `Move ${i.key} to ${state}`, hint: "move", group: "Actions", issue: i, act: move(state, `Moved ${i.key} to ${state}.`) });
    }
  }
  return out;
}

/** Every key the app answers to, in one sheet: "?" opens it, Esc closes it. */
export function KeysSheet(props: { onClose: () => void }) {
  const mod = /Mac|iPhone|iPad/.test(navigator.platform) ? "⌘" : "Ctrl";
  const groups: [string, [string, string][]][] = [
    [
      "Anywhere",
      [
        [`${mod} K`, "Go to a view, project or issue, or act on one"],
        ["j k", "Next or previous item on the page"],
        ["↵", "Open it"],
        ["?", "This sheet"],
        ["esc", "Close the sheet, the palette or the issue"],
      ],
    ],
    [
      "In the Inbox, on the decision open",
      [
        ["a", "Approve"],
        ["t", "Send back to Todo"],
        ["1–9", "Answer with that option"],
        ["r", "Answer in your own words"],
        ["p", "Next project's decisions (shift p: the one before)"],
      ],
    ],
    [
      "In an open issue",
      [
        ["a", "Approve, when it waits on you"],
        ["t", "Back to Todo"],
        ["m then t d c", "Move it: Todo, Done…, Cancel…"],
      ],
    ],
  ];
  // While the sheet is open it has the keys: Esc or "?" closes it, and every other key stops here,
  // so nothing behind it (an open decision, the issue sheet) is approved or answered. Captured on
  // the window, before anything else sees the key.
  const sheet = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const was = document.activeElement as HTMLElement | null;
    sheet.current?.focus({ preventScroll: true });
    const onKey = (e: KeyboardEvent) => {
      e.stopImmediatePropagation();
      if (e.key === "Tab") e.preventDefault();
      if (e.key === "Escape" || e.key === "?") {
        e.preventDefault();
        props.onClose();
      }
    };
    window.addEventListener("keydown", onKey, true);
    return () => {
      window.removeEventListener("keydown", onKey, true);
      if (was?.isConnected) was.focus({ preventScroll: true });
    };
  }, [props.onClose]);
  return (
    <div className="palette-veil" onMouseDown={props.onClose}>
      <div className="palette keys-sheet" role="dialog" aria-modal="true" aria-label="Keys" tabIndex={-1} ref={sheet} onMouseDown={(e) => e.stopPropagation()}>
        <div className="keys-head">
          <h2>Keys</h2>
          <p>Keys that change something act only on the decision or issue you have open, with the focus in it.</p>
        </div>
        {groups.map(([title, keys]) => (
          <section key={title} className="keys-group">
            <h3>{title}</h3>
            <dl>
              {keys.map(([k, what]) => (
                <div key={k} className="keys-row">
                  <dt>
                    {k.split(" ").map((part, i) => (part === "then" ? <span key={i}>then</span> : <kbd key={i}>{part}</kbd>))}
                  </dt>
                  <dd>{what}</dd>
                </div>
              ))}
            </dl>
          </section>
        ))}
      </div>
    </div>
  );
}

/** Items j and k move between: whatever opens an issue or a row, in reading order. */
const ITEMS = [
  ".content .list-row",
  ".content .now-row:not(:disabled)",
  ".content .fchip",
  ".content .board-card .ui-token",
  ".content .inbox-row.on",
  ".content .act-row .ui-token",
  ".content .bmap-line .ui-token",
  ".content button.now-card-issue",
  ".content .feed-row .ui-token",
].join(", ");

/** True when a key goes into a field, so it is a letter and not a command. */
export function typing(target: EventTarget | null): boolean {
  const el = target as HTMLElement | null;
  return !!el && (el.isContentEditable || ["INPUT", "TEXTAREA", "SELECT"].includes(el.tagName));
}

/** Moves focus to the next (1) or previous (-1) item on the page, from the focused one. */
export function step(direction: 1 | -1) {
  const all = [...document.querySelectorAll<HTMLElement>(ITEMS)].filter((el) => el.offsetParent !== null);
  if (all.length === 0) return;
  const at = all.indexOf(document.activeElement as HTMLElement);
  const next = at < 0 ? (direction > 0 ? 0 : all.length - 1) : Math.max(0, Math.min(all.length - 1, at + direction));
  all[next].focus();
  all[next].scrollIntoView({ block: "nearest" });
}

/**
 * The app's keys: Ctrl/Cmd+K opens the palette; "?" the sheet of keys; j and k step through items
 * unless you are typing, the palette is open, or an issue sheet covers the page (they would move
 * the focus to items behind it).
 */
export function useKeys(openPalette: () => void, paletteOpen: boolean, openKeys: () => void, covered: boolean) {
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "k") {
        e.preventDefault();
        openPalette();
        return;
      }
      if (paletteOpen || e.ctrlKey || e.metaKey || e.altKey || typing(e.target)) return;
      if ((e.key === "j" || e.key === "k") && !covered) step(e.key === "j" ? 1 : -1);
      else if (e.key === "?") {
        e.preventDefault();
        openKeys();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [openPalette, paletteOpen, openKeys, covered]);
}
