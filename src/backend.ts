import { invoke as tauriInvoke, isTauri } from "@tauri-apps/api/core";
import { listen as tauriListen, type EventCallback, type UnlistenFn } from "@tauri-apps/api/event";

/**
 * True when the frontend runs in a plain browser under `npm run dev`: then every command goes to
 * the sample data in src/dev/fixtures.ts instead of Tauri. Always false in a production build, so
 * the fixtures are not shipped and the app's path to Tauri is unchanged.
 */
export const isPreview = import.meta.env.DEV && !isTauri();

/** Calls a Tauri command, or the fixture backend in the browser preview. */
export async function invoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  // Spelled out rather than `isPreview` so a production build drops the fixtures entirely.
  if (import.meta.env.DEV && !isTauri()) {
    const { fixtureInvoke } = await import("./dev/fixtures");
    return fixtureInvoke<T>(cmd, args ?? {});
  }
  return tauriInvoke<T>(cmd, args);
}

/** Listens for a backend event. The preview has no backend events; its data changes only when you act. */
export function listen<T>(event: string, handler: EventCallback<T>): Promise<UnlistenFn> {
  if (isPreview) return Promise.resolve(() => {});
  return tauriListen<T>(event, handler);
}
