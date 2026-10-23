import { Fragment, type ReactNode } from "react";
import { type Inline, issueKeyOf, parseMarkdown } from "../markdown";
import { openExternal } from "../util";
import "./markdown.css";

export function Markdown(props: { src: string; onOpenIssue?: (key: string) => void }) {
  const inline = (parts: Inline[]): ReactNode =>
    parts.map((p, i) => {
      switch (p.t) {
        case "text":
          return <Fragment key={i}>{p.s}</Fragment>;
        case "code":
          return <code key={i}>{p.s}</code>;
        case "strong":
          return <strong key={i}>{p.s}</strong>;
        case "em":
          return <em key={i}>{p.s}</em>;
        case "link": {
          const issue = issueKeyOf(p.href);
          if (issue && props.onOpenIssue)
            return (
              <button key={i} className="link" onClick={() => props.onOpenIssue!(issue)}>
                {p.s}
              </button>
            );
          if (/^https?:\/\//.test(p.href))
            return (
              <a key={i} href={p.href} onClick={openExternal(p.href)}>
                {p.s}
              </a>
            );
          return <Fragment key={i}>{p.s}</Fragment>;
        }
      }
    });
  return (
    <div className="brief-doc">
      {parseMarkdown(props.src).map((b, i) => {
        switch (b.t) {
          case "h": {
            const H = `h${Math.min(b.level + 1, 6)}` as "h2";
            return <H key={i}>{inline(b.text)}</H>;
          }
          case "p":
            return <p key={i}>{inline(b.text)}</p>;
          case "quote":
            return <blockquote key={i}>{inline(b.text)}</blockquote>;
          case "hr":
            return <hr key={i} />;
          case "code":
            return (
              <pre key={i}>
                <code>{b.s}</code>
              </pre>
            );
          case "list": {
            const L = b.ordered ? "ol" : "ul";
            return (
              <L key={i}>
                {b.items.map((it, j) => (
                  <li key={j} style={it.depth ? { marginLeft: `${it.depth * 1.4}em` } : undefined} className={it.done !== undefined ? "brief-task" : undefined}>
                    {it.done !== undefined && <span className={`ui-check-box${it.done ? " is-on" : ""}`} role="img" aria-label={it.done ? "done" : "open"} />}
                    {inline(it.text)}
                  </li>
                ))}
              </L>
            );
          }
          case "table":
            return (
              <div key={i} className="brief-table">
                <table>
                  <thead>
                    <tr>
                      {b.head.map((c, j) => (
                        <th key={j}>{inline(c)}</th>
                      ))}
                    </tr>
                  </thead>
                  <tbody>
                    {b.rows.map((r, j) => (
                      <tr key={j}>
                        {r.map((c, k) => (
                          <td key={k}>{inline(c)}</td>
                        ))}
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            );
        }
      })}
    </div>
  );
}
