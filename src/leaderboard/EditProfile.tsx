import { useEffect, useRef, useState } from "react";
import { invoke } from "../backend";
import type { Project } from "../types";
import { Button, Checkbox, RemoveButton, Segmented, Switch, TextField } from "../ui";
import { Avatar, type AvatarChoice } from "./Avatar";
import { Json, LinkField, fieldOf, type Field } from "./OptIn";
import { join, leave, sendNow, sharing, stopSharing, type Profile } from "./profile";
import { when } from "./when";
import { nowSecs } from "../util";

// L4: your leaderboard profile, from the sharing chip or Edit profile. The name, links and
// projects are saved together; the sharing switch applies at once, because turning it on sends
// this week's report and turning it off deletes what was sent.

export function ProfileDialog(props: { profile: Profile; projects: Project[]; onClose: () => void; onLeft: () => void }) {
  const p = props.profile;
  const [name, setName] = useState(p.display_name);
  const [github, setGithub] = useState(p.github ?? "");
  const [x, setX] = useState(p.x ?? "");
  const [website, setWebsite] = useState(p.website ?? "");
  const [avatar, setAvatar] = useState<AvatarChoice | "none">(p.avatar ?? "none");
  // Empty means every project; otherwise the keys that count.
  const all = props.projects.map((q) => q.key);
  const [counted, setCounted] = useState<string[]>(p.projects.length ? p.projects.filter((k) => all.includes(k)) : all);
  const projects = counted.length === all.length ? [] : counted;
  // A picture from a link that is now empty means none.
  const picture: AvatarChoice | null = avatar === "github" ? (github.trim() ? "github" : null) : avatar === "x" ? (x.trim() ? "x" : null) : null;
  const edited = { display_name: name.trim(), github: github.trim() || null, x: x.trim() || null, website: website.trim() || null, avatar: picture };
  const changed =
    edited.display_name !== p.display_name ||
    edited.github !== (p.github ?? null) ||
    edited.x !== (p.x ?? null) ||
    edited.website !== (p.website ?? null) ||
    edited.avatar !== (p.avatar ?? null) ||
    projects.join(",") !== p.projects.join(",") ||
    // No project chosen is not "every project" (the empty list's meaning): it can't be saved or shared.
    counted.length === 0;

  // Each field is checked inline by the report builder, the same check a save makes.
  const key = JSON.stringify([projects, edited]);
  const [checked, setChecked] = useState<{ key: string; error: string | null; report: unknown } | null>(null);
  useEffect(() => {
    let live = true;
    const t = setTimeout(() => {
      invoke<unknown>("leaderboard_report", { projects, profile: edited }).then(
        (report) => live && setChecked({ key, error: null, report }),
        (e) => live && setChecked({ key, error: String(e), report: null }),
      );
    }, 200);
    return () => {
      live = false;
      clearTimeout(t);
    };
  }, [key]);
  const error = checked?.key === key ? checked.error : null;
  const errorField = error ? fieldOf(error) : null;
  const errorFor = (f: Field) => (errorField === f ? error : null);

  const [showReport, setShowReport] = useState(false);
  const [busy, setBusy] = useState<"save" | "share" | "leave" | null>(null);
  const [failed, setFailed] = useState<string | null>(null);
  const [leaving, setLeaving] = useState(false);
  const act = async (what: "save" | "share" | "leave", run: () => Promise<unknown>) => {
    setBusy(what);
    setFailed(null);
    try {
      await run();
      return true;
    } catch (e) {
      setFailed(String(e));
      return false;
    } finally {
      setBusy(null);
    }
  };
  const save = async () => {
    if (await act("save", () => join(edited, projects))) props.onClose();
  };
  const on = sharing(p);

  // Esc closes, Tab stays inside, and the focus goes back where it was. Set up once when the
  // dialog opens: the page re-renders on every refresh, and must not pull the focus back.
  const box = useRef<HTMLDivElement>(null);
  const onClose = useRef(props.onClose);
  onClose.current = props.onClose;
  useEffect(() => {
    const was = document.activeElement as HTMLElement | null;
    box.current?.querySelector<HTMLElement>("input")?.focus();
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        e.stopImmediatePropagation();
        onClose.current();
      } else if (e.key === "Tab" && box.current) {
        const all = [...box.current.querySelectorAll<HTMLElement>("button:not([disabled]), input:not([disabled]), [tabindex='0']")];
        const first = all[0];
        const last = all[all.length - 1];
        if (e.shiftKey && document.activeElement === first) {
          e.preventDefault();
          last?.focus();
        } else if (!e.shiftKey && document.activeElement === last) {
          e.preventDefault();
          first?.focus();
        }
      }
    };
    window.addEventListener("keydown", onKey, true);
    return () => {
      window.removeEventListener("keydown", onKey, true);
      if (was?.isConnected) was.focus({ preventScroll: true });
    };
  }, []);

  return (
    <div className="palette-veil lb-veil" onMouseDown={props.onClose}>
      <div className="lb-dialog" role="dialog" aria-modal="true" aria-labelledby="lb-dialog-title" ref={box} onMouseDown={(e) => e.stopPropagation()}>
        <div className="lb-dialog-head">
          <h2 id="lb-dialog-title">Your leaderboard profile</h2>
          <RemoveButton label="Close" onClick={props.onClose} />
        </div>
        <div className="lb-dialog-body">
          <div className="lb-dialog-row">
            <label htmlFor="lb-edit-name" className="lb-form-label">
              Show me as
            </label>
            <TextField id="lb-edit-name" value={name} onChange={(e) => setName(e.target.value)} maxLength={40} autoComplete="off" error={errorFor("name")} />
          </div>
          <div className="lb-dialog-row lb-form-top">
            <span className="lb-form-label">Links</span>
            <div className="lb-links-form">
              <LinkField label="GitHub" prefix="github.com/" value={github} onChange={setGithub} placeholder="user name" error={errorFor("github")} clearable />
              <LinkField label="X" prefix="@" value={x} onChange={setX} placeholder="handle" error={errorFor("x")} clearable />
              <LinkField label="Website" value={website} onChange={setWebsite} placeholder="https://example.com" error={errorFor("website")} clearable />
              <span className="lb-sub">Shown on your row and card. Nitely only keeps the link; it never signs in to these sites.</span>
            </div>
          </div>
          <div className="lb-dialog-row lb-form-top">
            <span className="lb-form-label">Picture</span>
            <div className="lb-picture-form">
              <Avatar name={edited.display_name || "?"} choice={picture} links={{ github: edited.github, x: edited.x }} size={36} />
              <div className="lb-picture-choice">
                <Segmented<AvatarChoice | "none">
                  aria-label="Picture"
                  value={picture ?? "none"}
                  onChange={setAvatar}
                  options={[
                    { value: "none", label: "Initials" },
                    { value: "github", label: "GitHub", disabled: !github.trim() },
                    { value: "x", label: "X", disabled: !x.trim() },
                  ]}
                />
                <span className="lb-sub">
                  {picture === "x"
                    ? "Loaded from unavatar.io, a public avatar service, since X has no public picture address. Nitely keeps only the choice."
                    : "Loaded from that site when the board shows. Nitely keeps only the choice, never the image."}
                </span>
                {errorFor("avatar") && <span className="ui-field-error">{errorFor("avatar")}</span>}
              </div>
            </div>
          </div>
          <div className="lb-dialog-row">
            <span className="lb-form-label">Projects</span>
            <div className="lb-projects">
              {props.projects.map((q) => (
                <Checkbox
                  key={q.key}
                  label={q.name}
                  checked={counted.includes(q.key)}
                  onChange={(c) => setCounted(c ? all.filter((k) => k === q.key || counted.includes(k)) : counted.filter((k) => k !== q.key))}
                />
              ))}
              <span className="lb-sub">{counted.length === 0 ? "Choose at least one" : "Only counted, never named"}</span>
            </div>
          </div>
          <div className="lb-dialog-row lb-form-top">
            <span className="lb-form-label">Sharing</span>
            <div className="lb-sharing">
              <Switch
                checked={on}
                disabled={busy !== null || changed}
                onChange={(next) => act("share", next ? sendNow : stopSharing)}
                label={
                  busy === "share"
                    ? on
                      ? "Deleting what you sent…"
                      : "Sending this week's report…"
                    : on
                      ? `On · last report ${p.last_sent_week}${p.next_send_at !== null ? `, next ${when(nowSecs(), p.next_send_at)}` : ""}, then daily`
                      : "Off · nothing is sent"
                }
              />
              <span className="lb-sub">
                {changed
                  ? "Save your changes first."
                  : on
                    ? "Turning it off deletes every report you sent from the board."
                    : "Turning it on sends this week's report now, then one a day; Sunday evening's closes the week."}
              </span>
              {p.last_error && <span className="ui-field-error">The last send failed: {p.last_error}</span>}
              <button className="link lb-see-report" aria-expanded={showReport} onClick={() => setShowReport(!showReport)}>
                {showReport ? "Hide the report" : "See the report"}
              </button>
              {showReport && (
                <div className="lb-report-box">
                  {checked?.key === key && checked.report ? (
                    <pre className="lb-json">
                      <Json text={JSON.stringify(checked.report, null, 2)} />
                    </pre>
                  ) : (
                    <p className="lb-report-wait">{error ? "Fix the field marked above to see the report." : "Building the report…"}</p>
                  )}
                </div>
              )}
            </div>
          </div>
          {failed && (
            <span className="ui-field-error" role="alert">
              {failed}
            </span>
          )}
        </div>
        <div className="lb-dialog-foot">
          {leaving ? (
            <span className="lb-leave">
              <span className="lb-sub">{on ? "Delete what you sent and leave?" : "Leave? Nothing was sent."}</span>
              <Button
                variant="danger"
                disabled={busy !== null}
                onClick={async () => {
                  if (await act("leave", leave)) props.onLeft();
                }}
              >
                {busy === "leave" ? "Leaving…" : "Leave"}
              </Button>
              <Button variant="quiet" disabled={busy !== null} onClick={() => setLeaving(false)}>
                Keep my profile
              </Button>
            </span>
          ) : (
            <button className="link lb-leave-link" onClick={() => setLeaving(true)}>
              Leave and delete my data
            </button>
          )}
          <span className="lb-dialog-spacer" />
          <Button onClick={props.onClose}>Cancel</Button>
          <Button variant="primary" disabled={!changed || checked?.key !== key || !!error || counted.length === 0 || busy !== null} onClick={save}>
            {busy === "save" ? "Saving…" : "Save"}
          </Button>
        </div>
      </div>
    </div>
  );
}
