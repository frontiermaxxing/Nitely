import { useEffect, useState } from "react";
import { invoke } from "./backend";
import { capWords, chipName, chipWords, executorName, laneChips, limitsOf, machineWords, memoryWords, paceHeadline, queueWords, remoteRuns, remoteWords, sizeWords, waiting, type LaneChip } from "./lanes";
import type { LaneRun, LanesOverview, MachineNow, MemoryTrimSetting } from "./types";
import { openExternal } from "./util";
import { Button, Select, SettingsRow, StateDot, TextField } from "./ui";
import "./styles/lanes.css";

/** Reads every repo's build lanes, again whenever the data changes (`version`). Null until read. */
export function useLanes(version: number): LanesOverview | null {
  const [lanes, setLanes] = useState<LanesOverview | null>(null);
  useEffect(() => {
    let live = true;
    invoke<LanesOverview>("lanes_overview")
      .then((l) => live && setLanes(l))
      .catch(() => live && setLanes(null));
    return () => {
      live = false;
    };
  }, [version]);
  return lanes;
}

/**
 * The build lanes on Now and Overview (docs/build-lanes.md): one chip per lane with its project,
 * what it runs or that it is idle, and each project's queue. Nothing when no repo has lanes.
 */
export function LanesStrip(props: { version: number; now: number; onOpenIssue: (key: string) => void; landQueue?: number }) {
  const lanes = useLanes(props.version);
  if (!lanes || lanes.projects.length === 0) return null;
  return (
    <section className="lanes-strip" aria-label="Build lanes">
      <h2>
        Build lanes
        {machineWords(lanes).length > 0 && (
          <span
            className={`head-note${lanes.waiting_for_memory ? " memory-wait" : ""}`}
            title={[lanes.waiting_for_memory, lanes.machine?.below_normal && "Builds, gates, reviews and crew workers run below your own apps.", paceHeadline(lanes.machine?.pace)].filter(Boolean).join("\n")}
          >
            {machineWords(lanes).join(" · ")}
          </span>
        )}
        {/* The land queue gates its stacks in these lanes (docs/batching.md). */}
        {!!props.landQueue && (
          <span className="head-note" title="Reviewed changes waiting in or landing through a land queue">
            {props.landQueue} in the land queue
          </span>
        )}
      </h2>
      <div className="lanes-chips">
        {lanes.projects.map((p) => {
          const queued = waiting(p);
          const words = queueWords(p, props.now);
          const elsewhere = remoteRuns(p);
          return (
            <div key={`${p.project}-${p.repo}`} className="lanes-project">
              {laneChips(p).map((c) => (
                <Chip key={c.lane} chip={c} now={props.now} onOpenIssue={props.onOpenIssue} />
              ))}
              {queued > 0 && (
                <span className={`lanes-queue${words.long ? " long" : ""}`} title={words.lines.join("\n")}>
                  {words.label}
                </span>
              )}
              {elsewhere.length > 0 && (
                <div className="remote-runs" aria-label={`${p.project} remote requests`}>
                  <span className="remote-runs-label">Other machines &amp; CI</span>
                  {elsewhere.map((run) => <RemoteRun key={run.id} run={run} now={props.now} onOpenIssue={props.onOpenIssue} />)}
                </div>
              )}
            </div>
          );
        })}
      </div>
    </section>
  );
}

function RemoteRun(props: { run: LaneRun; now: number; onOpenIssue: (key: string) => void }) {
  const [expanded, setExpanded] = useState(false);
  const r = props.run;
  const words = remoteWords(r, props.now);
  const caption = `${r.issue ?? `Run ${r.id}`} · ${words.label} · ${words.duration}`;
  return (
    <div className="remote-run">
      <div className={`lane-chip lane-${words.state}`} title={`${r.kind}: ${r.command}`}>
        {words.state === "unhealthy" ? <span className="lane-problem-dot" role="img" aria-label="Failed" /> : <StateDot state={words.state === "running" ? "working" : "ended"} />}
        <span className="lane-name mono">{executorName(r.executor)}</span>
        {r.issue ? <button className="lane-words link" onClick={() => props.onOpenIssue(r.issue!)}>{caption}</button> : <span className="lane-words">{caption}</span>}
        <button className="link remote-details-toggle" aria-label={`Run ${r.id} details`} aria-expanded={expanded} onClick={() => setExpanded(!expanded)}>Details</button>
      </div>
      {expanded && (
        <div className="remote-run-detail">
          <p className="mono">{r.command}</p>
          {words.detail && <pre>{words.detail}</pre>}
          {words.checks && <a href={words.checks} onClick={openExternal(words.checks)}>View CI checks</a>}
          {!words.detail && !words.checks && <p className="muted">No additional result was retained.</p>}
          <p className="muted small">Run {r.id} · {r.kind} · {r.project}</p>
        </div>
      )}
    </div>
  );
}

