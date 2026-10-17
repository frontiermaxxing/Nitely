//! Spend: the tokens agents used, read from what they already write. Claude Code keeps a transcript
//! per session (`~/.claude/projects/<folder>/*.jsonl`) with the usage of each model response; Codex
//! keeps a log per session (`~/.codex/sessions/YYYY/MM/DD/*.jsonl`) with running token counts.
//! Only counts, the model, the time, the folder and the branch are kept, never what was said: the
//! same rule as the hooks. Nothing is stored either; files are read again as they grow, and a
//! process keeps what it read so far in memory.
//!
//! A session's usage belongs to the issue its branch (or worktree folder) names, as `nitely-3-…` is
//! NIT-3, and to the project of the repo its folder is in. A session that works on issues from the
//! main checkout gives each response to the issue it last named before it, in a tool call its hooks
//! recorded (`start_task: NIT-3`, a path in the issue's worktree) or a report it wrote; until it
//! names one, it counts toward the project only.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::config::Config;
use crate::db::Db;
use crate::paths::normalize;

/// One model response (Claude) or turn step (Codex): what it cost in tokens.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Usage {
    /// `claude` or `codex`.
    pub vendor: &'static str,
    pub session: String,
    pub ts: i64,
    pub model: String,
    /// Input not read from the cache.
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cwd: String,
    pub branch: String,
}

impl Usage {
    /// The tokens counted as spend: new input, cache writes and output. Cache reads are cheap and
    /// would drown everything else, so they are counted apart.
    pub fn tokens(&self) -> u64 {
        self.input + self.output + self.cache_write
    }
}

/// A usage with the project and issue it belongs to.
#[derive(Debug, Clone, Serialize)]
pub struct Attributed {
    #[serde(flatten)]
    pub usage: Usage,
    pub project: Option<String>,
    pub issue: Option<String>,
}

// ----- reading -----

#[derive(Deserialize)]
struct ClaudeLine {
    #[serde(rename = "type")]
    kind: Option<String>,
    timestamp: Option<String>,
    #[serde(rename = "sessionId")]
    session: Option<String>,
    cwd: Option<String>,
    #[serde(rename = "gitBranch")]
    branch: Option<String>,
    message: Option<ClaudeMessage>,
}

#[derive(Deserialize)]
struct ClaudeMessage {
    id: Option<String>,
    model: Option<String>,
    usage: Option<ClaudeUsage>,
}

#[derive(Deserialize, Default)]
struct ClaudeUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
}

/// What a file has given so far, so the next read starts where this one stopped.
#[derive(Default)]
struct FileState {
    /// Bytes read up to the end of the last whole line.
    offset: u64,
    records: Vec<Usage>,
    /// Claude writes a response once per content block, each with the same usage: one per id.
    claude_ids: HashMap<String, usize>,
    codex: CodexState,
    /// A Codex session's folder, from its first line, while no watched repo holds it: it is not
    /// read further, and is checked again on the next scan (a repo may be linked since).
    unwanted_cwd: Option<String>,
}

#[derive(Default, Clone)]
struct CodexState {
    session: String,
    cwd: String,
    branch: String,
    model: String,
    /// The last running total seen: input, cached input, output.
    total: (u64, u64, u64),
}

/// Reads Claude transcript lines, adding or updating records by message id.
fn read_claude_lines(text: &str, state: &mut FileState) {
    for line in text.lines() {
        // Only responses carry usage; skip the rest (prompts, tool results) without parsing them.
        if !line.contains("\"usage\"") || !line.contains("\"assistant\"") {
            continue;
        }
        let Ok(l) = serde_json::from_str::<ClaudeLine>(line) else { continue };
        if l.kind.as_deref() != Some("assistant") {
            continue;
        }
        let Some(m) = l.message else { continue };
        let Some(u) = m.usage else { continue };
        let model = m.model.unwrap_or_default();
        if model.starts_with('<') {
            continue; // `<synthetic>`: no model ran
        }
        let record = Usage {
            vendor: "claude",
            session: l.session.unwrap_or_default(),
            ts: l.timestamp.as_deref().and_then(crate::linear_import::parse_iso).unwrap_or(0),
            model,
            input: u.input_tokens,
            output: u.output_tokens,
            cache_read: u.cache_read_input_tokens,
            cache_write: u.cache_creation_input_tokens,
            cwd: l.cwd.unwrap_or_default(),
            branch: l.branch.unwrap_or_default(),
        };
        match m.id.filter(|id| !id.is_empty()) {
            Some(id) => match state.claude_ids.get(&id) {
                Some(&i) => state.records[i] = record,
                None => {
                    state.claude_ids.insert(id, state.records.len());
                    state.records.push(record);
                }
            },
            None => state.records.push(record),
        }
    }
}

