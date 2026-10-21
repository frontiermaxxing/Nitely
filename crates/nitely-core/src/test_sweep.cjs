// Bounded evidence collector. Report-only is the default; execution is explicitly requested.
const fs = require('node:fs');
const path = require('node:path');
const cp = require('node:child_process');
const crypto = require('node:crypto');
const hash = s => crypto.createHash('sha256').update(s).digest('hex');
const MAX = 16 * 1024 * 1024;
function read(file) { if (fs.statSync(file).size > MAX) throw Error('Evidence file exceeds 16 MiB'); return fs.readFileSync(file, 'utf8'); }
function parseLog(text, run, revision, source) {
  // The tests and ids test_evidence.rs reads from the same text: libtest's `<binary>::<name>`, and
  // TAP's leaf tests by name, with a parent counted only when it failed on its own (a hook threw).
  const rows = []; let suite; let lastIndent; let leaf;
  for (const raw of text.split(/\r?\n/)) {
    const line = raw.trimEnd();
    const binary = line.match(/^\s*Running .*\((?:.*[\\/])?([^\\/()]+?)-[0-9a-f]{6,}(?:\.exe)?\)$/);
    if (binary) { suite = binary[1]; continue; }
    const rust = line.match(/^test (\S+)(?: - should panic)? \.\.\. (ok|FAILED|ignored)(?:, [^<]*)?(?: <(\d+(?:\.\d+)?)s>)?$/);
    if (rust) { rows.push({ id: suite ? suite + '::' + rust[1] : rust[1], name: rust[1], runner: 'rust', outcome: rust[2] === 'ok' ? 'pass' : rust[2] === 'FAILED' ? 'fail' : 'skip', run, revision, source, ...(rust[3] ? { duration_ms: Number(rust[3]) * 1000 } : {}) }); continue; }
    const tap = line.match(/^( *)(not )?ok \d+(?: - (.+?))?(?: # (SKIP|TODO|skip|todo)\b.*)?$/);
    if (tap) {
      const indent = tap[1].length; const parent = lastIndent !== undefined && lastIndent > indent; lastIndent = indent; leaf = undefined;
      if (parent && !(tap[2] && !tap[4])) continue;
      const name = (tap[3] || '').trim();
      leaf = { id: name, name, runner: 'node', outcome: tap[4] ? 'skip' : tap[2] ? 'fail' : 'pass', run, revision, source }; rows.push(leaf); continue;
    }
    const duration = line.match(/^\s+duration_ms: ([\d.]+)/); if (duration && leaf) { leaf.duration_ms = Number(duration[1]); leaf = undefined; }
  }
  // Identical test display names are ambiguous, not repeated evidence of a flake.
  const count = new Map(); for (const r of rows) count.set(r.id, (count.get(r.id) || 0) + 1);
  return rows.map(r => ({ ...r, ambiguous: count.get(r.id) > 1 }));
}
// A command's output with stdout and stderr in the order it wrote them (cargo prints each test
// binary's `Running` header on stderr and its results on stdout), through one file for both.
function merged(command, args, options) {
  const dir = fs.mkdtempSync(path.join(require('node:os').tmpdir(), 'nitely-sweep-run-')); const file = path.join(dir, 'out.log');
  const fd = fs.openSync(file, 'w');
  try {
    const out = cp.spawnSync(command, args, { ...options, stdio: ['ignore', fd, fd] });
    fs.closeSync(fd);
    const size = fs.statSync(file).size;
    return { ...out, text: size > MAX ? '' : fs.readFileSync(file, 'utf8'), error: out.error || (size > MAX ? Error('output exceeds 16 MiB') : undefined) };
  } finally { try { fs.closeSync(fd); } catch {} fs.rmSync(dir, { recursive: true, force: true }); }
}
// Mechanical hints for classifying a test (NIT-555): counts of what its body does, in any language.
// Leads for the agent reading the code, never a verdict.
const HINTS = {
  // A wait on the clock or on machine load.
  waits: /\b(?:sleep|setTimeout|Thread\.sleep|time\.sleep|asyncio\.sleep|from_secs|from_millis|wait_for|waitFor|waitForTimeout|Task\.Delay|timeout)\b/g,
  // UI text or element presence.
  ui: /\b(?:textContent|innerText|innerHTML|outerHTML|getByText|findByText|queryByText|getByRole|getByLabelText|toBeInTheDocument|toHaveTextContent|toBeVisible|querySelector(?:All)?|html)\b/g,
  // Real I/O a scenario proves, or a unit test pays for.
  process: /\b(?:Command::new|spawn(?:Sync)?|execFile(?:Sync)?|execSync|child_process|subprocess|exec\.Command|Process\.Start|ProcessBuilder)\b/g,
  git: /\bgit\b/g,
  network: /\b(?:fetch|reqwest|TcpListener|TcpStream|http\.Get|HttpClient|requests\.(?:get|post)|urlopen|WebSocket|listen)\b/g,
  db: /\b(?:Db::open|rusqlite|sqlite3?|Connection|createConnection|sql\.Open|DriverManager|SqlConnection|database)\b/gi,
};
function hints(body) {
  const out = {};
  for (const [key, pattern] of Object.entries(HINTS)) { const n = (body.match(pattern) || []).length; if (n) out[key] = n; }
  return out;
}
// Where named test bodies start, by language: a pattern whose last character opens the body, and the group holding the name.
const STARTS = [
  [/\.rs$/, /#\[(?:test|tokio::test[^\]]*)\][\s\S]{0,180}?\bfn\s+(\w+)\s*\([^)]*\)[^{]*\{/g, 1],
  [/\.[cm]?[jt]sx?$/, /\b(?:test|it)\s*\(\s*(['"`])([^\n]*?)\1[\s\S]{0,120}?=>\s*\{/g, 2],
  [/\.go$/, /\bfunc\s+(Test\w*)\s*\([^)]*\)\s*\{/g, 1],
  [/\.(?:java|kt)$/, /@(?:Test|ParameterizedTest)\b[\s\S]{0,240}?\b(?:void|fun)\s+(\w+)\s*\([^)]*\)[^{]*\{/g, 1],
  [/\.cs$/, /\[(?:Fact|Theory|Test|TestMethod)\b[^\]]*\][\s\S]{0,240}?\b(?:void|Task)\s+(\w+)\s*\([^)]*\)[^{]*\{/g, 1],
];
function bodies(text, ext) {
  if (ext === '.py') return pythonBodies(text);
  const start = STARTS.find(([file]) => file.test(ext)); if (!start) return [];
  const [, pattern, group] = start; pattern.lastIndex = 0;
  // Conservatively extract named test bodies, balancing braces while masking comments/strings.
  const masked = text.replace(/\/\*[\s\S]*?\*\/|\/\/[^\n]*|"(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'|`(?:\\.|[^`\\])*`/g, x => x.replace(/[^\n]/g, ' '));
  const out = []; let m;
  while ((m = pattern.exec(text))) {
    const open = pattern.lastIndex - 1; let depth = 1, end = open + 1;
    for (; end < masked.length && depth; end++) { if (masked[end] === '{') depth++; else if (masked[end] === '}') depth--; }
    if (depth) continue;
    const body = text.slice(open + 1, end - 1);
    if (body.length > 50000) continue;
    out.push(record(m[group], text.slice(0, m.index).split('\n').length, body));
  }
  return out;
}
// pytest's `def test_...` (top level or in a class), its body the lines indented under it.
function pythonBodies(text) {
  const lines = text.split('\n'); const out = [];
  lines.forEach((line, i) => {
    const m = line.match(/^(\s*)(?:async\s+)?def\s+(test\w*)\s*\(/); if (!m) return;
    let end = i + 1;
    while (end < lines.length && (!lines[end].trim() || lines[end].match(/^\s*/)[0].length > m[1].length)) end++;
    const body = lines.slice(i + 1, end).join('\n');
    if (body.length <= 50000) out.push(record(m[2], i + 1, body));
  });
  return out;
}
function record(name, line, body) {
  const normalized = body.replace(/\/\*[\s\S]*?\*\/|\/\/[^\n]*|#[^\n]*/g, '').replace(/\s+/g, ' ').trim();
  return { name, line, hash: hash(body), signature: normalized.length >= 100 ? hash(normalized) : undefined, mocks: (body.match(/\b(?:mock|spyOn|mockImplementation|mockReturnValue|expect_call|withf|Mock|patch)\s*[.(]/g) || []).length, hints: hints(body) };
}
// Test files by name, in the languages `bodies` reads.
const TEST_FILE = /(?:\.rs|(?:test|spec)\.[cm]?[jt]sx?|_test\.go|(?:^|\/)test_\w+\.py|_test\.py|Tests?\.(?:java|kt|cs))$/;
function inventory(root) {
  const listed = cp.execFileSync('git', ['ls-files', '-z'], { cwd: root, maxBuffer: MAX }).toString().split('\0').filter(Boolean);
  const tests = []; let bytes = 0; let truncated = false;
  for (const file of listed) {
    if (!TEST_FILE.test(file)) continue;
    const absolute = path.join(root, file); if (!fs.existsSync(absolute) || fs.lstatSync(absolute).isSymbolicLink()) continue;
    const size = fs.statSync(absolute).size;
    if (size > 1024 * 1024 || bytes + size > MAX || tests.length >= 10000) { truncated = true; continue; }
    bytes += size; const text = fs.readFileSync(absolute, 'utf8');
    for (const test of bodies(text, path.extname(file))) tests.push({ ...test, path: file.replaceAll('\\', '/') });
  }
  return { tests:tests.slice(0,10000), truncated:truncated || tests.length>10000 };
}
// The measuring method with the most timed observations at `revision` (a fresh --execute run is a lane's).
function timingMethod(observations, revision) {
  const count = new Map();
  for (const o of observations) if (o.revision === revision && Number.isFinite(o.duration_ms)) count.set(o.method || 'lane', (count.get(o.method || 'lane') || 0) + 1);
  return [...count].sort((a, b) => b[1] - a[1])[0]?.[0];
}
function classify(observations, tests, revision) {
  const candidates = []; const by = new Map(); const signatures = new Map();
  for (const t of tests) { if (t.signature) { const group = signatures.get(t.signature) || []; group.push(t); signatures.set(t.signature, group); } }
  for (const o of observations) { if (o.revision !== revision || o.ambiguous || o.outcome === 'skip') continue; const group = by.get(o.id) || []; group.push(o); by.set(o.id, group); }
  const attach = (name) => { const matches = tests.filter(t => t.name === name || name.endsWith('::' + t.name)); return matches.length === 1 ? matches[0] : undefined; };
  const push = (kind, title, evidence, t, id) => candidates.push({ id: hash(kind + ':' + id).slice(0, 16), kind, title, evidence, path: t?.path, line: t?.line, source_hash: t?.hash });
  // Times are compared within one measuring method only (a CI runner, a lane, a log): the one
  // that timed the most observations.
  const method = timingMethod(observations, revision);
  const times = [];
  for (const [id, all] of by) {
    // One observation per run: a run's jobs and reports (a matrix, JUnit beside its console) are
    // one execution, where a failure anywhere outweighs a pass.
    const byRun = new Map(); for (const o of all) { const seen = byRun.get(o.run); if (!seen || (seen.outcome !== 'fail' && o.outcome === 'fail')) byRun.set(o.run, o); }
    const rows = [...byRun.values()]; const pass = rows.filter(o => o.outcome === 'pass').length, fail = rows.filter(o => o.outcome === 'fail').length;
    const t = attach(rows[0].name);
    // A pass that needed a retry; a retry that failed too is a failure like any other.
    const retried = rows.filter(o => o.flaky && o.outcome === 'pass').length;
    if (pass && fail) push('fix', 'Flaky: ' + id, `${pass} passing and ${fail} failing distinct runs at ${revision}. Sources: ${rows.map(o=>o.source).join(', ')}`, t, id);
    else if (retried) push('fix', 'Flaky: ' + id, `${retried} run(s) at ${revision} passed only after a failed attempt (the runner's retry). Sources: ${rows.filter(o => o.flaky && o.outcome === 'pass').map(o=>o.source).join(', ')}`, t, id);
    else if (fail >= 2) push('fix', 'Repeated failure: ' + id, `${fail} failing runs and no passing run at ${revision}. Repair or explicitly justify retirement; failure alone is not evidence to delete. Sources: ${rows.map(o=>o.source).join(', ')}`, t, id);
    // One time per run, from any of its reports that timed the test (JUnit, when a console did not).
    const timedRuns = new Map(); for (const o of all) if ((o.method || 'lane') === method && Number.isFinite(o.duration_ms)) timedRuns.set(o.run, Math.max(timedRuns.get(o.run) ?? 0, o.duration_ms));
    const durations = [...timedRuns.values()].sort((a,b) => a-b); if (durations.length) times.push({ id, t, ms: durations[Math.floor(durations.length / 2)], n: durations.length });
  }
  times.sort((a,b) => b.ms - a.ms);
  for (const t of times.slice(0, Math.ceil(times.length * .05))) push('speedup', 'Slow tail: ' + t.id, `Median ${t.ms.toFixed(2)} ms across ${t.n} observations timed by ${method}; top 5% of ${times.length} tests with timings, not of unmeasured tests.`, t.t, t.id);
  for (const group of signatures.values()) if (group.length > 1) {
    for (const t of group.slice(1)) push('inspect', 'Possible duplicate: ' + t.name, `Same normalized body as ${group[0].path}:${group[0].line}. Imports, fixtures and scope may differ. Verify equivalent contract before proposing deletion.`, t, t.path + ':' + t.line);
  }
  for (const t of tests.filter(t => t.mocks >= 3)) push('inspect', 'Mock coupling candidate: ' + t.name, `${t.mocks} mock/spy calls in extracted body. Heuristic only: inspect whether it protects behavior or just implementation.`, t, t.path + ':' + t.line);
  return candidates;
}
function main(args) {
  const opts = JSON.parse(read(args[0])); const root = process.cwd(); const revision = opts.revision || cp.execFileSync('git', ['rev-parse', 'HEAD'], { cwd: root }).toString().trim();
  const inv = inventory(root); const observations = []; const runs = []; const warnings = ['Static duplicates and mocks are review leads, never proof that deletion is safe.', 'A log without per-test times (libtest without --report-time) leaves timing unknown. Missing observations/timing are unknown, not passing.', 'Tests with duplicate display names are excluded from outcome classification.', 'Source extraction is conservative: macro-generated, parameterized and non-arrow JavaScript tests may be absent. Inventoried bodies are not a suite count.'];
  // Imported logs (JUnit XML, libtest, TAP) arrive already read by `nitely test-sweep collect`.
  const imported = opts.imported || {}; let evidenceTruncated = !!imported.truncated;
  observations.push(...(imported.observations || [])); runs.push(...(imported.runs || []));
  for (const r of runs.filter(r => r.partial)) warnings.push(`${r.source} (${r.run}) was not read whole: ${(r.warnings || []).join(' ')}`);
  if (opts.execute) {
    const end = Date.now() + Math.min(opts.seconds || 600, 3600) * 1000;
    for (let run = 0; run < Math.min(opts.runs || 2, 3); run++) {
      for (const [runner, file, command, argv] of [['rust', 'Cargo.toml', 'cargo', ['test', '--workspace']], ['node', 'package.json', process.platform === 'win32' ? 'npm.cmd' : 'npm', ['test', '--', '--test-reporter=tap']]]) {
        if (!fs.existsSync(file) || Date.now() >= end) continue;
        if (runner === 'rust' && opts.rust_timings) {
          const built = cp.spawnSync('cargo', ['test','--workspace','--no-run','--message-format=json'], {cwd:root, encoding:'utf8', timeout:Math.max(1,end-Date.now()),maxBuffer:MAX,windowsHide:true});
          if (built.status !== 0) { warnings.push('Rust timing build did not complete; timings unavailable.'); continue; }
          for (const line of (built.stdout || '').split('\n')) {
            let artifact; try { artifact=JSON.parse(line); } catch { continue; }
            if (!artifact.executable || !artifact.profile?.test || Date.now() >= end) continue;
            const listed=cp.spawnSync(artifact.executable,['--list'],{cwd:root,encoding:'utf8',timeout:Math.max(1,end-Date.now()),maxBuffer:MAX,windowsHide:true});
            const names=(listed.stdout || '').split(/\r?\n/).filter(s=>s.endsWith(': test')).map(s=>s.slice(0,-6));
            for (const name of names) {
              if(Date.now() >= end) break;
              const started=performance.now();
              const measured=cp.spawnSync(artifact.executable,['--exact',name,'--nocapture','--test-threads=1'],{cwd:root,encoding:'utf8',timeout:Math.max(1,end-Date.now()),maxBuffer:MAX,windowsHide:true});
              const rows=parseLog(measured.stdout || '',`fresh-${run}-rust`,revision,`bounded Rust timing run ${run+1}`);
              for(const row of rows) { row.id=artifact.target.name.replaceAll('-','_')+'::'+row.name; row.duration_ms=performance.now()-started; row.timing='isolated wall time including process startup'; }
              observations.push(...rows);
              if(measured.error) warnings.push(`Rust timing incomplete for ${name}; remaining observations unknown.`);
            }
          }
          continue;
        }
        // On Windows npm.cmd needs cmd.exe; all command text is fixed, never user supplied.
        const exe = process.platform === 'win32' && runner === 'node' ? 'cmd.exe' : command;
        const actual = exe === 'cmd.exe' ? ['/d', '/s', '/c', 'npm test'] : runner === 'node' ? ['test'] : argv;
        const started=performance.now();
        const out = merged(exe, actual, { cwd: root, timeout: Math.max(1, end-Date.now()), windowsHide: true });
        runs.push({runner,run:`fresh-${run}-${runner}`,revision,duration_ms:performance.now()-started,exit_code:out.status,partial:!!out.error||!!out.signal});
        const text = out.text;
        observations.push(...parseLog(text, `fresh-${run}-${runner}`, revision, `bounded ${runner} run ${run + 1}`));
        if (out.error || out.signal || out.status !== 0) warnings.push(`${runner} run ${run + 1}: exit ${out.status}, signal ${out.signal || 'none'}, incomplete/error ${!!out.error}; missing tests remain unknown.`);
      }
    }
  }
  const candidates = classify(observations, inv.tests, revision);
  const timedBy = timingMethod(observations, revision); const others = [...new Set(observations.filter(o => o.revision === revision && Number.isFinite(o.duration_ms) && (o.method || 'lane') !== timedBy).map(o => o.method || 'lane'))];
  if (others.length) warnings.push(`Times compare within one method: ${timedBy}. Times measured by ${others.join(', ')} are kept as observations but not compared with them.`);
  const known = new Set(opts.known_failures || []);
  for (const row of observations.filter(o=>o.revision===revision && o.outcome==='fail' && (known.has(o.id)||known.has(o.name)))) {
    if (!candidates.some(c=>c.title.endsWith(row.id))) candidates.push({id:hash('known:'+row.id).slice(0,16),kind:'fix',title:'Known failure: '+row.id,evidence:'Listed in the supplied known-failure list and failed in '+row.source+'. Known failures remain failures; repair or justify retirement.'});
  }
  if(opts.execute && opts.rust_timings) warnings.push('Rust timings are isolated wall times including process startup, not parallel-suite contribution. Compare before/after using the same method.'); const report = { version: 1, revision, generated_at: new Date().toISOString(), report_only: !opts.execute, runs, observations: observations.slice(0, 30000), tests: inv.tests, candidates: candidates.slice(0, 1000), truncated: evidenceTruncated || inv.truncated || observations.length > 30000 || candidates.length > 1000, warnings };
  fs.writeFileSync(opts.output, JSON.stringify(report, null, 2), { flag: 'wx' });
  console.log(JSON.stringify({ report: opts.output, revision, observed: observations.length, inventoried: inv.tests.length, candidates: candidates.length, truncated: report.truncated }));
}
module.exports = { merged, parseLog, bodies, classify, hints, timingMethod };
if (require.main === module) main(process.argv.slice(2));
