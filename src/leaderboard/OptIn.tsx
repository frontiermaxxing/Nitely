import { useEffect, useState } from "react";
import { invoke } from "../backend";
import type { Project } from "../types";
import { Button, Checkbox, TextField } from "../ui";
import { join as joinBoard } from "./profile";

// L1: joining the leaderboard. The right side is the report exactly as it would be sent, built
// by the same code that will send it (`leaderboard_report`, crates/nitely-crew/src/leaderboard_report.rs),
// so what it shows is the disclaimer. Joining keeps your choices on this machine and sends
// nothing: sending is a step of its own, turned on later.

/** What the report holds, in the words of its fields. Keep in step with leaderboard_report.rs. */
const SHARED = [
  "Changes landed: the daily average, and a count per week for 26 weeks",
  "Share that passed review on the first round, and review rounds",
  "Time from start to land: the median and the slowest tenth",
  "Time you took to answer the Inbox: the median and the slowest tenth",
  "Agent hours worked and tokens used",
  "Changes landed per agent vendor (Claude, Codex, Cursor…)",
  "Your records: most in a day and in a week, longest streak, fastest land",
  "How many projects count, as a number, and which week it is",
  "The name and links you choose here",
];

const NEVER = [
  "Code, diffs, file names and paths",
  "Issue keys, titles, bodies and comments",
  "Repo, project and branch names",
  "Prompts, findings and review text",
  "Your name, email and git identity",
  "Your time zone, and which days and hours you worked",
];

type Field = "name" | "github" | "x" | "website";

/** Which field a refusal from the report builder is about. */
function fieldOf(error: string): Field | null {
  if (/display name/i.test(error)) return "name";
  if (/github/i.test(error)) return "github";
  if (/\bX link\b/.test(error)) return "x";
  if (/website/i.test(error)) return "website";
  return null;
}

