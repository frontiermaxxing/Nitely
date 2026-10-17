import { openUrl } from "@tauri-apps/plugin-opener";
import type { MouseEvent } from "react";
import { isPreview } from "./backend";

export function ago(now: number, ts: number): string {
  const s = Math.max(0, Math.round(now - ts));
  if (s < 60) return `${s}s ago`;
  if (s < 3600) return `${Math.round(s / 60)}m ago`;
  if (s < 86400) return `${Math.round(s / 3600)}h ago`;
  return `${Math.round(s / 86400)}d ago`;
}

/** Opens links in the system browser instead of inside the app window. */
export function openExternal(url: string) {
  return (e: MouseEvent) => {
    e.preventDefault();
    if (isPreview) window.open(url, "_blank", "noopener");
    else openUrl(url).catch(() => {});
  };
}

export const PRIORITY = ["", "Urgent", "High", "Medium", "Low"];

export { agentLabel, agentTitle } from "./ui/agents";

export function nowSecs() {
  return Date.now() / 1000;
}

/** The home view's name in the sidebar, the palette and its own header. */
export const HOME_LABEL = "Overview";
