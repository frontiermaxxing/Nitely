import { useState } from "react";
import type { InstallPlan, Tool } from "../types";
import { Button } from "../ui";

/** An install in progress or one that stopped, with the lines its installer printed (`tool-install` events, NIT-688). */
export interface InstallState {
  state: "installing" | "failed";
  /** "Installing with npm". */
  progress?: string;
  log: string[];
  /** Why it stopped. */
  error?: string;
}

const NOTES: Record<string, string> = {
  git: "Nitely and every agent work in git. Needed.",
  claude: "Anthropic's coding agent. You need Claude Code or Codex, or both.",
  codex: "OpenAI's coding agent. With both, each reviews the other's work.",
  gh: "Adds pull requests and CI checks to a change. Without it, changes land straight on your branch.",
};

const MARKS: Record<string, string> = { git: "G", claude: "C", codex: "X", gh: "gh" };

/** A command shown to copy: what an install or a sign-in runs. */
export function CommandLine(props: { command: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <div className="fr-cmd">
      <span className="fr-cmd-prompt">&gt;</span>
      <span className="fr-cmd-text">{props.command}</span>
      <Button
        variant="quiet"
        className="fr-cmd-copy"
        onClick={() => {
          navigator.clipboard?.writeText(props.command).catch(() => {});
          setCopied(true);
          setTimeout(() => setCopied(false), 1500);
        }}
      >
        {copied ? "Copied" : "Copy"}
      </Button>
    </div>
  );
}

export function Chip(props: { tone: "ready" | "needs" | "fail" | "quiet" | "busy"; children: string }) {
  return (
    <span className={`fr-chip fr-chip-${props.tone}`}>
      <span className="fr-dot" />
      {props.children}
    </span>
  );
}

export function ToolMark(props: { command: string }) {
  return <span className={`fr-mark fr-mark-${props.command}`}>{MARKS[props.command] ?? props.command[0]?.toUpperCase()}</span>;
}

/** Git, Claude Code, Codex and the GitHub CLI, each found, missing, installing or failed (NIT-690). */
export function ToolsStep(props: { tools: Tool[] | null; plans: InstallPlan[]; installs: Record<string, InstallState>; onInstall: (command: string) => void }) {
  // The tool whose Install was clicked: its command shows, and it runs only on Run it (NIT-692).
  const [asking, setAsking] = useState<string | null>(null);
  if (!props.tools) return <p className="fr-note">Looking for the tools on this machine…</p>;
  return (
    <div className="fr-list">
      {props.tools.map((t) => {
        const install = props.installs[t.command];
        const plan = props.plans.find((p) => p.tool === t.command);
        const command = plan?.shown ?? null;
        // Nitely runs the installer only where the plan says it may; elsewhere the command is shown to run by hand.
        const installs = !!plan?.runs;
        return (
          <div key={t.command} className="fr-row">
            <div className="fr-row-main">
              <div className="fr-name">
                <ToolMark command={t.command} />
                {t.command === "git" ? "git" : t.name}
                {t.version && <span className="fr-version">{t.version}</span>}
                {t.command === "gh" && <span className="fr-optional">optional</span>}
              </div>
              <div className="fr-note">{NOTES[t.command]}</div>
              {!t.path && install?.state === "installing" && (
                <>
                  <div className="fr-progress">
                    <span>{install.progress ?? "Installing"}</span>
                  </div>
                  {install.log.length > 0 && (
                    <div className="fr-log">
                      {install.log.map((l, i) => (
                        <span key={i}>{l}</span>
                      ))}
                    </div>
                  )}
                </>
              )}
              {!t.path && install?.state === "failed" && <div className="fr-notice fr-notice-fail">{install.error ?? "The installer stopped."} Try again, or run the command yourself.</div>}
              {!t.path && asking === t.command && command && (
                <div className="fr-confirm">
                  <div className="fr-note">Nitely runs this, as you{plan?.elevates ? ", and the installer then asks for administrator rights in a prompt from Windows or macOS" : ""}:</div>
                  <CommandLine command={command} />
                  <div className="fr-actions">
                    <Button
                      variant="primary"
                      onClick={() => {
                        setAsking(null);
                        props.onInstall(t.command);
                      }}
                    >
                      Run it
                    </Button>
                    <Button variant="quiet" onClick={() => setAsking(null)}>
                      Cancel
                    </Button>
                  </div>
                </div>
              )}
              {!t.path && install?.state !== "installing" && asking !== t.command && command && (
                <details className="fr-disclosure" open={install?.state === "failed" || !installs}>
                  <summary>{installs ? "The command it runs" : "Run this yourself"}</summary>
                  <CommandLine command={command} />
                </details>
              )}
              {!t.path && install?.state !== "installing" && asking !== t.command && plan?.elevates && <div className="fr-note">The installer asks for administrator rights; Windows or macOS shows that prompt, not Nitely.</div>}
              {!t.path && plan?.note && <div className="fr-note">{plan.note}</div>}
            </div>
            <div className="fr-row-side">
              {t.path ? (
                <Chip tone="ready">Found</Chip>
              ) : install?.state === "installing" ? (
                <Chip tone="busy">Installing</Chip>
              ) : install?.state === "failed" ? (
                <>
                  <Chip tone="fail">Failed</Chip>
                  {installs && asking !== t.command && <Button onClick={() => setAsking(t.command)}>Try again</Button>}
                </>
              ) : (
                <>
                  <Chip tone="quiet">Not installed</Chip>
                  {installs && asking !== t.command && (
                    <Button variant="primary" onClick={() => setAsking(t.command)}>
                      Install
                    </Button>
                  )}
                </>
              )}
            </div>
          </div>
        );
      })}
    </div>
  );
}

