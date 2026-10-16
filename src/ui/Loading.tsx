import { useEffect, useState } from "react";

/**
 * What a view shows while its data is on the way: nothing for the first 300ms, since most reads
 * are faster and a flash of text is worse than a pause, then one quiet line.
 */
export function Loading(props: { what?: string }) {
  const [show, setShow] = useState(false);
  useEffect(() => {
    const t = setTimeout(() => setShow(true), 300);
    return () => clearTimeout(t);
  }, []);
  return show ? (
    <p className="ui-loading" role="status">
      Loading{props.what ? ` ${props.what}` : ""}…
    </p>
  ) : null;
}
