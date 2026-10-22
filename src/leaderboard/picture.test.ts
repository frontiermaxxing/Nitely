import { test } from "node:test";
import assert from "node:assert/strict";
import { avatarUrl } from "./picture.ts";

test("a picture comes from a link however it was typed, as the report accepts it (NIT-630)", () => {
  // The editor previews the picture from the fields as typed, before the report normalizes them.
  for (const github of ["octocat", "@octocat", "github.com/octocat", "https://github.com/octocat/", " https://github.com/octocat "]) {
    assert.equal(avatarUrl("github", { github }), "https://github.com/octocat.png?size=96", github);
  }
  for (const x of ["octo_cat", "@octo_cat", "x.com/octo_cat", "https://x.com/octo_cat/", "https://twitter.com/octo_cat"]) {
    assert.equal(avatarUrl("x", { x }), "https://unavatar.io/x/octo_cat?fallback=false", x);
  }
  assert.equal(avatarUrl("github", { github: "https://github.com/a/b" }), null, "a repository is not a user");
  assert.equal(avatarUrl("github", { github: "https://evil.example/octocat" }), null);
  assert.equal(avatarUrl("x", { github: "octocat" }), null, "the choice names the link it comes from");
});