function Chip(props: { chip: LaneChip; now: number; onOpenIssue: (key: string) => void }) {
  const c = props.chip;
  const issue = c.run?.issue;
  const words = chipWords(c, props.now);
  return (
    <div className={`lane-chip lane-${c.state}`} title={c.state === "running" ? `${c.run!.kind}: ${c.run!.command}` : words}>
      {c.state === "unhealthy" ? (
        <span className="lane-problem-dot" role="img" aria-label="Unhealthy" />
      ) : (
        <StateDot state={c.state === "running" ? "working" : c.state === "idle" ? "idle" : c.state === "waiting" ? "waiting" : "ended"} />
      )}
      <span className="lane-name mono">{chipName(c)}</span>
      {issue ? (
        <button className="lane-words link" onClick={() => props.onOpenIssue(issue)}>
          {words}
        </button>
      ) : (
        <span className="lane-words">{words}</span>
      )}
    </div>
  );
}

/** The machine's memory floor and run cap (NIT-266): what is free now, and a field for each. */
function MachineLimits(props: { machine: MachineNow | undefined; slots: LanesOverview["slots"]; onSaved: () => void }) {
  // Its own copy, refilled from what a save returns: the strip's next read may still be the old one.
  const [machine, setMachine] = useState(props.machine);
  const l = machine?.limits;
  const gb = (bytes: number | undefined) => (bytes == null ? "" : (bytes / 1024 ** 3).toFixed(1));
  const setting = (from: string | undefined, bytes: number | undefined) => (from === "settings" && bytes != null ? String(+(bytes / 1024 ** 3).toFixed(2)) : "");
  const [floor, setFloor] = useState(setting(l?.floor_from, l?.floor));
  const [cap, setCap] = useState(setting(l?.cap_from, l?.cap));
  const [busy, setBusy] = useState(false);
  const [note, setNote] = useState<string | null>(null);
  const words = memoryWords(machine);
  // Live availability follows each read; edits and saved limits keep their local state.
  const free = memoryWords(props.machine).free;
  const parsed = limitsOf(floor, cap);
  const save = async () => {
    if (typeof parsed === "string") return;
    setBusy(true);
    try {
      const now = await invoke<MachineNow>("set_machine_limits", parsed);
      setMachine(now);
      setFloor(setting(now.limits?.floor_from, now.limits?.floor));
      setCap(setting(now.limits?.cap_from, now.limits?.cap));
      setNote("Saved. Requests waiting now use it from their next look.");
      props.onSaved();
    } catch (e) {
      setNote(String(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <div className="setting-sub">
      <h3>This machine</h3>
      <p className="setting-note">
        Builds, gates, reviews and crew workers run at below-normal priority, so your own apps come first. Nothing new starts while free memory is under the floor, and a
        run that goes over its cap is stopped as out of memory in the lane, never taking the machine with it.
        {props.slots && (
          <>
            {" "}
            Up to {props.slots.total} builds run at once ({props.slots.used} now).
          </>
        )}
        {free && <> {free}.</>}
      </p>
      <SettingsRow label="Memory floor" why={`${words.floor ? `Now ${words.floor}. ` : ""}0 turns the wait off.`} htmlFor="machine-floor">
        <span className="setting-inline">
          <TextField id="machine-floor" className="setting-number" mono inputMode="decimal" placeholder={gb(l?.floor)} value={floor} onChange={(e) => setFloor(e.target.value)} />
          <span className="setting-unit">GB</span>
        </span>
      </SettingsRow>
      <SettingsRow label="Memory cap" why={`${words.cap ? `Now ${words.cap}. ` : ""}Empty is the default.`} htmlFor="machine-cap">
        <span className="setting-inline">
          <TextField id="machine-cap" className="setting-number" mono inputMode="decimal" placeholder={gb(l?.cap)} value={cap} onChange={(e) => setCap(e.target.value)} />
          <span className="setting-unit">GB</span>
        </span>
      </SettingsRow>
      <div className="setting-actions">
        <Button variant="primary" disabled={busy || typeof parsed === "string"} onClick={save}>
          Save
        </Button>
        {typeof parsed === "string" && <span className="error">{parsed}</span>}
      </div>
      {note && <p className="muted setting-feedback">{note}</p>}
      <MemoryTrim />
    </div>
  );
}

type TrimMode = MemoryTrimSetting["mode"];

/** Whether Nitely may empty idle programs' working memory while a run waits for memory (NIT-657).
 * It saves as soon as one is picked; on macOS and Linux it is off, with why. */
function MemoryTrim() {
  const [setting, setSetting] = useState<MemoryTrimSetting | null>(null);
  const [note, setNote] = useState<string | null>(null);
  useEffect(() => {
    invoke<MemoryTrimSetting>("memory_trim_setting").then(setSetting, () => setSetting(null));
  }, []);
  if (!setting) return null;
  const choose = async (mode: TrimMode | null) => {
    try {
      setSetting(await invoke<MemoryTrimSetting>("set_memory_trim", { mode }));
      setNote(null);
    } catch (e) {
      setNote(String(e));
    }
  };
  const why = setting.available
    ? "When a run has waited two minutes for memory, Nitely empties the working memory of your programs that sat idle while it waited, at most every ten minutes. They read it back when you use them again. Never the window you're in, and no admin rights."
    : setting.why;
  return (
    <>
      <SettingsRow label="Free idle programs' memory" why={why}>
        <Select<TrimMode>
          aria-label="Free idle programs' memory"
          value={setting.mode === "away" ? null : setting.mode}
          defaultValue="away"
          disabled={!setting.available}
          onChange={choose}
          options={[
            { value: "off", label: "Off" },
            { value: "away", label: "When I'm away", detail: "No input for ten minutes" },
            { value: "always", label: "Whenever a run waits" },
          ]}
        />
      </SettingsRow>
      {note && <p className="error setting-feedback">{note}</p>}
    </>
  );
}

/** Settings' Build lanes section: the machine's limits, then each repo's lanes as its playbook sets
 * them, and where they live. */
export function LanesSettings(props: { version: number }) {
  const [saved, setSaved] = useState(0);
  const lanes = useLanes(props.version + saved);
  if (!lanes) return null;
  const machine = <MachineLimits machine={lanes.machine} slots={lanes.slots} onSaved={() => setSaved((n) => n + 1)} />;
  if (lanes.projects.length === 0) {
    return (
      <>
        {machine}
        <p className="setting-note">
          No project has build lanes yet. A project gets them when its playbook has a <span className="mono">[lanes]</span> table: builds and tests then run in a few warm
          checkouts instead of in every agent's worktree.
        </p>
      </>
    );
  }
  return (
    <>
      {machine}
      <p className="setting-note">
        Each lane runs one request at a time; requests wait in their project's queue. Change a project's lanes in its playbook's{" "}
        <span className="mono">[lanes]</span> table.
      </p>
      {lanes.projects.map((p) => (
        <div key={`${p.project}-${p.repo}`} className="setting-sub">
          <h3>
            {p.project} <span className="muted mono">{p.repo}</span>
          </h3>
          <Fact label="Executor">
            {executorName(p.rules.executor)}
            {p.rules.executor === "ci" ? " for gates; ordinary runs use this machine." : p.rules.executor?.startsWith("machine:") ? ", which needs permission in this machine’s executor configuration." : ""}
          </Fact>
          <Fact label="Lanes">
            {p.rules.large_repo ? (
              <>
                Your main checkout (large-repo mode). A build runs there only while it is clean and on its integration branch, and puts it back on that branch after;
                nothing in it is stashed, committed or cleaned.
              </>
            ) : (
              <>
                {p.rules.count} on this machine
                {p.lanes.some((l) => !l.made) && <span className="muted"> ({p.lanes.filter((l) => l.made).length} made so far)</span>}
              </>
            )}
          </Fact>
          {p.rules.base && p.rules.base !== "tip" && <Fact label="New work starts from">the integration branch as it was when the base last moved; it moves {p.rules.base}</Fact>}
          <Fact label="Keeps between runs">
            <span className="mono">{p.rules.keep.join(", ") || "nothing"}</span>
          </Fact>
          <Fact label="Stops a run after">{p.rules.timeout_minutes} minutes</Fact>
          <Fact label="Size cap" why="Over it, the incremental caches are emptied; over twice it, the lane builds cold once.">
            {capWords(p)}
            {sizeWords(p).length > 0 && <span className="muted"> · {sizeWords(p).join(" · ")}</span>}
          </Fact>
          <Fact label="Runs on">this machine</Fact>
          <Fact label="Lives in">
            <span className="mono">{p.dir}</span>
          </Fact>
        </div>
      ))}
    </>
  );
}

/** One fact of a project's lanes, read from its playbook: changed there, not here. */
function Fact(props: { label: string; why?: string; children: React.ReactNode }) {
  return (
    <SettingsRow label={props.label} why={props.why}>
      <span className="setting-value">{props.children}</span>
    </SettingsRow>
  );
}
