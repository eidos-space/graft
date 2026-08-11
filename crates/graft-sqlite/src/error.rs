use std::borrow::Cow;

use graft::{GraftErr, repo::RepoErr};
use thiserror::Error;

/// Error returned by SQLite-aware repository commands.
#[derive(Debug, Error)]
pub enum CommandError {
    #[error("Graft error: {0}")]
    Graft(#[from] GraftErr),

    #[error("Unknown command")]
    UnknownCommand,

    #[error("Command error: {0}")]
    InvalidCommand(Cow<'static, str>),

    #[error("Invalid repository session state")]
    InvalidVolumeState,

    #[error("Graft repository error: {0}")]
    Repo(#[from] RepoErr),

    #[error("Graft setup error: {0}")]
    Setup(#[from] graft::setup::InitErr),

    #[error("SQLite database error: {0}")]
    SqliteDatabase(#[from] rusqlite::Error),

    #[error(transparent)]
    IoErr(#[from] std::io::Error),

    #[error(transparent)]
    FmtErr(#[from] std::fmt::Error),
}

/// Kept as an internal alias while the repository command implementation is split out of its
/// historical PRAGMA module.
pub type ErrCtx = CommandError;
