//! Mechanical, revision-bound verification advice. This first rollout is shadow-only:
//! no result here can skip a gate, reviewer, approval, or release check.
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{path::Path, process::Command};
use tree_sitter::{Node, Parser};
use crate::{Db, issues::{Actor, IssueRecord}, playbook::{git, Playbook, PLAYBOOK_PATH}};

const CLASSIFIER: &str = "syntax-v1-ts0271-rust0242-ts0232-js0250";
const MAX_FILE: usize = 512 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Rules {
    /// Only off/shadow are implemented. Unknown modes conservatively recommend Full.
    pub mode: String,
    pub version: u32,
    /// Regexes anchored by the classifier against the complete repository-relative path.
    pub prose: Vec<String>,
    pub comments: Vec<String>,
    pub display_copy: Vec<String>,
    pub protected: Vec<String>,
    pub backstop: crate::verification_backstop::Rules,
    pub audit_percent: u8,
}
impl Default for Rules {
    fn default()->Self {Self {mode:String::new(),version:0,prose:vec![],comments:vec![],display_copy:vec![],protected:vec![],backstop:Default::default(),audit_percent:10}}
}
fn default_audit_percent()->u8 {10}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    #[serde(default="default_audit_percent")]
    pub audit_percent: u8,
    #[serde(default)]
    pub affected: Option<crate::affected_tests::Selection>,
    #[serde(default)]
    pub audit_sample: Option<crate::verification_audit::Sample>,
    #[serde(default)]
    pub policy_fingerprint: Option<String>,
    #[serde(default)]
    pub rule_ids: Vec<String>,
    pub head: String,
    pub tree: String,
    pub base: String,
    pub policy_commit: String,
    pub classifier: String,
    pub declared: Option<String>,
    pub declared_by: Option<String>,
    pub declared_source: Option<String>,
    pub declared_at: Option<i64>,
    pub effective: String,
    pub depth: String,
    pub shadow: bool,
    pub reasons: Vec<String>,
    pub files: Vec<FileProof>,
    pub enforcement: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileProof { pub path: String, pub proof: String }

fn matches(patterns: &[String], path: &str) -> Result<bool> {
    for p in patterns {
        if regex::Regex::new(&format!("^(?:{p})$"))?.is_match(path) { return Ok(true); }
    }
    Ok(false)
}

fn built_in_protected(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    let name = p.rsplit('/').next().unwrap_or(&p);
    p.starts_with('.') || p.split('/').any(|s| matches!(s, "migrations" | "skills" | "vendor" | "generated" | "node_modules"))
        || matches!(name, "agents.md" | "claude.md" | "skill.md" | "cargo.toml" | "cargo.lock" | "package.json" | "package-lock.json" | "build.rs")
        || ["approval", "credential", "keychain", "auth", "release", "install", "verification", "playbook", "land.", "land_queue", "recovery", "db.", "test", "spec"].iter().any(|s| p.contains(s))
}

fn ordinary_comment(text: &str) -> bool {
    let t = text.to_ascii_lowercase();
    !t.starts_with("///") && !t.starts_with("//!") && !t.starts_with("/**") && !t.starts_with("/*!")
        && !["@", "#", "eslint", "prettier", "istanbul", "coverage", "sourceurl", "sourcemappingurl", "rustfmt", "doctest", "pragma"].iter().any(|s| t.contains(s))
}

fn simple_copy(s: &str) -> bool {
    !s.trim().is_empty() && !s.contains(['{', '}', '\\', '`', '<', '>']) && !s.contains("://")
}

// Only text children of native DOM elements and a narrow set of literal label attributes.
// Custom components can interpret children/props as behavior; they are not covered by v1.
fn native_element(node: Node<'_>, source: &str) -> bool {
    let opening = if node.kind() == "jsx_element" { node.child_by_field_name("open_tag") } else { Some(node) };
    opening.and_then(|n| n.child_by_field_name("name"))
        .and_then(|n| n.utf8_text(source.as_bytes()).ok())
        .is_some_and(|n| matches!(n, "div" | "span" | "p" | "button" | "label" | "input" | "h1" | "h2" | "h3" | "li" | "td" | "th" | "section"))
}

