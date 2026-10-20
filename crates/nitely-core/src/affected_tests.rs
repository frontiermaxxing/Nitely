//! Revision-bound affected-test advice. An approved input map supplies Rust edges; JavaScript
//! edges come from both immutable syntax trees. This module never authorizes a reduced gate.
use crate::playbook::git;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};
use tree_sitter::{Node, Parser};

pub const POLICY_PATH: &str = ".nitely/affected-tests.toml";
pub const SELECTOR: &str = "affected-v1-ts0271-ts0232-js0250";
const MAX_FILES: usize = 10_000;
const MAX_SOURCE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Selection {
    pub base: String,
    pub head: String,
    pub tree: String,
    pub policy_blob: Option<String>,
    pub selector: String,
    pub full: bool,
    pub components: Vec<String>,
    /// Ordered commands are part of the evidence identity, never a cross-tree cache key.
    pub checks: Vec<Check>,
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Check {
    pub component: String,
    pub suite: Option<String>,
    pub command: String,
    /// JavaScript IDs are source test files. Cargo IDs are runner-reported test names.
    pub required_tests: Vec<String>,
    pub discovery: Option<CargoDiscovery>,
    pub whole_suite: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CargoDiscovery {
    pub command: String,
    pub ignored_command: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Policy {
    version: u32,
    components: Vec<Component>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Component {
    id: String,
    /// Exact files or directory prefixes ending in '/', never regular expressions.
    roots: Vec<String>,
    #[serde(default)]
    depends_on: Vec<String>,
    checks: Vec<String>,
    suites: Vec<Suite>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Suite {
    id: String,
    /// Approved reverse-consumer mapping, including shared fixtures/helpers. On JavaScript
    /// suites this explicitly chooses whole-suite mapping instead of static graph inference.
    #[serde(default)]
    inputs: Vec<String>,
    #[serde(default)]
    cargo: Option<CargoSuite>,
    #[serde(default)]
    javascript: Option<JsSuite>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CargoSuite {
    manifest: String,
    package: String,
    target_kind: TargetKind,
    /// Required for bin/test; absent for lib.
    #[serde(default)]
    target: Option<String>,
    #[serde(default)]
    filter: Option<String>,
    #[serde(default)]
    test_ids: Vec<String>,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum TargetKind {
    Lib,
    Bin,
    Test,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct JsSuite {
    /// A trusted whole-component runner; v1 does not invent a filename-filter interface.
    command: String,
    test_files: Vec<String>,
}

#[derive(Debug, Clone)]
struct Entry {
    mode: String,
    oid: String,
}
type Snapshot = BTreeMap<String, Entry>;
type Sources = BTreeMap<String, String>;

fn snapshot(repo: &Path, revision: &str) -> Result<Snapshot> {
    let text = git(repo, &["ls-tree", "-r", "-z", revision])?;
    let mut files = BTreeMap::new();
    for entry in text.split('\0').filter(|s| !s.is_empty()) {
        let (meta, path) = entry.split_once('\t').context("malformed tree entry")?;
        let fields: Vec<_> = meta.split_whitespace().collect();
        if fields.len() != 3 {
            bail!("malformed tree metadata");
        }
        files.insert(
            path.into(),
            Entry {
                mode: fields[0].into(),
                oid: fields[2].into(),
            },
        );
        if files.len() > MAX_FILES {
            bail!("repository exceeds selector inventory limit");
        }
    }
    Ok(files)
}

fn read(repo: &Path, files: &Snapshot, path: &str) -> Result<String> {
    let e = files
        .get(path)
        .with_context(|| format!("missing input {path}"))?;
    if !matches!(e.mode.as_str(), "100644" | "100755") {
        bail!("nonregular input {path}");
    }
    let size: usize = git(repo, &["cat-file", "-s", &e.oid])?.trim().parse()?;
    if size > 512 * 1024 {
        bail!("input exceeds selector file limit: {path}");
    }
    git(repo, &["cat-file", "blob", &e.oid])
}

fn source_path(path: &str) -> bool {
    matches!(
        path.rsplit('.').next(),
        Some("js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx")
    )
}

fn under(path: &str, input: &str) -> bool {
    path == input || (input.ends_with('/') && path.starts_with(input))
}

fn valid_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains(['\\', ':', '\0'])
        && path
            .trim_end_matches('/')
            .split('/')
            .all(|p| !p.is_empty() && p != "." && p != "..")
}

fn token(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('-')
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_-/.:".contains(&c))
}

fn protected(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    path.starts_with('.')
        || matches!(
            name,
            "Cargo.toml" | "Cargo.lock" | "package.json" | "package-lock.json" | "build.rs"
        )
        || name.starts_with("tsconfig")
        || name.contains("lock")
        || path.split('/').any(|p| matches!(p, "generated" | "vendor"))
}

/// The only policy source is the immutable base commit. `full == true` means ignore every
/// proposed reduced check (checks are cleared). Callers must additionally apply risk floors.
pub fn select(repo: &Path, base: &str, head: &str) -> Result<Selection> {
    select_with_policy(repo,base,head,base)
}

pub fn select_with_policy(repo: &Path, base: &str, head: &str, policy_commit:&str) -> Result<Selection> {
    let base = git(
        repo,
        &["rev-parse", "--verify", &format!("{base}^{{commit}}")],
    )?
    .trim()
    .to_owned();
    let head = git(
        repo,
        &["rev-parse", "--verify", &format!("{head}^{{commit}}")],
    )?
    .trim()
    .to_owned();
    let tree = git(repo, &["rev-parse", &format!("{head}^{{tree}}")])?
        .trim()
        .to_owned();
    let mut selection = Selection {
        base,
        head,
        tree,
        policy_blob: None,
        selector: SELECTOR.into(),
        full: true,
        components: vec![],
        checks: vec![],
        reasons: vec![],
    };
    if let Err(e) = select_inner(repo, &mut selection, policy_commit) {
        selection.full = true;
        selection.checks.clear();
        selection.reasons.push(format!("Full fallback: {e:#}"));
    }
    Ok(selection)
}

fn validate(policy: &Policy, before: &Snapshot, after: &Snapshot) -> Result<()> {
    if policy.version != 1 || policy.components.is_empty() {
        bail!("unsupported or empty affected-test policy");
    }
    let mut ids = BTreeSet::new();
    for c in &policy.components {
        if !ids.insert(&c.id)
            || c.id.trim().is_empty()
            || c.roots.is_empty()
            || c.checks.is_empty()
            || c.suites.is_empty()
        {
            bail!("incomplete or duplicate component inventory");
        }
        if c.checks.iter().any(|s| s.trim().is_empty()) {
            bail!("empty component check");
        }
        let mut suites = BTreeSet::new();
        for root in &c.roots {
            if !valid_path(root) || !before.keys().chain(after.keys()).any(|p| under(p, root)) {
                bail!("invalid or stale component root: {root}");
            }
        }
        for s in &c.suites {
            if !suites.insert(&s.id)
                || s.id.trim().is_empty()
                || s.cargo.is_some() == s.javascript.is_some()
            {
                bail!("invalid or duplicate suite inventory");
            }
            for input in &s.inputs {
                if !valid_path(input) || !before.keys().chain(after.keys()).any(|p| under(p, input))
                {
                    bail!("invalid or stale approved input: {input}");
                }
            }
            if let Some(js) = &s.javascript {
                if js.command.trim().is_empty() || js.test_files.is_empty() {
                    bail!("empty JavaScript suite");
                }
                for test in &js.test_files {
                    if !valid_path(test)
                        || !source_path(test)
                        || !after.contains_key(test)
                        || !c.roots.iter().any(|r| under(test, r))
                    {
                        bail!("missing or out-of-component JavaScript test: {test}");
                    }
                }
            }
            if let Some(cargo) = &s.cargo {
                if s.inputs.is_empty()
                    || !valid_path(&cargo.manifest)
                    || !token(&cargo.manifest)
                    || !token(&cargo.package)
                {
                    bail!("incomplete Cargo mapping");
                }
                if cargo
                    .filter
                    .as_ref()
                    .is_some_and(|f| !token(f) || cargo.test_ids.is_empty())
                {
                    bail!("Cargo filters need explicit test IDs and safe syntax");
                }
                if cargo
                    .test_ids
                    .iter()
                    .any(|id| !token(id) || cargo.filter.as_ref().is_some_and(|f| !id.contains(f)))
                {
                    bail!("invalid Cargo test inventory");
                }
            }
        }
    }
    for c in &policy.components {
        if c.depends_on
            .iter()
            .any(|id| id == &c.id || !ids.contains(id))
        {
            bail!("unknown component dependency");
        }
        if c.suites.iter().any(|s| s.javascript.is_some()) {
            for path in after.keys().filter(|p| {
                c.roots.iter().any(|r| under(p, r))
                    && source_path(p)
                    && (p.contains(".test.") || p.contains(".spec."))
            }) {
                if !c
                    .suites
                    .iter()
                    .filter_map(|s| s.javascript.as_ref())
                    .any(|s| s.test_files.contains(path))
                {
                    bail!("JavaScript test missing from inventory: {path}");
                }
            }
        }
    }
    // Ambiguous ownership would hide dependencies at the boundary.
    for path in before.keys().chain(after.keys()) {
        if policy
            .components
            .iter()
            .filter(|c| c.roots.iter().any(|r| under(path, r)))
            .count()
            > 1
        {
            bail!("overlapping component roots at {path}");
        }
    }
    Ok(())
}

fn owner<'a>(policy: &'a Policy, path: &str) -> Option<&'a Component> {
    policy
        .components
        .iter()
        .find(|c| c.roots.iter().any(|r| under(path, r)))
}

fn select_inner(repo: &Path, result: &mut Selection, policy_commit:&str) -> Result<()> {
    let before = snapshot(repo, &result.base)?;
    let after = snapshot(repo, &result.head)?;
    let policy_files=snapshot(repo,policy_commit)?;
    result.policy_blob = policy_files.get(POLICY_PATH).map(|e| e.oid.clone());
    let policy: Policy = toml::from_str(&read(repo, &policy_files, POLICY_PATH)?)?;
    validate(&policy, &before, &after)?;
    let changed: BTreeSet<_> = before
        .keys()
        .chain(after.keys())
        .filter(|p| {
            before.get(*p).map(|e| (&e.oid, &e.mode)) != after.get(*p).map(|e| (&e.oid, &e.mode))
        })
        .cloned()
        .collect();
    if changed.is_empty() {
        bail!("empty behavioral selection");
    }
    let mut affected = BTreeSet::new();
    for path in &changed {
        if protected(path) {
            bail!("changed policy, manifest, generator or configuration: {path}");
        }
        for files in [&before, &after] {
            if files
                .get(path)
                .is_some_and(|e| !matches!(e.mode.as_str(), "100644" | "100755"))
            {
                bail!("nonregular changed input: {path}");
            }
        }
        let c = owner(&policy, path).with_context(|| format!("unmapped changed input: {path}"))?;
        affected.insert(c.id.clone());
    }

    // Scan the whole tracked JS inventory: a consumer outside mapped roots is not invisible.
    // Parse both sides so deleting or moving an import cannot erase the old reverse edge.
    let needs_js = changed.iter().any(|p| {
        source_path(p)
            || owner(&policy, p).is_some_and(|c| c.suites.iter().any(|s| s.javascript.is_some()))
    });
    let explicit_js = policy
        .components
        .iter()
        .flat_map(|c| &c.suites)
        .any(|s| s.javascript.is_some() && !s.inputs.is_empty());
    if explicit_js {
        if policy
            .components
            .iter()
            .flat_map(|c| &c.suites)
            .any(|s| s.javascript.is_some() && s.inputs.is_empty())
        {
            bail!("mixed explicit and inferred JavaScript mappings");
        }
        // An approved whole-suite fallback must account for the entire tracked JS domain,
        // not conceal a consumer outside its roots because static parsing was inconvenient.
        for path in before.keys().chain(after.keys()).filter(|p| source_path(p)) {
            if owner(&policy, path).is_none()
                || !policy
                    .components
                    .iter()
                    .flat_map(|c| &c.suites)
                    .any(|s| s.javascript.is_some() && s.inputs.iter().any(|i| under(path, i)))
            {
                bail!("incomplete explicit JavaScript inventory: {path}");
            }
        }
        result.reasons.push(
            "JavaScript uses approved whole-suite input mappings, not inferred static coverage."
                .into(),
        );
    }
    let graph = if needs_js && !explicit_js {
        js_graph(repo, &before, &after)?
    } else {
        BTreeMap::new()
    };
    let mut reached = changed.clone();
    loop {
        let old = reached.len();
        for (consumer, inputs) in &graph {
            if inputs.iter().any(|input| reached.contains(input)) {
                reached.insert(consumer.clone());
            }
        }
        if old == reached.len() {
            break;
        }
    }
    for path in &reached {
        if let Some(c) = owner(&policy, path) {
            affected.insert(c.id.clone());
        } else {
            bail!("reverse consumer outside approved component inventory: {path}");
        }
    }
    for c in &policy.components {
        if c.suites
            .iter()
            .any(|s| reached.iter().any(|p| s.inputs.iter().any(|i| under(p, i))))
        {
            affected.insert(c.id.clone());
        }
    }
    loop {
        let old = affected.len();
        for c in &policy.components {
            if c.depends_on.iter().any(|id| affected.contains(id)) {
                affected.insert(c.id.clone());
            }
        }
        if old == affected.len() {
            break;
        }
    }

    for c in policy
        .components
        .iter()
        .filter(|c| affected.contains(&c.id))
    {
        result.components.push(c.id.clone());
        for command in &c.checks {
            result.checks.push(Check {
                component: c.id.clone(),
                suite: None,
                command: command.clone(),
                required_tests: vec![],
                discovery: None,
                whole_suite: false,
            });
        }
        // Dependency-only selection deliberately runs every suite in the consumer component.
        let dependency = c.depends_on.iter().any(|id| affected.contains(id));
        let mut selected = 0;
        for s in &c.suites {
            if let Some(cargo) = &s.cargo {
                if dependency || reached.iter().any(|p| s.inputs.iter().any(|i| under(p, i))) {
                    result.checks.push(cargo_check(repo, &after, c, s, cargo)?);
                    selected += 1;
                }
            }
            if let Some(js) = &s.javascript {
                let approved =
                    explicit_js && reached.iter().any(|p| s.inputs.iter().any(|i| under(p, i)));
                let tests: Vec<_> = js
                    .test_files
                    .iter()
                    .filter(|p| dependency || approved || reached.contains(*p))
                    .cloned()
                    .collect();
                if !tests.is_empty() {
                    result.checks.push(Check {
                        component: c.id.clone(),
                        suite: Some(s.id.clone()),
                        command: js.command.clone(),
                        required_tests: tests,
                        discovery: None,
                        whole_suite: true,
                    });
                    selected += 1;
                }
            }
        }
        if selected == 0 {
            bail!("no affected tests in component {}", c.id);
        }
        for path in changed
            .iter()
            .filter(|p| owner(&policy, p).is_some_and(|o| o.id == c.id))
        {
            let mapped = c
                .suites
                .iter()
                .any(|s| s.inputs.iter().any(|i| under(path, i)))
                || c.suites
                    .iter()
                    .filter_map(|s| s.javascript.as_ref())
                    .any(|s| s.test_files.iter().any(|t| reaches(&graph, t, path)));
            if !mapped {
                bail!("changed input has no test consumer: {path}");
            }
        }
    }
    result.full = false;
    result.reasons.push("Approved input mappings or static JavaScript reverse imports; JavaScript runs whole suites. Discovery is required before Cargo execution. Risk floors and the existing gate still apply.".into());
    Ok(())
}

fn reaches(graph: &BTreeMap<String, BTreeSet<String>>, start: &str, target: &str) -> bool {
    let mut pending = vec![start];
    let mut seen = BTreeSet::new();
    while let Some(path) = pending.pop() {
        if path == target {
            return true;
        }
        if seen.insert(path) {
            if let Some(inputs) = graph.get(path) {
                pending.extend(inputs.iter().map(String::as_str));
            }
        }
    }
    false
}

fn cargo_check(
    repo: &Path,
    files: &Snapshot,
    c: &Component,
    s: &Suite,
    suite: &CargoSuite,
) -> Result<Check> {
    let manifest: toml::Value = toml::from_str(&read(repo, files, &suite.manifest)?)?;
    if manifest
        .get("package")
        .and_then(|p| p.get("name"))
        .and_then(|v| v.as_str())
        != Some(&suite.package)
    {
        bail!("Cargo package mapping is stale");
    }
    let root = suite
        .manifest
        .strip_suffix("Cargo.toml")
        .context("not a Cargo manifest")?;
    if files.contains_key(&format!("{root}build.rs"))
        || manifest
            .get("package")
            .and_then(|p| p.get("build"))
            .is_some_and(|v| v.as_bool() != Some(false))
    {
        bail!("generated Cargo inputs require Full");
    }
    let (flag, target_path, target_table) = match suite.target_kind {
        TargetKind::Lib => {
            if suite.target.is_some() {
                bail!("lib target must not name a bin/test");
            }
            let lib = manifest.get("lib");
            let path = lib
                .and_then(|t| t.get("path"))
                .and_then(|v| v.as_str())
                .unwrap_or("src/lib.rs");
            ("--lib".into(), format!("{root}{path}"), lib)
        }
        TargetKind::Bin | TargetKind::Test => {
            let name = suite.target.as_deref().context("missing Cargo target")?;
            if !token(name) {
                bail!("invalid Cargo target name");
            }
            let kind = if suite.target_kind == TargetKind::Bin {
                "bin"
            } else {
                "test"
            };
            let table = manifest.get(kind).and_then(|v| v.as_array()).and_then(|a| {
                a.iter()
                    .find(|t| t.get("name").and_then(|v| v.as_str()) == Some(name))
            });
            let default = if kind == "bin" && name == suite.package {
                "src/main.rs".to_owned()
            } else if kind == "bin" {
                format!("src/bin/{name}.rs")
            } else {
                format!("tests/{name}.rs")
            };
            let path = table
                .and_then(|t| t.get("path"))
                .and_then(|v| v.as_str())
                .unwrap_or(&default);
            (format!("--{kind} {name}"), format!("{root}{path}"), table)
        }
    };
    if !valid_path(&target_path) || !files.contains_key(&target_path) {
        bail!("missing Cargo target source");
    }
    if target_table.is_some_and(|t| {
        t.get("required-features").is_some()
            || t.get("harness").and_then(|v| v.as_bool()) == Some(false)
            || t.get("test").and_then(|v| v.as_bool()) == Some(false)
    }) {
        bail!("unsupported Cargo target features/harness");
    }
    let filter = suite
        .filter
        .as_ref()
        .map(|s| format!(" {s}"))
        .unwrap_or_default();
    let command = format!(
        "cargo test --manifest-path {} -p {} {flag}{filter}",
        suite.manifest, suite.package
    );
    Ok(Check {
        component: c.id.clone(),
        suite: Some(s.id.clone()),
        discovery: Some(CargoDiscovery {
            command: format!("{command} -- --list"),
            ignored_command: format!("{command} -- --ignored --list"),
        }),
        command,
        required_tests: suite.test_ids.clone(),
        whole_suite: suite.filter.is_none(),
    })
}

/// Validate successful `--list` and `--ignored --list` outputs before executing a Cargo check.
/// Ignored tests cannot satisfy required coverage. Callers must still require successful test
/// execution and persist output/results against the complete Selection identity.
pub fn verify_cargo_discovery(check: &Check, listed: &str, ignored: &str) -> Result<Vec<String>> {
    if check.discovery.is_none() {
        bail!("not a Cargo discovery check");
    }
    let parse = |text: &str| -> BTreeSet<String> {
        text.lines()
            .filter_map(|l| l.strip_suffix(": test"))
            .map(str::to_owned)
            .collect()
    };
    let ignored = parse(ignored);
    let active: BTreeSet<_> = parse(listed).difference(&ignored).cloned().collect();
    if active.is_empty() {
        bail!("Cargo discovery selected zero nonignored tests");
    }
    for id in &check.required_tests {
        if !active.contains(id) {
            bail!("required test missing or ignored: {id}");
        }
    }
    Ok(active.into_iter().collect())
}

fn js_graph(
    repo: &Path,
    before: &Snapshot,
    after: &Snapshot,
) -> Result<BTreeMap<String, BTreeSet<String>>> {
    let mut graph = BTreeMap::<String, BTreeSet<String>>::new();
    let mut cache = Sources::new();
    let mut bytes = 0;
    for files in [before, after] {
        for path in files.keys() {
            let name = path.rsplit('/').next().unwrap_or(path);
            if [
                "vite.config.",
                "webpack.config.",
                "rollup.config.",
                "babel.config.",
                "jest.config.",
            ]
            .iter()
            .any(|prefix| name.starts_with(prefix))
            {
                bail!("custom JavaScript resolver requires an approved whole-suite mapping");
            }
            if name.starts_with("tsconfig") && name.ends_with(".json") {
                let config: serde_json::Value = serde_json::from_str(&read(repo, files, path)?)
                    .context("non-JSON TypeScript resolver configuration")?;
                if config.get("extends").is_some()
                    || config.get("compilerOptions").is_some_and(|c| {
                        ["paths", "baseUrl", "rootDirs", "moduleSuffixes"]
                            .iter()
                            .any(|k| c.get(k).is_some())
                    })
                {
                    bail!("custom TypeScript resolver requires an approved whole-suite mapping");
                }
            }
        }
        for (path, entry) in files.iter().filter(|(p, _)| source_path(p)) {
            let source = if let Some(source) = cache.get(&entry.oid) {
                source.clone()
            } else {
                let source = read(repo, files, path)?;
                bytes += source.len();
                if bytes > MAX_SOURCE_BYTES {
                    bail!("JavaScript inventory exceeds byte limit");
                }
                cache.insert(entry.oid.clone(), source.clone());
                source
            };
            let imports = imports(path, &source)?;
            for specifier in imports {
                if specifier.starts_with('.') {
                    let input = resolve(files, path, &specifier)?;
                    graph.entry(path.clone()).or_default().insert(input);
                } else if !external(repo, files, path, &specifier)? {
                    bail!("unresolved package/alias in {path}: {specifier}");
                }
            }
        }
    }
    Ok(graph)
}

fn external(repo: &Path, files: &Snapshot, path: &str, specifier: &str) -> Result<bool> {
    if specifier.starts_with("node:") {
        return Ok(true);
    }
    let package = if specifier.starts_with('@') {
        specifier.split('/').take(2).collect::<Vec<_>>().join("/")
    } else {
        specifier.split('/').next().unwrap_or("").into()
    };
    let mut parts: Vec<_> = path.split('/').collect();
    parts.pop();
    loop {
        let manifest = if parts.is_empty() {
            "package.json".into()
        } else {
            format!("{}/package.json", parts.join("/"))
        };
        if files.contains_key(&manifest) {
            let value: serde_json::Value = serde_json::from_str(&read(repo, files, &manifest)?)?;
            // Workspaces/self-references can refer to local source, so they are never external.
            if value.get("workspaces").is_some()
                || value.get("name").and_then(|v| v.as_str()) == Some(&package)
            {
                return Ok(false);
            }
            return Ok(["dependencies", "devDependencies", "peerDependencies"]
                .iter()
                .any(|k| {
                    value
                        .get(k)
                        .and_then(|v| v.get(&package))
                        .and_then(|v| v.as_str())
                        .is_some_and(|v| {
                            !["file:", "link:", "workspace:"]
                                .iter()
                                .any(|prefix| v.starts_with(prefix))
                        })
                }));
        }
        if parts.pop().is_none() {
            return Ok(false);
        }
    }
}

fn resolve(files: &Snapshot, consumer: &str, specifier: &str) -> Result<String> {
    if specifier.contains(['\\', '?', '#']) {
        bail!("unsupported import resolver syntax");
    }
    let mut parts: Vec<_> = consumer.split('/').collect();
    parts.pop();
    for part in specifier.split('/') {
        match part {
            "." => {}
            ".." => {
                if parts.pop().is_none() {
                    bail!("import leaves repository");
                }
            }
            "" => bail!("empty import path segment"),
            _ => parts.push(part),
        }
    }
    let stem = parts.join("/");
    let mut candidates = BTreeSet::new();
    candidates.insert(stem.clone());
    for extension in ["ts", "tsx", "js", "jsx", "mjs", "cjs"] {
        candidates.insert(format!("{stem}.{extension}"));
        candidates.insert(format!("{stem}/index.{extension}"));
    }
    // TypeScript commonly writes emitted .js paths. Ambiguous alternatives fail closed.
    if let Some(stem) = stem.strip_suffix(".js") {
        candidates.insert(format!("{stem}.ts"));
        candidates.insert(format!("{stem}.tsx"));
    }
    let matches: Vec<_> = candidates
        .into_iter()
        .filter(|p| files.contains_key(p))
        .collect();
    if matches.len() != 1 {
        bail!("missing or ambiguous import from {consumer}: {specifier}");
    }
    Ok(matches[0].clone())
}

fn imports(path: &str, source: &str) -> Result<BTreeSet<String>> {
    if source.contains("<reference") {
        bail!("TypeScript reference directive needs an explicit graph");
    }
    let lang = if path.ends_with(".tsx") {
        tree_sitter_typescript::LANGUAGE_TSX.into()
    } else if path.ends_with(".ts") {
        tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
    } else {
        tree_sitter_javascript::LANGUAGE.into()
    };
    let mut parser = Parser::new();
    parser.set_language(&lang)?;
    let tree = parser.parse(source, None).context("missing syntax tree")?;
    if tree.root_node().has_error() {
        bail!("JavaScript parse error in {path}");
    }
    let mut out = BTreeSet::new();
    imports_node(tree.root_node(), source, &mut out, 0).with_context(|| format!("in {path}"))?;
    Ok(out)
}

fn string(node: Node<'_>, source: &str) -> Result<String> {
    let text = node.utf8_text(source.as_bytes())?;
    if node.kind() != "string" || text.len() < 2 || text.contains(['\\', '\n', '\r']) {
        bail!("nonliteral or escaped module specifier");
    }
    let specifier = &text[1..text.len() - 1];
    if matches!(
        specifier,
        "node:module" | "module" | "node:vm" | "vm" | "node:worker_threads" | "worker_threads"
    ) {
        bail!("runtime loader builtin requires approved input mapping");
    }
    Ok(specifier.into())
}

fn imports_node(
    node: Node<'_>,
    source: &str,
    out: &mut BTreeSet<String>,
    depth: usize,
) -> Result<()> {
    if depth > 256 {
        bail!("syntax depth exceeds inventory limit");
    }
    if matches!(
        node.kind(),
        "ambient_declaration" | "internal_module" | "import_alias" | "meta_property"
    ) {
        bail!("ambient or runtime module dependency");
    }
    if node.kind() == "new_expression"
        && node
            .child_by_field_name("constructor")
            .and_then(|n| n.utf8_text(source.as_bytes()).ok())
            .is_some_and(|s| matches!(s, "URL" | "Worker" | "SharedWorker" | "Function"))
    {
        bail!("runtime module or code construction");
    }
    if matches!(node.kind(), "import_statement" | "export_statement") {
        if let Some(input) = node.child_by_field_name("source") {
            out.insert(string(input, source)?);
        } else if node.kind() == "import_statement" {
            bail!("unsupported import form");
        }
    }
    if node.kind() == "call_expression" {
        if let Some(function) = node.child_by_field_name("function") {
            let name = function.utf8_text(source.as_bytes())?;
            if name == "import" || name == "require" {
                let arguments = node
                    .child_by_field_name("arguments")
                    .context("missing import arguments")?;
                if arguments.named_child_count() != 1 {
                    bail!("unsupported module arguments");
                }
                out.insert(string(
                    arguments.named_child(0).context("missing module input")?,
                    source,
                )?);
            } else if name.contains("require")
                || name.contains("import")
                || name == "eval"
                || name == "Function"
            {
                bail!("opaque module loader requires Full");
            }
        }
    }
    // Aliasing a loader or ambient module declaration invalidates static closure.
    if node.kind() == "identifier"
        && matches!(node.utf8_text(source.as_bytes())?, "require" | "eval")
    {
        let direct = node.parent().is_some_and(|p| {
            p.kind() == "call_expression"
                && p.child_by_field_name("function")
                    .is_some_and(|f| f.id() == node.id())
        });
        if !direct {
            bail!("aliased module loader");
        }
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        imports_node(child, source, out, depth + 1)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_repo::{git as g, init};

    const JS_POLICY: &str = r#"
version = 1
[[components]]
id = "web"
roots = ["src/"]
checks = ["npm run build"]
[[components.suites]]
id = "unit"
[components.suites.javascript]
command = "npm test"
test_files = ["src/math.test.ts", "src/other.test.ts"]
"#;
    const CARGO_POLICY: &str = r#"
version = 1
[[components]]
id = "math"
roots = ["src/", "tests/"]
checks = ["cargo clippy -p example --all-targets -- -D warnings"]
[[components.suites]]
id = "math-unit"
inputs = ["src/math.rs", "tests/helper.rs"]
[components.suites.cargo]
manifest = "Cargo.toml"
package = "example"
target_kind = "lib"
filter = "math::tests::"
test_ids = ["math::tests::adds"]
[[components.suites]]
id = "math-integration"
inputs = ["tests/helper.rs", "tests/arithmetic.rs"]
[components.suites.cargo]
manifest = "Cargo.toml"
package = "example"
target_kind = "test"
target = "arithmetic"
test_ids = ["adds"]
"#;

    fn js_repo() -> std::path::PathBuf {
        crate::test_repo::copy("affected-js", |p| {
            init(
                p,
                &[
                    (POLICY_PATH, JS_POLICY),
                    ("package.json", "{}"),
                    ("src/math.ts", "export const sum = 1;"),
                    ("src/helper.ts", "export {sum} from './math';"),
                    (
                        "src/math.test.ts",
                        "import {sum} from './helper.js'; export const result = sum;",
                    ),
                    ("src/other.test.ts", "export const result = 1;"),
                ],
            )
        })
    }

    fn cargo_repo() -> std::path::PathBuf {
        crate::test_repo::copy("affected-cargo", |p| {
            init(
                p,
                &[
                    (POLICY_PATH, CARGO_POLICY),
                    ("Cargo.toml", "[package]\nname='example'\nversion='0.1.0'"),
                    ("src/lib.rs", "mod math;"),
                    ("src/math.rs", "pub fn add()->i32 {1}"),
                    ("tests/helper.rs", "pub fn value()->i32 {1}"),
                    ("tests/arithmetic.rs", "mod helper; #[test] fn adds() {}"),
                ],
            )
        })
    }

    fn edit(repo: &Path, path: &str, text: &str) -> String {
        let base = g(repo, &["rev-parse", "HEAD"]);
        std::fs::create_dir_all(repo.join(path).parent().unwrap()).unwrap();
        std::fs::write(repo.join(path), text).unwrap();
        g(repo, &["add", "."]);
        g(repo, &["commit", "-qm", "change"]);
        base
    }

    fn selected(repo: &Path, base: &str) -> Selection {
        let selected = select(repo, base, "HEAD").unwrap();
        assert!(!selected.full, "{:?}", selected.reasons);
        selected
    }

    #[test]
    fn reverse_imports_select_helpers_and_tests_but_execute_whole_suite() {
        let repo = js_repo();
        let base = edit(&repo, "src/math.ts", "export const sum = 2;");
        let plan = selected(&repo, &base);
        assert_eq!(
            plan.checks
                .iter()
                .map(|c| c.command.as_str())
                .collect::<Vec<_>>(),
            ["npm run build", "npm test"]
        );
        assert_eq!(plan.checks[1].required_tests, ["src/math.test.ts"]);
        assert!(plan.checks[1].whole_suite);
        assert_eq!(plan.base, base);
        assert_eq!(plan.head, g(&repo, &["rev-parse", "HEAD"]));
        assert_eq!(plan.tree, g(&repo, &["rev-parse", "HEAD^{tree}"]));
        assert!(plan.policy_blob.is_some());
        let base = edit(
            &repo,
            "src/math.test.ts",
            "import './helper.js'; export const changed = true;",
        );
        assert_eq!(
            selected(&repo, &base).checks[1].required_tests,
            ["src/math.test.ts"]
        );
        std::fs::remove_dir_all(repo).unwrap();
    }

    #[test]
    fn deleted_source_keeps_previous_consumers() {
        let repo = js_repo();
        let base = g(&repo, &["rev-parse", "HEAD"]);
        std::fs::remove_file(repo.join("src/math.ts")).unwrap();
        std::fs::write(repo.join("src/helper.ts"), "export const sum = 1;").unwrap();
        g(&repo, &["add", "."]);
        g(&repo, &["commit", "-qm", "remove source"]);
        assert_eq!(
            selected(&repo, &base).checks[1].required_tests,
            ["src/math.test.ts"]
        );
        std::fs::remove_dir_all(repo).unwrap();
    }

    #[test]
    fn dynamic_import_missing_consumer_and_candidate_policy_fail_closed() {
        for (path, text, reason) in [
            (
                "src/math.ts",
                "export const sum = import(name);",
                "nonliteral",
            ),
            (
                "src/uncovered.ts",
                "export const x = 1;",
                "no affected tests",
            ),
            (POLICY_PATH, "version=1\ncomponents=[]", "changed policy"),
            (
                "src/new.test.ts",
                "import './math';",
                "missing from inventory",
            ),
        ] {
            let repo = js_repo();
            let base = edit(&repo, path, text);
            let plan = select(&repo, &base, "HEAD").unwrap();
            assert!(plan.full && plan.checks.is_empty(), "{path}: {plan:?}");
            assert!(
                plan.reasons.join(" ").contains(reason),
                "{path}: {:?}",
                plan.reasons
            );
            std::fs::remove_dir_all(repo).unwrap();
        }
    }

    #[test]
    fn consumers_outside_component_and_unknown_policy_fields_are_full() {
        let repo = js_repo();
        edit(&repo, "outside/consumer.ts", "import '../src/math';");
        let base = edit(&repo, "src/math.ts", "export const sum = 2;");
        assert!(select(&repo, &base, "HEAD")
            .unwrap()
            .reasons
            .join(" ")
            .contains("outside approved"));
        edit(
            &repo,
            POLICY_PATH,
            &format!("unsupported=true\n{JS_POLICY}"),
        );
        let base = edit(&repo, "src/math.ts", "export const sum = 3;");
        assert!(select(&repo, &base, "HEAD").unwrap().full);
        std::fs::remove_dir_all(repo).unwrap();
    }

    #[test]
    fn cargo_explicit_mapping_selects_target_and_shared_helper_consumers() {
        let repo = cargo_repo();
        let base = edit(&repo, "src/math.rs", "pub fn add()->i32 {2}");
        let plan = selected(&repo, &base);
        assert_eq!(plan.checks.len(), 2);
        assert_eq!(
            plan.checks[1].command,
            "cargo test --manifest-path Cargo.toml -p example --lib math::tests::"
        );
        assert!(!plan.checks[1].whole_suite);
        let base = edit(&repo, "tests/helper.rs", "pub fn value()->i32 {2}");
        let plan = selected(&repo, &base);
        assert_eq!(plan.checks.len(), 3);
        assert!(plan.checks[2].command.ends_with("--test arithmetic"));
        // Missing a target cannot become a successful zero-test command.
        let base = g(&repo, &["rev-parse", "HEAD"]);
        std::fs::remove_file(repo.join("tests/arithmetic.rs")).unwrap();
        g(&repo, &["add", "."]);
        g(&repo, &["commit", "-qm", "remove target"]);
        assert!(select(&repo, &base, "HEAD").unwrap().full);
        std::fs::remove_dir_all(repo).unwrap();
    }

    #[test]
    fn discovery_rejects_empty_missing_and_ignored_tests() {
        let check = Check {
            component: "math".into(),
            suite: Some("unit".into()),
            command: "cargo test".into(),
            required_tests: vec!["math::tests::adds".into()],
            discovery: Some(CargoDiscovery {
                command: "list".into(),
                ignored_command: "ignored".into(),
            }),
            whole_suite: false,
        };
        let output = "math::tests::adds: test\n\n1 test, 0 benchmarks\n";
        assert_eq!(
            verify_cargo_discovery(&check, output, "0 tests, 0 benchmarks").unwrap(),
            ["math::tests::adds"]
        );
        assert!(verify_cargo_discovery(&check, "0 tests, 0 benchmarks", "").is_err());
        assert!(verify_cargo_discovery(&check, "other: test", "").is_err());
        assert!(verify_cargo_discovery(&check, output, output).is_err());
    }

    #[test]
    fn static_parser_finds_reexports_and_literal_loads_rejects_opaque_loaders() {
        assert_eq!(imports("sample.ts", "import {a} from './a'; export {b} from './b'; const c = require('./c'); const d = import('./d');").unwrap(), ["./a", "./b", "./c", "./d"].into_iter().map(String::from).collect());
        for source in [
            "const load = require; load(name);",
            "import(name);",
            "import.meta.glob('./*.ts');",
            "new URL('./worker.ts', import.meta.url);",
            "/// <reference path='x.ts' />\nexport const x = 1;",
            "import x = require('./x');",
            "import {createRequire as cr} from 'node:module'; const load = cr('x'); load(name);",
        ] {
            assert!(imports("sample.ts", source).is_err(), "{source}");
        }
    }

    #[test]
    fn resolver_rejects_ambiguous_extensions_and_escaping_paths() {
        let files = ["src/a.ts", "src/a.js"]
            .into_iter()
            .map(|p| {
                (
                    p.into(),
                    Entry {
                        mode: "100644".into(),
                        oid: "x".into(),
                    },
                )
            })
            .collect();
        assert!(resolve(&files, "src/test.ts", "./a").is_err());
        assert!(resolve(&files, "src/test.ts", "../../a").is_err());
        assert!(resolve(&files, "src/test.ts", "./missing").is_err());
    }

    #[test]
    fn approved_whole_suite_mapping_handles_opaque_inputs_without_claiming_a_graph() {
        let repo = js_repo();
        edit(
            &repo,
            POLICY_PATH,
            &JS_POLICY.replace("id = \"unit\"", "id = \"unit\"\ninputs = [\"src/\"]"),
        );
        let base = edit(&repo, "src/math.ts", "export const sum = import(name);");
        let plan = selected(&repo, &base);
        assert_eq!(
            plan.checks[1].required_tests,
            ["src/math.test.ts", "src/other.test.ts"]
        );
        assert!(plan
            .reasons
            .join(" ")
            .contains("not inferred static coverage"));
        let base = edit(&repo, "outside/consumer.ts", "import '../src/math';");
        assert!(select(&repo, &base, "HEAD").unwrap().full);
        std::fs::remove_dir_all(repo).unwrap();
    }

    #[test]
    fn declared_component_consumers_run_all_their_suites() {
        let repo = cargo_repo();
        edit(&repo, "consumer/src/lib.rs", "pub fn use_math() {} ");
        edit(
            &repo,
            "consumer/Cargo.toml",
            "[package]\nname='consumer'\nversion='0.1.0'",
        );
        edit(
            &repo,
            POLICY_PATH,
            &format!(
                r#"{CARGO_POLICY}
[[components]]
id = "consumer"
roots = ["consumer/src/"]
depends_on = ["math"]
checks = ["cargo clippy -p consumer -- -D warnings"]
[[components.suites]]
id = "unit"
inputs = ["consumer/src/"]
[components.suites.cargo]
manifest = "consumer/Cargo.toml"
package = "consumer"
target_kind = "lib"
"#
            ),
        );
        let base = edit(&repo, "src/math.rs", "pub fn add()->i32 {2}");
        let plan = selected(&repo, &base);
        assert_eq!(plan.components, ["math", "consumer"]);
        assert!(plan.checks.iter().any(
            |c| c.command == "cargo test --manifest-path consumer/Cargo.toml -p consumer --lib"
        ));
        std::fs::remove_dir_all(repo).unwrap();
    }
}
