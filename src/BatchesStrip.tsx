import { useEffect, useState } from "react";
import { invoke } from "./backend";
import { batchRows } from "./batches";
import type { BatchesOverview } from "./types";
import { StateDot } from "./ui";
import { ago } from "./util";
import "./styles/batches.css";

/** Reads the review batches and land queues going now, again whenever the data changes (`version`). */
export function useBatches(version: number): BatchesOverview | null {
  const [batches, setBatches] = useState<BatchesOverview | null>(null);
  useEffect(() => {
    let live = true;
    invoke<BatchesOverview>("batches_overview")
      .then((b) => live && setBatches(b))
      .catch(() => live && setBatches(null));
    return () => {
      live = false;
    };
  }, [version]);
  return batches;
}

/**
 * The batches running now, on the board (docs/batching.md): one row each, "Review batch · 3
 * changes" or "Landing 3 changes", with its issues and its step. A shadow batch only records, so
 * its row says so and stays quiet. Nothing when no batch runs.
 */
export function BatchesStrip(props: { batches: BatchesOverview | null; now: number; onOpenIssue: (key: string) => void }) {
  const rows = batchRows(props.batches);
  if (rows.length === 0) return null;
  return (
    <section className="batches-strip" aria-label="Batches">
      <h2>Batches</h2>
      <div className="batch-rows">
        {rows.map((r) => (
          <div key={r.id} className={`batch-row batch-${r.kind}`}>
            <StateDot state={r.kind === "shadow" ? "idle" : "working"} title={r.kind === "shadow" ? "Recorded only: each change's own review decided it" : "Running"} />
            <span className="batch-title">
              {r.title}
              {r.repo && <span className="muted"> · {r.repo}</span>}
            </span>
            <span className="batch-keys">
              {r.keys.map((k) => (
                <button key={k} className="link mono" onClick={() => props.onOpenIssue(k)}>
                  {k}
                </button>
              ))}
            </span>
            <span className="batch-step">{r.step}</span>
            <span className="batch-age muted" title={`Started ${new Date(r.since * 1000).toLocaleString()}`}>
              {ago(props.now, r.since).replace(" ago", "")}
            </span>
          </div>
        ))}
      </div>
    </section>
  );
}
