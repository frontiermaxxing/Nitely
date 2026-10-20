import type { RulesDraft } from "./shared";
import { waitsSummary } from "./shared";

/** How a change lands under the draft, in one line: what waits for you. */
export function LandsStrip({ draft, landBy, crossVendor }: Pick<RulesDraft, "draft" | "landBy" | "crossVendor">) {
  return <p className="rules-summary">{waitsSummary(draft, landBy, crossVendor)}</p>;
}
