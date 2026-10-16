/** A run's result as a mark: a quiet tick when it passed, a red cross when it failed, a dashed ring while it runs. */
export function ResultMark(props: { result: "pass" | "fail" | "pending"; title?: string; size?: number }) {
  const size = props.size ?? 12;
  return (
    <svg className={`ui-result ui-result-${props.result}`} width={size} height={size} viewBox="0 0 12 12" role={props.title ? "img" : undefined} aria-label={props.title} aria-hidden={props.title ? undefined : true}>
      {props.title && <title>{props.title}</title>}
      {props.result === "pass" && <path d="M2.5 6.3 5 8.6 9.5 3.6" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round" />}
      {props.result === "fail" && <path d="M3 3l6 6M9 3 3 9" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" />}
      {props.result === "pending" && <circle cx="6" cy="6" r="3.8" fill="none" stroke="currentColor" strokeWidth="1.5" strokeDasharray="4 2.2" />}
    </svg>
  );
}
