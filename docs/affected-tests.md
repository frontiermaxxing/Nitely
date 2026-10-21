# Affected-test selection

[NIT-300](http://127.0.0.1:7423/i/NIT-300) supplies revision-bound test selection. Shadow mode records advice; active
verification consumes a complete selection only with approved policy and healthy backstop. `affected_tests::select(repo, base, head)` reads only immutable commits.
The coordinator still applies protected-path, risk, size, platform and integration floors.
A project must explicitly approve activation before it receives reduced checks.

The optional `.nitely/affected-tests.toml` is a human-approved input inventory. Version 1
rejects unknown fields. Its blob comes from authoritative integration (which can be newer than the diff base); editing it in the candidate requires Full.
Missing policy, overlapping roots, missing inputs, an empty selection or an unsupported
dependency produces `full = true`, clears the proposed checks and explains the fallback.

## Approved Cargo mappings

Rust macro expansion, conditional modules, external fixtures and cross-crate consumers are
not inferred from filenames. The approved map must list **all** consumers of a mapped input,
including helper files and component dependencies. An omitted dependency is a policy defect;
this selector does not claim Rust semantic coverage. Approve a narrow helper mapping only
after checking its consumers. Inputs without that proof stay on Full.

```toml
version = 1

[[components]]
id = "arithmetic"
roots = ["crates/math/src/", "crates/math/tests/"]
checks = ["cargo clippy -p example-math --all-targets -- -D warnings"]

[[components.suites]]
id = "unit"
inputs = ["crates/math/src/arithmetic.rs", "crates/math/tests/helper.rs"]
[components.suites.cargo]
manifest = "crates/math/Cargo.toml"
package = "example-math"
target_kind = "lib"
filter = "arithmetic::tests::"
test_ids = ["arithmetic::tests::adds", "arithmetic::tests::subtracts"]

[[components.suites]]
id = "integration"
inputs = ["crates/math/tests/helper.rs", "crates/math/tests/arithmetic.rs"]
[components.suites.cargo]
manifest = "crates/math/Cargo.toml"
package = "example-math"
target_kind = "test"
target = "arithmetic"
```

Paths are exact repository-relative files or directory prefixes ending in `/`. They are
not globs. Changing the shared helper above selects both targets. `target_kind` accepts
`lib`, `bin` or `test`; bin/test require `target`. Package names and target source paths
are checked against the candidate's committed Cargo manifest. Custom harnesses, disabled
targets, required target features and build scripts require Full. Changed manifests,
lockfiles, generators and selector policy also require Full.

Omit `filter` to run a whole target. A filter requires explicit expected test IDs. Before
executing a Cargo check, an executor must successfully run **both** discovery commands
(`-- --list` and `-- --ignored --list`) from `check.discovery`, then call
`verify_cargo_discovery(check, listed, ignored)`. That rejects zero active tests and required
IDs that are absent or ignored. Discovery is not a passing test result: the actual check
must then succeed on the same revision and environment. The returned active IDs should be
recorded with check evidence; retain ignored output separately. Quarantine tools or custom
runners need an explicit integration before they can supply this evidence.

## JavaScript reverse imports and whole suites

```toml
version = 1

[[components]]
id = "web"
roots = ["src/"]
checks = ["npm run build"]

[[components.suites]]
id = "unit"
[components.suites.javascript]
command = "npm test"
test_files = ["src/arithmetic.test.ts", "src/form.test.ts"]
```

The selector parses every tracked JS/JSX/TS/TSX/MJS/CJS source in both base and candidate.
It follows static imports, re-exports and literal `require()` / `import()` calls backwards
from changed files. Both graphs are retained, so deleted files and removed imports keep
their old consumers. Changed tests select themselves; changed helpers select their reverse
consumers. Test files must exist in the candidate. Conventional `.test.` and `.spec.` files
inside a component must appear in its inventory. Instead of listing each file, a suite can
give `test_patterns` (`*` within one path segment, `**` for any number of segments), such as
`["src/**/*.test.ts", "scripts/*.test.mjs"]`: every tracked source file in the candidate that
a pattern matches belongs to the suite, so a new test file joins it without editing the map.
A pattern's literal directories must lie inside the component's roots and it must match at
least one file. A test file no pattern or list covers, or one outside every mapped root,
still requires Full. Other naming conventions need a list or pattern too; this is not
test-runner discovery.

Relative imports resolve only an unambiguous committed path, standard source extension,
index module or TypeScript source behind a `.js` import. Runtime loaders, aliases without
a graph, ambient modules, reference directives, escaped specifiers, missing imports and
ambiguous resolution require Full. A reverse consumer outside the approved component
inventory also requires Full. Bare package imports must resolve to a declared external
dependency; local package links, workspace packages and self-references require Full.
`node:` builtins are external. These conservative rules can reject a valid project; they
do not silently omit an unknown edge.

Custom bundler configuration and TypeScript resolver options (including inherited configs)
require an approved whole-suite mapping. To explicitly use that fallback, give **every**
JavaScript suite an `inputs` list, just like Cargo. For example, a single frontend suite
can map `inputs = ["src/", "scripts/"]` while owning those roots and retaining its complete
test-file inventory. Every tracked JavaScript file must belong to a component and appear
in at least one approved input mapping. Mixed inferred/explicit JS mappings are rejected.
This mode selects every listed test in a matching suite and does not parse opaque loaders
or claim inferred static coverage. The human-approved mapping must include their consumers
and cross-component dependencies. Missing mapping knowledge still requires Full.

`required_tests` lists affected **source test files**, not individual runtime test IDs.
The executable check remains the approved whole-suite command (`whole_suite = true`).
Nitely's existing `npm test` compiles and runs its entire test inventory; it does not accept
an affected-file manifest. The selector therefore never invents such an interface. A
future filtered runner must discover actual test IDs and prove they executed before this
can become reduced execution evidence. A passing command alone does not prove nonzero
JavaScript tests ran.

## Cross-component impact and evidence

Use `depends_on = ["arithmetic"]` on a consumer component to record approved edges not
represented by JavaScript imports, including native or Cargo boundaries. An affected
dependency selects **every** suite in its consumer, transitively. Static JavaScript edges
also identify mapped consumer components. The caller raises multi-component work to at
least Integration; this module returns component IDs rather than lowering that floor.
Component checks must include the applicable lint, type and build context. Commands are
trusted only because they come from the authoritative, approved inventory.

`Selection` binds full base/head commit IDs, candidate tree, authoritative policy blob,
selector/parser version, ordered commands, required test inventory and fallback reasons.
Persist it as advice with the verification plan. Execution evidence must additionally bind
toolchain/lockfile/features/platform/environment and successful discovery/results. Never
reuse an ordinary run or a different tree's results merely because its selected commands
match. Existing full-gate cache rules remain authoritative.

V1 bounds inventory at 10,000 tracked files, 512 KiB per input and 16 MiB of unique
JavaScript source. Exceeding a bound requires Full. No representative speedup is claimed:
collect selected/full timing on the same candidate during shadow evaluation before
proposing activation. No extra full gate is launched solely for a performance estimate.

## Measure without rerunning a full suite

`nitely verification --issue APP-123` and MCP `get_issue.verification_timing` compare
retained reduced-check evidence with the latest Full backstop attempt on that exact commit
and authoritative policy. They never launch tests. Successful executed local lane results
must supply all planned commands; cached, skipped, remote and flaky results do not count.
A newer failed or unfinished Full attempt prevents an older green result from becoming a
comparison. Reports retain at most the last twenty submission records.

Times are command seconds, excluding queue/setup. Load and cache warmth may differ, so
these observations are not normalized benchmarks or gate certificates. A sub-second Full
run has no percentage. An empty report means no matching pair has been measured yet;
allow the ordinary post-merge backstop to provide one instead of running Full again just
for a speedup number.
