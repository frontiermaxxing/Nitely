//! Shared core for the Nitely app and the `nitely` CLI: configuration, the SQLite
//! event store, the git worktree reader, and agent hook parsing.

pub mod activity;
pub mod approval;
pub mod brief;
pub mod config;
pub mod connect;
pub mod crew;
pub mod instructions;
pub mod credentials;
pub mod db;
pub mod git;
pub mod goal_plans;
pub mod goals;
pub mod http;
pub mod github;
pub mod hook;
pub mod install;
pub mod land;
pub mod linear_import;
pub mod mirror;
pub mod overlap;
pub mod issues;
pub mod paths;
pub mod phone;
pub mod playbook;
pub mod private_terms;
pub mod procs;
pub mod proposals;
pub mod projects;
pub mod relay;
pub mod remote;
pub mod similar;
pub mod review;
pub mod rules;
pub mod snapshot;
pub mod track;
pub mod usage;
pub mod pulse;
pub mod watch;
pub mod submit;

pub use config::{resolve_project, Config, RepoConfig};
pub use db::Db;
pub use issues::{Actor, IssueRecord, NewIssue};
pub use snapshot::{snapshot, Board, RepoView, TaskRow};
pub use pulse::{pulse, Pulse};
pub use track::Track;
