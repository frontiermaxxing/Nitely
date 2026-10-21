// The window's own title bar, drawn in the app on Windows (src-tauri/tauri.windows.conf.json
// turns the native one off): minimize, maximize or restore, and close at the top right, and a strip
// above the main pane to drag the window by. The sidebar's brand row is the other drag strip.
// On the Mac the native bar is hidden behind its traffic lights (src-tauri/tauri.macos.conf.json),
// which stay: the app draws no buttons there, only the drag strips, and leaves the lights room.
// The browser preview has none.

import { isTauri } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { useEffect, useState } from "react";

/** `?chrome=win` or `?chrome=mac` in the browser preview draws that title bar too, inert, to check how it looks. */
const previewChrome = import.meta.env.DEV && !isTauri() ? new URLSearchParams(location.search).get("chrome") : null;

/** True when the app draws its own title bar: the desktop app on Windows. */
export const ownTitleBar = (isTauri() && /Windows/.test(navigator.userAgent)) || previewChrome === "win";

/** True when the Mac's traffic lights sit over the app's top left: the desktop app on the Mac. */
export const macTitleBar = (isTauri() && /Macintosh/.test(navigator.userAgent)) || previewChrome === "mac";

/** True when the window is dragged by strips the app marks: Windows and the Mac. */
export const dragTitleBar = ownTitleBar || macTitleBar;

/** The Mac in full screen hides its lights and bar, so the room left for them goes too. */
function useMacFullscreen() {
  useEffect(() => {
    if (!macTitleBar || !isTauri()) return;
    const win = getCurrentWindow();
    const check = () => win.isFullscreen().then((on) => document.documentElement.classList.toggle("mac-fullscreen", on), () => {});
    check();
    const stop = win.onResized(check);
    return () => {
      stop.then((s) => s());
    };
  }, []);
}

/** The drag strip and the caption buttons; render once, at the top of the app. */
export function WindowControls() {
  const [maximized, setMaximized] = useState(false);
  const [focused, setFocused] = useState(true);
  useMacFullscreen();
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
    // A window can open without the focus (the test harness's does), so ask rather than assume.
    win.isFocused().then(setFocused, () => {});
    const stops = [win.onResized(check), win.onFocusChanged(({ payload }) => setFocused(payload))];
    return () => stops.forEach((s) => s.then((stop) => stop()));
  }, []);
  if (macTitleBar) {
    return (
      <>
        <div className="titlebar-drag" data-tauri-drag-region aria-hidden="true" />
        {previewChrome === "mac" && (
          // Where the lights sit, for the preview only: the app keeps the system's own.
          <div className="mac-lights" aria-hidden="true">
            <span />
            <span />
            <span />
          </div>
        )}
      </>
    );
  }
  if (!ownTitleBar) return null;
  const win = previewChrome ? null : getCurrentWindow();
  return (
    <>
      <div className="titlebar-drag" data-tauri-drag-region aria-hidden="true" />
      <div className={`caption${focused ? "" : " caption-blurred"}`} role="group" aria-label="Window">
        <button className="caption-btn" data-test="minimize" aria-label="Minimize" title="Minimize" onClick={() => win?.minimize()}>
          <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true">
            <path d="M0 5h10" stroke="currentColor" strokeWidth="1" />
          </svg>
        </button>
        <button className="caption-btn" data-test="maximize" aria-label={maximized ? "Restore" : "Maximize"} title={maximized ? "Restore Down" : "Maximize"} onClick={() => win?.toggleMaximize()}>
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
        <button className="caption-btn caption-close" data-test="close" aria-label="Close" title="Close" onClick={() => win?.close()}>
          <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true">
            <path d="M.5.5l9 9M9.5.5l-9 9" stroke="currentColor" strokeWidth="1" />
          </svg>
        </button>
      </div>
    </>
  );
}
