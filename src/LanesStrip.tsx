import { useEffect, useState } from "react";
import { invoke } from "./backend";
import { capWords, chipName, chipWords, executorName, laneChips, limitsOf, machineWords, memoryWords, queueWords, remoteRuns, remoteWords, sizeWords, waiting, type LaneChip } from "./lanes";
import type { LaneRun, LanesOverview, MachineNow } from "./types";
import { openExternal } from "./util";
import { StateDot } from "./ui";
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
            title={[lanes.waiting_for_memory, lanes.machine?.below_normal && "Builds, gates, reviews and crew workers run below your own apps."].filter(Boolean).join("\n")}
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
    <div className="settings-project lanes-settings">
      <h3>This machine</h3>
      <p className="settings-why small">
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
      <div className="crew-grid">
        <label className="crew-field" htmlFor="machine-floor">
          <span className="crew-label">Memory floor</span>
          <span className="crew-hint muted">{words.floor ? `now ${words.floor}; ` : ""}0 turns the wait off</span>
        </label>
        <span className="crew-budget">
          <input id="machine-floor" className="mono crew-number" inputMode="decimal" placeholder={gb(l?.floor)} value={floor} onChange={(e) => setFloor(e.target.value)} />
          <span className="muted small">GB</span>
        </span>
        <label className="crew-field" htmlFor="machine-cap">
          <span className="crew-label">Memory cap</span>
          <span className="crew-hint muted">{words.cap ? `now ${words.cap}; ` : ""}empty is the default</span>
        </label>
        <span className="crew-budget">
          <input id="machine-cap" className="mono crew-number" inputMode="decimal" placeholder={gb(l?.cap)} value={cap} onChange={(e) => setCap(e.target.value)} />
          <span className="muted small">GB</span>
        </span>
      </div>
      <div className="settings-row crew-save">
        <button className="btn primary" disabled={busy || typeof parsed === "string"} onClick={save}>
          Save
        </button>
        {typeof parsed === "string" && <span className="error small">{parsed}</span>}
      </div>
      {note && <p className="muted small">{note}</p>}
    </div>
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
        <p className="muted small">
          No project has build lanes yet. A project gets them when its playbook has a <span className="mono">[lanes]</span> table: builds and tests then run in a few warm
          checkouts instead of in every agent's worktree.
        </p>
      </>
    );
  }
  return (
    <>
      {machine}
      <p className="settings-why small">
        Each lane runs one request at a time; requests wait in their project's queue. Change a project's lanes in its playbook's{" "}
        <span className="mono">[lanes]</span> table.
      </p>
      {lanes.projects.map((p) => (
        <div key={`${p.project}-${p.repo}`} className="settings-project lanes-settings">
          <h3>
            {p.project} <span className="muted mono">{p.repo}</span>
          </h3>
          <dl className="lanes-facts">
            <dt>Executor</dt>
            <dd>{executorName(p.rules.executor)}{p.rules.executor === "ci" ? " for gates; ordinary runs use this machine." : p.rules.executor?.startsWith("machine:") ? " — requires permission in this machine’s executor configuration." : ""}</dd>
            <dt>Lanes</dt>
            {p.rules.large_repo ? (
              <dd>
                Your main checkout (large-repo mode). A build runs there only while it is clean and on its integration branch, and puts it back on that branch after;
                nothing in it is stashed, committed or cleaned.
              </dd>
            ) : (
              <dd>
                {p.rules.count} on this machine
                {p.lanes.some((l) => !l.made) && <span className="muted"> ({p.lanes.filter((l) => l.made).length} made so far)</span>}
              </dd>
            )}
            {p.rules.base && p.rules.base !== "tip" && (
              <>
                <dt>New work starts from</dt>
                <dd>the integration branch as it was when the base last moved; it moves {p.rules.base}</dd>
              </>
            )}
            <dt>Keeps between runs</dt>
            <dd className="mono">{p.rules.keep.join(", ") || "nothing"}</dd>
            <dt>Stops a run after</dt>
            <dd>{p.rules.timeout_minutes} minutes</dd>
            <dt>Size cap</dt>
            <dd>
              {capWords(p)}
              <span className="muted">: over it, the incremental caches are emptied; over twice it, the lane builds cold once</span>
              {sizeWords(p).length > 0 && <div className="muted small">{sizeWords(p).join(" · ")}</div>}
            </dd>
            <dt>Runs on</dt>
            <dd>this machine</dd>
            <dt>Lives in</dt>
            <dd className="mono lanes-path">{p.dir}</dd>
          </dl>
        </div>
      ))}
    </>
  );
}
