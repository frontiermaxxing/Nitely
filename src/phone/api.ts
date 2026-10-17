// Talking to the phone server in the Nitely app. Every call after pairing is signed with this
// phone's key; the server refuses anything else (docs/phone.md, "Requests").
import { signedHeaders, type Device } from "./keys";

export interface Answer<T> {
  status: number;
  body: T & { error?: string };
}

/** In the browser preview (`/phone.html?fixtures`), a stand-in for the server. */
type Fake = (method: string, path: string, body: string) => Answer<unknown>;
let fake: Fake | null = null;
export function setFake(f: Fake) {
  fake = f;
}
export function isFake(): boolean {
  return fake !== null;
}

async function call<T>(method: string, path: string, body: string, headers: Record<string, string>): Promise<Answer<T>> {
  if (fake) return fake(method, path, body) as Answer<T>;
  const res = await fetch(path, { method, headers: { ...headers, ...(body ? { "Content-Type": "application/json" } : {}) }, body: body || undefined, cache: "no-store" });
  let json: unknown = {};
  try {
    json = await res.json();
  } catch {
    json = { error: `The PC answered ${res.status}.` };
  }
  return { status: res.status, body: json as T & { error?: string } };
}

export function post<T>(path: string, data: unknown): Promise<Answer<T>> {
  return call<T>("POST", path, JSON.stringify(data), {});
}

export function signedGet<T>(device: Device, path: string): Promise<Answer<T>> {
  return call<T>("GET", path, "", signedHeaders(device, "GET", path));
}

export function signedPost<T>(device: Device, path: string, data: unknown): Promise<Answer<T>> {
  const body = JSON.stringify(data);
  return call<T>("POST", path, body, signedHeaders(device, "POST", path, body));
}