/// Reads Codex log lines: the session's folder and branch, the model of each turn, and the growth
/// of the running token total.
fn read_codex_lines(text: &str, state: &mut FileState) {
    for line in text.lines() {
        let wanted = line.contains("\"session_meta\"") || line.contains("\"turn_context\"") || line.contains("\"token_count\"");
        if !wanted {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        let p = &v["payload"];
        let s = &mut state.codex;
        match v["type"].as_str() {
            Some("session_meta") => {
                s.session = p["id"].as_str().unwrap_or_default().to_string();
                s.cwd = p["cwd"].as_str().unwrap_or_default().to_string();
                s.branch = p["git"]["branch"].as_str().unwrap_or_default().to_string();
            }
            Some("turn_context") => {
                if let Some(m) = p["model"].as_str() {
                    s.model = m.to_string();
                }
            }
            Some("event_msg") if p["type"] == "token_count" => {
                let t = &p["info"]["total_token_usage"];
                let n = |k: &str| t[k].as_u64().unwrap_or(0);
                let total = (n("input_tokens"), n("cached_input_tokens"), n("output_tokens"));
                // The same total is reported again when only the rate limits changed.
                if total.0 + total.2 <= s.total.0 + s.total.2 {
                    continue;
                }
                let (input, cached, output) = (total.0 - s.total.0.min(total.0), total.1.saturating_sub(s.total.1), total.2.saturating_sub(s.total.2));
                s.total = total;
                state.records.push(Usage {
                    vendor: "codex",
                    session: s.session.clone(),
                    ts: v["timestamp"].as_str().and_then(crate::linear_import::parse_iso).unwrap_or(0),
                    model: s.model.clone(),
                    // OpenAI counts cached input inside input.
                    input: input.saturating_sub(cached),
                    output,
                    cache_read: cached,
                    cache_write: 0,
                    cwd: s.cwd.clone(),
                    branch: s.branch.clone(),
                });
            }
            _ => {}
        }
    }
}

/// What every file read so far gave, by path.
static FILES: Mutex<Option<HashMap<PathBuf, FileState>>> = Mutex::new(None);

/// The records of `path`, reading only what was added since the last call. A Codex log whose
/// first line puts it in a folder `wanted` refuses is skipped from then on.
fn read_file(path: &Path, codex: bool, wanted: &dyn Fn(&str) -> bool, files: &mut HashMap<PathBuf, FileState>) -> Vec<Usage> {
    let state = files.entry(path.to_path_buf()).or_default();
    let len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if len < state.offset {
        *state = FileState::default(); // rewritten or truncated: start over
    }
    if let Some(cwd) = &state.unwanted_cwd {
        if !wanted(cwd) {
            return Vec::new();
        }
        state.unwanted_cwd = None;
    }
    if codex && state.offset == 0 && len > 0 {
        let first = std::fs::File::open(path).ok().and_then(|f| {
            let mut line = String::new();
            std::io::BufRead::read_line(&mut std::io::BufReader::new(f), &mut line).ok().map(|_| line)
        });
        let cwd = first
            .as_deref()
            .and_then(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|v| v["type"] == "session_meta")
            .and_then(|v| v["payload"]["cwd"].as_str().map(str::to_string));
        if let Some(c) = cwd.filter(|c| !wanted(c)) {
            state.unwanted_cwd = Some(c);
            return Vec::new();
        }
    }
    if len > state.offset {
        if let Ok(mut f) = std::fs::File::open(path) {
            let mut buf = Vec::new();
            if f.seek(SeekFrom::Start(state.offset)).is_ok() && f.read_to_end(&mut buf).is_ok() {
                // Up to the last whole line; a line still being written is read next time.
                if let Some(end) = buf.iter().rposition(|&b| b == b'\n') {
                    let text = String::from_utf8_lossy(&buf[..=end]);
                    if codex {
                        read_codex_lines(&text, state);
                    } else {
                        read_claude_lines(&text, state);
                    }
                    state.offset += end as u64 + 1;
                }
            }
        }
    }
    state.records.clone()
}

/// Where Claude Code and Codex keep their sessions.
fn claude_home() -> Option<PathBuf> {
    std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from).or_else(|| dirs::home_dir().map(|h| h.join(".claude")))
}

fn codex_home() -> Option<PathBuf> {
    std::env::var_os("CODEX_HOME").map(PathBuf::from).or_else(|| dirs::home_dir().map(|h| h.join(".codex")))
}

/// Claude Code's folder name for a working directory: every character that is not a letter or a
/// digit becomes `-`.
fn claude_folder(dir: &str) -> String {
    dir.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect()
}

fn modified_since(path: &Path, since: i64) -> bool {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .is_none_or(|d| d.as_secs() as i64 >= since)
}

fn jsonl_in(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() && depth > 0 {
            jsonl_in(&p, depth - 1, out);
        } else if p.extension().is_some_and(|x| x == "jsonl") {
            out.push(p);
        }
    }
}

/// Where a repo's agents work: its main checkout and the folder its playbook puts worktrees in.
struct Place {
    project: String,
    root: String,
    worktrees: Option<String>,
}

fn places(config: &Config) -> Vec<Place> {
    config
        .repos
        .iter()
        .filter_map(|r| {
            let project = r.project.clone()?;
            let pb = crate::playbook::Playbook::load_with(Path::new(&r.root), config).unwrap_or_default();
            let probe = crate::IssueRecord { key: "X-1".into(), title: "x".into(), ..Default::default() };
            let worktrees = pb.worktree_for(Path::new(&r.root), "x", &probe).parent().map(|p| normalize(&p.to_string_lossy()));
            Some(Place { project, root: normalize(&r.root), worktrees })
        })
        .collect()
}

