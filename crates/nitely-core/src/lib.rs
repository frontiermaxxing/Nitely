//! Shared core for the Nitely app and the `nitely` CLI: configuration, the SQLite
//! event store, the git worktree reader, and agent hook parsing. The crew and the reports built
//! on it are in nitely-crew.

pub mod accept;
pub mod crew_rules;
pub mod activity;
pub mod approval;
pub mod awake;
mod blobs;
pub mod batches;
pub mod brief;
pub mod config;
pub mod connect;
pub mod instructions;
pub mod credentials;
pub mod db;
pub mod focus;
pub mod executor;
pub mod git;
mod git_read;
pub mod goal_plans;
pub mod goal_report;
pub mod goals;
pub mod goal_branch;
pub mod http;
pub mod github;
pub mod install;
pub mod land;
pub mod land_job;
pub mod lands_pipeline;
pub mod land_queue;
pub mod landed_elsewhere;
pub mod land_rules;
pub mod lanes;
mod lane_clone;
#[cfg(windows)]
mod lane_clone_windows;
pub mod leftovers;
pub mod light;
pub mod link_service;
pub mod links;
pub mod machine;
pub mod linear_import;
pub mod mirror;
pub mod overlap;
pub mod issues;
pub mod paths;
/// The large sample board and its performance budgets (NIT-255).
#[doc(hidden)]
pub mod performance;
pub mod performance_attribution;
pub mod performance_events;
pub mod pace;
pub mod phone;
pub mod playbook;
pub mod private_terms;
pub mod procs;
pub mod proposals;
pub mod projects;
pub mod project_yolo;
pub mod project_modes;
pub mod relay;
pub mod remote;
pub mod similar;
pub mod slim;
pub mod review;
pub mod rules;
pub mod sessions;
pub mod snapshot;
pub mod track;
pub mod update;
pub mod verification;
pub mod verification_backstop;
pub mod verification_audit;
pub mod affected_tests;
pub mod release_verification;
pub mod push;
pub mod quick;
pub mod watch;
pub mod submit;
pub mod submit_rules;
mod test_command_merge;
pub mod submit_job;
pub mod unlink;
pub mod updates;
pub mod submodules;
pub mod review_batch;
pub mod review_compare;
pub mod test_catches;
/// Test fixtures, for nitely-crew's tests too (its `test-support` dev-dependency).
#[cfg(any(test, feature = "test-support"))]
#[cfg_attr(not(test), allow(dead_code))]
#[doc(hidden)]
pub mod test_repo;

pub use config::{resolve_project, Config, RepoConfig};
pub use db::Db;
pub use issues::{Actor, IssueRecord, NewIssue};
pub use snapshot::{snapshot, Board, RepoView, TaskRow};
pub use track::Track;

pub mod test_budget;
pub mod test_evidence;
pub mod test_sweep;
pub mod periodic_review;
