// The agents' own icons, read by the backend from what is installed on this machine
// (src-tauri/src/agent_icons.rs). AgentMark draws one when it is here and the letters otherwise.

import { useSyncExternalStore } from "react";

let icons: Record<string, string> = {};
const listeners = new Set<() => void>();

/** Sets the icons, as PNG bytes by vendor; a vendor with none keeps its letters. */
export function setAgentIcons(found: Record<string, number[] | null>) {
  for (const url of Object.values(icons)) URL.revokeObjectURL(url);
  icons = {};
  for (const [vendor, png] of Object.entries(found)) {
    if (png?.length) icons[vendor] = URL.createObjectURL(new Blob([new Uint8Array(png)], { type: "image/png" }));
  }
  listeners.forEach((l) => l());
}

/** The icon to draw for a vendor, or null. */
export function useAgentIcon(vendor: string): string | null {
  return useSyncExternalStore(
    (on) => {
      listeners.add(on);
      return () => listeners.delete(on);
    },
    () => icons[vendor] ?? null,
  );
}
