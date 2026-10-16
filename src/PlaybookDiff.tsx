import type { PlaybookProposal } from "./types";

/** A proposed playbook change as a diff: removed lines, added lines, and a gap where unchanged ones are left out. */
export function PlaybookDiff(props: { lines: PlaybookProposal["diff"] }) {
  if (props.lines.length === 0) return <p className="muted small">No change to the playbook as it is now.</p>;
  return (
    <pre className="pb-diff" aria-label="The change to the playbook">
      {props.lines.map((l, i) =>
        l.op === "@" ? (
          <span key={i} className="pb-diff-gap" aria-label="unchanged lines left out">
            ⋯
          </span>
        ) : (
          <span key={i} className={l.op === "+" ? "pb-diff-add" : l.op === "-" ? "pb-diff-del" : undefined}>
            <span className="pb-diff-op" aria-hidden="true">
              {l.op === " " ? " " : l.op}
            </span>
            {l.text || " "}
          </span>
        ),
      )}
    </pre>
  );
}
