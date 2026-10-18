// A minimal MCP client over stdio for the harness: an agent's side of `nitely mcp`, so a scenario
// (and the fake crew agent) acts through the same tools a real agent calls.

import { spawn } from "node:child_process";

/**
 * Starts `nitely mcp` in `cwd` (the folder the agent works in) and initializes it as `client`
 * ("claude-code" makes it a Claude session). Returns `call(tool, args)`, which resolves to the
 * tool's JSON result or rejects with its error, and `close()`.
 */
export async function connect(nitely, { cwd, env, client = "claude-code" }) {
  const child = spawn(nitely, ["mcp"], { cwd, env, stdio: ["pipe", "pipe", "pipe"] });
  let buffer = "";
  let stderr = "";
  const waiting = new Map();
  let next = 1;
  child.stderr.on("data", (d) => (stderr += d));
  child.stdout.on("data", (d) => {
    buffer += d;
    let at;
    while ((at = buffer.indexOf("\n")) >= 0) {
      const line = buffer.slice(0, at).trim();
      buffer = buffer.slice(at + 1);
      if (!line) continue;
      const msg = JSON.parse(line);
      const w = waiting.get(msg.id);
      if (w) {
        waiting.delete(msg.id);
        w(msg);
      }
    }
  });
  const exited = new Promise((r) => child.once("exit", r));
  const request = (method, params) =>
    new Promise((resolve, reject) => {
      const id = next++;
      waiting.set(id, resolve);
      child.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", id, method, params })}\n`);
      exited.then((code) => waiting.has(id) && reject(new Error(`nitely mcp exited (${code}) during ${method}: ${stderr}`)));
    });
  await request("initialize", { protocolVersion: "2025-03-26", capabilities: {}, clientInfo: { name: client, version: "harness" } });
  child.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", method: "notifications/initialized" })}\n`);
  return {
    async call(name, args = {}) {
      const msg = await request("tools/call", { name, arguments: args });
      if (msg.error) throw new Error(`${name}: ${msg.error.message}`);
      const text = msg.result?.content?.[0]?.text ?? "";
      if (msg.result?.isError) throw new Error(`${name}: ${text}`);
      try {
        return JSON.parse(text);
      } catch {
        return text;
      }
    },
    close() {
      child.stdin.end();
      return Promise.race([exited, new Promise((r) => setTimeout(r, 5000))]).then(() => child.exitCode === null && child.kill());
    },
  };
}
