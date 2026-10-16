import { useEffect, useState } from "react";
import { invoke } from "./backend";
import type { Project } from "./types";
import { PrivateTerms } from "./PrivateTerms";

interface Rekeyed {
  from: string;
  to: string;
  issues: number;
  repos: number;
  machines: number;
  /** What could not follow the change, e.g. a config file that would not save. */
  warnings: string[];
}

interface Removed {
  key: string;
  issues: number;
  warnings: string[];
}

const KEY = /^[A-Z][A-Z0-9]{1,9}$/;

/**
 * A project's name and key, and removing it. A new key re-keys every issue (NIT-12 becomes
 * NX-12) and old links keep working; removing a project with issues deletes them, so it asks for
 * the key to be typed first. The backend (crates/nitely-core/src/projects.rs) enforces both.
 */
export function ProjectSettings(props: {
  project: Project;
  /** Issues in the project, open and closed. */
  issueCount: number;
  onChanged: () => void;
  /** The project now goes by another key, or is gone (null). */
  onMoved: (key: string | null) => void;
}) {
  const { project } = props;
  const [name, setName] = useState(project.name);
  const [key, setKey] = useState(project.key);
  const [confirm, setConfirm] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [note, setNote] = useState<string | null>(null);

  useEffect(() => {
    setName(project.name);
    setKey(project.key);
    setConfirm("");
  }, [project.key, project.name]);

  const run = async <T,>(work: () => Promise<T>, done: (r: T) => void) => {
    setBusy(true);
    setError(null);
    setNote(null);
    try {
      done(await work());
    } catch (e) {
      setError(String(e));
    }
    setBusy(false);
  };

  const newKey = key.trim().toUpperCase();
  const keyOk = KEY.test(newKey);
  const needsConfirm = props.issueCount > 0;
  const confirmed = !needsConfirm || confirm.trim().toUpperCase() === project.key;

  return (
    <div className="settings">
      <section className="settings-card">
        <h2>Name</h2>
        <form
          className="settings-row"
          onSubmit={(e) => {
            e.preventDefault();
            run(
              () => invoke<Project>("rename_project", { key: project.key, name }),
              (p) => {
                setNote(`Renamed to ${p.name}.`);
                props.onChanged();
              },
            );
          }}
        >
          <input value={name} onChange={(e) => setName(e.target.value)} aria-label="Project name" />
          <button className="btn" disabled={busy || !name.trim() || name.trim() === project.name}>
            Rename
          </button>
        </form>
      </section>

      <section className="settings-card">
        <h2>Key</h2>
        <p className="settings-why">
          The prefix of every issue. A new key renumbers nothing: {project.key}-12 becomes {keyOk && newKey !== project.key ? newKey : "XYZ"}-12, its
          history and reviews go with it, and links and branches that name {project.key}-12 keep finding it. Refused while one of its issues is checked
          out in a worktree.
        </p>
        <form
          className="settings-row"
          onSubmit={(e) => {
            e.preventDefault();
            run(
              () => invoke<Rekeyed>("rekey_project", { key: project.key, newKey }),
              (r) => {
                setNote(`${r.issues} issue${r.issues === 1 ? "" : "s"} now start with ${r.to}; ${r.from}-… links still open them.`);
                // The view moves to the new key at once, so what could not follow is said now.
                if (r.warnings.length) window.alert(r.warnings.join("\n"));
                props.onChanged();
                props.onMoved(r.to);
              },
            );
          }}
        >
          <input className="mono" value={key} onChange={(e) => setKey(e.target.value.toUpperCase())} maxLength={10} aria-label="Project key" />
          <button className="btn" disabled={busy || !keyOk || newKey === project.key}>
            Change key
          </button>
        </form>
        {!keyOk && key.trim() !== "" && <p className="settings-hint">2 to 10 letters or digits, starting with a letter.</p>}
      </section>

      <PrivateTerms projectKey={project.key} />

      <section className="settings-card settings-danger">
        <h2>Remove project</h2>
        <p className="settings-why">
          {needsConfirm
            ? `${props.issueCount} issue${props.issueCount === 1 ? "" : "s"} and everything recorded about them (timelines, reviews, questions) are deleted with it. This cannot be undone. Type ${project.key} to confirm.`
            : "It has no issues, so nothing else goes with it. Repos that name it stay watched, with no project."}
        </p>
        <form
          className="settings-row"
          onSubmit={(e) => {
            e.preventDefault();
            run(
              () => invoke<Removed>("remove_project", { key: project.key, confirm: needsConfirm ? confirm : null }),
              (r) => {
                if (r.warnings.length) window.alert(r.warnings.join("\n"));
                props.onChanged();
                props.onMoved(null);
              },
            );
          }}
        >
          {needsConfirm && <input className="mono" value={confirm} onChange={(e) => setConfirm(e.target.value)} placeholder={project.key} aria-label={`Type ${project.key} to confirm`} />}
          <button className="btn danger" disabled={busy || !confirmed}>
            Remove {project.key}
          </button>
        </form>
      </section>

      {note && <p className="muted small">{note}</p>}
      {error && <p className="error small">{error}</p>}
    </div>
  );
}
