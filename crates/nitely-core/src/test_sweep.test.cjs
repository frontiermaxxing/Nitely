const { test } = require('node:test');
const assert = require('node:assert/strict');
const { merged, parseLog, classify, bodies, hints, timingMethod, inventory, untimed } = require('./test_sweep.cjs');
const revision = 'a'.repeat(40);
test('same-revision distinct runs distinguish flakes from repeats and ambiguous names', () => {
 const passed = parseLog('test parser::roundtrip ... ok', '1', revision, 'lane 1');
 const failed = parseLog('test parser::roundtrip ... FAILED', '2', revision, 'lane 2');
 assert.match(classify([...passed,...failed],[],revision)[0].title,/Flaky/);
 assert.equal(classify([...failed,...failed],[],revision).length,0);
 assert.equal(classify([...passed,...failed],[],'b'.repeat(40)).length,0);
 const ambiguous=parseLog('test parser ... ok\ntest parser ... FAILED','3',revision,'lane 3');
 assert.equal(classify(ambiguous,[],revision).length,0);
});
test('TAP durations and skips remain evidence without fabricated Rust timings', () => {
 const rows=parseLog('# Subtest: waits for lock\nok 1 - waits for lock\n  ---\n  duration_ms: 812.4\n  ...\nok 2 - pending # SKIP\ntest parser ... ok','1',revision,'lane 1');
 assert.equal(rows[0].duration_ms,812.4); assert.equal(rows[1].outcome,'skip'); assert.equal(rows[2].duration_ms,undefined);
 assert.match(classify(rows,[],revision)[0].evidence,/1 tests with timings/);
});
test('identical source bodies are inspection leads, never automatic removal', () => {
 const body='assert_eq!(roundtrip(input), input);'.repeat(5);
 const extracted=bodies(`#[test]\nfn first() { ${body} }\n#[test]\nfn second() { ${body} }`,'.rs').map(t=>({...t,path:'tests/contracts.rs'}));
 assert.equal(extracted.length,2);
 const candidates=classify([],extracted,revision);
 assert.equal(candidates.length,1); assert.equal(candidates[0].kind,'inspect'); assert.match(candidates[0].evidence,/scope may differ/);
});
test('report-only collection inventories sources and imports evidence without executing scripts', () => {
 const fs=require('node:fs'), os=require('node:os'), path=require('node:path'), cp=require('node:child_process');
 const root=fs.mkdtempSync(path.join(os.tmpdir(),'nitely-sweep-fixture-'));
 try {
   cp.execFileSync('git',['init','-q',root]);
   fs.writeFileSync(path.join(root,'package.json'),JSON.stringify({scripts:{test:'exit 99'}}));
   fs.writeFileSync(path.join(root,'contracts.test.js'),"test('keeps contract', () => { assert.equal(actual(), expected); });");
   cp.execFileSync('git',['-C',root,'add','.']);
   const output=path.join(root,'report.json'); const options=path.join(root,'options.json');
   fs.writeFileSync(options,JSON.stringify({revision,imported:{observations:[{id:'keeps contract',name:'keeps contract',runner:'node',outcome:'pass',run:'lane 10',revision,source:'prior.log',ambiguous:false,duration_ms:17}],runs:[{runner:'node',run:'lane 10',revision,source:'prior.log',partial:true,warnings:['cut']}]},output}));
   cp.execFileSync(process.execPath,[path.join(__dirname,'test_sweep.cjs'),options],{cwd:root});
   const report=JSON.parse(fs.readFileSync(output,'utf8'));
   assert.equal(report.report_only,true); assert.equal(report.tests.length,1); assert.equal(report.observations.length,1); assert.equal(report.observations[0].duration_ms,17); assert.equal(report.runs.length,1); assert.ok(report.warnings.some(w=>w.includes('prior.log (lane 10) was not read whole')));
 } finally { fs.rmSync(root,{recursive:true,force:true}); }
});
test('fresh runs name tests as imported logs do, so a pass and a failure of one test meet', () => {
  // The same samples test_evidence.rs reads: its ids are `shop::cart::adds`, `child` and `parent`.
  const rust = parseLog('     Running unittests src/lib.rs (target/debug/deps/shop-0a1b2c3d4e5f6789)\nrunning 1 test\ntest cart::adds ... ok <0.250s>', '1', revision, 'lane 1');
  assert.deepEqual(rust.map(r => [r.id, r.duration_ms]), [['shop::cart::adds', 250]]);
  const tap = parseLog('TAP version 13\n    ok 1 - child\n    1..1\nnot ok 1 - parent\n1..1', '1', revision, 'lane 1');
  assert.deepEqual(tap.map(r => [r.id, r.outcome]), [['child', 'pass'], ['parent', 'fail']]);
});
test('a fresh run keeps stderr and stdout in order, and a pass only on retry is a flake', () => {
  // Cargo's `Running` header goes to stderr and its results to stdout: read in order, the binary names the test.
  const out = merged(process.execPath, ['-e', "console.error('     Running unittests src/lib.rs (target/debug/deps/shop-0a1b2c3d4e5f6789)'); console.log('test cart::adds ... ok')"], {});
  assert.deepEqual(parseLog(out.text, '1', revision, 'lane 1').map(r => r.id), ['shop::cart::adds']);
  const retried = { id: 'cart::adds', name: 'adds', outcome: 'pass', run: 'ci-1', revision, source: 'CI 1', flaky: true, ambiguous: false };
  assert.match(classify([retried], [], revision)[0].title, /^Flaky: cart::adds/);
  // Every attempt failed in two runs: a repeated failure, not a flake.
  const failedRetry = (run) => ({ ...retried, run, outcome: 'fail' });
  assert.match(classify([failedRetry('ci-1'), failedRetry('ci-2')], [], revision)[0].title, /^Repeated failure: cart::adds/);
  assert.deepEqual(parseLog('test rejects - should panic ... FAILED', '1', revision, 'lane 1').map(r => [r.id, r.outcome]), [['rejects', 'fail']]);
});
test('test bodies in each language carry hints of waits, UI presence and real I/O', () => {
  const py = bodies('import time\n\ndef test_total():\n    assert total() == 3\n\nclass TestPage:\n    def test_title(self):\n        time.sleep(2)\n        assert "Cart" in html\n', '.py');
  assert.deepEqual(py.map(t => [t.name, t.line, t.hints]), [['test_total', 3, {}], ['test_title', 7, { waits: 1, ui: 1 }]]);
  const go = bodies('func TestClone(t *testing.T) {\n\tout, _ := exec.Command("git", "clone").Output()\n\tif len(out) == 0 { t.Fatal() }\n}\n', '.go');
  assert.deepEqual(go.map(t => [t.name, t.hints]), [['TestClone', { process: 1, git: 1 }]]);
  const java = bodies('@Test\nvoid opens() {\n  Thread.sleep(500);\n}\n', '.java');
  assert.deepEqual(java.map(t => [t.name, t.hints]), [['opens', { waits: 1 }]]);
  const cs = bodies('[Fact]\npublic async Task Saves() {\n  using var c = new SqlConnection(cs);\n}\n', '.cs');
  assert.deepEqual(cs.map(t => [t.name, t.hints]), [['Saves', { db: 1 }]]);
  const js = bodies("test('shows the board', () => { expect(screen.getByText('Board')).toBeInTheDocument(); });", '.tsx');
  assert.deepEqual(js.map(t => [t.name, t.hints]), [['shows the board', { ui: 2 }]]);
  assert.deepEqual(hints('assert_eq!(decide(&a, &b), Verdict::Land);'), {});
  // NIT-559: a duration or a timeout passed as a value is no wait, and the word html alone names a
  // file or a field; a blocking receive is a wait.
  assert.deepEqual(hints('assert!(keep_waiting(false, Duration::from_secs(5))); run(cap, timeout); write(dir.join("index.html"), page);'), {});
  assert.deepEqual(hints('let got = rx.recv_timeout(Duration::from_millis(50));'), { waits: 1 });
});
test('one CI run is one execution, and its failure on one platform outweighs a pass on another', () => {
  const at = (run, outcome, source) => ({ id: 'cart::adds', name: 'adds', outcome, run, revision, source, method: 'ci-log', ambiguous: false });
  // Two workflow runs, each failing on Windows and passing on Linux: a repeated failure, not a flake.
  const rows = [at('ci-1-1', 'fail', 'windows'), at('ci-1-1', 'pass', 'linux'), at('ci-2-1', 'pass', 'linux'), at('ci-2-1', 'fail', 'windows')];
  assert.match(classify(rows, [], revision)[0].title, /^Repeated failure: cart::adds/);
  // A console without times after JUnit with them leaves the run timed.
  const many = Array.from({ length: 19 }, (_, i) => ({ ...at('ci-9-1', 'pass', 'junit'), id: 'q' + i, name: 'q' + i, duration_ms: 5 }));
  const slow = classify([...many, { ...at('ci-9-1', 'pass', 'junit'), duration_ms: 9000 }, at('ci-9-1', 'pass', 'console')], [], revision).filter(c => c.kind === 'speedup');
  assert.deepEqual(slow.map(c => c.title), ['Slow tail: cart::adds']);
  // JUnit and the console of one run report one failure once.
  assert.equal(classify([at('ci-3-1', 'fail', 'junit'), at('ci-3-1', 'fail', 'console')], [], revision).length, 0);
});
test('times from CI and from a lane are never compared with each other', () => {
  // Twenty tests timed in CI, one far slower test timed only in a lane: the lane's time is not ranked against CI's.
  const ci = Array.from({ length: 20 }, (_, i) => ({ id: 't' + i, name: 't' + i, outcome: 'pass', run: 'ci-1', revision, source: 'CI', method: 'ci-log', duration_ms: 10 + i }));
  const lane = { id: 'slow', name: 'slow', outcome: 'pass', run: 'lane-1', revision, source: 'lane 1', method: 'lane', duration_ms: 99999 };
  assert.equal(timingMethod([...ci, lane], revision), 'ci-log');
  const slow = classify([...ci, lane], [], revision).filter(c => c.kind === 'speedup');
  assert.deepEqual(slow.map(c => c.title), ['Slow tail: t19']);
  assert.match(slow[0].evidence, /timed by ci-log/);
});
test('a report on an earlier commit reads that commit\'s tests, not the checkout\'s', () => {
  const fs = require('node:fs'), os = require('node:os'), path = require('node:path'), cp = require('node:child_process');
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'nitely-sweep-at-'));
  const git = (...a) => cp.execFileSync('git', ['-c', 'user.name=t', '-c', 'user.email=t@example.com', ...a], { cwd: root }).toString().trim();
  try {
    git('init', '-q');
    fs.writeFileSync(path.join(root, 'cart.test.js'), "test('adds', () => { setTimeout(done, 10); });");
    git('add', '.'); git('commit', '-qm', 'one'); const first = git('rev-parse', 'HEAD');
    fs.writeFileSync(path.join(root, 'cart.test.js'), "test('totals', () => { expect(1).toBe(1); });");
    git('commit', '-qam', 'two');
    assert.deepEqual(inventory(root, first).tests.map(t => [t.name, t.hints]), [['adds', { waits: 1 }]]);
    assert.deepEqual(inventory(root).tests.map(t => t.name), ['totals']);
  } finally { fs.rmSync(root, { recursive: true, force: true }); }
});
test('a test written inside a string is a fixture, not a test', () => {
  const js = "test('real', () => { const fixture = \"test('fake', () => { waitFor(x); })\"; expect(run(fixture)).toBe(1); });";
  assert.deepEqual(bodies(js, '.js').map(t => t.name), ['real']);
  const py = 'def test_real():\n    src = """\ndef test_fake():\n    time.sleep(1)\n"""\n    assert run(src)\n';
  assert.deepEqual(bodies(py, '.py').map(t => t.name), ['test_real']);
  // An apostrophe in a regex literal opens no string: the tests after it are still read.
  const re = "test('one', () => { assert.match(said, /a machine's 25 ms/); });\ntest('two', () => { ok(); });";
  assert.deepEqual(bodies(re, '.ts').map(t => t.name), ['one', 'two']);
  const rs = "#[test]\nfn a() { let r: &'static str = x; }\n#[test]\nfn b() { ok(); }";
  assert.deepEqual(bodies(rs, '.rs').map(t => t.name), ['a', 'b'], 'a lifetime is no string');
});
test('a source whose tests carry no time is named, since none of them can be found slow', () => {
  const o = (source, ms, outcome = 'pass') => ({ id: 'x', source, outcome, ...(ms === undefined ? {} : { duration_ms: ms }) });
  const said = untimed([o('Gate (ubuntu-latest)', undefined), o('Gate (ubuntu-latest)', undefined), o('Gate (ubuntu-latest)', 12), o('suite', 40), o('suite', undefined, 'skip')]);
  assert.equal(said.length, 1, 'the timed source, whose only untimed test was skipped, is not named');
  assert.match(said[0], /^Gate \(ubuntu-latest\): 2 of 3 tests carry no time/);
});
