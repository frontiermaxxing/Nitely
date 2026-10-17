// Runs with `npm test` (scripts/test.mjs).
import test from "node:test";
import assert from "node:assert/strict";
import { unsigned } from "./dco.mjs";

const commit = (email, message) => ({ sha: "1234567890", email, subject: message.split("\n")[0], message });

test("a commit signed off by its author passes", () => {
  assert.deepEqual(unsigned([commit("sam@example.com", "Fix it\n\nSigned-off-by: Sam Lee <sam@example.com>\n")]), []);
});

test("the author's email matches in any case, among several sign-offs", () => {
  const c = commit("Sam@Example.com", "Fix it\n\nSigned-off-by: Ana Ruiz <ana@example.com>\nSigned-off-by: Sam Lee <sam@example.COM>");
  assert.deepEqual(unsigned([c]), []);
});

test("no sign-off, or only someone else's, fails", () => {
  const none = commit("sam@example.com", "Fix it");
  const other = commit("sam@example.com", "Fix it\n\nSigned-off-by: Ana Ruiz <ana@example.com>");
  const inline = commit("sam@example.com", "Fix it, see Signed-off-by: Sam Lee <sam@example.com> in the docs");
  assert.deepEqual(unsigned([none, other, inline]), [none, other, inline]);
});