/** Each installed tool's sign-in: signed in, not signed in, or unknown, checked through the tool itself (NIT-690). */
export function SignInStep(props: {
  tools: Tool[] | null;
  onCheck: () => void;
  checking: boolean;
  onSignIn: (command: string) => void;
  /** Tools whose sign-in window was opened and that have not said they are signed in yet. */
  waiting: string[];
  /** Why a sign-in window could not open, by tool. */
  errors: Record<string, string>;
}) {
  const installed = (props.tools ?? []).filter((t) => t.path && t.command !== "git");
  if (!props.tools) return <p className="fr-note">Checking each tool…</p>;
  if (installed.length === 0) return <p className="fr-note">No agent tool is installed yet. Go back to Tools to install Claude Code or Codex.</p>;
  return (
    <>
      <div className="fr-list">
        {installed.map((t) => {
          const waiting = t.signed_in !== true && props.waiting.includes(t.command);
          return (
            <div key={t.command} className="fr-row">
              <div className="fr-row-main">
                <div className="fr-name">
                  <ToolMark command={t.command} />
                  {t.name}
                  {t.command === "gh" && <span className="fr-optional">optional</span>}
                </div>
                <div className="fr-note">
                  {t.signed_in === true
                    ? "Signed in."
                    : t.signed_in === false
                      ? "Not signed in. Sign in opens its own login in a terminal window; come back here when it says you're in."
                      : "Couldn't tell whether it's signed in. Check again, or sign in to be sure."}
                </div>
                {waiting && (
                  <div className="fr-log fr-waiting">
                    <span className="fr-dot" />
                    Waiting for the terminal window. Nitely checks again when you come back.
                  </div>
                )}
                {props.errors[t.command] && <div className="fr-notice fr-notice-fail">{props.errors[t.command]}</div>}
              </div>
              <div className="fr-row-side">
                {t.signed_in === true ? <Chip tone="ready">Signed in</Chip> : t.signed_in === false ? <Chip tone="needs">Not signed in</Chip> : <Chip tone="quiet">Unknown</Chip>}
                {t.signed_in !== true && (
                  <div className="fr-actions">
                    <Button variant="quiet" disabled={props.checking} onClick={props.onCheck}>
                      Check again
                    </Button>
                    <Button variant={t.signed_in === false ? "primary" : "secondary"} onClick={() => props.onSignIn(t.command)}>
                      Sign in
                    </Button>
                  </div>
                )}
              </div>
            </div>
          );
        })}
      </div>
      <div className="fr-notice">You type passwords and codes only into each tool's own window. Nitely runs each tool's status check and never sees a login.</div>
    </>
  );
}