/// The usage of every session in the repos Nitely watches, from files written since `since`,
/// read from `claude` and `codex` (the agents' home folders). `aliases` are projects' earlier
/// keys, so branches named before a re-key still find their issue.
pub fn scan_in(config: &Config, aliases: &HashMap<String, String>, since: i64, claude: Option<&Path>, codex: Option<&Path>) -> Vec<Attributed> {
    let places = places(config);
    let roots: Vec<String> = places.iter().flat_map(|p| std::iter::once(claude_folder(&p.root)).chain(p.worktrees.as_deref().map(claude_folder))).collect();
    let mut paths: Vec<(PathBuf, bool)> = Vec::new();
    if let Some(dir) = claude.map(|c| c.join("projects")) {
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            // A repo's own folder, and its worktrees' (the worktree folder's name, then `-` and more).
            if roots.iter().any(|r| name.eq_ignore_ascii_case(r) || name.to_lowercase().starts_with(&format!("{}-", r.to_lowercase()))) {
                let mut files = Vec::new();
                jsonl_in(&e.path(), 2, &mut files);
                paths.extend(files.into_iter().filter(|f| modified_since(f, since)).map(|f| (f, false)));
            }
        }
    }
    if let Some(dir) = codex.map(|c| c.join("sessions")) {
        let mut files = Vec::new();
        jsonl_in(&dir, 3, &mut files);
        paths.extend(files.into_iter().filter(|f| modified_since(f, since)).map(|f| (f, true)));
    }
    let mut guard = FILES.lock().unwrap_or_else(|e| e.into_inner());
    let files = guard.get_or_insert_with(HashMap::new);
    let mut out = Vec::new();
    let mut projects: HashMap<String, Option<String>> = HashMap::new();
    let wanted = |cwd: &str| project_of(&places, cwd).is_some();
    for (path, is_codex) in paths {
        for u in read_file(&path, is_codex, &wanted, files) {
            if u.ts < since {
                continue;
            }
            let project = projects.entry(u.cwd.clone()).or_insert_with(|| project_of(&places, &u.cwd)).clone();
            let Some(project) = project else { continue };
            let issue = issue_of(&u, &project, aliases);
            out.push(Attributed { usage: u, project: Some(project), issue });
        }
    }
    out
}

/// As `scan_in`, from the agents' usual folders, with a main-checkout session's usage given to the
/// issue it was on at the time (`by_session`).
pub fn scan(config: &Config, db: &Db, since: i64) -> Vec<Attributed> {
    let aliases = db.project_aliases().unwrap_or_default();
    let mut records = scan_in(config, &aliases, since, claude_home().as_deref(), codex_home().as_deref());
    if let Ok(touches) = db.session_touches(since - TOUCH_LOOKBACK_SECS) {
        by_session(&mut records, &touches);
    }
    records
}

/// How far before a scan's start a session's last named issue is looked for: a session that took
/// an issue in the morning is still on it in the afternoon.
const TOUCH_LOOKBACK_SECS: i64 = 2 * 86_400;

/// The issues each session named, by its hook id (`claude:<session>`, as the transcripts' session
/// id), in time order.
pub type Touches = HashMap<String, Vec<(i64, String)>>;

/// Tools that read about an issue or triage it rather than work on it: naming one there does not
/// move the session onto it (a coordinator reads and moves many).
const NOT_WORK_TOOLS: &[&str] = &["get_issue", "list_issues", "file_issue", "file_issues", "update_issue", "get_answer", "get_project"];

impl Db {
    /// The issues each session named since `since`: the issue of an event it wrote (a report, a
    /// note), else the one issue a tool call names (`start_task: NIT-3`, a path in its worktree).
    /// A call that names several, or only reads about or triages one, says nothing.
    pub fn session_touches(&self, since: i64) -> Result<Touches> {
        let mut keys: std::collections::HashSet<String> = {
            let mut stmt = self.conn.prepare("SELECT key FROM issue")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        // A summary written before a re-key names the issue by its old key.
        let aliases = self.project_aliases().unwrap_or_default();
        let old: Vec<String> = keys
            .iter()
            .filter_map(|k| k.split_once('-'))
            .flat_map(|(p, n)| aliases.iter().filter(move |(_, now)| now.eq_ignore_ascii_case(p)).map(move |(was, _)| format!("{}-{n}", was.to_uppercase())))
            .collect();
        keys.extend(old);
        let prefixes: std::collections::HashSet<String> = keys.iter().filter_map(|k| k.split_once('-')).map(|(p, _)| p.to_lowercase()).collect();
        let mut stmt = self.conn.prepare(
            "SELECT actor, ts, issue_key, COALESCE(summary, '') FROM event
             WHERE ts >= ?1 AND (actor LIKE 'claude:%' OR actor LIKE 'codex:%') AND (issue_key IS NOT NULL OR summary IS NOT NULL)
             ORDER BY ts, id",
        )?;
        let rows = stmt.query_map([since], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, Option<String>>(2)?, r.get::<_, String>(3)?)))?;
        let mut out = Touches::new();
        for row in rows {
            let (actor, ts, issue, summary) = row?;
            let tool = summary.split_once(':').map(|(t, _)| t).unwrap_or_default();
            let key = match issue {
                Some(k) => Some(k),
                None if NOT_WORK_TOOLS.iter().any(|t| tool.ends_with(t)) => None,
                None => match crate::activity::issue_keys(&summary, &prefixes, &keys)[..] {
                    [ref k] => Some(crate::projects::current_key(&aliases, k)),
                    _ => None,
                },
            };
            if let Some(k) = key {
                out.entry(actor).or_default().push((ts, k.to_uppercase()));
            }
        }
        Ok(out)
    }
}

