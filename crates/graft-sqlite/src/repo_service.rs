//! Repository command execution without routing through `SQLite` PRAGMAs.
//!
//! This is the control-plane entry point for CLI and embedding use cases. The `SQLite` VFS remains
//! a data-plane component and only exposes VFS-specific diagnostics and controls.

use std::{path::Path, sync::Arc};

use graft::{remote::RemoteConfig, repo::Repository, setup::setup_graft_temporary};

use crate::{
    file::vol_file::VolFile,
    pragma::GraftCommand,
    vfs::{ErrCtx, RepoRuntimeRegistry},
};

/// A parsed, type-checked repository control-plane command.
///
/// Parsing is kept at the CLI adapter boundary. Once constructed, command execution no longer
/// carries a string command name or `SQLite` PRAGMA value through the service layer.
pub struct RepositoryCommand {
    command: GraftCommand,
}

impl RepositoryCommand {
    pub fn parse(name: &str, argument: Option<&str>) -> Result<Self, ErrCtx> {
        let command = GraftCommand::parse_repository(name, argument)?;
        Ok(Self { command })
    }
}

/// Executes one repository command against the repository containing `target`.
///
/// The command is evaluated directly against a repository-scoped Graft runtime. No `SQLite`
/// connection is opened and no PRAGMA is issued. `target` may be a worktree database path or the
/// repository's `.graft` directory for commands that operate on the whole worktree.
pub fn execute_repository_command(
    target: &Path,
    command: RepositoryCommand,
) -> Result<Option<String>, ErrCtx> {
    let mut service = RepositoryCommandService::open(target)?;
    service.execute(command)
}

struct RepositoryCommandService {
    file: VolFile,
}

impl RepositoryCommandService {
    fn open(target: &Path) -> Result<Self, ErrCtx> {
        let base_runtime = setup_graft_temporary(RemoteConfig::Memory, None)?;
        let runtimes = Arc::new(RepoRuntimeRegistry::new(base_runtime.clone()));
        let repo = discover_target_repository(target);
        let runtime = match &repo {
            Some(repo) => runtimes.runtime_for(repo)?,
            None => base_runtime,
        };
        let session_path = repo.as_ref().map_or_else(
            || target.to_path_buf(),
            |repo| repo.graft_dir().to_path_buf(),
        );
        let repository_database = repo
            .as_ref()
            .filter(|repo| target != repo.graft_dir())
            .map(|_| target.to_path_buf());
        let file = VolFile::new_repository_session(
            runtime,
            session_path.to_string_lossy().into_owned(),
            repository_database,
            repo,
            runtimes,
        )?;
        Ok(Self { file })
    }

    fn execute(&mut self, command: RepositoryCommand) -> Result<Option<String>, ErrCtx> {
        let runtime = self.file.runtime().clone();
        command.command.eval(&runtime, &mut self.file)
    }
}

fn discover_target_repository(target: &Path) -> Option<Repository> {
    if target
        .file_name()
        .is_some_and(|name| name == graft::repo::GRAFT_DIR)
    {
        return target
            .parent()
            .and_then(|parent| Repository::discover(parent).ok());
    }
    Repository::discover_for_file(target).ok()
}
