import { useEffect, useState } from "react";
import { invoke } from "../backend";
import { SettingsSection } from "../ui";

/** One area of `nitely review-precision` (NIT-316): what became of the P0/P1s raised there. */
export interface PrecisionArea {
  area: string;
  raised: number;
  fixed: number;
  accepted: number;
  stale: number;
  open: number;
  /** fixed / (fixed + accepted + stale); null while none is settled. */
  precision: number | null;
}

const DAYS = 30;

/**
 * What became of each P0/P1 the reviews raised over the last 30 days, per area of the code: an
 * area with many accepted or stale findings is one the reviewer is too strict about. Read-only;
 * reviews of the two worst areas are shown their examples.
 */
export function ReviewerPrecision(props: { project: string }) {
  const [areas, setAreas] = useState<PrecisionArea[] | null>(null);
  useEffect(() => {
    let current = true;
    setAreas(null);
    invoke<{ areas: PrecisionArea[] }>("review_precision", { project: props.project, days: DAYS })
      .then((r) => current && setAreas(r.areas))
      .catch(() => current && setAreas([]));
    return () => {
      current = false;
    };
  }, [props.project]);
  if (!areas || areas.length === 0) return null;
  return (
    <SettingsSection
      title="Reviewer precision"
      description={`What became of each P0 and P1 raised in the last ${DAYS} days, by the file it cites. Precision is fixed out of fixed, accepted and stale.`}
    >
      <div className="rules-table" role="table" aria-label="Reviewer precision by area">
        <div className="rules-table-head rules-table-precision" role="row">
          <span role="columnheader">Area</span>
          <span role="columnheader">Raised</span>
          <span role="columnheader">Fixed</span>
          <span role="columnheader">Accepted</span>
          <span role="columnheader">Stale</span>
          <span role="columnheader">Open</span>
          <span role="columnheader">Precision</span>
        </div>
        {areas.map((a) => (
          <div className="rules-table-row rules-table-precision" role="row" key={a.area}>
            <span className="mono" role="cell">{a.area}</span>
            <span role="cell">{a.raised}</span>
            <span role="cell">{a.fixed}</span>
            <span role="cell">{a.accepted}</span>
            <span role="cell">{a.stale}</span>
            <span role="cell">{a.open}</span>
            <span role="cell">{a.precision === null ? "–" : `${Math.round(a.precision * 100)}%`}</span>
          </div>
        ))}
      </div>
    </SettingsSection>
  );
}
