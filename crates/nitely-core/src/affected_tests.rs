//! Revision-bound affected-test advice. An approved input map supplies Rust edges; JavaScript
//! edges come from both immutable syntax trees. This module never authorizes a reduced gate.
use crate::playbook::git;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet, HashMap},
    path::Path,
};
use tree_sitter::{Node, Parser};

pub const POLICY_PATH: &str = ".nitely/affected-tests.toml";
pub const SELECTOR: &str = "affected-v1-ts0271-ts0232-js0250";
const MAX_FILES: usize = 10_000;
const MAX_SOURCE_BYTES: usize = 16 * 1024 * 1024;
const MAX_FILE_BYTES: usize = 512 * 1024;

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
    /// The crate's build script and every input it reads, as approved (NIT-484): exact files or
    /// directory prefixes ending in '/'. A crate with a build script reduces only when its script
    /// is listed here, and a change to any of these keeps the full gate.
    #[serde(default)]
    build_inputs: Vec<String>,
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
    #[serde(default)]
    test_files: Vec<String>,
    /// Globs over the candidate's tracked files (`*` within a segment, `**` for any segments), so
    /// a new test inside the component belongs to the suite without editing the map (NIT-505).
    #[serde(default)]
    test_patterns: Vec<String>,
}

impl JsSuite {
    /// The listed test files and every candidate source file a pattern matches.
    fn tests(&self, files: &Snapshot) -> Vec<String> {
        let mut tests: BTreeSet<String> = self.test_files.iter().cloned().collect();
        tests.extend(
            files
                .keys()
                .filter(|p| source_path(p) && self.test_patterns.iter().any(|g| glob(g, p)))
                .cloned(),
        );
        tests.into_iter().collect()
    }
}

fn glob(pattern: &str, path: &str) -> bool {
    fn segments(pattern: &[&str], path: &[&str]) -> bool {
        match pattern.split_first() {
            None => path.is_empty(),
            Some((&"**", rest)) => (0..=path.len()).any(|i| segments(rest, &path[i..])),
            Some((first, rest)) => path
                .split_first()
                .is_some_and(|(p, tail)| segment(first.as_bytes(), p.as_bytes()) && segments(rest, tail)),
        }
    }
    fn segment(pattern: &[u8], name: &[u8]) -> bool {
        match pattern.split_first() {
            None => name.is_empty(),
            Some((b'*', rest)) => (0..=name.len()).any(|i| segment(rest, &name[i..])),
            Some((c, rest)) => name.split_first().is_some_and(|(n, tail)| n == c && segment(rest, tail)),
        }
    }
    let pattern: Vec<_> = pattern.split('/').collect();
    let path: Vec<_> = path.split('/').collect();
    segments(&pattern, &path)
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

/// Where the selector reads file contents, by object id: git in a repo, or a map in a test
/// (NIT-526). The decision reads nothing else.
pub(crate) trait Objects {
    fn text(&self, oid: &str) -> Result<String>;
    /// The blobs in `oids`, as `blobs::read` gives them: None when they come to over `total` bytes.
    fn texts(&self, oids: &[String], max: usize, total: usize) -> Result<Option<HashMap<String, crate::blobs::Blob>>>;
}

/// A repo's objects. Each text is read once per selection, however many suites share a manifest.
struct GitObjects<'a> {
    repo: &'a Path,
    read: RefCell<HashMap<String, String>>,
}

impl Objects for GitObjects<'_> {
    fn text(&self, oid: &str) -> Result<String> {
        if let Some(text) = self.read.borrow().get(oid) {
            return Ok(text.clone());
        }
        // One git, the size checked on what it read: the inputs are manifests and sources (NIT-508).
        let text = git(self.repo, &["cat-file", "blob", oid])?;
        self.read.borrow_mut().insert(oid.to_string(), text.clone());
        Ok(text)
    }

    fn texts(&self, oids: &[String], max: usize, total: usize) -> Result<Option<HashMap<String, crate::blobs::Blob>>> {
        crate::blobs::read(self.repo, oids, max, total)
    }
}

