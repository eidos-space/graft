//! Repository command execution without routing through `SQLite` PRAGMAs.
//!
//! This is the control-plane entry point for CLI and embedding use cases. The `SQLite` VFS remains
//! a data-plane component and only exposes VFS-specific diagnostics and controls.

use std::{path::Path, sync::Arc};

use graft::{
    remote::{RemoteConfig, RemoteCredentialErr, RemoteCredentials},
    repo::Repository,
    setup::setup_graft_temporary,
};

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

/// A long-lived repository command service.
///
/// Opening retains the repository-scoped runtime and its local storage lock until this value is
/// dropped. Callers must serialize [`Self::execute`] within one service; separate services for
/// separate repositories may execute concurrently.
pub struct RepositoryCommandService {
    file: VolFile,
    credentials: RemoteCredentials,
}

impl RepositoryCommandService {
    /// Opens a service with the legacy CLI environment credential policy.
    pub fn open(target: &Path) -> Result<Self, ErrCtx> {
        Self::open_with_credentials(target, RemoteCredentials::environment())
    }

    /// Opens a service with an explicit credential policy supplied by an embedder.
    pub fn open_with_credentials(
        target: &Path,
        credentials: RemoteCredentials,
    ) -> Result<Self, ErrCtx> {
        let base_runtime = setup_graft_temporary(RemoteConfig::Memory, None)?;
        let runtimes = Arc::new(RepoRuntimeRegistry::new(base_runtime.clone()));
        let repo = discover_target_repository(target)
            .map(|repo| repo.with_remote_credentials(credentials.clone()));
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
        let mut file = VolFile::new_repository_session(
            runtime,
            session_path.to_string_lossy().into_owned(),
            repository_database,
            repo,
            runtimes,
        )?;
        file.set_remote_credentials(credentials.clone());
        Ok(Self { file, credentials })
    }

    /// Executes one parsed repository command against the retained runtime.
    pub fn execute(&mut self, command: RepositoryCommand) -> Result<Option<String>, ErrCtx> {
        self.credentials.reset_http_clients();
        let runtime = self.file.runtime().clone();
        command.command.eval(&runtime, &mut self.file)
    }

    /// Injects or rotates an HTTP bearer token without writing it to repository config.
    pub fn set_http_bearer_token(
        &self,
        remote_name: &str,
        token: String,
    ) -> Result<(), RemoteCredentialErr> {
        self.credentials.set_http_bearer_token(remote_name, token)
    }

    /// Clears an injected HTTP bearer token.
    pub fn clear_http_bearer_token(&self, remote_name: &str) -> Result<(), RemoteCredentialErr> {
        self.credentials.clear_http_bearer_token(remote_name)
    }

    /// Redacts all credentials held by this service from a diagnostic string.
    pub fn redact(&self, message: &str) -> String {
        self.credentials.redact(message)
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
