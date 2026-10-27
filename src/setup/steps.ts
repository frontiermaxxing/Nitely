/**
 * The first-run setup's rules (NIT-690), as plain data in and out: which step is done, which needs
 * the person, where the flow opens, whether Continue may go on, and when the app opens it by itself.
 * The steps themselves render from these, and the tests check them here.
 */
import type { AgentPlan, Tool } from "../types.ts";

export const STEPS = [
  { id: "welcome", name: "Welcome", sub: "What this sets up" },
  { id: "tools", name: "Tools", sub: "git, Claude Code, Codex, GitHub CLI" },
  { id: "signin", name: "Sign in", sub: "In each tool's own window" },
  { id: "project", name: "First project", sub: "A folder Nitely tracks" },
  { id: "agents", name: "Agents", sub: "Connect them to Nitely" },
  { id: "skills", name: "Skills", sub: "What to ask them for" },
] as const;

export type StepId = (typeof STEPS)[number]["id"];

/** Done, needs the person, or not yet. The step open is shown as current on top of this. */
export type StepState = "done" | "needs" | "todo";

/** What the flow knows: the tools as `tools_check` found them, the projects, the agents' setup, and the steps seen. */
export interface SetupFacts {
  tools: Tool[] | null;
  projects: { key: string; name: string }[];
  agents: AgentPlan[] | null;
  /** Steps the person has been through: Welcome and Skills are done once seen. */
  seen: StepId[];
}

const AGENT_TOOLS = ["claude", "codex"];

const found = (tools: Tool[] | null, command: string) => !!tools?.find((t) => t.command === command)?.path;

/** git, and Claude Code or Codex: what every step after Tools needs. */
export function toolsReady(tools: Tool[] | null): boolean {
  return found(tools, "git") && AGENT_TOOLS.some((c) => found(tools, c));
}

/** The agent tools installed here, and whether each says it is signed in (null: it can't say). */
function agentSignIns(tools: Tool[] | null): (boolean | null)[] {
  return (tools ?? []).filter((t) => AGENT_TOOLS.includes(t.command) && t.path).map((t) => t.signed_in);
}

/** The agents found here that still have changes to make (Agent setup's pending list). */
function agentsPending(agents: AgentPlan[]): number {
  return agents.filter((a) => a.found).reduce((n, a) => n + a.changes.filter((c) => c.action !== "unchanged" && c.action !== "manual").length, 0);
}

export function stepState(id: StepId, f: SetupFacts): StepState {
  switch (id) {
    case "welcome":
    case "skills":
      return f.seen.includes(id) ? "done" : "todo";
    case "tools":
      if (f.tools === null) return "todo";
      return toolsReady(f.tools) ? "done" : "needs";
    case "signin": {
      const signins = agentSignIns(f.tools);
      if (signins.length === 0) return "todo";
      // An installed agent that says it is signed out needs the person; one that can't say does not.
      if (signins.some((s) => s === false)) return "needs";
      return signins.some((s) => s === true) ? "done" : "todo";
    }
    case "project":
      return f.projects.length > 0 ? "done" : "todo";
    case "agents": {
      if (f.agents === null || !f.agents.some((a) => a.found)) return "todo";
      return agentsPending(f.agents) === 0 ? "done" : "todo";
    }
  }
}

export function stepStates(f: SetupFacts): Record<StepId, StepState> {
  return Object.fromEntries(STEPS.map((s) => [s.id, stepState(s.id, f)])) as Record<StepId, StepState>;
}

/**
 * Where the flow opens: from Settings, on the first step not done (done steps stay done); on
 * first launch, where the person left it, else at the start.
 */
export function openingStep(f: SetupFacts, reopened: boolean, left: StepId | null): StepId {
  // Reopened, the person has been through Welcome: it never holds the opening step.
  if (reopened) return STEPS.find((s) => s.id !== "welcome" && stepState(s.id, f) !== "done")?.id ?? "skills";
  return left ?? "welcome";
}

/** Whether Continue may go on from `id`: Tools waits for git and one agent, First project for a project; the rest never hold. */
export function canContinue(id: StepId, f: SetupFacts): boolean {
  if (id === "tools") return toolsReady(f.tools);
  if (id === "project") return f.projects.length > 0;
  return true;
}

/**
 * Whether the app opens the setup by itself on launch: when no project is connected or a needed
 * tool is missing, and the person has not closed it. Not before the tools were checked, so a
 * machine that has everything never sees it flash.
 */
export function opensOnLaunch(tools: Tool[] | null, projects: number, closed: boolean): boolean {
  if (closed || tools === null) return false;
  return projects === 0 || !toolsReady(tools);
}

/** The line under a done step in the rail, from what it found: versions, the project, the agents. */
export function stepSummary(id: StepId, f: SetupFacts): string | null {
  const tools = f.tools ?? [];
  if (id === "tools" && toolsReady(f.tools)) {
    return tools
      .filter((t) => t.path && t.command !== "gh")
      .map((t) => `${t.command === "git" ? "git" : t.name}${t.version ? ` ${t.version.split(".").slice(0, 2).join(".")}` : ""}`)
      .join(" · ");
  }
  if (id === "signin") {
    const out = tools.filter((t) => AGENT_TOOLS.includes(t.command) && t.path && t.signed_in === false).map((t) => t.name);
    if (out.length) return `${out.join(" and ")} ${out.length === 1 ? "isn't" : "aren't"} signed in`;
  }
  if (id === "project" && f.projects.length) {
    const p = f.projects[0];
    return f.projects.length === 1 ? `${p.name} · ${p.key}` : `${p.name} and ${f.projects.length - 1} more`;
  }
  if (id === "agents" && stepState("agents", f) === "done") {
    return listWords((f.agents ?? []).filter((a) => a.found).map((a) => a.agent));
  }
  return null;
}

/** "A", "A and B", "A, B and C". */
export function listWords(items: string[]): string {
  return items.length <= 2 ? items.join(" and ") : `${items.slice(0, -1).join(", ")} and ${items[items.length - 1]}`;
}

/** The command that signs a tool in, in its own window: what Sign in runs. Claude Code asks to log in when it starts signed out. */
export function signInCommand(command: string): string | null {
  return ({ claude: "claude", codex: "codex login", gh: "gh auth login" } as Record<string, string>)[command] ?? null;
}