fn read(objects: &dyn Objects, files: &Snapshot, path: &str) -> Result<String> {
    let e = files
        .get(path)
        .with_context(|| format!("missing input {path}"))?;
    if !matches!(e.mode.as_str(), "100644" | "100755") {
        bail!("nonregular input {path}");
    }
    let text = objects.text(&e.oid)?;
    if text.len() > MAX_FILE_BYTES {
        bail!("input exceeds selector file limit: {path}");
    }
    Ok(text)
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

/// The thin shell around `decide`: git names the commits and lists their trees, and the decision
/// reads contents through `GitObjects`.
pub fn select_with_policy(repo: &Path, base: &str, head: &str, policy_commit:&str) -> Result<Selection> {
    let policy_is_base = policy_commit == base;
    // One git for the three, as verification reads them (NIT-508). git fails the whole call when
    // any of them names nothing, as --verify did for each.
    let ids = git(repo, &["rev-parse", &format!("{base}^{{commit}}"), &format!("{head}^{{commit}}"), &format!("{head}^{{tree}}")])?;
    let [base, head, tree]: [String; 3] = ids
        .lines()
        .map(|l| l.trim().to_string())
        .collect::<Vec<_>>()
        .try_into()
        .map_err(|_| anyhow::anyhow!("git rev-parse gave {ids:?}"))?;
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
    let (base, head) = (selection.base.clone(), selection.head.clone());
    let decided = (|| {
        let before = snapshot(repo, &base)?;
        let after = snapshot(repo, &head)?;
        // The policy is usually the base's own: the same tree, not listed again.
        let policy_files = if policy_is_base || policy_commit == base { before.clone() } else { snapshot(repo, policy_commit)? };
        let objects = GitObjects { repo, read: RefCell::new(HashMap::new()) };
        decide(&policy_files, &before, &after, &objects, &mut selection)
    })();
    fall_back(&mut selection, decided);
    Ok(selection)
}

/// The built-in check for a file no test reaches, by its kind, where one is cheap and certain
/// (NIT-623): a parse or syntax check `nitely check-file` runs, or for TypeScript the project's
/// own type check (the playbook's `basic` commands). None for any other kind.
pub fn builtin_check(path: &str, basic: &[String]) -> Option<Vec<String>> {
    let (_, ext) = path.rsplit_once('.')?;
    match ext.to_ascii_lowercase().as_str() {
        "ps1" | "psm1" | "sh" | "bash" | "py" | "json" | "toml" | "js" | "mjs" | "cjs" => Some(vec![format!("nitely check-file \"{path}\"")]),
        "ts" | "tsx" | "mts" | "cts" if !basic.is_empty() => Some(basic.to_vec()),
        _ => None,
    }
}

/// The checks of a small change's files that no test reaches (NIT-623). A file the map's tests
/// reach (`mapped`) keeps them and adds nothing here. Any other file runs, first found: the
/// playbook's `[checks.files]` command for it, the built-in check for its kind, or the checks of
/// the build it is an input to (`build_of`). A protected file, or one with none of these, keeps
/// the full gate: the error says which. Plain data in; the commands come out once each, in order.
pub fn own_checks(
    paths: &[String],
    mapped: impl Fn(&str) -> bool,
    files: &BTreeMap<String, String>,
    build_of: impl Fn(&str) -> Option<Vec<String>>,
    basic: &[String],
) -> std::result::Result<Vec<String>, String> {
    let mut commands: Vec<String> = Vec::new();
    for path in paths {
        if protected(path) { return Err(format!("{path} is a build, lock or policy file")); }
        if mapped(path) { continue; }
        let found = files.iter().find(|(g, _)| glob(g, path)).map(|(_, c)| vec![c.replace("{path}", path)])
            .or_else(|| builtin_check(path, basic))
            .or_else(|| build_of(path))
            .ok_or_else(|| format!("{path}: no test reaches it and it has no check of its own"))?;
        for c in found { if !commands.contains(&c) { commands.push(c); } }
    }
    Ok(commands)
}

/// `select_with_policy` for a change already found small: where the map would keep the full gate
/// only because some files no test reaches, those files run their own checks (`own_checks`) and
/// the rest keep their mapped tests. The full suite still runs after landing, on main.
pub fn select_small(repo: &Path, base: &str, head: &str, policy_commit: &str, files: &BTreeMap<String, String>, basic: &[String]) -> Result<Selection> {
    let mut selection = select_with_policy(repo, base, head, policy_commit)?;
    if !selection.full { return Ok(selection); }
    let (b, h) = (selection.base.clone(), selection.head.clone());
    let decided = (|| -> Result<Selection> {
        let before = snapshot(repo, &b)?;
        let after = snapshot(repo, &h)?;
        let policy_files = if policy_commit == b { before.clone() } else { snapshot(repo, policy_commit)? };
        let objects = GitObjects { repo, read: RefCell::new(HashMap::new()) };
        let policy: Option<Policy> = match policy_files.get(POLICY_PATH) {
            Some(_) => Some(toml::from_str(&read(&objects, &policy_files, POLICY_PATH)?)?),
            None => None,
        };
        let build_input = |p: &str| policy.iter().flat_map(|pol| &pol.components).find(|c| c.build_inputs.iter().any(|i| under(p, i)));
        let mapped = |p: &str| policy.as_ref().is_some_and(|pol| owner(pol, p).is_some()) && build_input(p).is_none();
        let changed: Vec<String> = before.keys().chain(after.keys())
            .filter(|p| before.get(*p).map(|e| (&e.oid, &e.mode)) != after.get(*p).map(|e| (&e.oid, &e.mode)))
            .cloned().collect::<BTreeSet<_>>().into_iter().collect();
        let own = own_checks(&changed, mapped, files, |p| build_input(p).map(|c| c.checks.clone()), basic).map_err(anyhow::Error::msg)?;
        let mut small = Selection { full: true, components: vec![], checks: vec![], reasons: vec![], ..selection.clone() };
        if changed.iter().any(|p| mapped(p)) {
            // The mapped files alone: the others as they were, so the map decides only what it owns.
            let mut rest = after.clone();
            for p in changed.iter().filter(|p| !mapped(p)) {
                match before.get(p) { Some(e) => { rest.insert(p.clone(), e.clone()); } None => { rest.remove(p); } }
            }
            decide(&policy_files, &before, &rest, &objects, &mut small)?;
        }
        small.full = false;
        for command in own {
            small.checks.push(Check { component: "own-file".into(), suite: None, command, required_tests: vec![], discovery: None, whole_suite: false });
        }
        small.reasons.push("A small change: files no test reaches run their own checks, the rest their mapped tests; the full suite runs on main after landing.".into());
        Ok(small)
    })();
    match decided {
        Ok(small) => selection = small,
        Err(e) => selection.reasons.push(format!("Own-file checks unavailable: {e:#}")),
    }
    Ok(selection)
}

/// What `nitely check-file` runs: parses a JSON or TOML file here, and asks the language's own
/// tool to check a PowerShell, shell, Python or JavaScript file's syntax without running it.
pub fn check_file(path: &Path) -> Result<()> {
    let ext = path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).unwrap_or_default();
    let run = |programs: &[&str], args: Vec<String>| -> Result<()> {
        for program in programs {
            match crate::playbook::shell_free_command(program).args(&args).output() {
                Ok(out) if out.status.success() => return Ok(()),
                Ok(out) => bail!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            }
        }
        bail!("none of {} is installed to check {}", programs.join(", "), path.display())
    };
    let p = path.to_string_lossy().into_owned();
    match ext.as_str() {
        "json" => { serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(path)?).with_context(|| p.clone())?; Ok(()) }
        "toml" => { toml::from_str::<toml::Value>(&std::fs::read_to_string(path)?).with_context(|| p.clone())?; Ok(()) }
        "ps1" | "psm1" => run(&["pwsh", "powershell"], vec!["-NoProfile".into(), "-NonInteractive".into(), "-Command".into(), format!(
            "$e=$null; [void][System.Management.Automation.Language.Parser]::ParseFile('{}',[ref]$null,[ref]$e); if ($e) {{ $e | ForEach-Object {{ $_.ToString() }}; exit 1 }}",
            std::path::absolute(path)?.to_string_lossy().replace('\'', "''"))]),
        "sh" | "bash" => run(&["bash"], vec!["-n".into(), p]),
        "py" => run(&["python3", "python"], vec!["-m".into(), "py_compile".into(), p]),
        "js" | "mjs" | "cjs" => run(&["node"], vec!["--check".into(), p]),
        _ => bail!("no built-in check for {p}"),
    }
}

