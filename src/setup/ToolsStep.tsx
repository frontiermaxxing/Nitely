import { useState } from "react";
import type { Tool } from "../types";
import { Button } from "../ui";
import { installCommand, platformOf, signInCommand } from "./steps";

/**
 * An install in progress or one that stopped, as the installer reports it (NIT-688 fills these in;
 * until then the command is shown to run by hand).
 */
export interface InstallState {
  state: "installing" | "failed";
  /** "Installing with npm · 38 s". */
  progress?: string;
  /** 0 to 1, when the installer says. */
  fraction?: number;
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
export function ToolsStep(props: { tools: Tool[] | null; installs: Record<string, InstallState>; onInstall?: (command: string) => void; onCancel?: (command: string) => void }) {
  const platform = platformOf(navigator.userAgent);
  if (!props.tools) return <p className="fr-note">Looking for the tools on this machine…</p>;
  return (
    <div className="fr-list">
      {props.tools.map((t) => {
        const install = props.installs[t.command];
        const command = installCommand(t.command, platform);
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
                  {install.fraction !== undefined && (
                    <div className="fr-bar">
                      <div style={{ width: `${Math.round(install.fraction * 100)}%` }} />
                    </div>
                  )}
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
              {!t.path && install?.state !== "installing" && command && (
                <details className="fr-disclosure" open={install?.state === "failed"}>
                  <summary>The command it runs</summary>
                  <CommandLine command={command} />
                </details>
              )}
            </div>
            <div className="fr-row-side">
              {t.path ? (
                <Chip tone="ready">Found</Chip>
              ) : install?.state === "installing" ? (
                <>
                  <Chip tone="busy">Installing</Chip>
                  {props.onCancel && (
                    <Button variant="quiet" onClick={() => props.onCancel!(t.command)}>
                      Cancel
                    </Button>
                  )}
                </>
              ) : install?.state === "failed" ? (
                <>
                  <Chip tone="fail">Failed</Chip>
                  {props.onInstall && <Button onClick={() => props.onInstall!(t.command)}>Try again</Button>}
                </>
              ) : (
                <>
                  <Chip tone="quiet">Not installed</Chip>
                  {props.onInstall && (
                    <Button variant="primary" onClick={() => props.onInstall!(t.command)}>
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
export function SignInStep(props: { tools: Tool[] | null; onCheck: () => void; checking: boolean; onSignIn?: (command: string) => void }) {
  const installed = (props.tools ?? []).filter((t) => t.path && t.command !== "git");
  if (!props.tools) return <p className="fr-note">Checking each tool…</p>;
  if (installed.length === 0) return <p className="fr-note">No agent tool is installed yet. Go back to Tools to install Claude Code or Codex.</p>;
  return (
    <>
      <div className="fr-list">
        {installed.map((t) => {
          const command = signInCommand(t.command);
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
                {t.signed_in !== true && command && !props.onSignIn && <CommandLine command={command} />}
              </div>
              <div className="fr-row-side">
                {t.signed_in === true ? <Chip tone="ready">Signed in</Chip> : t.signed_in === false ? <Chip tone="needs">Not signed in</Chip> : <Chip tone="quiet">Unknown</Chip>}
                {t.signed_in !== true && (
                  <div className="fr-actions">
                    <Button variant="quiet" disabled={props.checking} onClick={props.onCheck}>
                      Check again
                    </Button>
                    {props.onSignIn && (
                      <Button variant={t.signed_in === false ? "primary" : "secondary"} onClick={() => props.onSignIn!(t.command)}>
                        Sign in
                      </Button>
                    )}
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
