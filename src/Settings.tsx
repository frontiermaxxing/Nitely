import { useEffect } from "react";
import type { View } from "./App";
import { CrewSettings } from "./CrewSettings";
import { ArchivedProjects } from "./ArchivedProjects";
import { LanesSettings } from "./LanesStrip";
import { LinksSettings } from "./LinksSettings";
import { Network } from "./Network";
import { NotifySettingsPanel } from "./Notifications";
import { PhoneSetup } from "./PhoneSetup";
import { RuleDefaultsCard } from "./Rules";
import type { Board, Project } from "./types";
import { UpdatesSettings, type UpdateStatus } from "./Update";
import { EmptyState, SettingsSection as SettingsGroup } from "./ui";
import "./settings.css";

/** The sections of Settings, in order: each is `#settings/<id>`. */
export const SETTINGS_SECTIONS = [
  { id: "notifications", label: "Notifications" },
  { id: "updates", label: "Updates" },
  { id: "reviews", label: "Review defaults" },
  { id: "crew", label: "Crew" },
  { id: "lanes", label: "Build lanes" },
  { id: "archived", label: "Archived projects" },
  { id: "links", label: "Issue links" },
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
  /** The view as it was asked for: a new one each time, so asking for the same section again scrolls again. */
  asked: View;
  section?: SettingsSection;
  project?: string;
  board: Board | null;
  projects: Project[];
  now: number;
  /** Moves on whenever the data changes. */
  version: number;
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
    // Again when the projects come in, for a project's crew asked for before they had loaded.
  }, [props.asked, props.section, props.project, props.projects.length]);

  return (
    <div className="page settings-page">
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

      <Section id="notifications" label="Notifications" why="When a decision starts waiting on you, Nitely tells you once, outside the window. Several at once come as one.">
        <NotifySettingsPanel />
      </Section>

      <Section id="updates" label="Updates">
        <UpdatesSettings status={props.update} onStatus={props.onUpdate} />
      </Section>

      <Section id="reviews" label="Review defaults">
        <RuleDefaultsCard />
      </Section>

      <Section id="crew" label="Crew" why="The agent workers Nitely runs on each project's Todo issues, one issue each, through the same gate and review as any agent. Running workers always finish their issue; the limits only stop new ones.">
        {props.projects.length === 0 && <EmptyState title="No projects yet.">Add one from the sidebar; its crew is set here.</EmptyState>}
        {props.projects.map((p) => (
          <div key={p.key} className="setting-sub" id={`settings-crew-${p.key}`}>
            <h3>
              {p.name} <span className="muted mono">{p.key}</span>
            </h3>
            <CrewSettings projectKey={p.key} onSaved={props.onChanged} />
          </div>
        ))}
      </Section>

      <Section id="lanes" label="Build lanes" why="Where builds and tests run: a few warm checkouts per project that keep their build output, so agents' worktrees hold source only.">
        <LanesSettings version={props.version} />
      </Section>

      <Section id="archived" label="Archived projects" why="Archived projects are kept out of navigation and combined totals. Restoring brings their data and task selection back.">
        <ArchivedProjects version={props.version} onChanged={props.onChanged} />
      </Section>

      <Section id="links" label="Issue links" why="The links agents write to issues, in chats, pull requests and notes, open the issue here.">
        <LinksSettings version={props.version} />
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
    <SettingsGroup id={`settings-${props.id}`} title={props.label} description={props.why}>
      {props.children}
    </SettingsGroup>
  );
}
