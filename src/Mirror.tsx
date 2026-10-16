import { useEffect, useState } from "react";
import { invoke } from "./backend";
import type { IssueMirror, MirrorPreview } from "./types";

const BACKFILL_LABEL: Record<string, string> = {
  none: "Only issues filed from now on",
  open: "Also the open issues there are now",
  all: "Also every issue there is now, closed ones too",
};

/**
 * Mirroring the project's issues to its repo's own tracker (docs/mirror.md). Off until the person
 * turns it on here, after seeing how many issues would be published and how they would look.
 */
export function MirrorSettings(props: { projectKey: string }) {
  const [p, setP] = useState<MirrorPreview | null>(null);
  const [backfill, setBackfill] = useState("none");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    setP(null);
    setError(null);
    invoke<MirrorPreview>("mirror_preview", { project: props.projectKey })
      .then(setP)
      .catch((e) => setError(String(e)));
  }, [props.projectKey]);

  const set = (next: string | null) => {
    setBusy(true);
    setError(null);
    invoke<MirrorPreview>("set_mirror", { project: props.projectKey, backfill: next })
      .then(setP)
      .catch((e) => setError(String(e)))
      .finally(() => setBusy(false));
  };

  const where = p?.host ? `${p.host} (${p.repo})` : "the repo's tracker";
  const count = p?.counts.find((c) => c.backfill === backfill);
  const kept = count ? count.private + count.held : 0;

  return (
    <section className="settings-card">
      <h2>Mirror issues to {p?.host ?? "the code host"}</h2>
      {!p && !error && <p className="settings-why">Checking the repo's tracker…</p>}
      {p && p.enabled && (
        <>
          <p className="settings-why">
            Issues are mirrored to {where}, one way: Nitely stays where things are decided, and nothing said there is read back. Private
            issues, the human's own work, urgent bugs and anything labelled security stay off it.
          </p>
          <div className="settings-row">
            <button className="btn" disabled={busy} onClick={() => set(null)}>
              Stop mirroring
            </button>
            <span className="muted small">Published issues stay on {p.host}; new changes stop going there.</span>
          </div>
          {p.last_error && <p className="error small">Some issues could not be published yet: {p.last_error}</p>}
        </>
      )}
      {p && !p.enabled && p.unavailable && <p className="settings-why">Not available: {p.unavailable}</p>}
      {p && !p.enabled && !p.unavailable && (
        <>
          <p className="settings-why">
            Publish this project's issues to {where} so people who look at the repo see why each pull request exists: title, body, labels, and
            a few timeline entries (handoffs, review outcomes, lands). Local paths and agent names are rewritten, private terms hold an issue
            back, and private issues, the human's own work, urgent bugs and anything labelled security are never published.
          </p>
          <div className="mirror-backfill" role="radiogroup" aria-label="Which issues to publish">
            {p.counts.map((c) => (
              <label key={c.backfill} className="mirror-option">
                <input type="radio" name="backfill" checked={backfill === c.backfill} onChange={() => setBackfill(c.backfill)} />
                <span>{BACKFILL_LABEL[c.backfill] ?? c.backfill}</span>
                {c.backfill !== "none" && <span className="muted small">{c.publish} published now</span>}
              </label>
            ))}
          </div>
          {kept > 0 && <p className="muted small">{kept} of those stay off {p.host}: private, sensitive or holding a private term.</p>}
          {p.sample.length > 0 && backfill !== "none" && (
            <details className="mirror-sample">
              <summary>How {p.sample[0].key} would look</summary>
              <p className="mirror-title">{p.sample[0].title}</p>
              <pre className="mirror-body">{p.sample[0].body}</pre>
              {p.sample[0].labels.length > 0 && <p className="muted small">Labels: {p.sample[0].labels.join(", ")}</p>}
            </details>
          )}
          <div className="settings-row">
            <button
              className="btn primary"
              disabled={busy}
              onClick={() => {
                const n = count?.publish ?? 0;
                const what = backfill === "none" ? "new issues from now on" : `${n} issue${n === 1 ? "" : "s"} now, and new ones as they come`;
                if (window.confirm(`Publish ${what} to ${where}? Anyone who can see the repo will see them.`)) set(backfill);
              }}
            >
              Turn on
            </button>
          </div>
        </>
      )}
      {error && <p className="error small">{error}</p>}
    </section>
  );
}

const STATUS_WORD: Record<string, string> = { private: "Private", held: "Held back", withdrawn: "Withdrawn", unpublished: "Not published" };

/** An issue's place in the mirror, in the issue panel's facts: its link there, or why it is not. */
export function MirrorFact(props: { issueKey: string; version: number }) {
  const [m, setM] = useState<IssueMirror | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    let live = true;
    invoke<IssueMirror | null>("issue_mirror", { key: props.issueKey })
      .then((r) => live && setM(r))
      .catch(() => live && setM(null));
    return () => {
      live = false;
    };
  }, [props.issueKey, props.version]);
  if (!m) return null;
  const toggle = () =>
    invoke<IssueMirror | null>("set_issue_private", { key: props.issueKey, private: !m.marked_private })
      .then((r) => (setM(r), setError(null)))
      .catch((e) => setError(String(e)));
  return (
    <>
      <dt>{m.host ?? "Mirror"}</dt>
      <dd>
        {m.status === "mirrored" && m.url ? (
          <a href={m.url} target="_blank" rel="noreferrer">
            #{m.number}
          </a>
        ) : (
          <span title={m.reason ?? undefined}>
            {STATUS_WORD[m.status] ?? m.status}
            {m.reason ? `: ${m.reason}` : ""}
          </span>
        )}{" "}
        <button className="link" onClick={toggle}>
          {m.marked_private ? "Allow publishing" : "Keep private"}
        </button>
        {error && <span className="error small"> {error}</span>}
      </dd>
    </>
  );
}
