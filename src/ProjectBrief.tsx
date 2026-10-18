import { Fragment, useEffect, useRef, useState, type KeyboardEvent, type ReactNode } from "react";
import { invoke } from "./backend";
import type { ProjectBrief as Brief } from "./types";
import { type Inline, issueKeyOf, parseMarkdown } from "./markdown";
import { AgentMark, EmptyState, Loading } from "./ui";
import { agentLabel, ago, openExternal } from "./util";

/**
 * A project's brief: what agents learned about it as a whole, which they read with `get_project`
 * before exploring the repo, and whether the setup sweep has run. The human edits it here; a save
 * is refused when an agent wrote it meanwhile, and then says so instead of overwriting.
 */
export function ProjectBrief(props: { projectKey: string; version: number; now: number; onOpenIssue: (key: string) => void }) {
  const [brief, setBrief] = useState<Brief | null>(null);
  const [error, setError] = useState<string | null>(null);
  // While editing: the text, and the project and revision it started from.
  const [draft, setDraft] = useState<{ text: string; project: string; from: number } | null>(null);
  const [saving, setSaving] = useState(false);
  const [saveError, setSaveError] = useState<string | null>(null);
  // The project on screen, so a save that answers after a switch changes nothing here.
  const shown = useRef(props.projectKey);

  useEffect(() => {
    let live = true;
    invoke<Brief>("project_brief", { project: props.projectKey })
      .then((b) => live && (setBrief(b), setError(null)))
      .catch((e) => live && setError(String(e)));
    return () => {
      live = false;
    };
  }, [props.projectKey, props.version]);

  // A different project never shows, edits or saves the last one's brief.
  useEffect(() => {
    shown.current = props.projectKey;
    setBrief(null);
    setDraft(null);
    setSaveError(null);
  }, [props.projectKey]);

  const save = (revision: number) => {
    if (!draft || saving) return;
    setSaving(true);
    const at = props.projectKey;
    const here = () => shown.current === at;
    invoke<Brief>("set_project_brief", { project: draft.project, brief: draft.text, revision })
      .then((b) => {
        if (!here()) return;
        setBrief(b);
        setDraft(null);
        setSaveError(null);
      })
      .catch((e) =>
        // Refused because someone wrote since: show their revision, so the choice above the
        // editor replaces the error. Anything else is shown as it came.
        invoke<Brief>("project_brief", { project: draft.project })
          .then((fresh) => here() && (fresh.revision !== revision ? (setBrief(fresh), setSaveError(null)) : setSaveError(String(e))))
          .catch(() => here() && setSaveError(String(e))),
      )
      .finally(() => setSaving(false));
  };

  const keys = (e: KeyboardEvent) => {
    if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) {
      e.preventDefault();
      save(draft!.from);
    } else if (e.key === "Escape") {
      e.preventDefault();
      setDraft(null);
      setSaveError(null);
    }
  };

  if (error) return <p className="error">{error}</p>;
  if (!brief) return <Loading />;

  const edit = () => setDraft({ text: brief.brief, project: brief.project, from: brief.revision });
  // Someone wrote since the draft started: saving would be refused, so offer the choice instead.
  const behind = draft && brief.revision !== draft.from;

  return (
    <div className="brief">
      <div className="brief-meta">
        <SetupLine brief={brief} now={props.now} />
        {brief.updated_at != null && brief.updated_by && (
          <span className="brief-edited" title={new Date(brief.updated_at * 1000).toLocaleString()}>
            Edited {ago(props.now, brief.updated_at)} by {who(brief.updated_by)}
          </span>
        )}
        {!draft && brief.brief.trim() && (
          <button className="btn brief-edit" onClick={edit}>
            Edit
          </button>
        )}
      </div>

      {draft ? (
        <div className="brief-editor">
          {behind && (
            <div className="brief-behind">
              <span>{brief.updated_by === "human" ? "You" : agentLabel(brief.updated_by ?? "someone")} changed the brief while you were editing.</span>
              <button className="btn" onClick={() => setDraft({ text: brief.brief, project: brief.project, from: brief.revision })}>
                Take theirs
              </button>
              <button className="btn" disabled={saving} onClick={() => save(brief.revision)}>
                Keep mine
              </button>
            </div>
          )}
          <textarea
            aria-label="Project brief, in Markdown"
            value={draft.text}
            // What is sent is what was typed when Save was pressed, so typing waits for it.
            readOnly={saving}
            autoFocus
            spellCheck
            onChange={(e) => setDraft({ ...draft, text: e.target.value })}
            onKeyDown={keys}
            placeholder={PLACEHOLDER}
          />
          {saveError && !behind && <p className="error">{saveError}</p>}
          <div className="brief-actions">
            <span className="muted">Markdown. Agents read this with get_project.</span>
            <button className="btn quiet" onClick={() => (setDraft(null), setSaveError(null))}>
              Cancel <kbd>Esc</kbd>
            </button>
            <button className="btn primary" disabled={saving || !!behind} onClick={() => save(draft.from)}>
              Save <kbd>Ctrl ↵</kbd>
            </button>
          </div>
        </div>
      ) : brief.brief.trim() ? (
        <Markdown src={brief.brief} onOpenIssue={props.onOpenIssue} />
      ) : (
        <EmptyState
          title="No brief yet."
          action={
            <button className="btn" onClick={edit}>
              Write one
            </button>
          }
        >
          The brief is what agents know about {brief.name} as a whole: the vision, the state of each area, the order to
          work in and the questions only you can answer. Agents read it before they explore the repo, and the setup sweep
          (<span className="mono">/nitely-setup</span>) writes the first one.
        </EmptyState>
      )}
    </div>
  );
}

