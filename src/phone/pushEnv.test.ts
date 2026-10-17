import { test } from "node:test";
import assert from "node:assert/strict";
import { keyBytes, notifyState, type NotifyEnv } from "./pushEnv.ts";

const phone: NotifyEnv = { secure: true, supported: true, ios: false, standalone: false, permission: "default" };

test("notifications: ready on HTTPS with push, and why not otherwise", () => {
  assert.equal(notifyState(phone).kind, "ready");
  assert.equal(notifyState({ ...phone, permission: "granted" }).kind, "ready");
  const why = (env: NotifyEnv) => {
    const s = notifyState(env);
    return s.kind === "blocked" ? s.why : "";
  };
  assert.match(why({ ...phone, secure: false }), /HTTPS/);
  assert.match(why({ ...phone, ios: true }), /Home Screen/);
  assert.equal(notifyState({ ...phone, ios: true, standalone: true }).kind, "ready");
  assert.match(why({ ...phone, supported: false }), /cannot show/);
  assert.match(why({ ...phone, permission: "denied" }), /settings/);
  // Plain HTTP comes first: on an iPhone, the Home Screen alone would not help.
  assert.match(why({ ...phone, secure: false, ios: true }), /HTTPS/);
});

test("notifications: the app's base64url key becomes its 65 bytes", () => {
  const key = keyBytes("BP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8");
  assert.equal(key.length, 65);
  assert.equal(key[0], 4);
  assert.deepEqual([...keyBytes("_-8")], [0xff, 0xef]);
});