/// Gives each usage no branch or folder placed (a session in the main checkout) to the issue its
/// session named last before it, when that issue is in the usage's project.
pub fn by_session(records: &mut [Attributed], touches: &Touches) {
    let shared = codex_shared(records, touches);
    for r in records.iter_mut().filter(|r| r.issue.is_none()) {
        let id = format!("{}:{}", r.usage.vendor, r.usage.session);
        let Some(named) = shared.get(&id).or_else(|| touches.get(&id)) else { continue };
        let last = named.partition_point(|(ts, _)| *ts <= r.usage.ts);
        r.issue = last
            .checked_sub(1)
            .map(|i| named[i].1.clone())
            .filter(|k| r.project.as_deref().is_some_and(|p| k.split('-').next().is_some_and(|kp| kp.eq_ignore_ascii_case(p))));
    }
}

/// How far outside a Codex session's usage an event under its folder's id may still be its own.
const CODEX_SLACK_SECS: i64 = 10 * 60;

/// Codex writes its MCP events (`start_task`, notes) under one id per folder (`codex:c:/code/app`),
/// which every Codex session there shares, and its hooks name no tools. Such an event goes to the
/// one Codex session in that folder whose usage runs around then; with two at once, to neither.
/// Returns each such session's touches, its own and these, in time order.
fn codex_shared(records: &[Attributed], touches: &Touches) -> Touches {
    let mut spans: HashMap<(String, String), (i64, i64)> = HashMap::new();
    for r in records.iter().filter(|r| r.usage.vendor == "codex") {
        let span = spans.entry((format!("codex:{}", normalize(&r.usage.cwd)), format!("codex:{}", r.usage.session))).or_insert((r.usage.ts, r.usage.ts));
        span.0 = span.0.min(r.usage.ts);
        span.1 = span.1.max(r.usage.ts);
    }
    let mut out = Touches::new();
    for (agent, named) in touches.iter().filter(|(a, _)| a.starts_with("codex:")) {
        for (t, k) in named {
            let near: Vec<&String> = spans
                .iter()
                .filter(|((a, _), (from, to))| a == agent && *t >= from - CODEX_SLACK_SECS && *t <= to + CODEX_SLACK_SECS)
                .map(|((_, s), _)| s)
                .collect();
            if let [session] = near[..] {
                out.entry(session.clone()).or_default().push((*t, k.clone()));
            }
        }
    }
    for (session, list) in out.iter_mut() {
        list.extend(touches.get(session).into_iter().flatten().cloned());
        list.sort();
    }
    out
}

/// The project of the watched repo `cwd` is in: its main checkout or a worktree in the folder its
/// playbook puts them, the longest match. Worktrees are often gone by the time their usage is
/// read, so this goes by the path, not by asking git.
fn project_of(places: &[Place], cwd: &str) -> Option<String> {
    let dir = normalize(cwd);
    let under = |base: &str| dir == base || dir.starts_with(&format!("{base}/"));
    places
        .iter()
        .filter_map(|p| {
            let len = [Some(p.root.as_str()), p.worktrees.as_deref()].into_iter().flatten().filter(|b| under(b)).map(str::len).max()?;
            Some((len, p.project.clone()))
        })
        .max_by_key(|(len, _)| *len)
        .map(|(_, project)| project)
}

/// The issue a session worked on: the one its branch names, else its worktree folder, under the
/// project's key now (a key from before a re-key is followed through `aliases`).
fn issue_of(u: &Usage, project: &str, aliases: &HashMap<String, String>) -> Option<String> {
    let folder = normalize(&u.cwd).rsplit('/').next().unwrap_or_default().to_string();
    let found = [u.branch.as_str(), folder.as_str()]
        .into_iter()
        .filter_map(|s| crate::git::parse_branch(s).1)
        .map(|k| crate::projects::current_key(aliases, &k))
        .find(|k| k.split('-').next().is_some_and(|p| p.eq_ignore_ascii_case(project)));
    found
}

/// A token count in a few characters: `512`, `86k`, `1.2M`.
pub fn short(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => format!("{}k", (n + 500) / 1000),
        _ => {
            let m = n as f64 / 1e6;
            if m < 10.0 { format!("{m:.1}M") } else { format!("{}M", m.round()) }
        }
    }
}

// ----- totals -----

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct ModelTokens {
    pub vendor: String,
    pub model: String,
    pub tokens: u64,
    pub calls: u64,
}

