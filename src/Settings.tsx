import { useEffect } from "react";
import type { View } from "./App";
import { CrewSettings } from "./CrewSettings";
import { Network } from "./Network";
import { NotifySettingsPanel } from "./Notifications";
import { PhoneSetup } from "./PhoneSetup";
import { RuleDefaultsCard } from "./Rules";
import type { Board, Project } from "./types";
import { UpdatesCard, type UpdateStatus } from "./Update";
import { EmptyState } from "./ui";

/** The sections of Settings, in order: each is `#settings/<id>`. */
export const SETTINGS_SECTIONS = [
  { id: "notifications", label: "Notifications" },
  { id: "updates", label: "Updates" },
  { id: "reviews", label: "Review defaults" },
  { id: "crew", label: "Crew" },
  { id: "phone", label: "Phone" },
  { id: "machines", label: "Other machines" },
] as const;

export type SettingsSection = (typeof SETTINGS_SECTIONS)[number]["id"];

export const isSettingsSection = (s: string | undefined): s is SettingsSection => SETTINGS_SECTIONS.some((x) => x.id === s);

/**
 * Every app-wide setting in one place (docs/visualization.md, "Settings"): what tells you,
 * updates, the review and landing defaults, each project's crew, the phone and other machines.
 * A project's own settings (its name, key, private terms, mirror) stay on its Settings tab. Opened
 * on a section (`#settings/crew/NIT` for one project's crew), it scrolls there.
 */
export function Settings(props: {
  section?: SettingsSection;
  project?: string;
  board: Board | null;
  projects: Project[];
  now: number;
  update: UpdateStatus | null;
  onUpdate: (s: UpdateStatus) => void;
  onGo: (view: View) => void;
  onChanged: () => void;
}) {
  // To the section (or the project in it) asked for, each time it is asked for.
  useEffect(() => {
    if (!props.section) return;
    const id = props.project ? `settings-crew-${props.project}` : `settings-${props.section}`;
    // The sections above it fill in as their reads come back and push it down, so it is brought
    // back into view a few times while they load; once the person scrolls, it is theirs.
    const timers = [50, 300, 800, 1600].map((ms) => window.setTimeout(() => document.getElementById(id)?.scrollIntoView({ block: "start" }), ms));
    const stop = () => timers.forEach((t) => window.clearTimeout(t));
    const events = ["wheel", "touchstart", "keydown", "mousedown"];
    events.forEach((e) => window.addEventListener(e, stop, { once: true, passive: true }));
    return () => {
      stop();
      events.forEach((e) => window.removeEventListener(e, stop));
    };
  }, [props.section, props.project]);

  return (
    <div className="page page-form settings-page">
      <div className="page-head">
        <h1>Settings</h1>
      </div>
      <p className="page-lead">What applies to all of Nitely on this machine. A project's own settings (its name, key, private terms and mirror) are on its Settings tab.</p>
      <nav className="settings-index" aria-label="Settings sections">
        {SETTINGS_SECTIONS.map((s) => (
          <button key={s.id} className={`link${props.section === s.id ? " on" : ""}`} onClick={() => props.onGo({ kind: "settings", section: s.id })}>
            {s.label}
          </button>
        ))}
      </nav>

      <Section id="notifications" label="Notifications">
        <NotifySettingsPanel />
      </Section>

      <Section id="updates" label="Updates">
        <UpdatesCard status={props.update} onStatus={props.onUpdate} titled={false} />
      </Section>

      <Section id="reviews" label="Review defaults">
        <RuleDefaultsCard titled={false} />
      </Section>

      <Section id="crew" label="Crew" why="The agent workers Nitely runs on each project's Todo issues: how many, which agents, and their limits.">
        {props.projects.length === 0 && <EmptyState title="No projects yet.">Add one from the sidebar; its crew is set here.</EmptyState>}
        {props.projects.map((p) => (
          <div key={p.key} className="settings-project" id={`settings-crew-${p.key}`}>
            <h3>
              {p.name} <span className="muted mono">{p.key}</span>
            </h3>
            <CrewSettings projectKey={p.key} onSaved={props.onChanged} />
          </div>
        ))}
      </Section>

      <Section id="phone" label="Phone" why="Answer the agents' questions and approve their changes from your phone. A paired phone can do that and nothing else, and every approval it gives is signed with a key that never leaves it.">
        <PhoneSetup now={props.now} />
      </Section>

      <Section id="machines" label="Other machines" why={<MachinesWhy />}>
        <Network board={props.board} projects={props.projects} now={props.now} />
      </Section>
    </div>
  );
}

function MachinesWhy() {
  return (
    <>
      Agents on another machine on this network (a Mac beside this PC, say) reach this backlog through <span className="mono">nitely serve</span> here. Each
      machine gets its own token; its agents show as <span className="mono">claude@name</span>, file and take issues, and never approve.
    </>
  );
}

function Section(props: { id: SettingsSection; label: string; why?: React.ReactNode; children: React.ReactNode }) {
  return (
    <section className="settings-section" id={`settings-${props.id}`} aria-label={props.label}>
      <h2 className="settings-section-title">{props.label}</h2>
      {props.why && <p className="settings-why">{props.why}</p>}
      {props.children}
    </section>
  );
}
