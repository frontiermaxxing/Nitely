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
//
// `node scripts/update-manifest.mjs --pick CHANNEL --releases FILE --notes-out FILE` picks, from the
// repo's releases (`gh api repos/OWNER/REPO/releases`), the newest published one the channel's
// manifest should name, and prints it as KEY=value lines (tag, version, url, sig) for the shell,
// writing its notes to the notes file; it exits 5 when the channel has none yet. `--same A --as B`
// exits 0 when two manifest files name the same version, installer and signature, 6 otherwise. Every build writes
// the manifest from this, not from its own version, so builds finishing side by side all write the
// same thing, and a read-back that finds an older one can always derive the right one again.

import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { pathToFileURL } from "node:url";
import { compareVersions, decide } from "./release-meta.mjs";

/** Whether two manifests (parsed, or null) name the same version, installer and signature. */
export function same(a, b) {
  return !!a && !!b && a.version === b.version && JSON.stringify(a.platforms) === JSON.stringify(b.platforms);
}

/** The installer of a release, by the name the workflow gives it: `Nitely_<version>_x64-setup.exe`. */
const INSTALLER = /^Nitely_(.+)_x64-setup\.exe$/;

/**
 * The newest published release whose manifests include `channel`, from GitHub's releases list
 * (`tag_name`, `draft`, `body`, `assets[{ name, browser_download_url }]`): one that has its
 * installer and that installer's updater signature. Its version is the installer's, so the rolling
 * `nightly` tag counts too. Null when there is none.
 */
export function newest(releases, channel) {
  const manifestsOf = (tag) => {
    if (tag === "nightly") return ["nightly"];
    try {
      return decide(tag).manifests;
    } catch {
      return [];
    }
  };
  const candidates = [];
  for (const r of releases) {
    if (r.draft || !manifestsOf(r.tag_name).includes(channel)) continue;
    const installer = (r.assets ?? []).find((a) => INSTALLER.test(a.name));
    if (!installer || !(r.assets ?? []).some((a) => a.name === `${installer.name}.sig`)) continue;
    const version = INSTALLER.exec(installer.name)[1];
    candidates.push({ tag: r.tag_name, version, url: installer.browser_download_url, sig: `${installer.name}.sig`, notes: r.body ?? "" });
  }
  candidates.sort((a, b) => compareVersions(b.version, a.version));
  return candidates[0] ?? null;
}

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
    if (args.same !== undefined) {
      const read = (f) => (f && existsSync(f) ? JSON.parse(readFileSync(f, "utf8")) : null);
      process.exit(same(read(args.same), read(args.as)) ? 0 : 6);
    }
    if (args.pick !== undefined) {
      // `gh api --paginate --slurp` gives one array per page.
      const picked = newest(JSON.parse(readFileSync(args.releases, "utf8")).flat(), args.pick);
      if (!picked) process.exit(5);
      if (args["notes-out"]) writeFileSync(args["notes-out"], picked.notes);
      console.log(`tag=${picked.tag}\nversion=${picked.version}\nurl=${picked.url}\nsig=${picked.sig}`);
      process.exit(0);
    }
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