/// What an issue's agents used.
#[derive(Debug, Clone, Default, Serialize)]
pub struct IssueSpend {
    pub key: String,
    pub tokens: u64,
    pub cache_read: u64,
    /// Model responses (Claude) and turn steps (Codex).
    pub calls: u64,
    pub sessions: usize,
    /// Most tokens first.
    pub by_model: Vec<ModelTokens>,
    pub first: Option<i64>,
    pub last: Option<i64>,
}

fn by_model<'a>(records: impl Iterator<Item = &'a Usage>) -> Vec<ModelTokens> {
    let mut m: BTreeMap<(String, String), ModelTokens> = BTreeMap::new();
    for u in records {
        let e = m.entry((u.vendor.to_string(), u.model.clone())).or_insert_with(|| ModelTokens { vendor: u.vendor.into(), model: u.model.clone(), ..Default::default() });
        e.tokens += u.tokens();
        e.calls += 1;
    }
    let mut v: Vec<ModelTokens> = m.into_values().collect();
    v.sort_by(|a, b| b.tokens.cmp(&a.tokens));
    v
}

pub fn issue_spend(records: &[Attributed], key: &str) -> IssueSpend {
    let mine: Vec<&Usage> = records.iter().filter(|a| a.issue.as_deref().is_some_and(|k| k.eq_ignore_ascii_case(key))).map(|a| &a.usage).collect();
    let mut sessions: Vec<&str> = mine.iter().map(|u| u.session.as_str()).collect();
    sessions.sort();
    sessions.dedup();
    IssueSpend {
        key: key.to_uppercase(),
        tokens: mine.iter().map(|u| u.tokens()).sum(),
        cache_read: mine.iter().map(|u| u.cache_read).sum(),
        calls: mine.len() as u64,
        sessions: sessions.len(),
        by_model: by_model(mine.iter().copied()),
        first: mine.iter().map(|u| u.ts).min(),
        last: mine.iter().map(|u| u.ts).max(),
    }
}

