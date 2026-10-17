// The browser preview's phone server (src/PhoneSetup.tsx): a tunnel address and an ordinary one,
// a paired phone, and a pairing request that arrives a few seconds after a code is shown.
import type { PhoneOffer, PhoneStatus, TailscaleState } from "../types";

const NOW = () => Math.floor(Date.now() / 1000);
const ADDRESSES: PhoneStatus["addresses"] = [
  { ip: "127.0.0.1", interface: "this machine", kind: "loopback" },
  { ip: "100.101.7.42", interface: "Tailscale", kind: "tunnel" },
  { ip: "192.168.1.68", interface: "Wi-Fi", kind: null },
];
let listening: string[] = [];
let bound: string[] = [];
let devices: PhoneStatus["devices"] = [{ id: "9f2c41aa07d3e5b1", name: "Pixel 9", public_key: "ab".repeat(32), paired_at: NOW() - 3 * 86400 }];
let offeredAt = 0;
let decided = false;

/** A QR-like pattern; the preview never makes a real pairing secret. */
function fakeQr(): string {
  let cells = "";
  for (let y = 0; y < 25; y++) for (let x = 0; x < 25; x++) if ((x * 7 + y * 13 + x * y) % 3 === 0 || (x < 7 && y < 7) || (x > 17 && y < 7) || (x < 7 && y > 17)) cells += `<rect x="${x}" y="${y}" width="1" height="1"/>`;
  return `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 25 25" shape-rendering="crispEdges"><g fill="#000">${cells}</g></svg>`;
}

function status(): PhoneStatus {
  const requests =
    offeredAt && !decided && NOW() - offeredAt > 3
      ? [{ id: "c0ffee0011223344", name: "Andrew's iPhone", public_key: "cd".repeat(32), code: "4821-0937", at: offeredAt + 3, status: "waiting" as const }]
      : [];
  return { listening, bound, addresses: ADDRESSES, devices, requests, devices_error: null, notified: ["9f2c41aa07d3e5b1"] };
}

export function phoneStatus(): PhoneStatus {
  return status();
}

export function phoneTailscale(): TailscaleState {
  return { installed: true, running: true, backend: "Running", name: "pc.tail1234.ts.net", ips: ["100.101.7.42", "fd7a:115c:a1e0::1"], phones: [{ name: "Pixel-9", online: true }] };
}

export function phoneTailscaleName(): string {
  return "pc.tail1234.ts.net";
}

export function phoneStart(args: Record<string, unknown>): PhoneStatus {
  const ips = args.ips as string[];
  const tls = args.tls as { kind: string; host: string } | null;
  if (tls && tls.kind !== "none") {
    const host = tls.kind === "tailscale" ? phoneTailscaleName() : tls.host.trim();
    listening = [`https://${host}:7422/phone/`];
    bound = ips;
    return status();
  }
  const bad = ips.find((ip) => !ADDRESSES.find((a) => a.ip === ip)?.kind);
  if (bad) throw `${bad} is not this machine's Tailscale address, and plain HTTP on any other network could be changed by anyone who can intercept traffic; pick the Tailscale address (docs/phone.md)`;
  listening = ips.map((ip) => `http://${ip}:7422/phone/`);
  bound = ips;
  return status();
}

export function phoneStop(): PhoneStatus {
  listening = [];
  bound = [];
  offeredAt = 0;
  return status();
}

export function phonePair(): PhoneOffer {
  if (!listening.length) throw "Turn the phone server on first.";
  offeredAt = NOW();
  decided = false;
  return { urls: listening.map((u) => `${u}#pair=${"0".repeat(64)}`), qr_svg: fakeQr(), expires: NOW() + 600 };
}

export function phoneDecide(args: Record<string, unknown>): PhoneStatus {
  decided = true;
  if (args.accept) devices = [...devices, { id: String(args.id), name: "Andrew's iPhone", public_key: "cd".repeat(32), paired_at: NOW() }];
  return status();
}

export function phoneRevoke(args: Record<string, unknown>): PhoneStatus {
  devices = devices.filter((d) => d.id !== String(args.id));
  return status();
}
