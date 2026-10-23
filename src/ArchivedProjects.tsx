import { useEffect, useState } from "react";
import { invoke } from "./backend";
import type { Project } from "./types";
import { Button, Loading, SettingsRow } from "./ui";

/** Recovery stays available even when every project is archived. */
export function ArchivedProjects({ version, onChanged }: { version: number; onChanged: () => void }) {
  const [projects, setProjects] = useState<Project[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  useEffect(() => {
    let live = true;
    invoke<Project[]>("archived_projects").then(
      (rows) => { if (live) { setProjects(rows); setError(null); } },
      (e) => live && setError(String(e)),
    );
    return () => { live = false; };
  }, [version]);
  const restore = async (key: string) => {
    setBusy(key);
    setError(null);
    try {
      await invoke("set_project_archived", { key, archived: false });
      setProjects((rows) => rows?.filter((p) => p.key !== key) ?? null);
      onChanged();
    } catch (e) { setError(String(e)); }
    finally { setBusy(null); }
  };
  return <>
    {error && <p className="error">{error}</p>}
    {!projects && !error && <Loading />}
    {projects?.length === 0 && <p className="muted">No archived projects.</p>}
    {projects?.map((p) => <SettingsRow key={p.key} label={p.name} why={`${p.key} · ${p.open_issues} open issues preserved`}>
      <Button disabled={busy !== null} onClick={() => restore(p.key)}>{busy === p.key ? "Restoring…" : "Restore project"}</Button>
    </SettingsRow>)}
  </>;
}
