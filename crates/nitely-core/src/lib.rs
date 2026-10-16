//! Shared core for the Nitely app and the `nitely` CLI: configuration, the SQLite
//! event store, the git worktree reader, and agent hook parsing.

pub mod config;
pub mod credentials;
pub mod db;
pub mod git;
pub mod github;
pub mod hook;
pub mod issues;
pub mod paths;
pub mod remote;
pub mod snapshot;

pub use config::{resolve_project, Config, RepoConfig};
pub use db::Db;
pub use issues::{Actor, IssueRecord, NewIssue};
pub use snapshot::{snapshot, Board, RepoView, TaskRow};