fn display_node(node: Node<'_>, source: &str) -> bool {
    let Some(parent) = node.parent() else { return false; };
    if node.kind() == "jsx_text" { return native_element(parent, source); }
    if node.kind() != "string" || parent.kind() != "jsx_attribute" { return false; }
    let Some(name) = parent.named_child(0).and_then(|n| n.utf8_text(source.as_bytes()).ok()) else { return false; };
    matches!(name, "title" | "placeholder" | "aria-label") && parent.parent().is_some_and(|n| native_element(n, source))
}

fn normalize(node: Node<'_>, source: &str, comments: bool, display: bool, out: &mut Vec<String>, depth: usize) -> Result<()> {
    if depth > 256 { bail!("syntax nesting exceeds classifier limit"); }
    let text = node.utf8_text(source.as_bytes())?;
    if comments && matches!(node.kind(), "comment" | "line_comment" | "block_comment") && ordinary_comment(text) { return Ok(()); }
    out.push(format!("({}", node.kind()));
    if display && display_node(node, source) && simple_copy(text) {
        out.push("DISPLAY_TEXT".into());
    } else if node.child_count() == 0 {
        out.push(text.into());
    } else {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) { normalize(child, source, comments, display, out, depth + 1)?; }
    }
    out.push(")".into());
    Ok(())
}

fn syntax(path: &str, source: &str, comments: bool, display: bool) -> Result<Vec<String>> {
    let language = match Path::new(path).extension().and_then(|e| e.to_str()) {
        Some("rs") => tree_sitter_rust::LANGUAGE.into(),
        Some("ts") => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        Some("tsx") => tree_sitter_typescript::LANGUAGE_TSX.into(),
        Some("js" | "jsx" | "mjs" | "cjs") => tree_sitter_javascript::LANGUAGE.into(),
        _ => bail!("unsupported source grammar"),
    };
    let mut parser = Parser::new();
    parser.set_language(&language)?;
    let tree = parser.parse(source, None).context("parser did not produce a tree")?;
    if tree.root_node().has_error() { bail!("source has parse errors"); }
    let mut tokens = vec![];
    normalize(tree.root_node(), source, comments, display, &mut tokens, 0)?;
    Ok(tokens)
}

fn prove(path: &str, old: &str, new: &str, rules: &Rules) -> Result<String> {
    if built_in_protected(path) || matches(&rules.protected, path)? { bail!("protected path"); }
    if old.contains('\0') || new.contains('\0') { bail!("binary content"); }
    if matches(&rules.prose, path)? {
        if !matches!(Path::new(path).extension().and_then(|e| e.to_str()), Some("md" | "txt")) { bail!("unsupported prose format"); }
        // Do not silently discard embedded execution or instruction-bearing frontmatter.
        for text in [old, new] {
            if text.contains([ '<', '>' ]) || text.lines().any(|l| l.contains("```") || l.contains("~~~") || l.starts_with("    ") || l.starts_with('\t') || matches!(l.trim(), "---" | "+++")) {
                bail!("embedded markup, code or frontmatter is not proven prose");
            }
        }
        return Ok("approved prose without embedded code".into());
    }
    let comments = matches(&rules.comments, path)?;
    let display = matches(&rules.display_copy, path)?;
    if !comments && !display { bail!("no authoritative source rule"); }
    if syntax(path, old, comments, false)? == syntax(path, new, comments, false)? {
        return Ok("executable syntax unchanged; ordinary comments/whitespace only".into());
    }
    if display && syntax(path, old, comments, true)? == syntax(path, new, comments, true)? {
        return Ok("native JSX display text only under an approved location rule".into());
    }
    bail!("executable syntax or a non-display literal changed")
}

fn blob(repo: &Path, revision: &str, path: &str) -> Result<String> {
    let object = format!("{revision}:{path}");
    let size: usize = git(repo, &["cat-file", "-s", &object])?.trim().parse()?;
    if size > MAX_FILE { bail!("file exceeds 512 KiB classifier limit"); }
    let out = crate::playbook::run_git(Command::new("git").current_dir(repo).args(["cat-file", "blob", &object]))?;
    if !out.status.success() { bail!("cannot read revision blob"); }
    String::from_utf8(out.stdout).context("non-UTF8 source")
}

