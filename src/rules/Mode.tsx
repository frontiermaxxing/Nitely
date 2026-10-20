import type { RulesView } from "../types";

/** Where the rules live, and why they cannot be edited here when they cannot. */
export function Mode({ view: v }: { view: RulesView }) {
  return (
    <>
      <p className="rules-source">
        {v.source === "repo" ? "Committed in " : v.source === "stored" ? "Kept by Nitely in " : "Nitely's defaults; saving writes "}
        <span className="mono">{v.path}</span>
      </p>
      {v.read_only && <p className="rules-readonly">{v.read_only}</p>}
    </>
  );
}
