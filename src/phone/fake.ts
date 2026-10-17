// The browser preview's stand-in for the phone server (`/phone.html?fixtures`), so the phone's
// screens can be checked without a PC: pairing is confirmed a few seconds after it is asked.
import { setFake } from "./api";
import { pairingCode } from "./keys";

export function installFake() {
  let askedAt = 0;
  setFake((method, path, body) => {
    if (method === "POST" && path === "/phone/api/pair") {
      askedAt = Date.now();
      const req = JSON.parse(body) as { secret: string; public_key: string };
      return { status: 200, body: { id: "preview0001", code: pairingCode(req.secret, req.public_key) } };
    }
    if (path === "/phone/api/pair/status") return { status: 200, body: { status: Date.now() - askedAt > 4000 ? "paired" : "waiting" } };
    if (path === "/phone/api/hello") return { status: 200, body: { name: "Preview phone", id: "preview0001" } };
    return { status: 404, body: { error: "Not found." } };
  });
}
