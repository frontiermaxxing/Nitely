// `node scripts/update-manifest.mjs --version V --url URL --sig FILE --notes FILE --out FILE [--current FILE]`
// writes a channel's update manifest in the Tauri updater's format: the version, its notes, when it
// was published, and per platform the installer's URL with its updater signature. The release
// workflow publishes one per channel (stable.json, beta.json, nightly.json) for the app to read.
// With --current (the manifest published now), it writes nothing and exits 3 when that one is
// newer: two builds finishing out of order never move a channel back. The same version replaces
// it, so a release built again carries its new installer's signature.
//
// `node scripts/update-manifest.mjs --version V --holds FILE` exits 0 when FILE (a manifest read
// back after uploading) names V or a newer version, and 4 when it is missing or older: another
// build that read the same old manifest uploaded after this one.

import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { pathToFileURL } from "node:url";
import { compareVersions } from "./release-meta.mjs";

/** Whether a manifest for `version` should replace `current` (a parsed manifest, or null). */
export function replaces(version, current) {
  return !current?.version || compareVersions(version, current.version) >= 0;
}

/** Whether a published manifest (parsed, or null) still names `version` or a newer one. */
export function holds(version, published) {
  return !!published?.version && compareVersions(published.version, version) >= 0;
}

export function manifest({ version, url, signature, notes = "", date = new Date() }) {
  if (!version || !url || !signature) throw new Error("a manifest needs the version, the installer's URL and its signature");
  return {
    version,
    notes,
    pub_date: date.toISOString().replace(/\.\d{3}Z$/, "Z"),
    platforms: { "windows-x86_64": { signature: signature.trim(), url } },
  };
}

if (import.meta.url === pathToFileURL(process.argv[1] ?? "").href) {
  const args = Object.fromEntries(
    process.argv.slice(2).reduce((acc, a, i, all) => (a.startsWith("--") ? [...acc, [a.slice(2), all[i + 1]]] : acc), []),
  );
  try {
    if (args.holds !== undefined) {
      const published = existsSync(args.holds) ? JSON.parse(readFileSync(args.holds, "utf8")) : null;
      process.exit(holds(args.version, published) ? 0 : 4);
    }
    const current = args.current && existsSync(args.current) ? JSON.parse(readFileSync(args.current, "utf8")) : null;
    if (!replaces(args.version, current)) {
      console.log(`${args.out}: the published ${current.version} is newer than ${args.version}; left as it is`);
      process.exit(3);
    }
    const m = manifest({
      version: args.version,
      url: args.url,
      signature: args.sig && readFileSync(args.sig, "utf8"),
      notes: args.notes ? readFileSync(args.notes, "utf8") : "",
    });
    writeFileSync(args.out, JSON.stringify(m, null, 2) + "\n");
    console.log(`${args.out}: ${m.version}`);
  } catch (e) {
    console.error(e.message);
    process.exit(1);
  }
}
