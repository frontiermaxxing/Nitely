// The window's own title bar, drawn in the app on Windows (src-tauri/tauri.windows.conf.json
// turns the native one off): minimize, maximize or restore, and close at the top right, and a strip
// above the main pane to drag the window by. The sidebar's brand row is the other drag strip.
// Everywhere else (the Mac, which keeps its own bar, and the browser preview) there is none.

import { isTauri } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { useEffect, useState } from "react";

/** `?chrome=win` in the browser preview draws the title bar too, inert, to check how it looks. */
const previewChrome = import.meta.env.DEV && !isTauri() && new URLSearchParams(location.search).get("chrome") === "win";

/** True when the app draws its own title bar: the desktop app on Windows. */
export const ownTitleBar = (isTauri() && /Windows/.test(navigator.userAgent)) || previewChrome;

/** The drag strip and the caption buttons; render once, at the top of the app. */
export function WindowControls() {
  const [maximized, setMaximized] = useState(false);
  const [focused, setFocused] = useState(true);
  useEffect(() => {
    if (!ownTitleBar) return;
    if (previewChrome) {
      const on = () => setFocused(document.hasFocus());
      window.addEventListener("focus", on);
      window.addEventListener("blur", on);
      return () => {
        window.removeEventListener("focus", on);
        window.removeEventListener("blur", on);
      };
    }
    const win = getCurrentWindow();
    const check = () => win.isMaximized().then(setMaximized, () => {});
    check();
    const stops = [win.onResized(check), win.onFocusChanged(({ payload }) => setFocused(payload))];
    return () => stops.forEach((s) => s.then((stop) => stop()));
  }, []);
  if (!ownTitleBar) return null;
  const win = previewChrome ? null : getCurrentWindow();
  return (
    <>
      <div className="titlebar-drag" data-tauri-drag-region aria-hidden="true" />
      <div className={`caption${focused ? "" : " caption-blurred"}`} role="group" aria-label="Window">
        <button className="caption-btn" aria-label="Minimize" title="Minimize" onClick={() => win?.minimize()}>
          <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true">
            <path d="M0 5h10" stroke="currentColor" strokeWidth="1" />
          </svg>
        </button>
        <button className="caption-btn" aria-label={maximized ? "Restore" : "Maximize"} title={maximized ? "Restore Down" : "Maximize"} onClick={() => win?.toggleMaximize()}>
          {maximized ? (
            <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true">
              <path d="M2.5 2.5V.5h7v7h-2M.5 2.5h7v7h-7z" fill="none" stroke="currentColor" strokeWidth="1" />
            </svg>
          ) : (
            <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true">
              <rect x=".5" y=".5" width="9" height="9" fill="none" stroke="currentColor" strokeWidth="1" />
            </svg>
          )}
        </button>
        <button className="caption-btn caption-close" aria-label="Close" title="Close" onClick={() => win?.close()}>
          <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true">
            <path d="M.5.5l9 9M9.5.5l-9 9" stroke="currentColor" strokeWidth="1" />
          </svg>
        </button>
      </div>
    </>
  );
}
