//! "How a change lands" (NIT-492): the pipeline a change goes through in one repo, computed from
//! every layer (the review mode, the playbook, built-in defaults), each value naming where it
//! came from. Read-only: it decides nothing, and execution never reads it.
use crate::project_modes::{self, GoalPlans, ItemReview, Mode, PreReview, ProjectReviewMode, TestDepth};
use crate::playbook::Playbook;
use anyhow::Result;
use serde::Serialize;
use std::path::Path;

/// Where a value comes from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Source {
    /// The project's preset (Fast, Careful, Release).
    Preset { mode: Mode },
    /// A value the person set in Custom.
    Custom,
    /// A fact the repo's playbook states.
    Playbook,
    /// Nothing set it: Nitely's own default.
    Builtin,
}

/// How the strip colours a value: an agent or Nitely handles it, it waits for you, or it is off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Tone {
    Agent,
    You,
    Off,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Fact {
    pub value: String,
    pub why: String,
    pub tone: Tone,
    pub source: Source,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Pipeline {
    pub repo: String,
    pub mode: Mode,
    /// What runs before a change lands.
    pub checks: Fact,
    /// How a change is reviewed before it lands (who reviews is the repo's reviewer map).
    pub review: Fact,
    /// How a goal's items reach main.
    pub goals: Fact,
    /// How a change with no goal lands.
    pub lands: Fact,
    /// Where full suites run.
    pub suites: Fact,
    /// What checks main after a land.
    pub after: Fact,
    /// Settings that could not be read, or that the mode overrules.
    pub warnings: Vec<String>,
}

/// The source of a mode-decided value: the preset, a Custom override, or (left unset in Custom)
/// the built-in default. The playbook's policy keys are retired (NIT-499).
fn source(mode: Mode, set: bool) -> Source {
    match (mode, set) {
        (Mode::Custom, true) => Source::Custom,
        (Mode::Custom, false) => Source::Builtin,
        (mode, _) => Source::Preset { mode },
    }
}

fn minutes(seconds: i64) -> String {
    if seconds % 3600 == 0 { format!("{} h", seconds / 3600) } else { format!("{} min", (seconds + 59) / 60) }
}

/// The pipeline for one repo under `settings`. `has_map` says whether the integration branch
/// has a test map.
pub fn pipeline(repo: &str, settings: &ProjectReviewMode, pb: &Playbook, has_map: bool) -> Pipeline {
    let chosen = &settings.overrides;
    let o = settings.effective_options(pb);
    let mode = settings.mode;
    let mut warnings = project_modes::ignored_policy(&o, pb);

    let depth_src = source(mode, chosen.test_depth.is_some());
    let audit = o.audit_percent.unwrap_or(0);
    let audited = if audit > 0 { format!("; {audit}% audited") } else { String::new() };
    let checks = match o.test_depth.unwrap_or(TestDepth::Full) {
        TestDepth::Off => Fact { value: "Skipped".into(), why: "No automatic tests run before landing".into(), tone: Tone::Off, source: depth_src },
        TestDepth::Full => Fact { value: "Full gate".into(), why: "Every command in the playbook's gate".into(), tone: Tone::Agent, source: depth_src },
        TestDepth::Basic => Fact { value: "Basic commands".into(), why: format!("The playbook's basic commands; protected changes get the full gate{audited}"), tone: Tone::Agent, source: depth_src },
        TestDepth::Risk if has_map => Fact {
            value: "Quick checks".into(),
            why: format!("Compile, lint and the affected tests; protected or unmapped changes get the full gate{audited}"),
            tone: Tone::Agent,
            source: depth_src,
        },
        TestDepth::Risk => Fact { value: "Full gate".into(), why: "No test map on the integration branch".into(), tone: Tone::Agent, source: depth_src },
    };

    let review_src = source(mode, chosen.pre_review.is_some());
    let review = match o.pre_review.unwrap_or(PreReview::Individual) {
        PreReview::Off => Fact { value: "Off".into(), why: "Changes with no goal land unreviewed".into(), tone: Tone::Off, source: review_src },
        PreReview::Batched => Fact { value: "One per batch".into(), why: "One blocking review per batch of changes with no goal".into(), tone: Tone::Agent, source: review_src },
        PreReview::Individual => Fact { value: "Each change".into(), why: "A blocking review for each change with no goal".into(), tone: Tone::Agent, source: review_src },
    };

    let goal_src = source(mode, chosen.goal_branches.is_some() || chosen.goal_plans.is_some());
    // Which plans an agent drafts start by themselves (NIT-550).
    let plans = match o.goal_plans.unwrap_or(GoalPlans::Approve) {
        GoalPlans::Approve => "each plan waits for your approval",
        GoalPlans::Asked => "plans you asked for start by themselves",
        GoalPlans::All => "every plan starts by itself",
    };
    let routes = crate::goal_branch::goal_items_route(o.goal_branches, !pb.submodules.is_empty());
    let goals = if let Err(why) = routes {
        Fact { value: "Straight to main".into(), why: format!("{why}. {}", capital(plans)), tone: Tone::Agent, source: goal_src }
    } else {
        let item = match o.item_review.unwrap_or(ItemReview::Off) {
            ItemReview::Off => "items land on quick checks, unreviewed",
            ItemReview::Advisory => "each item is reviewed without blocking",
            ItemReview::Blocking => "each item gets a blocking review",
        };
        let when = match &o.checkpoint {
            Some(c) => format!("a coherent slice, {} items{}", c.items, if c.daily { " or a day" } else { "" }),
            None => "a coherent slice".into(),
        };
        Fact {
            value: "Through a goal branch".into(),
            why: format!("{}; main gets the goal at each checkpoint ({when}): one full run, one review. {}", capital(item), capital(plans)),
            tone: Tone::Agent,
            source: goal_src,
        }
    };

    let land_src = source(mode, chosen.land_batch.is_some());
    let lanes = o.shared_lanes != Some(false);
    let lands = if o.land_batch == Some(true) && lanes {
        Fact {
            value: format!("Batches of up to {}", pb.land.batch_max()),
            why: "What waits lands on one full run, bisected if it fails".into(),
            tone: Tone::Agent,
            source: land_src,
        }
    } else if o.land_batch == Some(true) {
        warnings.push("Batched landing needs build lanes, which are off: each change lands alone.".into());
        Fact { value: "Each change alone".into(), why: "Batching needs build lanes, which are off".into(), tone: Tone::Agent, source: land_src }
    } else {
        Fact { value: "Each change alone".into(), why: "One gate and one push per change".into(), tone: Tone::Agent, source: land_src }
    };

    let ci = pb.lanes.as_ref().map(|l| l.ci_commands.clone()).unwrap_or_default();
    let suites = if !ci.is_empty() {
        Fact { value: "In CI".into(), why: format!("{} on GitHub Actions, the rest in a build lane; local when CI is down", ci.join(", ")), tone: Tone::Agent, source: Source::Playbook }
    } else if lanes {
        Fact { value: "In a build lane".into(), why: "On this machine, one full suite at a time".into(), tone: Tone::Agent, source: if pb.lanes.is_some() { Source::Playbook } else { source(mode, chosen.shared_lanes.is_some()) } }
    } else {
        Fact { value: "In the task's worktree".into(), why: "Build lanes are off".into(), tone: Tone::Agent, source: source(mode, chosen.shared_lanes.is_some()) }
    };

    let b = &pb.verification.backstop;
    let backstop_src = source(mode, chosen.full_backstop.is_some());
    let after = match &o.full_backstop {
        Some(s) if s.enabled => {
            let every = if s.landings == 1 { "after every land".to_string() } else { format!("every {} lands or {}", s.landings, minutes(s.delay_seconds)) };
            let revert = if !b.auto_revert {
                "auto-revert is off in the playbook".to_string()
            } else if b.reversible_paths.as_ref().is_some_and(|p| p.is_empty()) {
                "the playbook marks no path reversible, so a failure is fixed by hand".to_string()
            } else {
                "a failure is reverted".to_string()
            };
            Fact { value: "Full suite on main".into(), why: format!("Runs {every}; {revert}"), tone: Tone::Agent, source: backstop_src }
        }
        _ => {
            warnings.push("The full suite on main is off: nothing checks main after reduced checks land.".into());
            Fact { value: "Nothing".into(), why: "The full suite on main is off".into(), tone: Tone::Off, source: backstop_src }
        }
    };

    warnings.sort();
    warnings.dedup();
    Pipeline { repo: repo.into(), mode, checks, review, goals, lands, suites, after, warnings }
}

fn capital(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

/// The pipeline for each of the project's repos.
pub fn for_project(db: &crate::Db, config: &crate::Config, project: &str) -> Result<Vec<Pipeline>> {
    let settings = db.project_review_mode(project)?;
    Ok(config
        .repos
        .iter()
        .filter(|r| r.project.as_deref().is_some_and(|p| p.eq_ignore_ascii_case(project)))
        .map(|repo| {
            let root = Path::new(&repo.root);
            let (pb, warning) = match Playbook::load_with(root, config) {
                Ok(pb) => (pb, None),
                Err(e) => (Playbook::default(), Some(format!("Repository settings unavailable: {e:#}; these are defaults."))),
            };
            let mut p = pipeline(&repo.name, &settings, &pb, committed_map(root, &repo.integration_branch).is_some());
            p.warnings.extend(warning);
            p
        })
        .collect())
}

fn committed_map(root: &Path, branch: &str) -> Option<String> {
    crate::playbook::git(root, &["show", &format!("refs/heads/{branch}:{}", crate::affected_tests::POLICY_PATH)]).ok()
}

/// What a change to one path would go through: the dry-run line under the strip.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DryRun {
    pub path: String,
    /// "refused", "full", "quick", "basic" or "none".
    pub checks: String,
    pub commands: Vec<String>,
    pub why: String,
    /// Why it would wait for you; empty when it would not.
    pub waits: Vec<String>,
}

pub fn dry_run(settings: &ProjectReviewMode, pb: &Playbook, map: Option<&str>, path: &str) -> DryRun {
    let path = path.trim().replace('\\', "/");
    let files = vec![path.clone()];
    let gate: Vec<String> = pb.gate.commands.iter().cloned().chain(pb.gate.steps.iter().map(|s| s.run.clone())).collect();
    let out = |checks: &str, commands: Vec<String>, why: String| DryRun { path: path.clone(), checks: checks.into(), commands, why, waits: vec![] };
    if let Err(e) = pb.approval.check_blocked(&files) {
        return out("refused", vec![], format!("{e:#}"));
    }
    let waits = pb.human_reasons(&files).unwrap_or_else(|e| vec![format!("{e:#}")]);
    let depth = settings.effective_options(pb).test_depth.unwrap_or(TestDepth::Full);
    let protected = pb.verification.protected.iter().any(|p| regex::Regex::new(&format!("^(?:{p})$")).is_ok_and(|re| re.is_match(&path)));
    let mut result = match depth {
        TestDepth::Off => out("none", vec![], "Automatic tests are off".into()),
        TestDepth::Full => out("full", gate, "The mode runs the full gate on every change".into()),
        _ if protected => out("full", gate, "A protected path in the playbook keeps the full gate".into()),
        _ if !waits.is_empty() => out("full", gate, "A path that needs you keeps the full gate".into()),
        TestDepth::Basic if pb.verification.basic.is_empty() => out("full", gate, "No basic commands are configured, so the full gate runs".into()),
        TestDepth::Basic => out("basic", pb.verification.basic.clone(), "The playbook's basic commands".into()),
        TestDepth::Risk => {
            let p = crate::affected_tests::preview(map.unwrap_or(""), &path);
            if p.full {
                out("full", gate, p.why)
            } else {
                let mut commands = pb.verification.basic.clone();
                commands.extend(p.commands.into_iter().filter(|c| !pb.verification.basic.contains(c)));
                out("quick", commands, format!("{}, from the test map; a larger change can run more", p.why))
            }
        }
    };
    result.waits = waits;
    result
}

/// The dry run for one of the project's repos, against its committed test map.
pub fn dry_run_for(db: &crate::Db, config: &crate::Config, project: &str, repo: &str, path: &str) -> Result<DryRun> {
    let settings = db.project_review_mode(project)?;
    let r = config.repos.iter().find(|r| r.name == repo).ok_or_else(|| anyhow::anyhow!("no repo {repo}"))?;
    let root = Path::new(&r.root);
    let pb = Playbook::load_with(root, config)?;
    Ok(dry_run(&settings, &pb, committed_map(root, &r.integration_branch).as_deref(), path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project_modes::Options;
    use crate::{Actor, Db};

    fn mode(db: &mut Db, m: Mode, o: Options) -> ProjectReviewMode {
        let rev = db.project_review_mode("APP").unwrap().revision;
        db.set_project_review_mode("APP", m, o, rev, &Actor::Human).unwrap()
    }

    /// Each value names its layer: a preset, a Custom override, a playbook fact (where full suites
    /// run), or the built-in default; and the dry run follows the test map, the protected paths and the paths that need
    /// the person, the way the gate would.
    #[test]
    fn values_name_their_source_and_the_dry_run_follows_the_floors() {
        let dir = crate::paths::unique_temp("nitely-lands", "");
        std::fs::create_dir_all(&dir).unwrap();
        let mut db = Db::open(&dir.join("state.db")).unwrap();
        db.create_project("APP", "Example").unwrap();
        let pb = Playbook::parse(
            "[gate]\ncommands = [\"npm test\"]\n[land]\nbatch = \"waiting\"\nbatch_max = 3\n[lanes]\nci_commands = [\"npm test\"]\n\
             [verification]\nprotected = ['src/billing/.*']\nbasic = ['npm run lint']\n[verification.backstop]\nenabled = true\nlandings = 7\n\
             [[approval.human_paths]]\npattern = '^migrations/'\nreason = 'schema change'\n",
        )
        .unwrap();

        let fast = mode(&mut db, Mode::Fast, Options::default());
        let p = pipeline("app", &fast, &pb, true);
        assert_eq!(p.checks.source, Source::Preset { mode: Mode::Fast });
        assert_eq!((p.checks.value.as_str(), p.review.value.as_str()), ("Quick checks", "One per batch"));
        assert_eq!(p.lands.value, "Batches of up to 3");
        assert_eq!((p.suites.value.as_str(), &p.suites.source), ("In CI", &Source::Playbook));
        assert_eq!(p.after.why.split(';').next(), Some("Runs every 5 lands or 30 min"), "the preset decides the cadence, not the playbook");
        assert!(p.warnings.iter().any(|w| w.contains("[verification.backstop]")), "{:?}", p.warnings);

        let custom = mode(&mut db, Mode::Custom, Options { test_depth: Some(TestDepth::Full), ..Default::default() });
        let p = pipeline("app", &custom, &pb, true);
        assert_eq!((p.checks.value.as_str(), &p.checks.source), ("Full gate", &Source::Custom));
        // What Custom leaves unset is Nitely's default, never the playbook's retired keys (NIT-499).
        assert_eq!((p.lands.value.as_str(), &p.lands.source), ("Each change alone", &Source::Builtin));
        assert_eq!(p.review.source, Source::Builtin);
        assert_eq!((p.after.tone, &p.after.source), (Tone::Off, &Source::Builtin), "the playbook's enabled = true no longer turns it on");
        let off = mode(&mut db, Mode::Custom, Options { full_backstop: Some(project_modes::Backstop { enabled: false, landings: 5, delay_seconds: 1800 }), ..Default::default() });
        let p = pipeline("app", &off, &pb, true);
        assert_eq!((p.after.tone, &p.after.source), (Tone::Off, &Source::Custom), "only Custom turns it off, and it shows red");

        let map = include_str!("../../../.nitely/affected-tests.toml");
        let quick = dry_run(&fast, &pb, Some(map), "src/App.tsx");
        assert_eq!(quick.checks, "quick", "{quick:?}");
        assert_eq!(quick.commands[0], "npm run lint");
        assert_eq!(dry_run(&fast, &pb, Some(map), "src/billing/plan.ts").checks, "full");
        let human = dry_run(&fast, &pb, Some(map), "migrations/001.sql");
        assert_eq!((human.checks.as_str(), human.waits.len()), ("full", 1));
        assert_eq!(dry_run(&fast, &pb, None, "src/App.tsx").checks, "full", "no map keeps the full gate");
    }
}
