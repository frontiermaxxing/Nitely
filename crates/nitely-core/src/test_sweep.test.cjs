const { test } = require('node:test');
const assert = require('node:assert/strict');
const { parseLog, classify, bodies } = require('./test_sweep.cjs');
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
   const log=path.join(root,'prior.log'); fs.writeFileSync(log,'# Subtest: keeps contract\nok 1 - keeps contract\n  ---\n  duration_ms: 17\n  ...');
   const output=path.join(root,'report.json'); const options=path.join(root,'options.json');
   fs.writeFileSync(options,JSON.stringify({revision,logs:[{path:log,revision,run:'lane 10'}],output}));
   cp.execFileSync(process.execPath,[path.join(__dirname,'test_sweep.cjs'),options],{cwd:root});
   const report=JSON.parse(fs.readFileSync(output,'utf8'));
   assert.equal(report.report_only,true); assert.equal(report.tests.length,1); assert.equal(report.observations.length,1); assert.equal(report.observations[0].duration_ms,17); assert.equal(report.runs.length,0);
 } finally { fs.rmSync(root,{recursive:true,force:true}); }
});
