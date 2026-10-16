import { useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "./backend";
import type { View } from "./App";
import { HOME_LABEL } from "./util";
import type { IssueRecord, Project } from "./types";
import { IssueToken } from "./ui";
import "./styles/palette.css";

interface Item {
  id: string;
  label: string;
  hint?: string;
  issue?: IssueRecord;
  run: () => void;
}

const VIEWS: { label: string; view: View }[] = [
  { label: HOME_LABEL, view: { kind: "pulse" } },
  { label: "Inbox", view: { kind: "inbox" } },
  { label: "Now", view: { kind: "now" } },
  { label: "Activity", view: { kind: "activity" } },
  { label: "Agent setup", view: { kind: "agents" } },
  { label: "Add project", view: { kind: "add" } },
];

/** An issue key: a project key (letters, then letters or digits), a dash, a number. */
const KEY = /^[A-Za-z][A-Za-z0-9]*-\d+$/;

/**
 * Ctrl+K (Cmd+K on a Mac): go to any view, project tab or open issue by typing. Arrow keys pick,
 * Enter goes, Esc closes. An exact key like "nit-12" opens that issue even if no list shows it.
 */
export function Palette(props: { projects: Project[]; onGo: (view: View) => void; onOpenIssue: (key: string) => void; onClose: () => void }) {
  const [query, setQuery] = useState("");
  const [picked, setPicked] = useState(0);
  const [issues, setIssues] = useState<IssueRecord[]>([]);
  const list = useRef<HTMLUListElement>(null);
  const input = useRef<HTMLInputElement>(null);

  useEffect(() => {
    let live = true;
    Promise.all(props.projects.map((p) => invoke<IssueRecord[]>("issues", { project: p.key, includeClosed: false }).catch(() => [])))
      .then((all) => live && setIssues(all.flat()));
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
      ...VIEWS.map((v) => ({ id: `view-${v.label}`, label: v.label, hint: "view", run: go(v.view) })),
      ...props.projects.flatMap((p) =>
        (["list", "board", "branches", "settings"] as const).map((tab) => ({
          id: `project-${p.key}-${tab}`,
          label: `${p.name} ${tab}`,
          hint: p.key,
          run: go({ kind: "project", key: p.key, tab }),
        })),
      ),
      ...issues.map((i) => ({ id: `issue-${i.key}`, label: `${i.key} ${i.title}`, issue: i, run: open(i.key) })),
    ];
    const found = q ? all.filter((it) => it.label.toLowerCase().includes(q) || it.hint?.toLowerCase() === q) : all.filter((it) => !it.issue);
    // An exact key always comes first, so "nit-1" never opens NIT-100; one that no list shows
    // (a closed issue) still opens.
    if (KEY.test(query.trim())) {
      const at = found.findIndex((it) => it.issue?.key.toLowerCase() === q);
      if (at >= 0) found.unshift(...found.splice(at, 1));
      else {
        const key = query.trim().toUpperCase();
        found.unshift({ id: `key-${key}`, label: `Open ${key}`, hint: "issue", run: open(key) });
      }
    }
    return found.slice(0, 12);
  }, [query, issues, props]);

  useEffect(() => setPicked(0), [query]);
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
      items[picked]?.run();
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
          {query.trim() && (
            <span className="palette-count">
              {items.length} match{items.length === 1 ? "" : "es"}
            </span>
          )}
        </div>
        <ul className="palette-list" id="palette-list" role="listbox" ref={list}>
          {items.map((it, n) => (
            <li
              key={it.id}
              id={`palette-${it.id}`}
              role="option"
              aria-selected={n === picked}
              className={`${n === picked ? "on" : ""}${n === 0 || !it.issue !== !items[n - 1].issue ? " palette-first" : ""}`}
              data-group={n === 0 || !it.issue !== !items[n - 1].issue ? (it.issue ? "Issues" : "Go to") : undefined}
              onMouseEnter={() => setPicked(n)}
              onClick={it.run}
            >
              {it.issue ? <IssueToken issue={it.issue} title /> : <span className="palette-label">{it.label}</span>}
              {it.hint && <span className="palette-hint">{it.hint}</span>}
            </li>
          ))}
          {items.length === 0 && <li className="palette-none">Nothing matches.</li>}
        </ul>
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
        </div>
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

function typing(target: EventTarget | null): boolean {
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

/** The app's keys: Ctrl/Cmd+K opens the palette; j and k step through items unless you are typing or the palette is open. */
export function useKeys(openPalette: () => void, paletteOpen: boolean) {
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "k") {
        e.preventDefault();
        openPalette();
        return;
      }
      if (paletteOpen || e.ctrlKey || e.metaKey || e.altKey || typing(e.target)) return;
      if (e.key === "j") step(1);
      else if (e.key === "k") step(-1);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [openPalette, paletteOpen]);
}
