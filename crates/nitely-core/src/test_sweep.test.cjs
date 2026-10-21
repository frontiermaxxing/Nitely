const { test } = require('node:test');
const assert = require('node:assert/strict');
const { merged, parseLog, classify, bodies } = require('./test_sweep.cjs');
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
});
