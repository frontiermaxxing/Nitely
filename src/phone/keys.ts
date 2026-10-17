// The phone's device key and signing (docs/phone.md). The page is served over plain HTTP inside a
// tunnel, which browsers do not count as a secure origin, so WebCrypto is not there: it signs with
// noble's audited pure-JavaScript Ed25519, with randomness from crypto.getRandomValues, which is.
// The secret key stays in this page's own storage on the phone and is never sent anywhere.
import * as ed from "@noble/ed25519";
import { sha256, sha512 } from "@noble/hashes/sha2.js";

ed.hashes.sha512 = sha512;

export interface Device {
  /** The id the PC gave the pairing request; the device's id once paired. */
  id: string;
  name: string;
  /** Hex. */
  secret: string;
  public_key: string;
  /** The pairing code both screens show. */
  code: string;
  paired: boolean;
}

const STORE = "nitely.phone";
const enc = new TextEncoder();

export const toHex = (b: Uint8Array) => Array.from(b, (x) => x.toString(16).padStart(2, "0")).join("");
export function fromHex(h: string): Uint8Array {
  const out = new Uint8Array(h.length / 2);
  for (let i = 0; i < out.length; i++) out[i] = parseInt(h.slice(i * 2, i * 2 + 2), 16);
  return out;
}

export function loadDevice(): Device | null {
  try {
    const raw = localStorage.getItem(STORE);
    return raw ? (JSON.parse(raw) as Device) : null;
  } catch {
    return null;
  }
}

export function saveDevice(d: Device | null) {
  try {
    if (d) localStorage.setItem(STORE, JSON.stringify(d));
    else localStorage.removeItem(STORE);
  } catch {
    // A phone that refuses storage cannot stay paired; the page says so when it reloads.
  }
}

/** A new key pair, from the browser's randomness. */
export function newKey(): { secret: string; public_key: string } {
  const secret = ed.utils.randomSecretKey();
  return { secret: toHex(secret), public_key: toHex(ed.getPublicKey(secret)) };
}

export function sign(secretHex: string, message: string): string {
  return toHex(ed.sign(enc.encode(message), fromHex(secretHex)));
}

/** The code both screens show, as crates/nitely-core/src/phone.rs `pairing_code` works it out. */
export function pairingCode(pairSecretHex: string, publicKeyHex: string): string {
  const digest = sha256(new Uint8Array([...fromHex(pairSecretHex), ...enc.encode(publicKeyHex.toLowerCase())]));
  let n = 0n;
  for (const b of digest.slice(0, 8)) n = (n << 8n) | BigInt(b);
  const v = Number(n % 100_000_000n);
  return `${String(Math.floor(v / 10_000)).padStart(4, "0")}-${String(v % 10_000).padStart(4, "0")}`;
}

export const pairMessage = (pairSecretHex: string, publicKeyHex: string) => `nitely-pair-v1\n${pairSecretHex}\n${publicKeyHex.toLowerCase()}`;

/** The headers that sign a request, as crates/nitely-core/src/phone.rs `request_message` checks them. */
export function signedHeaders(d: Device, method: string, path: string, body = ""): Record<string, string> {
  const time = Math.floor(Date.now() / 1000);
  // Two identical requests in the same second must not carry the same signature.
  const nonce = toHex(crypto.getRandomValues(new Uint8Array(8)));
  const message = `nitely-phone-v1\n${method}\n${path}\n${time}\n${nonce}\n${toHex(sha256(enc.encode(body)))}`;
  return { "X-Nitely-Device": d.id, "X-Nitely-Time": String(time), "X-Nitely-Nonce": nonce, "X-Nitely-Signature": sign(d.secret, message) };
}

/** What the phone signs for an approval: the same message the app signs (docs/approvals.md). */
export const approvalMessage = (key: string, patch: string, time: number) => `nitely-approval-v1\n${key.toUpperCase()}\n${patch}\n${time}`;
export const answerMessage = (ask: number, answer: string, time: number) => `nitely-answer-v1\n${ask}\n${answer}\n${time}`;
export const sendBackMessage = (key: string, reason: string, time: number) => `nitely-sendback-v1\n${key.toUpperCase()}\n${reason}\n${time}`;