/// Uses immutable integration policy, never the candidate's edited rules. On uncertainty,
/// report Full; do not turn a classifier failure into a validation exemption.
pub fn assess(repo: &Path, base: &str, head: &str, issue: &IssueRecord) -> Result<Plan> {
    let base = git(repo, &["rev-parse", "--verify", &format!("{base}^{{commit}}")])?.trim().to_string();
    let head = git(repo, &["rev-parse", "--verify", &format!("{head}^{{commit}}")])?.trim().to_string();
    let mut plan = Plan {
        affected: None, audit_sample: None, policy_fingerprint: None, rule_ids: vec![], audit_percent:10,
        tree: git(repo, &["rev-parse", &format!("{head}^{{tree}}")])?.trim().into(),
        head, base: base.clone(), policy_commit: base, classifier: CLASSIFIER.into(),
        declared: issue.review_level.clone(), declared_by: issue.review_level_by.clone(), declared_source: issue.review_level_source.clone(),
        declared_at: issue.review_level_at,
        effective: "sensitive".into(), depth: "Full".into(), shadow: true, reasons: vec![], files: vec![],
        enforcement: "Shadow only: the existing full gate, review and approval rules still apply.".into(),
    };
    let attempted = assess_inner(repo, issue, &mut plan);
    if let Err(error) = attempted { plan.reasons.push(format!("Full fallback: {error:#}")); }
    // Retain affected-test advice separately: an input map does not overrule a safety floor.
    plan.affected = Some(crate::affected_tests::select(repo,&plan.base,&plan.head)?);
    Ok(plan)
}

fn assess_inner(repo: &Path, issue: &IssueRecord, plan: &mut Plan) -> Result<()> {
    let policy = blob(repo, &plan.policy_commit, PLAYBOOK_PATH).context("committed integration policy unavailable")?;
    let pb = Playbook::parse(&policy)?;
    plan.policy_fingerprint=Some(crate::verification_audit::policy_fingerprint(policy.as_bytes(),CLASSIFIER));
    if pb.unknown.iter().any(|key| key.starts_with("verification")) { bail!("unknown verification policy fields"); }
    let rules = &pb.verification;
    if !(10..=100).contains(&rules.audit_percent) {bail!("audit_percent must be between 10 and 100");}
    plan.audit_percent=rules.audit_percent;
    if rules.mode != "shadow" || rules.version != 1 { bail!("verification shadow policy version 1 is not configured"); }
    if !pb.review.flags.is_empty() { bail!("reviewer-only conditions require the existing full review"); }
    let declared = issue.review_level.as_deref().context("undeclared changes retain existing full verification")?;
    crate::issues::check_review_level(declared)?;
    let raw = git(repo, &["diff", "--raw", "-z", "--no-renames", "--no-ext-diff", &plan.base, &plan.head, "--"])?;
    let parts: Vec<&str> = raw.split('\0').filter(|p| !p.is_empty()).collect();
    if !parts.len().is_multiple_of(2) || parts.is_empty() || parts.len() > 20 { bail!("empty/unsupported diff or more than ten files"); }
    let mut changes = 0usize;
    for row in git(repo, &["diff", "--numstat", "--no-renames", "--no-ext-diff", &plan.base, &plan.head, "--"])?.lines() {
        let mut cells = row.split('\t');
        changes += cells.next().context("missing added count")?.parse::<usize>().context("binary/unknown diff count")?;
        changes += cells.next().context("missing removed count")?.parse::<usize>()?;
        if changes > 400 { bail!("more than 400 changed lines"); }
    }
    for pair in parts.chunks_exact(2) {
        let fields: Vec<_> = pair[0].split_whitespace().collect();
        let path = pair[1];
        // v1 only proves edits to ordinary existing files. Rename is delete+add and escalates.
        if fields.len() != 5 || fields[0] != ":100644" || fields[1] != "100644" || fields[4] != "M" {
            bail!("{path}: additions, deletions, renames, mode changes and submodules require Full");
        }
        if let Some(hit) = pb.approval.protected_hit(&[path.into()])? { bail!("{path}: {hit}"); }
        let proof = prove(path, &blob(repo, &plan.base, path)?, &blob(repo, &plan.head, path)?, rules)
            .with_context(|| path.to_string())?;
        for (kind,patterns) in [("prose",&rules.prose),("comments",&rules.comments),("display_copy",&rules.display_copy)] {
            for pattern in patterns {
                if matches(std::slice::from_ref(pattern),path)? {
                    let id=format!("{kind}:{pattern}");
                    if !plan.rule_ids.contains(&id) {plan.rule_ids.push(id);}
                }
            }
        }
        plan.files.push(FileProof { path: path.into(), proof });
    }
    // A declaration can raise the computed floor, never lower it. Unsupported behavioral
    // cases have already fallen back to Full above; affected tests arrive separately.
    if plan.rule_ids.len()>100 || plan.rule_ids.iter().any(|r|r.len()>256) {bail!("lowering rule identity exceeds audit bounds");}
    plan.effective = declared.into();
    plan.depth = crate::issues::review_depth(declared).into();
    plan.reasons.push("Every changed file has a mechanical Basic proof; declaration supplies any higher minimum.".into());
    if issue.review_level_source.as_deref() == Some("implementer") {
        plan.reasons.push("Implementer's declaration is a hint; eligibility came from the committed policy and parsed diff.".into());
    }
    Ok(())
}

