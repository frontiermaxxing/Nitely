import { useEffect, useState } from "react";
import { invoke } from "./backend";

/** crates/nitely-core/src/private_terms.rs: one term per entry, `re:` for a regular expression. */
interface Terms {
  all: string[];
  projects: Record<string, string[]>;
}

const lines = (text: string) =>
  text
    .split("\n")
    .map((l) => l.trim())
    .filter(Boolean);

/**
 * The words, paths and patterns submit and land refuse to let a branch add to this project's
 * repos, and those refused in every project. Kept on this machine only; agents see where a term
 * was found, never the list.
 */
export function PrivateTerms(props: { projectKey: string }) {
  const key = props.projectKey;
  const [saved, setSaved] = useState<Terms | null>(null);
  const [mine, setMine] = useState("");
  const [all, setAll] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [note, setNote] = useState<string | null>(null);

  const show = (t: Terms) => {
    setSaved(t);
    setMine((t.projects[key] ?? []).join("\n"));
    setAll(t.all.join("\n"));
  };

  useEffect(() => {
    setError(null);
    setNote(null);
    invoke<Terms>("private_terms")
      .then(show)
      .catch((e) => setError(String(e)));
  }, [key]);

  const same = (a: string[], b: string[]) => a.length === b.length && a.every((x, i) => x === b[i]);
  const changed = saved !== null && (!same(lines(mine), saved.projects[key] ?? []) || !same(lines(all), saved.all));

  const save = async () => {
    if (!saved) return;
    setBusy(true);
    setError(null);
    setNote(null);
    try {
      const next: Terms = { all: lines(all), projects: { ...saved.projects, [key]: lines(mine) } };
      const t = await invoke<Terms>("set_private_terms", { terms: next });
      show(t);
      const n = (t.projects[key] ?? []).length + t.all.length;
      setNote(n === 0 ? "No private terms: nothing is refused." : `Saved. Submit and land now check ${n} term${n === 1 ? "" : "s"} here.`);
    } catch (e) {
      setError(String(e));
    }
    setBusy(false);
  };

  return (
    <section className="settings-card">
      <h2>Private terms</h2>
      <p className="settings-why">
        Names, paths or patterns that must never reach {key}'s repos, such as another project, a client or a local folder. Submit and land refuse a
        branch that adds one to a line, a file path or a commit message, and say only where: agents never see this list. One per line, matched
        anywhere and ignoring case (<code>\</code> and <code>/</code> match each other); start a line with <code>re:</code> for a regular expression.
        Kept on this machine only, never in a repo.
      </p>
      <form
        className="terms"
        onSubmit={(e) => {
          e.preventDefault();
          save();
        }}
      >
        <label className="terms-list">
          <span>Kept out of {key}</span>
          <textarea value={mine} onChange={(e) => setMine(e.target.value)} rows={4} spellCheck={false} placeholder={"Bluebird\nC:\\Work\\client-notes\nre:ACME-\\d+"} disabled={!saved} />
        </label>
        <label className="terms-list">
          <span>Kept out of every project</span>
          <textarea value={all} onChange={(e) => setAll(e.target.value)} rows={3} spellCheck={false} disabled={!saved} />
        </label>
        <div className="settings-row">
          <button className="btn" disabled={busy || !changed}>
            Save terms
          </button>
          {changed && (
            <button type="button" className="btn" disabled={busy} onClick={() => saved && show(saved)}>
              Undo
            </button>
          )}
        </div>
      </form>
      {note && <p className="muted small">{note}</p>}
      {error && <p className="error small">{error}</p>}
    </section>
  );
}
