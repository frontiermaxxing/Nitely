import { useEffect, useState } from "react";
import { invoke } from "./backend";
import { capWords, chipName, chipWords, laneChips, sizeWords, waiting, type LaneChip } from "./lanes";
import type { LanesOverview } from "./types";
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
export function LanesStrip(props: { version: number; now: number; onOpenIssue: (key: string) => void }) {
  const lanes = useLanes(props.version);
  if (!lanes || lanes.projects.length === 0) return null;
  return (
    <section className="lanes-strip" aria-label="Build lanes">
      <h2>
        Build lanes
        {lanes.slots && (
          <span className="head-note">
            builds {lanes.slots.used} of {lanes.slots.total}
          </span>
        )}
      </h2>
      <div className="lanes-chips">
        {lanes.projects.map((p) => {
          const queued = waiting(p);
          return (
            <div key={`${p.project}-${p.repo}`} className="lanes-project">
              {laneChips(p).map((c) => (
                <Chip key={c.lane} chip={c} now={props.now} onOpenIssue={props.onOpenIssue} />
              ))}
              {queued > 0 && (
                <span className="lanes-queue" title={p.queue.filter((r) => r.state === "queued").map((r) => `${r.issue ?? "no issue"}: ${r.command}`).join("\n")}>
                  {queued} waiting
                </span>
              )}
            </div>
          );
        })}
      </div>
    </section>
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

/** Settings' Build lanes section: each repo's lanes as its playbook sets them, and where they live. */
export function LanesSettings(props: { version: number }) {
  const lanes = useLanes(props.version);
  if (!lanes) return null;
  if (lanes.projects.length === 0) {
    return (
      <p className="muted small">
        No project has build lanes yet. A project gets them when its playbook has a <span className="mono">[lanes]</span> table: builds and tests then run in a few warm
        checkouts instead of in every agent's worktree.
      </p>
    );
  }
  return (
    <>
      <p className="settings-why small">
        {lanes.slots ? (
          <>
            This machine runs up to {lanes.slots.total} builds at once ({lanes.slots.used} now).
          </>
        ) : (
          <>Each lane runs one request at a time; requests wait in their project's queue, oldest first.</>
        )}{" "}
        Change a project's lanes in its playbook's <span className="mono">[lanes]</span> table.
      </p>
      {lanes.projects.map((p) => (
        <div key={`${p.project}-${p.repo}`} className="settings-project lanes-settings">
          <h3>
            {p.project} <span className="muted mono">{p.repo}</span>
          </h3>
          <dl className="lanes-facts">
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
