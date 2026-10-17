import { parseInline } from "../markdown";

/**
 * An agent's line of text with its inline markdown shown, not printed: code, bold, italics, and
 * links as their text. The phone opens no links: an issue link (nitely://) reads as its key, and
 * a web link as its words, so nothing on the phone leads away from the decision.
 */
export function Rich(props: { text: string }) {
  // One element, so a flex row that holds it (a finding beside its severity mark) keeps it one sentence.
  return (
    <span className="ph-rich">
      {parseInline(props.text).map((p, i) => {
        switch (p.t) {
          case "code":
            return <code key={i}>{p.s}</code>;
          case "strong":
            return <strong key={i}>{p.s}</strong>;
          case "em":
            return <em key={i}>{p.s}</em>;
          case "link":
            return p.href.startsWith("nitely://") ? (
              <span key={i} className="ph-key">
                {p.s}
              </span>
            ) : (
              <span key={i}>{p.s}</span>
            );
          default:
            return <span key={i}>{p.s}</span>;
        }
      })}
    </span>
  );
}
