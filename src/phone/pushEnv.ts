// What a phone's browser needs before it can have notifications (push.ts), kept free of imports
// so the tests can run it.

/** What this browser can do, read once from the page. */
export interface NotifyEnv {
  /** HTTPS (or this machine): browsers offer push only there. */
  secure: boolean;
  /** Service workers and the Push API. */
  supported: boolean;
  /** An iPhone or iPad, which shows web push only to a page added to the Home Screen. */
  ios: boolean;
  /** Opened from the Home Screen. */
  standalone: boolean;
  permission: "default" | "granted" | "denied";
}

/** Whether the person can turn notifications on here, and if not, what they must do first. */
export type NotifyState = { kind: "ready" } | { kind: "blocked"; why: string };

export function notifyState(env: NotifyEnv): NotifyState {
  if (!env.secure) return { kind: "blocked", why: "Notifications need the HTTPS link. On the PC, choose HTTPS on the Phone screen, then pair this phone again from the new link." };
  if (env.ios && !env.standalone)
    return { kind: "blocked", why: "On iPhone, notifications come only to Nitely on the Home Screen: tap Share, then Add to Home Screen, open Nitely from there and pair it." };
  if (!env.supported) return { kind: "blocked", why: "This browser cannot show notifications from a web page." };
  if (env.permission === "denied") return { kind: "blocked", why: "Notifications are turned off for this page in the phone's settings. Allow them there, then come back." };
  return { kind: "ready" };
}

/** The app's base64url key as the bytes `applicationServerKey` takes. */
export function keyBytes(b64url: string): Uint8Array<ArrayBuffer> {
  const b64 = b64url.replace(/-/g, "+").replace(/_/g, "/") + "=".repeat((4 - (b64url.length % 4)) % 4);
  const raw = atob(b64);
  const out = new Uint8Array(new ArrayBuffer(raw.length));
  for (let i = 0; i < raw.length; i++) out[i] = raw.charCodeAt(i);
  return out;
}
