// A live check of one real Windows toast (not part of `npm run app:test`, since it shows a toast on
// the desktop): `node scripts/harness/toast.check.mjs`. It asks a question on throwaway data with
// NITELY_HARNESS_TOASTS=1, then confirms the toast from Windows' notification history, reading only
// toasts under the dev build's app id that carry this run's own question.

import { spawnSync } from "node:child_process";
import { badge, launch, nitely, notifications } from "./app.mjs";

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
// The app id a dev or harness build's toasts go out under (tauri-winrt-notification's POWERSHELL_APP_ID).
const POWERSHELL_APP_ID = String.raw`{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\WindowsPowerShell\v1.0\powershell.exe`;
const app = await launch({ env: { NITELY_HARNESS_TOASTS: "1" } });
const { page, home } = app;
const question = `Toast check ${Date.now() % 100000}: keep it?`;
let result;
try {
  await page.open("#overview");
  await sleep(4000);
  nitely(home, "ask", question, "--issue", "APP-1");
  for (let i = 0; i < 60 && notifications(home).length === 0; i++) await sleep(250);
  // A dev or harness build has no app id of its own; Windows shows its toasts as PowerShell's.
  const ps = `
    [void][Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime]
    $id = '${POWERSHELL_APP_ID}'
    foreach ($t in [Windows.UI.Notifications.ToastNotificationManager]::History.GetHistory($id)) {
      if ($null -ne $t.Content) { $x = $t.Content.GetXml(); if ($x.Contains('${question}')) { $x } }
    }`;
  // Windows takes a moment to file a toast in its history.
  let shown = "";
  for (let i = 0; i < 10 && !shown.includes(question); i++) {
    await sleep(1000);
    // Encoded, so the app id's backslashes survive the trip through the command line.
    const encoded = Buffer.from(ps, "utf16le").toString("base64");
    shown = spawnSync("powershell.exe", ["-NoProfile", "-EncodedCommand", encoded], { encoding: "utf8" }).stdout.trim();
  }
  result = { asked: notifications(home), badge: badge(home), inWindowsHistory: shown.includes(question), toast: shown };
} finally {
  await app.close();
}
console.log(JSON.stringify(result, null, 2));
process.exit(result.inWindowsHistory ? 0 : 1);
