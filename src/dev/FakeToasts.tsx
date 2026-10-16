// The browser preview's stand-in for the OS notifications (src/Notifications.tsx): each one the app
// would show appears in the corner for a while, and a click acts like a click on the real toast.

import { useEffect, useState } from "react";

interface Toast {
  n: number;
  title: string;
  body: string;
  decision: string | null;
}

export function FakeToasts() {
  const [toasts, setToasts] = useState<Toast[]>([]);
  useEffect(() => {
    let n = 0;
    const on = (e: Event) => {
      const t = { ...(e as CustomEvent<Omit<Toast, "n">>).detail, n: ++n };
      setToasts((all) => [...all, t]);
      setTimeout(() => setToasts((all) => all.filter((x) => x.n !== t.n)), 8000);
    };
    window.addEventListener("nitely:preview-notification", on);
    return () => window.removeEventListener("nitely:preview-notification", on);
  }, []);
  if (toasts.length === 0) return null;
  return (
    <div className="fake-toasts" aria-live="polite">
      {toasts.map((t) => (
        <button
          key={t.n}
          className="fake-toast"
          onClick={() => {
            window.dispatchEvent(new CustomEvent("nitely://open-decision", { detail: t.decision ?? "" }));
            setToasts((all) => all.filter((x) => x.n !== t.n));
          }}
        >
          <span className="fake-toast-app">Nitely · preview notification</span>
          <strong>{t.title}</strong>
          <span className="fake-toast-body">{t.body}</span>
        </button>
      ))}
    </div>
  );
}
