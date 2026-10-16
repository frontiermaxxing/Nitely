import type { IssueRecord } from "../types";
import { openExternal } from "../util";
import type { Track } from "./lifecycle";
import { describeTrack, trackOf } from "./lifecycle";
import { StationGlyph } from "./StationGlyph";

/**
 * An issue, everywhere it appears: its station glyph and key, optionally its title and kind.
 * Native issues open in the panel through `onOpen`; Linear ones open in the browser.
 */
export function IssueToken(props: { issue: IssueRecord; track?: Track; title?: boolean; onOpen?: (key: string) => void }) {
  const { issue } = props;
  const track = props.track ?? trackOf(issue);
  const hover = [issue.key, describeTrack(track), issue.kind].filter(Boolean).join(" · ");
  const body = (
    <>
      <StationGlyph track={track} />
      <span className="ui-token-key">{issue.key}</span>
      {props.title && <span className="ui-token-title">{issue.title}</span>}
      {props.title && issue.kind && <span className="ui-token-kind">{issue.kind}</span>}
    </>
  );
  const quiet = track.closed || track.station === "done";
  const cls = `ui-token${quiet ? " ui-token-quiet" : ""}${props.title ? " ui-token-wide" : ""}`;
  if (issue.origin !== "nitely" && issue.url) {
    return (
      <a className={cls} href={issue.url} onClick={openExternal(issue.url)} title={`${hover} · opens in ${issue.origin}`}>
        {body}
      </a>
    );
  }
  if (props.onOpen) {
    const open = props.onOpen;
    return (
      <button type="button" className={cls} onClick={() => open(issue.key)} title={hover}>
        {body}
      </button>
    );
  }
  return (
    <span className={cls} title={hover}>
      {body}
    </span>
  );
}