/// Anything the selector cannot prove keeps the full gate, and says why.
fn fall_back(selection: &mut Selection, decided: Result<()>) {
    if let Err(e) = decided {
        selection.full = true;
        selection.checks.clear();
        selection.reasons.push(format!("Full fallback: {e:#}"));
    }
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
        for input in &c.build_inputs {
            if !valid_path(input) || !before.keys().chain(after.keys()).any(|p| under(p, input)) {
                bail!("invalid or stale build-script input: {input}");
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
                if js.command.trim().is_empty() || js.tests(after).is_empty() {
                    bail!("empty JavaScript suite");
                }
                for pattern in &js.test_patterns {
                    // The pattern's literal directories must sit inside the component, and it
                    // must still match something, like any other approved path.
                    let literal: Vec<_> = pattern.split('/').take_while(|p| !p.contains('*')).collect();
                    let prefix = format!("{}/", literal.join("/"));
                    if !valid_path(pattern)
                        || literal.is_empty()
                        || !c.roots.iter().any(|r| under(&prefix, r))
                        || !after.keys().any(|p| glob(pattern, p))
                    {
                        bail!("invalid, stale or out-of-component JavaScript test pattern: {pattern}");
                    }
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
            let inventory: BTreeSet<String> = c
                .suites
                .iter()
                .filter_map(|s| s.javascript.as_ref())
                .flat_map(|s| s.tests(after))
                .collect();
            for path in after.keys().filter(|p| {
                c.roots.iter().any(|r| under(p, r))
                    && source_path(p)
                    && (p.contains(".test.") || p.contains(".spec."))
            }) {
                if !inventory.contains(path) {
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

/// The selection for a change from `before` to `after` under the map in `policy_files`: which
/// components and checks it needs, or an error, which keeps the full gate. Plain data in, and
/// contents only through `objects` (NIT-526).
fn decide(policy_files: &Snapshot, before: &Snapshot, after: &Snapshot, objects: &dyn Objects, result: &mut Selection) -> Result<()> {
    result.policy_blob = policy_files.get(POLICY_PATH).map(|e| e.oid.clone());
    let policy: Policy = toml::from_str(&read(objects, policy_files, POLICY_PATH)?)?;
    validate(&policy, before, after)?;
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
        for files in [before, after] {
            if files
                .get(path)
                .is_some_and(|e| !matches!(e.mode.as_str(), "100644" | "100755"))
            {
                bail!("nonregular changed input: {path}");
            }
        }
        if policy.components.iter().any(|c| c.build_inputs.iter().any(|i| under(path, i))) {
            bail!("changed build-script input: {path}");
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
        js_graph(objects, before, after)?
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
                    result.checks.push(cargo_check(objects, after, c, s, cargo)?);
                    selected += 1;
                }
            }
            if let Some(js) = &s.javascript {
                let approved =
                    explicit_js && reached.iter().any(|p| s.inputs.iter().any(|i| under(p, i)));
                let tests: Vec<_> = js
                    .tests(after)
                    .into_iter()
                    .filter(|p| dependency || approved || reached.contains(p))
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
                    .any(|s| s.tests(after).iter().any(|t| reaches(&graph, t, path)));
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
    objects: &dyn Objects,
    files: &Snapshot,
    c: &Component,
    s: &Suite,
    suite: &CargoSuite,
) -> Result<Check> {
    let manifest: toml::Value = toml::from_str(&read(objects, files, &suite.manifest)?)?;
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
    // A build script generates inputs this selector cannot see. It reduces only when the approved
    // map names the script and what it reads (`build_inputs`), none of which changed (checked
    // with the changed paths); otherwise the crate keeps the full gate.
    let script = match manifest.get("package").and_then(|p| p.get("build")) {
        Some(toml::Value::Boolean(false)) => None,
        Some(toml::Value::String(path)) => Some(format!("{root}{path}")),
        Some(_) => bail!("unsupported Cargo build setting"),
        None => Some(format!("{root}build.rs")).filter(|p| files.contains_key(p)),
    };
    if let Some(script) = script {
        if !c.build_inputs.iter().any(|i| under(&script, i)) {
            bail!("generated Cargo inputs require Full: approve {script} and what it reads in build_inputs");
        }
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
    objects: &dyn Objects,
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
                let config: serde_json::Value = serde_json::from_str(&read(objects, files, path)?)
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
        // The sources not read yet, in two git processes rather than two per file (NIT-508).
        let regular = |e: &Entry| matches!(e.mode.as_str(), "100644" | "100755");
        let wanted: Vec<String> = files
            .iter()
            .filter(|(p, e)| source_path(p) && regular(e) && !cache.contains_key(&e.oid))
            .map(|(_, e)| e.oid.clone())
            .collect();
        let Some(fetched) = objects.texts(&wanted, MAX_FILE_BYTES, MAX_SOURCE_BYTES - bytes)? else {
            bail!("JavaScript inventory exceeds byte limit");
        };
        for (path, entry) in files.iter().filter(|(p, _)| source_path(p)) {
            let source = if let Some(source) = cache.get(&entry.oid) {
                source.clone()
            } else {
                let source = match fetched.get(&entry.oid) {
                    Some(crate::blobs::Blob::Text(text)) => String::from_utf8_lossy(text).into_owned(),
                    Some(crate::blobs::Blob::TooLarge(_)) => bail!("input exceeds selector file limit: {path}"),
                    // Says why as reading it alone does: not a regular file, or not there.
                    _ => read(objects, files, path)?,
                };
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
                } else if !external(objects, files, path, &specifier)? {
                    bail!("unresolved package/alias in {path}: {specifier}");
                }
            }
        }
    }
    Ok(graph)
}

fn external(objects: &dyn Objects, files: &Snapshot, path: &str, specifier: &str) -> Result<bool> {
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
            let value: serde_json::Value = serde_json::from_str(&read(objects, files, &manifest)?)?;
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

/// What the map says a change to one path would run, for the "How a change lands" dry run
/// (NIT-492). It reads only the map's text: owners by root, then every component that depends
/// on them. The real selection also follows imports and checks the commit, so it can only run
/// more, never less, and a path no component owns keeps the full gate.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PathPreview {
    pub full: bool,
    pub components: Vec<String>,
    pub commands: Vec<String>,
    pub why: String,
}

pub fn preview(policy: &str, path: &str) -> PathPreview {
    let full = |why: String| PathPreview { full: true, components: vec![], commands: vec![], why };
    let path = path.trim().replace('\\', "/");
    if !valid_path(&path) {
        return full("Not a path in the repo".into());
    }
    if protected(&path) || path == POLICY_PATH {
        return full("A build, lock or policy file keeps the full gate".into());
    }
    let policy: Policy = match toml::from_str(policy) {
        Ok(p) => p,
        Err(_) => return full("The test map is missing or does not parse".into()),
    };
    let mut affected: BTreeSet<&str> = policy
        .components
        .iter()
        .filter(|c| c.roots.iter().any(|r| under(&path, r)))
        .map(|c| c.id.as_str())
        .collect();
    if affected.is_empty() {
        return full("No component in the test map owns it".into());
    }
    if policy.components.iter().any(|c| affected.contains(c.id.as_str()) && c.build_inputs.iter().any(|i| under(&path, i))) {
        return full("A build-script input keeps the full gate".into());
    }
    let owners = affected.len();
    loop {
        let old = affected.len();
        for c in &policy.components {
            if c.depends_on.iter().any(|id| affected.contains(id.as_str())) {
                affected.insert(c.id.as_str());
            }
        }
        if old == affected.len() {
            break;
        }
    }
    let mut commands: Vec<String> = vec![];
    let mut add = |c: String| if !commands.contains(&c) { commands.push(c) };
    for c in policy.components.iter().filter(|c| affected.contains(c.id.as_str())) {
        c.checks.iter().cloned().for_each(&mut add);
        for s in &c.suites {
            if let Some(js) = &s.javascript {
                add(js.command.clone());
            }
            if let Some(cargo) = &s.cargo {
                add(format!("cargo test -p {}", cargo.package));
            }
        }
    }
    let components: Vec<String> = policy.components.iter().filter(|c| affected.contains(c.id.as_str())).map(|c| c.id.clone()).collect();
    let why = if components.len() > owners { format!("{} and what depends on it", components[0]) } else { format!("Owned by {}", components.join(", ")) };
    PathPreview { full: false, components, commands, why }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_repo::{git as g, init};

    /// NIT-623: how a small change's files are checked when no test reaches them.
    #[test]
    fn a_file_no_test_reaches_runs_its_own_check_and_one_with_none_keeps_the_full_gate() {
        let files: BTreeMap<String, String> = [("scripts/*.ps1".to_string(), "pwsh -File lint.ps1 {path}".to_string())].into();
        let basic = vec!["tsc --noEmit".to_string()];
        let mapped = |p: &str| p.starts_with("src/");
        let build_of = |p: &str| p.starts_with("crates/core/migrations/").then(|| vec!["cargo check -p core".to_string()]);
        let checks = |paths: &[&str]| own_checks(&paths.iter().map(|p| p.to_string()).collect::<Vec<_>>(), mapped, &files, build_of, &basic);
        assert_eq!(checks(&["scripts/build.ps1"]).unwrap(), ["pwsh -File lint.ps1 scripts/build.ps1"], "the playbook's own command");
        assert_eq!(checks(&["tools/build.ps1"]).unwrap(), ["nitely check-file \"tools/build.ps1\""], "the built-in parse");
        assert_eq!(checks(&["tools/gen.ts", "tools/more.ts"]).unwrap(), ["tsc --noEmit"], "TypeScript runs the project's type check once");
        assert_eq!(checks(&["crates/core/migrations/0001.sql"]).unwrap(), ["cargo check -p core"], "the build it feeds");
        assert_eq!(checks(&["src/App.tsx", "tools/x.py"]).unwrap(), ["nitely check-file \"tools/x.py\""], "a mapped file keeps its tests and adds nothing");
        assert!(checks(&["docs/diagram.svg"]).unwrap_err().contains("no check of its own"));
        assert!(checks(&["tools/Cargo.toml"]).unwrap_err().contains("build, lock or policy"));
    }

    #[test]
    fn check_file_parses_json_and_toml() {
        let dir = crate::paths::unique_temp("nitely-check-file", "");
        std::fs::create_dir_all(&dir).unwrap();
        let write = |name: &str, text: &str| { let p = dir.join(name); std::fs::write(&p, text).unwrap(); p };
        assert!(check_file(&write("ok.json", "{\"a\": [1, 2]}")).is_ok());
        assert!(check_file(&write("bad.json", "{\"a\": [1, 2}")).is_err());
        assert!(check_file(&write("ok.toml", "[a]\nb = 1\n")).is_ok());
        assert!(check_file(&write("bad.toml", "[a\nb = 1\n")).is_err());
        assert!(check_file(&write("x.svg", "<svg/>")).is_err(), "no built-in check");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The dry run reads this repo's own map: a frontend file gets the web checks, a lock file
    /// and a file no component owns keep the full gate.
    #[test]
    fn preview_reads_the_map() {
        let map = include_str!("../../../.nitely/affected-tests.toml");
        let web = preview(map, "src/rules/LandsStrip.tsx");
        assert!(!web.full, "{web:?}");
        assert!(web.commands.contains(&"npm test".to_string()) && web.commands.contains(&"npm run build".to_string()), "{web:?}");
        assert!(preview(map, "Cargo.lock").full);
        assert!(preview(map, "nowhere/else.txt").full);
        assert!(preview("not toml [", "src/App.tsx").full);
    }

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

    /// The decision alone, on trees and contents in memory (NIT-526): each case below is one a
    /// test above makes with a repo and commits, here without git. Only the repo's own map reads
    /// this checkout's tree, once, to have the real file list.
    mod decided {
        use super::*;
        use crate::blobs::Blob;
        use std::hash::{Hash, Hasher};

        /// Contents by object id, as git gives them, and optionally files on disk by their id.
        #[derive(Default)]
        struct Mem {
            texts: HashMap<String, String>,
            disk: Option<(std::path::PathBuf, HashMap<String, String>)>,
        }

        impl Objects for Mem {
            fn text(&self, oid: &str) -> Result<String> {
                if let Some(text) = self.texts.get(oid) {
                    return Ok(text.clone());
                }
                let (root, paths) = self.disk.as_ref().with_context(|| format!("no object {oid}"))?;
                let path = paths.get(oid).with_context(|| format!("no object {oid}"))?;
                Ok(std::fs::read_to_string(root.join(path))?)
            }

            fn texts(&self, oids: &[String], max: usize, total: usize) -> Result<Option<HashMap<String, Blob>>> {
                let mut found = HashMap::new();
                let mut bytes = 0;
                for oid in oids {
                    let blob = match self.text(oid) {
                        Ok(text) if text.len() > max => Blob::TooLarge(text.len()),
                        Ok(text) => {
                            bytes += text.len();
                            Blob::Text(text.into_bytes())
                        }
                        Err(_) => Blob::Missing,
                    };
                    found.insert(oid.clone(), blob);
                }
                Ok((bytes <= total).then_some(found))
            }
        }

        impl Mem {
            fn put(&mut self, text: &str) -> String {
                let mut h = std::collections::hash_map::DefaultHasher::new();
                text.hash(&mut h);
                let oid = format!("{:016x}", h.finish());
                self.texts.insert(oid.clone(), text.to_string());
                oid
            }

            /// A tree of `files`, each under an id of its contents.
            fn tree(&mut self, files: &[(&str, &str)]) -> Snapshot {
                self.change(&Snapshot::new(), &files.iter().map(|(p, t)| (*p, Some(*t))).collect::<Vec<_>>())
            }

            /// `tree` with each path set to its text, or removed for None.
            fn change(&mut self, tree: &Snapshot, changes: &[(&str, Option<&str>)]) -> Snapshot {
                let mut tree = tree.clone();
                for (path, text) in changes {
                    match text {
                        Some(text) => {
                            let oid = self.put(text);
                            tree.insert(path.to_string(), Entry { mode: "100644".into(), oid });
                        }
                        None => {
                            tree.remove(*path);
                        }
                    }
                }
                tree
            }
        }

        /// What `select_with_policy` gives for `before` to `after` under the map in `policy`.
        fn under(policy: &Snapshot, before: &Snapshot, after: &Snapshot, objects: &Mem) -> Selection {
            let mut s = Selection { base: "base".into(), head: "head".into(), tree: "tree".into(), policy_blob: None, selector: SELECTOR.into(), full: true, components: vec![], checks: vec![], reasons: vec![] };
            let decided = decide(policy, before, after, objects, &mut s);
            fall_back(&mut s, decided);
            s
        }

        fn decided(before: &Snapshot, after: &Snapshot, objects: &Mem) -> Selection {
            under(before, before, after, objects)
        }

        fn reduced(before: &Snapshot, after: &Snapshot, objects: &Mem) -> Selection {
            let s = decided(before, after, objects);
            assert!(!s.full, "{:?}", s.reasons);
            s
        }

        fn full_because(before: &Snapshot, after: &Snapshot, objects: &Mem, why: &str) {
            let s = decided(before, after, objects);
            assert!(s.full && s.checks.is_empty(), "{s:?}");
            assert!(s.reasons.join(" ").contains(why), "{why}: {:?}", s.reasons);
        }

        fn js() -> (Mem, Snapshot) {
            let mut m = Mem::default();
            let t = m.tree(&[
                (POLICY_PATH, JS_POLICY),
                ("package.json", "{}"),
                ("src/math.ts", "export const sum = 1;"),
                ("src/helper.ts", "export {sum} from './math';"),
                ("src/math.test.ts", "import {sum} from './helper.js'; export const result = sum;"),
                ("src/other.test.ts", "export const result = 1;"),
            ]);
            (m, t)
        }

        fn cargo() -> (Mem, Snapshot) {
            let mut m = Mem::default();
            let t = m.tree(&[
                (POLICY_PATH, CARGO_POLICY),
                ("Cargo.toml", "[package]\nname='example'\nversion='0.1.0'"),
                ("src/lib.rs", "mod math;"),
                ("src/math.rs", "pub fn add()->i32 {1}"),
                ("tests/helper.rs", "pub fn value()->i32 {1}"),
                ("tests/arithmetic.rs", "mod helper; #[test] fn adds() {}"),
            ]);
            (m, t)
        }

        /// As `the_repos_own_map_selects_the_frontend_and_keeps_everything_else_full`, on this
        /// checkout's own tree and map, with each candidate made in memory.
        #[test]
        fn the_repos_own_map_decides_in_memory() {
            let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
            let base = snapshot(&repo, "HEAD").unwrap();
            let paths = base.iter().map(|(p, e)| (e.oid.clone(), p.clone())).collect();
            let mut m = Mem { texts: HashMap::new(), disk: Some((repo.clone(), paths)) };
            // A candidate: the base with each path set to a text, or (None) its own text and one
            // more line.
            let mut make = |changes: &[(&str, Option<&str>)]| -> Snapshot {
                let texts: Vec<(String, String)> = changes
                    .iter()
                    .map(|(p, t)| (p.to_string(), t.map(str::to_string).unwrap_or_else(|| format!("{}
// edited
", m.text(&base[*p].oid).unwrap()))))
                    .collect();
                let pairs: Vec<(&str, Option<&str>)> = texts.iter().map(|(p, t)| (p.as_str(), Some(t.as_str()))).collect();
                m.change(&base, &pairs)
            };
            let frontend = ["src/Rules.tsx", "scripts/version.mjs"];
            let build = ["src-tauri/tauri.conf.json", "src-tauri/capabilities/default.json", "crates/nitely-cli/build.rs"];
            let rust = "crates/nitely-core/src/lanes.rs";
            let fronts: Vec<Snapshot> = frontend.iter().map(|p| make(&[(p, None)])).collect();
            let new_test = make(&[("src/rules/new.test.ts", Some("export {};
"))]);
            let crate_changes: Vec<Snapshot> = ["crates/nitely-cli/src/ui.rs", "src-tauri/src/notify.rs", "crates/nitely-crew/src/waits.rs"].iter().map(|p| make(&[(p, None)])).collect();
            let builds: Vec<Snapshot> = build.iter().map(|p| make(&[(p, None)])).collect();
            let core = make(&[(rust, None)]);
            let cases_change = make(&[("crates/nitely-core/src/track_cases.json", None)]);
            // Anything the map cannot prove keeps the full gate: a migration or the core's build
            // script (what its build script reads), the map itself, a manifest, a file no
            // component owns, a test outside every mapped root, and one inside the frontend that no
            // pattern matches.
            let stay_full: Vec<Snapshot> = vec![
                make(&[("crates/nitely-core/migrations/20261021-test-catches.sql", None)]),
                make(&[("crates/nitely-core/build.rs", None)]),
                make(&[(POLICY_PATH, None)]),
                make(&[("package.json", None)]),
                make(&[("docs/build-lanes.md", None)]),
                make(&[("docs/new.test.ts", Some("export {};
"))]),
                make(&[("src/new.spec.ts", Some("export {};
"))]),
            ];

            for (path, after) in frontend.iter().zip(&fronts) {
                let s = reduced(&base, after, &m);
                assert_eq!(s.components, ["web"], "{path}");
                let commands: Vec<&str> = s.checks.iter().map(|c| c.command.as_str()).collect();
                assert_eq!(commands, ["npm run build", "npm test"], "{path}");
            }
            let s = reduced(&base, &new_test, &m);
            assert!(s.checks.iter().any(|c| c.required_tests.iter().any(|t| t == "src/rules/new.test.ts")), "{:?}", s.checks);
            for ((path, component, clippy, package), after) in [
                ("crates/nitely-cli/src/ui.rs", "cli", "cargo clippy -p nitely-cli -- -D warnings", "nitely-cli"),
                ("src-tauri/src/notify.rs", "app", "cargo clippy -p nitely-app -- -D warnings", "nitely-app"),
            ]
            .into_iter()
            .zip(&crate_changes)
            {
                let s = reduced(&base, after, &m);
                assert_eq!(s.components, [component], "{path}");
                assert!(s.checks.iter().any(|c| c.command == clippy), "{path}");
                assert!(s.checks.iter().filter(|c| c.discovery.is_some()).all(|c| c.command.contains(&format!("-p {package}"))), "{path}: {:?}", s.checks);
                assert!(!s.checks.iter().any(|c| c.command.contains("nitely-core")), "{path}");
            }
            let s = reduced(&base, &crate_changes[2], &m);
            assert_eq!(s.components, ["crew", "cli", "app", "leaderboard"]);
            assert!(s.checks.iter().any(|c| c.command == "cargo clippy -p nitely-crew -- -D warnings"), "{:?}", s.checks);
            assert!(!s.checks.iter().any(|c| c.command.contains("nitely-core")), "{:?}", s.checks);
            // A core change runs the core and every crate above it, with clippy per crate, instead
            // of the whole-workspace fallback (NIT-581).
            let s = reduced(&base, &core, &m);
            assert_eq!(s.components, ["core", "crew", "cli", "app", "leaderboard"]);
            for package in ["nitely-core", "nitely-crew", "nitely-cli", "nitely-app", "nitely-leaderboard"] {
                assert!(s.checks.iter().any(|c| c.command == format!("cargo clippy -p {package} -- -D warnings")), "{package}: {:?}", s.checks);
            }
            assert!(s.checks.iter().any(|c| c.command.starts_with("cargo test --manifest-path crates/nitely-core/Cargo.toml -p nitely-core --lib")), "{:?}", s.checks);
            // A core file a frontend test reads runs that test too (NIT-583 review).
            let cases = reduced(&base, &cases_change, &m);
            assert!(cases.components.contains(&"web".to_string()) && cases.checks.iter().any(|c| c.command == "npm test"), "{:?}", cases.checks);
            // Every integration test of a mapped crate is in its inventory: a dependency-only
            // selection runs only listed suites, so one left out would never run (NIT-583 review).
            let map = m.text(&base[POLICY_PATH].oid).unwrap();
            for root in ["crates/nitely-core/", "crates/nitely-crew/", "crates/nitely-cli/", "src-tauri/", "server/leaderboard/"] {
                for path in base.keys().filter(|p| p.starts_with(&format!("{root}tests/")) && p.matches('/').count() == root.matches('/').count() + 1) {
                    let name = path.rsplit('/').next().unwrap().trim_end_matches(".rs");
                    assert!(map.contains(&format!("target = \"{name}\"")), "{path} has no suite in {POLICY_PATH}");
                }
            }
            for (path, after) in build.iter().zip(&builds) {
                assert!(decided(&base, after, &m).full, "{path}");
            }
            for after in &stay_full {
                let s = decided(&base, after, &m);
                assert!(s.full && s.checks.is_empty(), "{:?}", s.reasons);
            }
        }

        /// As `a_build_script_reduces_only_with_its_inputs_approved_and_unchanged`.
        #[test]
        fn a_build_script_reduces_only_with_its_inputs_approved_and_unchanged() {
            let (mut m, t) = cargo();
            let script = "fn main() { println!(\"cargo:rerun-if-changed=assets\"); }";
            let t = m.change(&t, &[("build.rs", Some(script)), ("assets/table.txt", Some("1"))]);
            let math = |m: &mut Mem, t: &Snapshot, n: &str| m.change(t, &[("src/math.rs", Some(&format!("pub fn add()->i32 {{{n}}}")))]);
            let after = math(&mut m, &t, "2");
            full_because(&t, &after, &m, "approve build.rs");
            let approved = CARGO_POLICY.replace("checks = [", "build_inputs = [\"build.rs\", \"assets/\"]\nchecks = [");
            let t = m.change(&t, &[(POLICY_PATH, Some(&approved))]);
            let after = math(&mut m, &t, "3");
            assert_eq!(reduced(&t, &after, &m).components, ["math"]);
            let after = m.change(&t, &[("assets/table.txt", Some("2"))]);
            full_because(&t, &after, &m, "changed build-script input: assets/table.txt");
            let after = m.change(&t, &[("build.rs", Some("fn main() {}"))]);
            full_because(&t, &after, &m, "build.rs");
            let stale = m.change(&t, &[(POLICY_PATH, Some(&approved.replace("\"assets/\"", "\"missing/\"")))]);
            let after = math(&mut m, &stale, "4");
            full_because(&stale, &after, &m, "invalid or stale build-script input: missing/");
        }

        /// As `reverse_imports_select_helpers_and_tests_but_execute_whole_suite`, the decision
        /// part: the git scenario stays for the commit ids and the policy blob.
        #[test]
        fn reverse_imports_select_helpers_and_tests_and_run_the_whole_suite() {
            let (mut m, t) = js();
            let after = m.change(&t, &[("src/math.ts", Some("export const sum = 2;"))]);
            let s = reduced(&t, &after, &m);
            assert_eq!(s.checks.iter().map(|c| c.command.as_str()).collect::<Vec<_>>(), ["npm run build", "npm test"]);
            assert_eq!(s.checks[1].required_tests, ["src/math.test.ts"]);
            assert!(s.checks[1].whole_suite);
            assert_eq!(s.policy_blob.as_deref(), Some(t[POLICY_PATH].oid.as_str()));
            let after = m.change(&t, &[("src/math.test.ts", Some("import './helper.js'; export const changed = true;"))]);
            assert_eq!(reduced(&t, &after, &m).checks[1].required_tests, ["src/math.test.ts"]);
        }

        /// As `deleted_source_keeps_previous_consumers`.
        #[test]
        fn deleted_source_keeps_previous_consumers() {
            let (mut m, t) = js();
            let after = m.change(&t, &[("src/math.ts", None), ("src/helper.ts", Some("export const sum = 1;"))]);
            assert_eq!(reduced(&t, &after, &m).checks[1].required_tests, ["src/math.test.ts"]);
        }

        /// As `dynamic_import_missing_consumer_and_candidate_policy_fail_closed`.
        #[test]
        fn dynamic_import_missing_consumer_and_candidate_policy_fail_closed() {
            for (path, text, why) in [
                ("src/math.ts", "export const sum = import(name);", "nonliteral"),
                ("src/uncovered.ts", "export const x = 1;", "no affected tests"),
                (POLICY_PATH, "version=1\ncomponents=[]", "changed policy"),
                ("src/new.test.ts", "import './math';", "missing from inventory"),
            ] {
                let (mut m, t) = js();
                let after = m.change(&t, &[(path, Some(text))]);
                full_because(&t, &after, &m, why);
            }
        }

        /// As `consumers_outside_component_and_unknown_policy_fields_are_full`.
        #[test]
        fn consumers_outside_component_and_unknown_policy_fields_are_full() {
            let (mut m, t) = js();
            let t = m.change(&t, &[("outside/consumer.ts", Some("import '../src/math';"))]);
            let after = m.change(&t, &[("src/math.ts", Some("export const sum = 2;"))]);
            full_because(&t, &after, &m, "outside approved");
            let t = m.change(&t, &[(POLICY_PATH, Some(&format!("unsupported=true\n{JS_POLICY}")))]);
            let after = m.change(&t, &[("src/math.ts", Some("export const sum = 3;"))]);
            assert!(decided(&t, &after, &m).full);
        }

        /// As `cargo_explicit_mapping_selects_target_and_shared_helper_consumers`.
        #[test]
        fn cargo_explicit_mapping_selects_target_and_shared_helper_consumers() {
            let (mut m, t) = cargo();
            let after = m.change(&t, &[("src/math.rs", Some("pub fn add()->i32 {2}"))]);
            let s = reduced(&t, &after, &m);
            assert_eq!(s.checks.len(), 2);
            assert_eq!(s.checks[1].command, "cargo test --manifest-path Cargo.toml -p example --lib math::tests::");
            assert!(!s.checks[1].whole_suite);
            let after = m.change(&t, &[("tests/helper.rs", Some("pub fn value()->i32 {2}"))]);
            let s = reduced(&t, &after, &m);
            assert_eq!(s.checks.len(), 3);
            assert!(s.checks[2].command.ends_with("--test arithmetic"));
            // A missing target cannot become a passing run of zero tests.
            let after = m.change(&after, &[("tests/arithmetic.rs", None)]);
            assert!(decided(&t, &after, &m).full);
        }

        /// As `approved_whole_suite_mapping_handles_opaque_inputs_without_claiming_a_graph`.
        #[test]
        fn approved_whole_suite_mapping_handles_opaque_inputs_without_claiming_a_graph() {
            let (mut m, t) = js();
            let t = m.change(&t, &[(POLICY_PATH, Some(&JS_POLICY.replace("id = \"unit\"", "id = \"unit\"\ninputs = [\"src/\"]")))]);
            let after = m.change(&t, &[("src/math.ts", Some("export const sum = import(name);"))]);
            let s = reduced(&t, &after, &m);
            assert_eq!(s.checks[1].required_tests, ["src/math.test.ts", "src/other.test.ts"]);
            assert!(s.reasons.join(" ").contains("not inferred static coverage"));
            let after = m.change(&after, &[("outside/consumer.ts", Some("import '../src/math';"))]);
            assert!(decided(&t, &after, &m).full);
        }

        /// As `declared_component_consumers_run_all_their_suites`.
        #[test]
        fn declared_component_consumers_run_all_their_suites() {
            let (mut m, t) = cargo();
            let policy = format!(
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
            );
            let t = m.change(&t, &[
                ("consumer/src/lib.rs", Some("pub fn use_math() {} ")),
                ("consumer/Cargo.toml", Some("[package]\nname='consumer'\nversion='0.1.0'")),
                (POLICY_PATH, Some(&policy)),
            ]);
            let after = m.change(&t, &[("src/math.rs", Some("pub fn add()->i32 {2}"))]);
            let s = reduced(&t, &after, &m);
            assert_eq!(s.components, ["math", "consumer"]);
            assert!(s.checks.iter().any(|c| c.command == "cargo test --manifest-path consumer/Cargo.toml -p consumer --lib"));
        }

        /// The map comes from the policy commit when it is not the base (a goal branch measured
        /// against main's newer map): that map decides, and its blob is the one recorded. No git
        /// test reached this before.
        #[test]
        fn a_policy_commit_other_than_the_base_decides_with_its_own_map() {
            let (mut m, t) = cargo();
            let t = m.change(&t, &[("build.rs", Some("fn main() {}"))]);
            let after = m.change(&t, &[("src/math.rs", Some("pub fn add()->i32 {2}"))]);
            full_because(&t, &after, &m, "approve build.rs");
            let approved = CARGO_POLICY.replace("checks = [", "build_inputs = [\"build.rs\"]\nchecks = [");
            let main = m.change(&t, &[(POLICY_PATH, Some(&approved))]);
            let s = under(&main, &t, &after, &m);
            assert!(!s.full, "{:?}", s.reasons);
            assert_eq!(s.policy_blob.as_deref(), Some(main[POLICY_PATH].oid.as_str()));
        }
    }
}
