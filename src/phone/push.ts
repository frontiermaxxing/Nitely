// Notifications on the phone (docs/phone.md, "Notifications"): the page subscribes this phone's
// browser to push under the Nitely app's key and hands the app the subscription, signed. The app
// then pushes straight to the browser's own push service when a decision waits.
import { isFake, signedGet, signedPost } from "./api";
import type { Device } from "./keys";
import { keyBytes, notifyState, type NotifyEnv } from "./pushEnv";

export { notifyState, type NotifyEnv };

export function readEnv(): NotifyEnv {
  const nav = navigator as Navigator & { standalone?: boolean };
  const ios = /iPhone|iPad|iPod/.test(nav.userAgent) || (nav.platform === "MacIntel" && nav.maxTouchPoints > 1);
  // The preview (`/phone.html?fixtures&env=ios`, `env=http`) shows what another phone would.
  const as = isFake() ? (new URLSearchParams(location.search).get("env") ?? "phone") : null;
  if (as) return { secure: as !== "http", supported: true, ios: as === "ios", standalone: false, permission: "default" };
  return {
    secure: window.isSecureContext,
    supported: "serviceWorker" in nav && "PushManager" in window && "Notification" in window,
    ios,
    standalone: nav.standalone === true || window.matchMedia("(display-mode: standalone)").matches,
    permission: "Notification" in window ? Notification.permission : "denied",
  };
}

function sameKey(a: ArrayBuffer | null | undefined, b: Uint8Array): boolean {
  if (!a || a.byteLength !== b.length) return false;
  const x = new Uint8Array(a);
  return x.every((v, i) => v === b[i]);
}

interface PushState {
  subscribed: boolean;
  key: string;
}

async function registration(): Promise<ServiceWorkerRegistration> {
  await navigator.serviceWorker.register("/phone/sw.js", { scope: "/phone/" });
  return navigator.serviceWorker.ready;
}

/** Subscribes this browser under the app's key and tells the app. Returns why not, or null. */
async function subscribe(device: Device): Promise<string | null> {
  const reg = await registration();
  const r = await signedGet<PushState>(device, "/phone/api/push");
  if (r.status !== 200) return r.body.error ?? `The PC answered ${r.status}.`;
  const key = keyBytes(r.body.key);
  let sub = await reg.pushManager.getSubscription();
  // Made under another key (the app's key was made anew): the push service would refuse the app.
  if (sub && !sameKey(sub.options.applicationServerKey, key)) {
    await sub.unsubscribe();
    sub = null;
  }
  sub ??= await reg.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: key });
  const saved = await signedPost<PushState>(device, "/phone/api/push", sub.toJSON());
  return saved.status === 200 ? null : (saved.body.error ?? `The PC answered ${saved.status}.`);
}

/** Asks the person, then subscribes. Call it from the tap itself: iPhones ask only then. */
export async function turnOn(device: Device): Promise<string | null> {
  if (isFake()) return (await signedPost(device, "/phone/api/push", { endpoint: "https://fcm.googleapis.com/fcm/send/preview" })).status === 200 ? null : "The preview refused.";
  const permission = await Notification.requestPermission();
  if (permission !== "granted") return permission === "denied" ? "Notifications are turned off for this page in the phone's settings." : "Notifications were not allowed.";
  return subscribe(device);
}

export async function turnOff(device: Device): Promise<string | null> {
  if (isFake()) return (await signedPost(device, "/phone/api/push", { endpoint: null })).status === 200 ? null : "The preview refused.";
  const reg = await navigator.serviceWorker.getRegistration("/phone/");
  await (await reg?.pushManager.getSubscription())?.unsubscribe();
  const r = await signedPost<PushState>(device, "/phone/api/push", { endpoint: null });
  return r.status === 200 ? null : (r.body.error ?? `The PC answered ${r.status}.`);
}

/**
 * Whether notifications are on for this phone. With permission given, the browser's current
 * subscription is handed to the app again: the push service may have replaced it since.
 */
export async function current(device: Device, env: NotifyEnv): Promise<boolean> {
  if (isFake()) return (await signedGet<PushState>(device, "/phone/api/push")).body.subscribed;
  if (notifyState(env).kind !== "ready" || env.permission !== "granted") return false;
  const reg = await navigator.serviceWorker.getRegistration("/phone/");
  if (!reg || !(await reg.pushManager.getSubscription())) return false;
  return (await subscribe(device)) === null;
}
