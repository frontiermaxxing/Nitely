import { useState } from "react";
import { Button } from "../ui";

/** The three skills to ask an agent for, each with a prompt to paste (NIT-690). */
export function skills(projectKey: string) {
  return [
    {
      name: "/nitely",
      what: "How an agent takes a task, reports as it works, and hands its change in to be checked and landed.",
      when: "Every time an agent works in a project Nitely tracks. It loads on its own once the agent is connected.",
      prompt: `Take the next task in ${projectKey} with /nitely and work it through to landing.`,
    },
    {
      name: "/nitely-setup",
      what: "Reads the project once and fills its board: issues, a short brief for agents, and review rules to approve.",
      when: "Once, right after you add a project.",
      prompt: `Run /nitely-setup for ${projectKey}.`,
    },
    {
      name: "/nitely-test-optimize",
      what: "Measures the tests, flags the slow and flaky ones, and proposes one goal to fix them for you to approve.",
      when: "When the tests get slow or flaky.",
      prompt: `Run /nitely-test-optimize on ${projectKey}'s tests.`,
    },
  ];
}

export function SkillsStep(props: { projectKey: string }) {
  const [copied, setCopied] = useState<string | null>(null);
  return (
    <>
      <div className="fr-grid3">
        {skills(props.projectKey).map((k) => (
          <div key={k.name} className="fr-card">
            <div className="fr-skill-name">{k.name}</div>
            <div className="fr-skill-what">{k.what}</div>
            <div className="fr-skill-when">
              <span>When</span>
              <span className="fr-note">{k.when}</span>
            </div>
            <div className="fr-log fr-prompt">{k.prompt}</div>
            <div className="fr-actions">
              <Button
                variant={copied === k.name ? "quiet" : "secondary"}
                className={copied === k.name ? "fr-copied" : undefined}
                onClick={() => {
                  navigator.clipboard?.writeText(k.prompt).catch(() => {});
                  setCopied(k.name);
                }}
              >
                {copied === k.name ? "Copied" : "Copy prompt"}
              </Button>
              {copied === k.name && <span className="fr-hint">Paste it into Claude Code or Codex</span>}
            </div>
          </div>
        ))}
      </div>
      <div className="fr-note">Paste a prompt into Claude Code or Codex in the project's folder.</div>
    </>
  );
}