const PLACEHOLDER = "# Vision\n\n# State of each area\n\n# Target order\n\n# Questions for the owner";

function who(actor: string): string {
  return actor === "human" ? "you" : agentLabel(actor);
}

/** Whether the first-run sweep has read the project, and at which commit. */
function SetupLine(props: { brief: Brief; now: number }) {
  const s = props.brief.setup;
  if (!s) return <span className="brief-setup brief-setup-none">Not set up yet</span>;
  return (
    <span className="brief-setup" title={`Setup sweep finished ${new Date(s.at * 1000).toLocaleString()}`}>
      Set up {ago(props.now, s.at)} by
      <AgentMark vendor={s.by} size={14} title={who(s.by)} />
      {s.by === "human" ? "you" : agentLabel(s.by)} at <span className="mono">{s.sha.slice(0, 7)}</span>
    </span>
  );
}

function Markdown(props: { src: string; onOpenIssue: (key: string) => void }) {
  const inline = (parts: Inline[]): ReactNode =>
    parts.map((p, i) => {
      switch (p.t) {
        case "text":
          return <Fragment key={i}>{p.s}</Fragment>;
        case "code":
          return <code key={i}>{p.s}</code>;
        case "strong":
          return <strong key={i}>{p.s}</strong>;
        case "em":
          return <em key={i}>{p.s}</em>;
        case "link": {
          const issue = issueKeyOf(p.href);
          if (issue)
            return (
              <button key={i} className="link" onClick={() => props.onOpenIssue(issue)}>
                {p.s}
              </button>
            );
          if (/^https?:\/\//.test(p.href))
            return (
              <a key={i} href={p.href} onClick={openExternal(p.href)}>
                {p.s}
              </a>
            );
          return <Fragment key={i}>{p.s}</Fragment>;
        }
      }
    });
  return (
    <div className="brief-doc">
      {parseMarkdown(props.src).map((b, i) => {
        switch (b.t) {
          case "h": {
            const H = `h${Math.min(b.level + 1, 6)}` as "h2";
            return <H key={i}>{inline(b.text)}</H>;
          }
          case "p":
            return <p key={i}>{inline(b.text)}</p>;
          case "quote":
            return <blockquote key={i}>{inline(b.text)}</blockquote>;
          case "hr":
            return <hr key={i} />;
          case "code":
            return (
              <pre key={i}>
                <code>{b.s}</code>
              </pre>
            );
          case "list": {
            const L = b.ordered ? "ol" : "ul";
            return (
              <L key={i}>
                {b.items.map((it, j) => (
                  <li key={j} style={it.depth ? { marginLeft: `${it.depth * 1.4}em` } : undefined} className={it.done !== undefined ? "brief-task" : undefined}>
                    {it.done !== undefined && <input type="checkbox" checked={it.done} readOnly tabIndex={-1} aria-label={it.done ? "done" : "open"} />}
                    {inline(it.text)}
                  </li>
                ))}
              </L>
            );
          }
          case "table":
            return (
              <div key={i} className="brief-table">
                <table>
                  <thead>
                    <tr>
                      {b.head.map((c, j) => (
                        <th key={j}>{inline(c)}</th>
                      ))}
                    </tr>
                  </thead>
                  <tbody>
                    {b.rows.map((r, j) => (
                      <tr key={j}>
                        {r.map((c, k) => (
                          <td key={k}>{inline(c)}</td>
                        ))}
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            );
        }
      })}
    </div>
  );
}