/// Tokens a project's agents used since `since`.
pub fn project_tokens_since(records: &[Attributed], project: &str, since: i64) -> u64 {
    records.iter().filter(|a| a.usage.ts >= since && a.project.as_deref().is_some_and(|p| p.eq_ignore_ascii_case(project))).map(|a| a.usage.tokens()).sum()
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct DaySpend {
    pub day_start: i64,
    pub tokens: u64,
    pub landed: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct IssueTokens {
    pub key: String,
    pub tokens: u64,
}

/// A project's spend over the last days.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ProjectSpend {
    pub project: String,
    /// Oldest first, one per day, the last one today.
    pub days: Vec<DaySpend>,
    pub tokens: u64,
    pub cache_read: u64,
    pub by_agent: Vec<ModelTokens>,
    pub by_model: Vec<ModelTokens>,
    pub landed: usize,
    /// Tokens spent on the issues that landed in the window, per issue.
    pub per_landed: Option<u64>,
    /// The issues that used the most, most first.
    pub top_issues: Vec<IssueTokens>,
}

/// `project`'s spend over `days` days, the last starting at `today_start` (local midnight).
pub fn project_spend(db: &Db, records: &[Attributed], project: &str, today_start: i64, days: usize) -> Result<ProjectSpend> {
    let days = days.max(1);
    let from = today_start - (days as i64 - 1) * 86_400;
    let mine: Vec<&Attributed> = records.iter().filter(|a| a.usage.ts >= from && a.project.as_deref().is_some_and(|p| p.eq_ignore_ascii_case(project))).collect();
    let landed = db.landed_since(project, from)?;
    let day_of = |ts: i64| ((ts - from).div_euclid(86_400)).clamp(0, days as i64 - 1) as usize;
    let mut series: Vec<DaySpend> = (0..days).map(|i| DaySpend { day_start: from + i as i64 * 86_400, ..Default::default() }).collect();
    for a in &mine {
        series[day_of(a.usage.ts)].tokens += a.usage.tokens();
    }
    for (_, ts) in &landed {
        series[day_of(*ts)].landed += 1;
    }
    let mut agents = by_model(mine.iter().map(|a| &a.usage));
    // By agent: the models folded into their vendor.
    let mut vendors: BTreeMap<String, ModelTokens> = BTreeMap::new();
    for m in &agents {
        let e = vendors.entry(m.vendor.clone()).or_insert_with(|| ModelTokens { vendor: m.vendor.clone(), ..Default::default() });
        e.tokens += m.tokens;
        e.calls += m.calls;
    }
    let by_model = std::mem::take(&mut agents);
    let mut by_agent: Vec<ModelTokens> = vendors.into_values().collect();
    by_agent.sort_by(|a, b| b.tokens.cmp(&a.tokens));
    let mut per_issue: BTreeMap<String, u64> = BTreeMap::new();
    for a in &mine {
        if let Some(k) = &a.issue {
            *per_issue.entry(k.clone()).or_insert(0) += a.usage.tokens();
        }
    }
    // Landed issues' spend counts all their usage, also from before the window.
    let landed_tokens: u64 = landed.iter().map(|(k, _)| issue_spend(records, k).tokens).sum();
    let mut top: Vec<IssueTokens> = per_issue.into_iter().map(|(key, tokens)| IssueTokens { key, tokens }).collect();
    top.sort_by(|a, b| b.tokens.cmp(&a.tokens));
    top.truncate(5);
    Ok(ProjectSpend {
        project: project.to_uppercase(),
        days: series,
        tokens: mine.iter().map(|a| a.usage.tokens()).sum(),
        cache_read: mine.iter().map(|a| a.usage.cache_read).sum(),
        by_agent,
        by_model,
        landed: landed.len(),
        per_landed: (!landed.is_empty() && landed_tokens > 0).then(|| landed_tokens / landed.len() as u64),
        top_issues: top,
    })
}

/// An issue's spend, reading usage from a day before it was filed.
pub fn spend_of_issue(config: &Config, db: &Db, key: &str) -> Result<IssueSpend> {
    let issue = db.issue(key)?.ok_or_else(|| anyhow::anyhow!("no issue {key}"))?;
    let since = crate::linear_import::parse_iso(&issue.created_at).unwrap_or(0) - 86_400;
    Ok(issue_spend(&scan(config, db, since.max(0)), &issue.key))
}

/// A project's spend over `days` days ending today (`today_start` is local midnight). The issues
/// that landed in the window count all their usage, so it is read from when the oldest was filed.
pub fn spend_of_project(config: &Config, db: &Db, project: &str, today_start: i64, days: usize) -> Result<ProjectSpend> {
    let days = days.clamp(1, 90);
    let from = today_start - (days as i64 - 1) * 86_400;
    let mut since = from;
    for (key, _) in db.landed_since(project, from)? {
        if let Some(created) = db.issue(&key)?.and_then(|i| crate::linear_import::parse_iso(&i.created_at)) {
            since = since.min(created - 86_400);
        }
    }
    project_spend(db, &scan(config, db, since.max(0)), project, today_start, days)
}

impl Db {
    /// Issues of `project` that landed since `since`, with when (the last land of each).
    pub fn landed_since(&self, project: &str, since: i64) -> Result<Vec<(String, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT issue_key, MAX(ts) FROM event WHERE type = 'landed' AND ts >= ?2
               AND issue_key LIKE ?1 || '-%' COLLATE NOCASE GROUP BY issue_key ORDER BY 2",
        )?;
        let rows = stmt.query_map(rusqlite::params![project, since], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claude_line(id: &str, ts: &str, branch: &str, cwd: &str, input: u64, output: u64) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"{ts}","sessionId":"s1","cwd":"{cwd}","gitBranch":"{branch}","message":{{"id":"{id}","model":"claude-opus-5-5","content":[{{"type":"text","text":"secret words"}}],"usage":{{"input_tokens":{input},"output_tokens":{output},"cache_read_input_tokens":1000,"cache_creation_input_tokens":10}}}}}}"#,
            cwd = cwd.replace('\\', "\\\\")
        )
    }

    #[test]
    fn reads_claude_responses_once_and_never_their_text() {
        let mut state = FileState::default();
        let text = [
            r#"{"type":"user","message":{"role":"user","content":"do the thing"}}"#.to_string(),
            claude_line("m1", "2026-10-16T10:00:00.000Z", "claude/nit-3-x", "C:\\dev\\app-worktrees\\nit-3-x", 5, 50),
            claude_line("m1", "2026-10-16T10:00:01.000Z", "claude/nit-3-x", "C:\\dev\\app-worktrees\\nit-3-x", 5, 80),
            claude_line("m2", "2026-10-16T10:01:00.000Z", "claude/nit-3-x", "C:\\dev\\app-worktrees\\nit-3-x", 2, 20),
        ]
        .join("\n");
        read_claude_lines(&text, &mut state);
        assert_eq!(state.records.len(), 2, "a response's blocks count once");
        assert_eq!((state.records[0].output, state.records[0].tokens()), (80, 95));
        assert_eq!(state.records[0].cache_read, 1000);
        assert!(!format!("{:?}", state.records).contains("secret"), "no text is kept");
    }

    #[test]
    fn a_sibling_folder_is_not_the_repo_but_its_worktrees_are() {
        let places = vec![Place { project: "NIT".into(), root: normalize(r"C:\code\app"), worktrees: Some(normalize(r"C:\code\app-worktrees")) }];
        assert_eq!(project_of(&places, r"C:\code\app\src").as_deref(), Some("NIT"));
        assert_eq!(project_of(&places, r"C:\code\app-worktrees\nit-3-x").as_deref(), Some("NIT"));
        assert_eq!(project_of(&places, r"C:\code\app-other"), None, "another checkout that only starts with the name");
        let custom = vec![Place { project: "NIT".into(), root: normalize(r"C:\code\app"), worktrees: Some(normalize(r"D:\trees")) }];
        assert_eq!(project_of(&custom, r"D:\trees\nit-3-x").as_deref(), Some("NIT"), "a playbook's own worktree folder");
    }

    #[test]
    fn short_counts() {
        assert_eq!([short(512), short(86_400), short(1_234_567), short(12_600_000)], ["512", "86k", "1.2M", "13M"]);
    }

    #[test]
    fn reads_codex_token_growth() {
        let mut state = FileState::default();
        let text = r#"{"type":"session_meta","payload":{"id":"c1","cwd":"C:\\dev\\app-worktrees\\nit-4-y","git":{"branch":"codex/nit-4-y"}}}
{"type":"turn_context","payload":{"model":"gpt-6"}}
{"timestamp":"2026-10-16T10:00:00Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1000,"cached_input_tokens":400,"output_tokens":50}}}}
{"timestamp":"2026-10-16T10:00:05Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1000,"cached_input_tokens":400,"output_tokens":50}}}}
{"timestamp":"2026-10-16T10:01:00Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1600,"cached_input_tokens":900,"output_tokens":80}}}}"#;
        read_codex_lines(text, &mut state);
        let got: Vec<(u64, u64, u64)> = state.records.iter().map(|u| (u.input, u.cache_read, u.output)).collect();
        assert_eq!(got, [(600, 400, 50), (100, 500, 30)], "a repeated total is not counted twice");
        assert_eq!((state.records[0].model.as_str(), state.records[0].branch.as_str()), ("gpt-6", "codex/nit-4-y"));
    }

    #[test]
    fn scans_watched_repos_attributes_issues_and_sums_a_project() {
        let base = crate::paths::unique_temp("nitely-usage", "");
        let (claude, codex) = (base.join("claude"), base.join("codex"));
        let root = r"C:\dev\app";
        let folder = claude.join("projects").join(claude_folder(r"C:\dev\app-worktrees\nit-3-x"));
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::create_dir_all(claude.join("projects").join(claude_folder(r"C:\dev\other"))).unwrap();
        let file = folder.join("s1.jsonl");
        std::fs::write(&file, claude_line("m1", "2026-10-16T10:00:00Z", "claude/nit-3-x", r"C:\dev\app-worktrees\nit-3-x", 5, 50) + "\n").unwrap();
        std::fs::write(
            claude.join("projects").join(claude_folder(r"C:\dev\other")).join("s2.jsonl"),
            claude_line("m9", "2026-10-16T10:00:00Z", "main", r"C:\dev\other", 5, 50) + "\n",
        )
        .unwrap();
        let dir = codex.join("sessions/2026/10/16");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("rollout.jsonl"),
            r#"{"type":"session_meta","payload":{"id":"c1","cwd":"C:\\dev\\app","git":{"branch":"main"}}}
{"timestamp":"2026-10-16T11:00:00Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":300,"cached_input_tokens":0,"output_tokens":20}}}}
"#,
        )
        .unwrap();
        let config = Config { repos: vec![crate::RepoConfig { name: "app".into(), root: root.into(), integration_branch: "main".into(), project: Some("NIT".into()) }], ..Default::default() };
        let none = HashMap::new();
        let records = scan_in(&config, &none, 0, Some(&claude), Some(&codex));
        assert_eq!(records.len(), 2, "the other repo's session is not read");
        let spend = issue_spend(&records, "NIT-3");
        assert_eq!((spend.tokens, spend.calls, spend.sessions), (65, 1, 1));
        assert_eq!(issue_spend(&records, "NIT-4").tokens, 0, "main-checkout work counts toward the project only");

        // A line added later is read without reading the file again from the start.
        let mut f = std::fs::OpenOptions::new().append(true).open(&file).unwrap();
        std::io::Write::write_all(&mut f, (claude_line("m2", "2026-10-16T12:00:00Z", "claude/nit-3-x", r"C:\dev\app-worktrees\nit-3-x", 1, 9) + "\n").as_bytes()).unwrap();
        let records = scan_in(&config, &none, 0, Some(&claude), Some(&codex));
        assert_eq!(issue_spend(&records, "NIT-3").tokens, 85);

        // After a re-key, the old branch's usage belongs to the issue under its new key.
        let renamed = Config { repos: vec![crate::RepoConfig { project: Some("APP".into()), ..config.repos[0].clone() }], ..Default::default() };
        let aliases = HashMap::from([("NIT".to_string(), "APP".to_string())]);
        assert_eq!(issue_spend(&scan_in(&renamed, &aliases, 0, Some(&claude), Some(&codex)), "APP-3").tokens, 85);

        let db = Db::open(&base.join("t.db")).unwrap();
        db.conn.execute("INSERT INTO event (ts, actor, issue_key, type, summary) VALUES (?1, 'a', 'NIT-3', 'landed', 'x')", [crate::linear_import::parse_iso("2026-10-16T13:00:00Z").unwrap()]).unwrap();
        let today = crate::linear_import::parse_iso("2026-10-16T00:00:00Z").unwrap();
        let p = project_spend(&db, &records, "NIT", today, 3).unwrap();
        assert_eq!(p.days.iter().map(|d| (d.tokens, d.landed)).collect::<Vec<_>>(), [(0, 0), (0, 0), (405, 1)]);
        assert_eq!((p.tokens, p.landed, p.per_landed), (405, 1, Some(85)));
        assert_eq!(p.by_agent.iter().map(|a| (a.vendor.as_str(), a.tokens)).collect::<Vec<_>>(), [("codex", 320), ("claude", 85)]);
        assert_eq!(p.top_issues.iter().map(|i| (i.key.as_str(), i.tokens)).collect::<Vec<_>>(), [("NIT-3", 85)]);
        assert_eq!(project_tokens_since(&records, "nit", today + 10 * 3600 + 1800), 340, "since 10:30: the Codex turn at 11:00 and the reply at 12:00");
        drop(db);
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn a_main_checkout_session_works_on_the_issue_it_named_last() {
        let base = crate::paths::unique_temp("nitely-usage-touch", "");
        let mut db = Db::open(&base.join("t.db")).unwrap();
        db.create_project("NIT", "Nitely").unwrap();
        let new = |title: &str| crate::NewIssue { project: "NIT".into(), title: title.into(), body: "b".into(), kind: "feature".into(), source: "claude".into(), evidence: "e".into(), priority: 3, allow_similar: true, ..Default::default() };
        let human = crate::Actor::Human;
        let (a, b) = (db.create_issue(&new("One"), &human).unwrap().key, db.create_issue(&new("Two"), &human).unwrap().key);
        let tool = |ts: i64, actor: &str, summary: &str| {
            db.conn.execute("INSERT INTO event (ts, actor, type, summary) VALUES (?1, ?2, 'tool', ?3)", rusqlite::params![ts, actor, summary]).unwrap();
        };
        tool(100, "claude:s1", &format!("mcp__nitely__start_task: {a}"));
        tool(200, "claude:s1", &format!("mcp__nitely__get_issue: {b}"));
        tool(210, "claude:s1", &format!("mcp__nitely__update_issue: {b}"));
        tool(220, "claude:s1", &format!("Bash: diff {a} {b}"));
        tool(300, "claude:s1", r"Read: C:\dev\app-worktrees\nit-2-two\src\lib.rs");
        tool(150, "claude:other", &format!("mcp__nitely__start_task: {b}"));
        db.conn.execute("INSERT INTO event (ts, actor, type, summary, issue_key) VALUES (400, 'codex:c1', 'status', 'Fixing it', ?1)", [&a]).unwrap();
        let touches = db.session_touches(0).unwrap();
        assert_eq!(touches["claude:s1"], [(100, a.clone()), (300, b.clone())], "reading, triaging and naming two say nothing");
        // A summary from before a re-key names the old key.
        db.conn.execute("INSERT INTO project_alias (old_key, project, created_at) VALUES ('OLD', 'NIT', 0)", []).unwrap();
        tool(350, "claude:s2", "mcp__nitely__start_task: OLD-2");
        assert_eq!(db.session_touches(0).unwrap()["claude:s2"], [(350, b.clone())]);

        let usage = |vendor: &'static str, session: &str, ts: i64| Attributed {
            usage: Usage { vendor, session: session.into(), ts, model: "m".into(), input: 1, output: 1, cache_read: 0, cache_write: 0, cwd: r"C:\dev\app".into(), branch: "main".into() },
            project: Some("NIT".into()),
            issue: None,
        };
        let mut records = vec![usage("claude", "s1", 50), usage("claude", "s1", 120), usage("claude", "s1", 250), usage("claude", "s1", 300), usage("codex", "c1", 500), usage("claude", "s9", 500)];
        by_session(&mut records, &touches);
        let got: Vec<Option<&str>> = records.iter().map(|r| r.issue.as_deref()).collect();
        assert_eq!(got, [None, Some(a.as_str()), Some(a.as_str()), Some(b.as_str()), Some(a.as_str()), None]);

        // Codex's MCP events are under its folder's id: they go to the one Codex session there then.
        let codex_touches = Touches::from([(format!("codex:{}", normalize("C:/dev/app")), vec![(600, b.clone()), (5000, a.clone())])]);
        let codex = |session: &str, ts: i64| Attributed { usage: Usage { cwd: "C:/dev/app".into(), ..usage("codex", session, ts).usage }, ..usage("codex", session, ts) };
        let mut alone = vec![codex("c1", 590), codex("c1", 700), codex("c2", 9000)];
        by_session(&mut alone, &codex_touches);
        assert_eq!(alone.iter().map(|r| r.issue.as_deref()).collect::<Vec<_>>(), [None, Some(b.as_str()), None], "c2 ran long after");
        let mut two = vec![codex("c1", 700), codex("c2", 650)];
        by_session(&mut two, &codex_touches);
        assert_eq!(two.iter().map(|r| r.issue.as_deref()).collect::<Vec<_>>(), [None, None], "two at once: neither");

        // A branch's issue stands, and an issue of another project is not taken.
        let mut placed = vec![Attributed { issue: Some("NIT-9".into()), ..usage("claude", "s1", 250) }, Attributed { project: Some("APP".into()), ..usage("claude", "s1", 250) }];
        by_session(&mut placed, &touches);
        assert_eq!(placed.iter().map(|r| r.issue.as_deref()).collect::<Vec<_>>(), [Some("NIT-9"), None]);
        drop(db);
        std::fs::remove_dir_all(base).ok();
    }
}