pub fn assess_and_record(db:&mut Db,repo:&Path,base:&str,head:&str,issue:&IssueRecord,actor:&Actor)->Result<Plan> {
    let mut plan=assess(repo,base,head,issue)?;
    if let Some(project)=&issue.project {
        let disabled=crate::verification_audit::disabled_rules(db,project)?;
        if plan.rule_ids.iter().any(|r|disabled.contains(r)) {
            plan.effective="sensitive".into(); plan.depth="Full".into();
            plan.reasons.push("A confirmed escape disabled a matching lowering rule.".into());
        }
        if matches!(plan.effective.as_str(),"inert"|"contained") {
            let branch=git(repo,&["rev-parse","--abbrev-ref","HEAD"])?;
            let (vendor,_)=crate::git::parse_branch(branch.trim());
            if let Some(candidate)=audit_candidate(repo,issue,&plan,vendor.as_deref().unwrap_or(actor.id()))? {
                plan.audit_sample=Some(crate::verification_audit::record_candidate(db,&candidate,Some(plan.audit_percent))?);
            }
        }
    }
    record(db,issue,&plan,actor)?;
    Ok(plan)
}
pub(crate) fn audit_candidate(repo:&Path,issue:&IssueRecord,plan:&Plan,author:&str)->Result<Option<crate::verification_audit::Candidate>> {
    let (Some(project),Some(policy))=(issue.project.clone(),plan.policy_fingerprint.clone()) else {return Ok(None);};
    Ok(Some(crate::verification_audit::Candidate {repo:crate::verification_audit::repository_id(repo)?,project,issue_key:issue.key.clone(),policy,author:author.into(),declarer:plan.declared_by.clone(),declaration_source:plan.declared_source.clone(),declared:plan.declared.clone(),effective:plan.effective.clone(),rules:plan.rule_ids.clone()}))
}

pub fn record(db: &Db, issue: &IssueRecord, plan: &Plan, actor: &Actor) -> Result<()> {
    let inserted = db.conn.execute(
        "INSERT INTO verification_plan(issue_key, head, policy_commit, classifier, plan, recorded_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
         ON CONFLICT(issue_key, head, policy_commit, classifier) DO UPDATE SET plan=excluded.plan, recorded_at=excluded.recorded_at WHERE plan != excluded.plan",
        rusqlite::params![issue.key, plan.head, plan.policy_commit, plan.classifier, serde_json::to_string(plan)?, crate::db::now()],
    )?;
    if inserted > 0 {
        db.add_note(&issue.key, &format!("Verification shadow at {}: {}. {} {}", plan.head, plan.depth, plan.reasons.join("; "), plan.enforcement), "verification_plan", actor)?;
    }
    Ok(())
}

