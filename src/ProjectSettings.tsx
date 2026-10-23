import { useEffect, useState } from "react";
import { invoke } from "./backend";
import type { Project } from "./types";
import { PrivateTerms } from "./PrivateTerms";
import { MirrorSettings } from "./Mirror";
import { ProjectReviewModeSettings, type ReviewModeControl } from "./ProjectReviewMode";
import { Button, SettingsRow, SettingsSection, TextField } from "./ui";
import "./settings.css";

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
 * A project's name and key, its crew, and removing it. A new key re-keys every issue (NIT-12 becomes
 * NX-12) and old links keep working; removing a project with issues deletes them, so it asks for
 * the key to be typed first. The backend (crates/nitely-core/src/projects.rs) enforces both.
 */
export function ProjectSettings(props: {
  project: Project;
  reviewMode: ReviewModeControl;
  version: number;
  /** Issues in the project, open and closed. */
  issueCount: number;
  onChanged: () => void;
  /** The project now goes by another key, or is gone (null). */
  onMoved: (key: string | null) => void;
  /** Opens Settings on this project's crew. */
  onCrewSettings: () => void;
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
    <div className="project-settings">
      <ProjectReviewModeSettings control={props.reviewMode} version={props.version} />
      <SettingsSection title="Project" description={`What ${project.key} is called, and the key its issues start with.`}>
        <SettingsRow label="Name" htmlFor="project-name">
          <form
            className="setting-inline"
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
            <TextField id="project-name" className="setting-wide" value={name} onChange={(e) => setName(e.target.value)} />
            <Button type="submit" disabled={busy || !name.trim() || name.trim() === project.name}>
              Rename
            </Button>
          </form>
        </SettingsRow>

        <SettingsRow
          label="Key"
          htmlFor="project-key"
          why={`The prefix of every issue. A new key renumbers nothing: ${project.key}-12 becomes ${keyOk && newKey !== project.key ? newKey : "XYZ"}-12, its history and reviews go with it, and links and branches that name ${project.key}-12 keep finding it. Refused while one of its issues is checked out in a worktree.`}
        >
          <form
            className="setting-inline"
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
            <TextField
              id="project-key"
              className="setting-number"
              mono
              value={key}
              onChange={(e) => setKey(e.target.value.toUpperCase())}
              maxLength={10}
              error={!keyOk && key.trim() !== "" ? "2 to 10 letters or digits, starting with a letter." : null}
            />
            <Button type="submit" disabled={busy || !keyOk || newKey === project.key}>
              Change key
            </Button>
          </form>
        </SettingsRow>

        <SettingsRow label="Crew" why="The agents Nitely runs on this project's Todo issues are set with the app's other settings, beside every other project's crew.">
          <span className="setting-inline">
            <Button onClick={props.onCrewSettings}>Open the crew in Settings</Button>
          </span>
        </SettingsRow>
      </SettingsSection>

      <PrivateTerms projectKey={project.key} />

      <MirrorSettings projectKey={project.key} />

      <SettingsSection title="Archive project" description="Hide this project from navigation, daily views and combined totals. Its issues, history and repository connection stay intact. New tasks stop; work already underway can finish. Restore it from Settings → Archived projects.">
        <Button disabled={busy} onClick={() => run(
          () => invoke<Project>("set_project_archived", { key: project.key, archived: true }),
          () => { props.onChanged(); props.onMoved(null); },
        )}>Archive project</Button>
      </SettingsSection>

      <SettingsSection
        title="Remove project"
        description={
          needsConfirm
            ? `${props.issueCount} issue${props.issueCount === 1 ? "" : "s"} and everything recorded about them (timelines, reviews, questions) are deleted with it. This cannot be undone.`
            : "It has no issues, so nothing else goes with it. Repos that name it stay watched, with no project."
        }
      >
        <SettingsRow label={`Remove ${project.key}`} why={needsConfirm ? `Type ${project.key} to confirm.` : undefined} htmlFor={needsConfirm ? "project-remove" : undefined}>
          <form
            className="setting-inline"
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
            {needsConfirm && <TextField id="project-remove" className="setting-number" mono value={confirm} onChange={(e) => setConfirm(e.target.value)} placeholder={project.key} />}
            <Button type="submit" variant="danger" disabled={busy || !confirmed}>
              Remove {project.key}
            </Button>
          </form>
        </SettingsRow>
      </SettingsSection>

      {note && <p className="muted setting-feedback">{note}</p>}
      {error && <p className="error setting-feedback">{error}</p>}
    </div>
  );
}
