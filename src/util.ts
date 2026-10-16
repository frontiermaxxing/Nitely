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

/** `claude:c:/development/nitely` → `claude · nitely` */
export function agentLabel(id: string): string {
  const [vendor, ...rest] = id.split(":");
  const path = rest.join(":");
  const last = path.split("/").filter(Boolean).pop();
  return last ? `${vendor} · ${last}` : vendor;
}

export function nowSecs() {
  return Date.now() / 1000;
}
