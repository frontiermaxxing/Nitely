import { test } from "node:test";
import assert from "node:assert/strict";
import { plain } from "./plain.ts";

test("the phone reads an agent's markdown as text: links as their text, marks dropped", () => {
  assert.equal(plain("This decides the currency work in [DEM-5](nitely://issue/DEM-5)."), "This decides the currency work in DEM-5.");
  assert.equal(plain("Run `npm ci` **first**, see [the docs](https://example.com/docs)"), "Run npm ci first, see the docs");
  assert.equal(plain("EUR and USD"), "EUR and USD");
});