export function OptIn(props: { projects: Project[] }) {
  const [name, setName] = useState("");
  const [github, setGithub] = useState("");
  const [x, setX] = useState("");
  const [website, setWebsite] = useState("");
  // Every project counts until you leave one out.
  const [left, setLeft] = useState<Set<string>>(new Set());
  const [consent, setConsent] = useState(false);
  // The last report built, and the last refusal, each with the choices it was built from.
  const [built, setBuilt] = useState<{ key: string; report: unknown } | null>(null);
  const [refused, setRefused] = useState<{ key: string; error: string } | null>(null);
  const counted = props.projects.map((p) => p.key).filter((k) => !left.has(k));
  // Empty means every project, as the stats read it; all of them chosen is the same.
  const projects = counted.length === props.projects.length ? [] : counted;
  const profile = { display_name: name, github: github || null, x: x || null, website: website || null };

  const key = JSON.stringify([projects, profile]);
  useEffect(() => {
    let live = true;
    const t = setTimeout(() => {
      invoke<unknown>("leaderboard_report", { projects, profile }).then(
        (r) => live && (setBuilt({ key, report: r }), setRefused(null)),
        (e) => live && setRefused({ key, error: String(e) }),
      );
    }, 200);
    return () => {
      live = false;
      clearTimeout(t);
    };
  }, [key]);

  // A refusal speaks only for the choices it was given; a report shows until the next one is built.
  const error = refused?.key === key ? refused.error : null;
  const report = refused ? null : built?.report;
  const errorField = error ? fieldOf(error) : null;
  const errorFor = (f: Field) => (errorField === f && (f !== "name" || name.trim()) ? error : null);
  const json = report && !error ? JSON.stringify(report, null, 2) : null;
  const bytes = json ? new TextEncoder().encode(JSON.stringify(report)).length : null;
  const canJoin = !!json && consent && counted.length > 0;

  // Joining saves your choices in Nitely's data folder and makes this install's signing key; it sends nothing.
  const [joining, setJoining] = useState(false);
  const [joinError, setJoinError] = useState<string | null>(null);
  const join = () => {
    setJoining(true);
    setJoinError(null);
    joinBoard({ display_name: name.trim(), github: github.trim() || null, x: x.trim() || null, website: website.trim() || null }, projects)
      .catch((e) => setJoinError(String(e)))
      .finally(() => setJoining(false));
  };

  return (
    <div className="lb-optin">
      <div className="lb-optin-main">
        <p className="lb-intro">
          See how your crew compares with other people running agents on Nitely: how much lands, how often it passes review the
          first time, and how quickly you unblock your agents. Nothing is sent until you join and then turn sending on.
        </p>
        <div className="lb-lists">
          <section className="lb-list">
            <h2>
              <span className="lb-list-dot lb-list-shared" aria-hidden="true" />
              Shared, once a week
            </h2>
            <ul>
              {SHARED.map((s) => (
                <li key={s}>
                  <svg width="12" height="12" viewBox="0 0 16 16" aria-hidden="true">
                    <path d="M3.5 8.2 6.5 11 12.5 5" fill="none" stroke="var(--live)" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" />
                  </svg>
                  {s}
                </li>
              ))}
            </ul>
          </section>
          <section className="lb-list lb-list-never">
            <h2>
              <span className="lb-list-dot" aria-hidden="true" />
              Never leaves this machine
            </h2>
            <ul>
              {NEVER.map((s) => (
                <li key={s}>
                  <svg width="12" height="12" viewBox="0 0 14 14" aria-hidden="true">
                    <path d="M3.5 3.5l7 7M10.5 3.5l-7 7" stroke="var(--fg-3)" strokeWidth="1.6" strokeLinecap="round" />
                  </svg>
                  {s}
                </li>
              ))}
            </ul>
            <p className="lb-ip">
              Your IP address: sending the report is a request over the internet, so the leaderboard's service and the proxy in
              front of it see the address it comes from. They don't store it.
            </p>
          </section>
        </div>

        <form
          className="lb-form"
          onSubmit={(e) => {
            e.preventDefault();
            if (canJoin && !joining) join();
          }}
        >
          <div className="lb-form-row">
            <label htmlFor="lb-name" className="lb-form-label">
              Show me as
            </label>
            <div className="lb-form-control">
              <TextField id="lb-name" value={name} onChange={(e) => setName(e.target.value)} placeholder="a name for the board" maxLength={40} autoComplete="off" error={errorFor("name")} />
              <span className="lb-sub">Not your name or git email</span>
            </div>
          </div>
          <div className="lb-form-row">
            <span className="lb-form-label">Projects</span>
            <div className="lb-form-control lb-projects">
              {props.projects.map((p) => (
                <Checkbox
                  key={p.key}
                  label={p.name}
                  checked={!left.has(p.key)}
                  onChange={(on) => {
                    const next = new Set(left);
                    if (on) next.delete(p.key);
                    else next.add(p.key);
                    setLeft(next);
                  }}
                />
              ))}
              <span className="lb-sub">{counted.length === 0 ? "Choose at least one" : "Only counted, never named"}</span>
            </div>
          </div>
          <div className="lb-form-row lb-form-top">
            <span className="lb-form-label">
              Links
              <span className="lb-sub">optional, public</span>
            </span>
            <div className="lb-form-control lb-links-form">
              <LinkField label="GitHub" prefix="github.com/" value={github} onChange={setGithub} placeholder="user name" error={errorFor("github")} />
              <LinkField label="X" prefix="@" value={x} onChange={setX} placeholder="handle" error={errorFor("x")} />
              <LinkField label="Website" value={website} onChange={setWebsite} placeholder="https://example.com" error={errorFor("website")} />
            </div>
          </div>
          <div className="lb-consent">
            <Checkbox
              checked={consent}
              onChange={setConsent}
              label="Once I turn sending on, send this report to the leaderboard once a week. I can pause it or leave at any time."
            />
          </div>
          <div className="lb-join">
            <Button variant="primary" type="submit" disabled={!canJoin || joining}>
              {joining ? "Joining…" : "Join the leaderboard"}
            </Button>
            <span className="lb-sub">Joining sends nothing: browse first, turn sending on later.</span>
            {joinError && (
              <span className="ui-field-error" role="alert">
                Could not join: {joinError}
              </span>
            )}
          </div>
        </form>
      </div>

      <section className="lb-report" aria-label="The report">
        <div className="lb-report-head">
          <h2>The report, exactly as it would be sent</h2>
          <span className="lb-sub">this week, from your data</span>
        </div>
        <div className="lb-report-box">
          <div className="lb-report-bar">
            <span>weekly report</span>
            <span>{bytes !== null ? `${bytes.toLocaleString("en-US")} bytes` : ""}</span>
          </div>
          {json ? (
            <pre className="lb-json">
              <Json text={json} />
            </pre>
          ) : (
            <p className="lb-report-wait">
              {error && !name.trim() && errorField === "name"
                ? "Choose a display name to see the report it goes with."
                : error && errorField
                  ? "Fix the field marked on the left to see the report."
                  : error
                    ? `Could not build the report: ${error}`
                    : "Building the report…"}
            </p>
          )}
        </div>
        <p className="lb-note">
          This is built by the same code that will send it, from this machine's numbers. Nothing here has been sent.
        </p>
      </section>
    </div>
  );
}

function LinkField(props: { label: string; prefix?: string; value: string; onChange: (v: string) => void; placeholder: string; error: string | null }) {
  return (
    <div className="lb-link-field">
      <div className={`lb-link-input${props.error ? " lb-invalid" : ""}`}>
        <span className="lb-link-name">{props.label}</span>
        {props.prefix && <span className="lb-link-prefix">{props.prefix}</span>}
        <input
          aria-label={props.label}
          aria-invalid={props.error ? true : undefined}
          value={props.value}
          onChange={(e) => props.onChange(e.target.value)}
          placeholder={props.placeholder}
          spellCheck={false}
          autoComplete="off"
        />
      </div>
      {props.error && (
        <span className="ui-field-error" role="alert">
          {props.error}
        </span>
      )}
    </div>
  );
}

/** Pretty-printed JSON with keys a step back from the values. */
function Json(props: { text: string }) {
  return (
    <>
      {props.text.split("\n").map((line, i) => {
        const m = /^(\s*)("[^"]*":\s?)?(.*)$/.exec(line)!;
        return (
          <span key={i} className="lb-json-line">
            {m[1]}
            {m[2] && <span className="lb-json-key">{m[2]}</span>}
            <span className={/^"/.test(m[3]) ? "lb-json-str" : "lb-json-val"}>{m[3]}</span>
            {"\n"}
          </span>
        );
      })}
    </>
  );
}
