// The browser preview's stand-in for the phone server (`/phone.html?fixtures`), so the phone's
// screens can be checked without a PC: pairing is confirmed a few seconds after it is asked, and a
// question, a review finding to accept and two approvals wait. Deciding takes them off the list until reload.
import { setFake } from "./api";
import { pairingCode } from "./keys";
import type { PhoneDecision } from "./Inbox";

const NOW = () => Math.floor(Date.now() / 1000);
const track = (station: string, reached: Record<string, number>) => ({ station, reached, rounds: 0, stuck: null, asking: false, closed: null, backlog: false });

let decisions: PhoneDecision[] = [
  {
    kind: "ask",
    id: "ask-12",
    since: NOW() - 900,
    issue: { key: "SHOP-3", title: "Show prices in the visitor's currency", kind: "feature", priority: 3, state: "In Progress", track: track("working", { todo: NOW() - 9000, started: NOW() - 8000, working: NOW() - 7000 }) as never },
    ask: { id: 12, question: "Which currencies should the price page offer at launch? It decides the rounding in [DEMO-1](nitely://issue/DEMO-1) and the `currency` setting.", options: ["EUR and USD", "EUR, USD and GBP", "Every currency Stripe supports"], asked_by: "claude:c:/code/demo-worktrees/demo-3" },
  },
  {
    kind: "ask",
    id: "ask-14",
    since: NOW() - 600,
    issue: { key: "DEMO-4", title: "Keep each shop's settings apart", kind: "bug", priority: 1, state: "Needs You", track: track("needs", { todo: NOW() - 30000, started: NOW() - 28000, working: NOW() - 27000, gate: NOW() - 3000, review: NOW() - 2000, needs: NOW() - 600 }) as never },
    ask: {
      id: 14,
      question: "You decided not to migrate old shop settings: there are no outside users yet. The reviewer keeps raising it; fixing it means copying settings between shops, which this change stops.",
      options: [],
      asked_by: "codex:c:/code/demo-worktrees/demo-4",
      finding: { priority: 1, text: "No migration for existing shops: src/settings.ts, load: a shop set up before this starts with empty settings" },
    },
    patch_id: "patch-demo-4",
  },
  {
    kind: "approve",
    id: "approve-DEMO-1",
    since: NOW() - 3600,
    issue: { key: "DEMO-1", title: "Round invoice totals to the cent", kind: "feature", priority: 1, state: "Needs You", track: track("needs", { todo: NOW() - 20000, started: NOW() - 18000, working: NOW() - 17000, gate: NOW() - 9000, review: NOW() - 8000, "needs": NOW() - 3600 }) as never },
    patch_id: "patch-demo-1",
    reasons: ["billing and money: src/billing/invoice.rs"],
    review: { reviewer: "codex", verdict: "pass", note: "", findings: [{ priority: 3, text: "Rename `amt` to `amount` in invoice.rs" }] },
  },
  {
    kind: "approve",
    id: "approve-SHOP-2",
    since: NOW() - 1800,
    issue: { key: "SHOP-2", title: "Retry webhook deliveries with backoff", kind: "feature", priority: 2, state: "Needs You", track: track("needs", { todo: NOW() - 20000, started: NOW() - 18000, working: NOW() - 15000, gate: NOW() - 4000, review: NOW() - 3000, "needs": NOW() - 1800 }) as never },
    patch_id: "patch-demo-2",
    reasons: ["the change touches the release process"],
    review: { reviewer: "claude", verdict: "pass", note: "same vendor: codex not installed", findings: [] },
  },
];

export function installFake() {
  let askedAt = 0;
  let notifying = false;
  setFake((method, path, body) => {
    if (method === "POST" && path === "/phone/api/pair") {
      askedAt = Date.now();
      const req = JSON.parse(body) as { secret: string; public_key: string };
      return { status: 200, body: { id: "preview0001", code: pairingCode(req.secret, req.public_key) } };
    }
    if (path === "/phone/api/pair/status") return { status: 200, body: { status: Date.now() - askedAt > 4000 ? "paired" : "waiting" } };
    if (path === "/phone/api/hello") return { status: 200, body: { name: "Preview phone", id: "preview0001" } };
    if (path === "/phone/api/push") {
      if (method === "POST") notifying = (JSON.parse(body) as { endpoint: string | null }).endpoint !== null;
      return { status: 200, body: { subscribed: notifying, key: "BP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8" } };
    }
    if (path === "/phone/api/decisions") return { status: 200, body: { decisions, at_pc: 2 } };
    if (method === "POST" && ["/phone/api/approve", "/phone/api/sendback"].includes(path)) {
      const key = (JSON.parse(body) as { key: string }).key;
      decisions = decisions.filter((d) => d.issue?.key !== key);
      return { status: 200, body: { ok: true } };
    }
    if (method === "POST" && ["/phone/api/answer", "/phone/api/accept", "/phone/api/decline"].includes(path)) {
      const ask = (JSON.parse(body) as { ask: number }).ask;
      decisions = decisions.filter((d) => d.ask?.id !== ask);
      return { status: 200, body: { ok: true } };
    }
    return { status: 404, body: { error: "Not found." } };
  });
}