/// Historical advice, explicitly bound to revisions; never interpreted as a current pass.
pub fn recent(db: &Db, key: &str) -> Result<Vec<Plan>> {
    let mut stmt = db.conn.prepare("SELECT plan FROM verification_plan WHERE issue_key=?1 COLLATE NOCASE ORDER BY recorded_at DESC, rowid DESC LIMIT 10")?;
    let json = stmt.query_map([key], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    json.into_iter().map(|s| Ok(serde_json::from_str(&s)?)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn rules() -> Rules { Rules { mode: "shadow".into(), version: 1, prose: vec!["docs/.*\\.md".into()], comments: vec!["src/.*".into()], display_copy: vec!["src/.*\\.tsx".into()], ..Default::default() } }
    #[test]
    fn comments_preserve_code_directives_strings_and_doc_tests() {
        let r = rules();
        assert!(prove("src/math.rs", "// old\nfn f()->i32 { 1 }", "// clearer\nfn f()->i32 { 1 }", &r).is_ok());
        for (a,b) in [("// old\nfn f()->i32 { 1 }", "// clearer\nfn f()->i32 { 2 }"), ("/// old\nfn f() {}", "/// new\nfn f() {}"), ("// @ts-ignore\nlet x=1;", "// ordinary\nlet x=1;")] {
            assert!(prove("src/math.rs", a, b, &r).is_err());
        }
        assert!(prove("src/math.ts", "const x='// old';", "const x='// new';", &r).is_err());
        assert!(prove("src/math.ts", "function f(){return\n1}", "function f(){return 1}", &r).is_err());
    }
    #[test]
    fn display_copy_requires_native_location_and_unchanged_behavior() {
        let r = rules();
        // Option text supplies its form value when no value attribute exists; textarea
        // children supply initial state. Neither is a display-only proof.
        for tag in ["option","textarea"] {
            assert!(prove("src/Page.tsx",&format!("const x=<{tag}>Old</{tag}>"),&format!("const x=<{tag}>New</{tag}>"),&r).is_err());
        }
        for (a,b) in [("const x=<button>Save</button>", "const x=<button>Save changes</button>"), ("const x=<input placeholder=\"Name\"/>", "const x=<input placeholder=\"Your name\"/>")] {
            assert!(prove("src/Page.tsx", a, b, &r).is_ok(), "{a}");
        }
        for (a,b) in [("const x=<Widget>Save</Widget>", "const x=<Widget>Delete</Widget>"), ("const x=<input value=\"1\"/>", "const x=<input value=\"2\"/>"), ("const x=<button key=\"old\">Save</button>", "const x=<button key=\"new\">Save</button>"), ("const x=<button>{count} Save</button>", "const x=<button>{other} Save</button>"), ("const x=<button>Save</button>", "const x=<button>https://example.com</button>")] {
            assert!(prove("src/Page.tsx", a, b, &r).is_err(), "{a}");
        }
    }
    #[test]
    fn instructions_embedded_code_unknown_parsers_and_protected_paths_escalate() {
        let r = rules();
        assert!(prove("docs/help.md", "A sentence", "A better sentence", &r).is_ok());
        for path in ["AGENTS.md", "docs/skills/help.md", "src/auth.tsx", "src/example.py"] { assert!(prove(path, "old", "new", &r).is_err()); }
        assert!(prove("docs/help.md", "text", "```sh\nrun\n```", &r).is_err());
        assert!(prove("src/Page.tsx", "const x=<button>Old</button>", "const x=<button>New", &r).is_err());
        assert!(prove("src/thing.ts", "// before\nlet a=1;", "// after\nlet b=1;", &r).is_err());
    }

    #[test]
    fn revision_policy_and_declaration_bound_advice_never_grants_a_gate_exemption() {
        use crate::test_repo::git as g;
        let repo = crate::test_repo::copy("verification-policy", |dir| {
            g(dir, &["init", "-b", "main"]);
            std::fs::create_dir_all(dir.join(".nitely")).unwrap();
            std::fs::create_dir_all(dir.join("src")).unwrap();
            std::fs::write(dir.join(PLAYBOOK_PATH), "[verification]\nversion=1\nmode='shadow'\ndisplay_copy=['src/Page[.]tsx']\n").unwrap();
            std::fs::write(dir.join("src/Page.tsx"), "const x=<button>Save</button>").unwrap();
            g(dir, &["add", "."]); g(dir, &["commit", "-m", "base"]);
        });
        let mut db = Db::open(&repo.join("state.sqlite")).unwrap();
        db.create_project("APP", "Example").unwrap();
        let mut issue = db.create_issue(&crate::NewIssue { project: "APP".into(), title: "Improve copy".into(), body: "Clarify a display label".into(), kind: "feature".into(), source: "codex".into(), evidence: "test".into(), review_level: Some("inert".into()), ..Default::default() }, &Actor::Human).unwrap();
        let base = g(&repo, &["rev-parse", "HEAD"]).trim().to_string();
        std::fs::write(repo.join("src/Page.tsx"), "const x=<button>Save changes</button>").unwrap();
        g(&repo, &["add", "src/Page.tsx"]); g(&repo, &["commit", "-m", "copy"]);
        let plan = assess(&repo, &base, "HEAD", &issue).unwrap();
        assert_eq!(plan.depth, "Basic", "{:?}", plan.reasons);
        assert!(plan.shadow && plan.enforcement.contains("existing full gate"));
        assert_eq!(plan.policy_commit, base);
        record(&db, &issue, &plan, &Actor::Human).unwrap();
        record(&db, &issue, &plan, &Actor::Human).unwrap();
        assert_eq!(db.conn.query_row("SELECT count(*) FROM verification_plan", [], |r| r.get::<_,i64>(0)).unwrap(), 1);
        issue.review_level = Some("sensitive".into());
        let raised = assess(&repo, &base, "HEAD", &issue).unwrap();
        assert_eq!(raised.depth, "Full");
        record(&db, &issue, &raised, &Actor::Human).unwrap();
        assert_eq!(recent(&db, "app-1").unwrap()[0].depth, "Full");
        let saved: String = db.conn.query_row("SELECT plan FROM verification_plan", [], |r| r.get(0)).unwrap();
        assert_eq!(serde_json::from_str::<Plan>(&saved).unwrap().depth, "Full");
        issue.review_level = None;
        assert_eq!(assess(&repo, &base, "HEAD", &issue).unwrap().depth, "Full");
        issue.review_level = Some("inert".into());
        let sampled=assess_and_record(&mut db,&repo,&base,"HEAD",&issue,&Actor::Agent("codex:test".into())).unwrap();
        let again=assess_and_record(&mut db,&repo,&base,"HEAD",&issue,&Actor::Agent("codex:test".into())).unwrap();
        assert_eq!(sampled.audit_sample,again.audit_sample);
        let candidate=audit_candidate(&repo,&issue,&sampled,"codex").unwrap().unwrap();
        crate::verification_audit::landed(&mut db,&candidate,&base,&sampled.head,None).unwrap();
        crate::verification_audit::confirmed_landing_culprit(&mut db,"APP",&candidate.repo,&sampled.head,"isolated reproduction evidence").unwrap();
        let disabled=assess_and_record(&mut db,&repo,&base,"HEAD",&issue,&Actor::Agent("codex:test".into())).unwrap();
        assert_eq!(disabled.depth,"Full");
        assert!(disabled.reasons.iter().any(|r|r.contains("confirmed escape")));
        // The candidate cannot authorize itself by changing its own policy.
        std::fs::write(repo.join(PLAYBOOK_PATH), "[verification]\nversion=1\nmode='shadow'\nprose=['.*']\n").unwrap();
        g(&repo, &["add", PLAYBOOK_PATH]); g(&repo, &["commit", "-m", "policy change"]);
        assert_eq!(assess(&repo, &base, "HEAD", &issue).unwrap().depth, "Full");
        drop(db);
        std::fs::remove_dir_all(repo).unwrap();
    }
}
