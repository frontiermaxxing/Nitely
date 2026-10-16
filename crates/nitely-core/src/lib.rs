//! Shared core for the Nitely app and the `nitely` CLI: configuration, the SQLite
//! event store, the git worktree reader, and agent hook parsing.

pub mod config;
pub mod credentials;
pub mod db;
pub mod git;
pub mod hook;
pub mod paths;
pub mod remote;
pub mod snapshot;

pub use config::{Config, RepoConfig};
pub use db::Db;
pub use snapshot::{snapshot, Board, RepoView, TaskRow};
