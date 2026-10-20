// Serialized into the isolated harness page. Tauri's invoke and IPC functions are immutable;
// Windows custom-protocol IPC calls the writable fetch transport instead. Do not log headers.
export function installPerformanceProbe(target) {
  const original = target.fetch;
  const queryUrl = target.__TAURI_INTERNALS__.convertFileSrc("performance_query", "ipc");
  const snapshotUrl = target.__TAURI_INTERNALS__.convertFileSrc("snapshot", "ipc");
  const probe = { reads: [], snapshots: 0, transform: null, restore: () => { target.fetch = original; } };
  const wrapped = async (input, init) => {
    const url = typeof input === "string" ? input : input.url ?? String(input);
    const query = url === queryUrl ? JSON.parse(init.body).query : null;
    if (query) probe.reads.push(query);
    if (url === snapshotUrl) probe.snapshots++;
    const response = await original.call(target, input, init);
    if (!query || !probe.transform || response.headers.get("Tauri-Response") !== "ok") return response;
    const report = probe.transform(await response.json(), query);
    return new target.Response(JSON.stringify(report), { status: response.status, headers: response.headers });
  };
  target.fetch = wrapped;
  if (target.fetch !== wrapped) throw new Error("Performance probe could not observe the fetch transport");
  target.__perfChecks = probe;
}
