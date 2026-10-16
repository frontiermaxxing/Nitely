import { invoke as tauriInvoke, isTauri } from "@tauri-apps/api/core";
import { listen as tauriListen, type EventCallback, type UnlistenFn } from "@tauri-apps/api/event";

/**
 * True when the frontend runs in a plain browser under `npm run dev`: then every command goes to
 * the sample data in src/dev/fixtures.ts instead of Tauri. Always false in a production build, so
 * the fixtures are not shipped and the app's path to Tauri is unchanged.
 */
export const isPreview = import.meta.env.DEV && !isTauri();

/**
 * The preview on live data (`?data=live`): reads go to the real database through the dev server
 * and `nitely ui`, which runs reads only and refuses every command that changes Nitely.
 */
export const isLive = isPreview && new URLSearchParams(location.search).get("data") === "live";

/** Calls a Tauri command, or the fixture backend or the live read-only one in the browser preview. */
export async function invoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  // Spelled out rather than `isPreview` so a production build drops the fixtures entirely.
  if (import.meta.env.DEV && !isTauri()) {
    if (isLive) return liveInvoke<T>(cmd, args ?? {});
    const { fixtureInvoke } = await import("./dev/fixtures");
    return fixtureInvoke<T>(cmd, args ?? {});
  }
  return tauriInvoke<T>(cmd, args);
}

async function liveInvoke<T>(cmd: string, args: Record<string, unknown>): Promise<T> {
  const res = await fetch("/__nitely/invoke", {
    method: "POST",
    headers: { "Content-Type": "application/json", "x-nitely-preview": "1" },
    body: JSON.stringify({ cmd, args }),
  });
  const body = await res.json().catch(() => null);
  // Tauri rejects with the command's error string; so does this.
  if (!res.ok) throw body?.error ?? `${cmd} failed (${res.status})`;
  return body as T;
}

/**
 * Listens for a backend event. The preview has no backend; there the sample data (src/dev) sends
 * the same events as window events, so a change it makes or a fake notification's click arrives
 * the way the app's would.
 */
export function listen<T>(event: string, handler: EventCallback<T>): Promise<UnlistenFn> {
  if (isPreview) {
    const on = (e: Event) => handler({ event, id: 0, payload: (e as CustomEvent<T>).detail } as Parameters<EventCallback<T>>[0]);
    window.addEventListener(event, on);
    return Promise.resolve(() => window.removeEventListener(event, on));
  }
  return tauriListen<T>(event, handler);
}
