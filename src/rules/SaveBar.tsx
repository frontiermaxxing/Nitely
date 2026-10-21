import { useEffect, useRef } from "react";
import { Button } from "../ui";

const MAC = typeof navigator !== "undefined" && /Mac|iPhone|iPad/.test(navigator.platform);

/**
 * The bar pinned to the bottom of the content while there are unsaved changes: how many, what they
 * are in one line, Discard, and Save (Ctrl S, or ⌘S on a Mac). Nothing shows while nothing changed.
 */
export function SaveBar(props: { changes: string[]; label: string; busy: boolean; onSave: () => void; onDiscard?: () => void }) {
  const n = props.changes.length;
  // The latest handler, so the key listener stays put while the draft changes under it.
  const save = useRef(props.onSave);
  save.current = props.onSave;
  const ready = n > 0 && !props.busy;
  useEffect(() => {
    if (!ready) return;
    const key = (e: KeyboardEvent) => {
      if ((MAC ? e.metaKey : e.ctrlKey) && !e.altKey && !e.shiftKey && e.key.toLowerCase() === "s") {
        e.preventDefault();
        save.current();
      }
    };
    window.addEventListener("keydown", key);
    return () => window.removeEventListener("keydown", key);
  }, [ready]);
  if (n === 0) return null;
  return (
    <div className="rules-savebar" role="region" aria-label="Unsaved changes">
      <span className="rules-savebar-dot" aria-hidden="true" />
      <span className="rules-savebar-count">
        {n} unsaved change{n === 1 ? "" : "s"}
      </span>
      <span className="rules-savebar-summary" title={props.changes.join("\n")}>
        {props.changes.join("; ")}
      </span>
      <span className="rules-savebar-gap" />
      {props.onDiscard && (
        <Button disabled={props.busy} onClick={props.onDiscard}>
          Discard
        </Button>
      )}
      <Button variant="primary" disabled={props.busy} onClick={props.onSave} shortcut={MAC ? "⌘S" : "Ctrl S"}>
        {props.label}
      </Button>
    </div>
  );
}
