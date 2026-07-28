//! Long-lived, serialized repository sessions for embedding Graft.
//!
//! This crate is the stable boundary between Graft's repository command implementation and
//! language bindings. It deliberately reuses [`graft_sqlite::repo_service`] rather than
//! reimplementing repository or remote protocols.

use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU8, Ordering},
};

use graft::remote::{RemoteCredentialErr, RemoteCredentials};
use graft_sqlite::{
    repo_service::{RepositoryCommand, RepositoryCommandService},
    vfs::ErrCtx,
};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

const LIFECYCLE_CLOSED: u8 = 0;
const LIFECYCLE_OPENING: u8 = 1;
const LIFECYCLE_OPEN: u8 = 2;
const LIFECYCLE_CLOSING: u8 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionLifecycle {
    Closed,
    Opening,
    Open,
    Closing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SdkErrorCode {
    SessionClosed,
    SessionOpening,
    SessionClosing,
    SessionAlreadyOpen,
    RepositoryBusy,
    InvalidArgument,
    InvalidResponse,
    RepositoryCommand,
}

impl SdkErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SessionClosed => "GRAFT_SDK_SESSION_CLOSED",
            Self::SessionOpening => "GRAFT_SDK_SESSION_OPENING",
            Self::SessionClosing => "GRAFT_SDK_SESSION_CLOSING",
            Self::SessionAlreadyOpen => "GRAFT_SDK_SESSION_ALREADY_OPEN",
            Self::RepositoryBusy => "GRAFT_SDK_REPOSITORY_BUSY",
            Self::InvalidArgument => "GRAFT_SDK_INVALID_ARGUMENT",
            Self::InvalidResponse => "GRAFT_SDK_INVALID_RESPONSE",
            Self::RepositoryCommand => "GRAFT_SDK_REPOSITORY_COMMAND",
        }
    }
}

#[derive(Debug, Error)]
#[error("{message}")]
pub struct SdkError {
    code: SdkErrorCode,
    message: String,
}

impl SdkError {
    pub fn code(&self) -> SdkErrorCode {
        self.code
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    fn new(code: SdkErrorCode, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }
}

pub type Result<T> = std::result::Result<T, SdkError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepositoryOperation {
    Init,
    Status,
    AddAll,
    Commit,
    Diff,
    History,
    Restore,
    RemoteConfigure,
    Push,
    Fetch,
    Pull,
    Clone,
}

impl RepositoryOperation {
    /// Whether the operation can replace, create, or remove physical worktree files.
    pub const fn materializes_worktree(self) -> bool {
        matches!(self, Self::Restore | Self::Pull | Self::Clone)
    }
}

#[derive(Debug, Clone, Default)]
pub struct DiffOptions {
    pub rows: bool,
    pub staged: bool,
    pub root: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub path: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct RestoreOptions {
    pub source: Option<String>,
    pub expected_head: Option<String>,
    pub require_clean: bool,
    pub path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct RemoteConfigureOptions {
    pub name: String,
    pub url: String,
    pub bearer_token: Option<String>,
    pub overwrite: bool,
    pub upstream_branch: Option<String>,
}

struct SessionState {
    service: Option<RepositoryCommandService>,
}

/// One long-lived repository session.
///
/// Every operation locks this session for its full duration. Calls on the same session therefore
/// serialize, while independent session instances can run on different worker threads.
pub struct RepositorySession {
    target: PathBuf,
    credentials: RemoteCredentials,
    lifecycle: AtomicU8,
    state: Mutex<SessionState>,
}

impl std::fmt::Debug for RepositorySession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RepositorySession")
            .field("target", &self.target)
            .field("lifecycle", &self.lifecycle())
            .finish()
    }
}

impl RepositorySession {
    /// Creates a closed session. [`Self::open`] performs the potentially expensive runtime setup.
    pub fn new(target: impl AsRef<Path>) -> Self {
        Self {
            target: repository_session_target(target.as_ref()),
            credentials: RemoteCredentials::explicit(),
            lifecycle: AtomicU8::new(LIFECYCLE_CLOSED),
            state: Mutex::new(SessionState { service: None }),
        }
    }

    pub fn target(&self) -> &Path {
        &self.target
    }

    pub fn lifecycle(&self) -> SessionLifecycle {
        lifecycle_from_raw(self.lifecycle.load(Ordering::Acquire))
    }

    /// Opens the retained repository runtime.
    pub fn open(&self) -> Result<()> {
        match self.lifecycle.compare_exchange(
            LIFECYCLE_CLOSED,
            LIFECYCLE_OPENING,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {}
            Err(LIFECYCLE_OPEN) => {
                return Err(SdkError::new(
                    SdkErrorCode::SessionAlreadyOpen,
                    "repository session is already open",
                ));
            }
            Err(LIFECYCLE_OPENING) => return Err(session_opening_error()),
            Err(LIFECYCLE_CLOSING) => return Err(session_closing_error()),
            Err(_) => unreachable!("invalid repository session lifecycle"),
        }

        let mut state = self.state.lock();
        let service =
            RepositoryCommandService::open_with_credentials(&self.target, self.credentials.clone())
                .map_err(|error| self.command_error(error));
        let service = match service {
            Ok(service) => service,
            Err(error) => {
                self.lifecycle.store(LIFECYCLE_CLOSED, Ordering::Release);
                return Err(error);
            }
        };

        if self.lifecycle.load(Ordering::Acquire) == LIFECYCLE_CLOSING {
            drop(service);
            self.lifecycle.store(LIFECYCLE_CLOSED, Ordering::Release);
            return Err(session_closing_error());
        }

        state.service = Some(service);
        self.lifecycle.store(LIFECYCLE_OPEN, Ordering::Release);
        Ok(())
    }

    /// Waits for the in-flight operation, releases the retained runtime, and rejects queued work.
    pub fn close(&self) -> Result<()> {
        let previous = self.lifecycle.swap(LIFECYCLE_CLOSING, Ordering::AcqRel);
        if previous == LIFECYCLE_CLOSED {
            self.lifecycle.store(LIFECYCLE_CLOSED, Ordering::Release);
            return Ok(());
        }

        let mut state = self.state.lock();
        state.service = None;
        self.lifecycle.store(LIFECYCLE_CLOSED, Ordering::Release);
        Ok(())
    }

    /// Closes and reconstructs the runtime from durable repository state.
    pub fn reopen(&self) -> Result<()> {
        self.lifecycle.store(LIFECYCLE_CLOSING, Ordering::Release);
        let mut state = self.state.lock();
        state.service = None;
        self.lifecycle.store(LIFECYCLE_OPENING, Ordering::Release);

        match RepositoryCommandService::open_with_credentials(
            &self.target,
            self.credentials.clone(),
        ) {
            Ok(service) => {
                state.service = Some(service);
                self.lifecycle.store(LIFECYCLE_OPEN, Ordering::Release);
                Ok(())
            }
            Err(error) => {
                self.lifecycle.store(LIFECYCLE_CLOSED, Ordering::Release);
                Err(self.command_error(error))
            }
        }
    }

    /// Injects or rotates a bearer token in memory. This is allowed while the session is closed.
    pub fn set_http_bearer_token(&self, remote_name: &str, token: String) -> Result<()> {
        self.credentials
            .set_http_bearer_token(remote_name, token)
            .map_err(credential_error)
    }

    pub fn clear_http_bearer_token(&self, remote_name: &str) -> Result<()> {
        self.credentials
            .clear_http_bearer_token(remote_name)
            .map_err(credential_error)
    }

    pub fn init(&self) -> Result<Value> {
        self.execute_json("json_init", None)
    }

    pub fn status(&self) -> Result<Value> {
        self.execute_json("json_status", None)
    }

    pub fn add_all(&self) -> Result<Value> {
        self.execute_json("json_add", Some("--all"))
    }

    pub fn commit(&self, message: &str) -> Result<Value> {
        if message.trim().is_empty() {
            return Err(invalid_argument("commit message must not be empty"));
        }
        self.execute_json("json_commit", Some(message))
    }

    pub fn diff(&self, options: &DiffOptions) -> Result<Value> {
        let argument = diff_argument(options)?;
        self.execute_json("json_diff", argument.as_deref())
    }

    pub fn history(&self, limit: usize, after: Option<&str>) -> Result<Value> {
        if limit == 0 {
            return Err(invalid_argument("history limit must be greater than zero"));
        }
        let mut argument = format!("--with-status --limit {limit}");
        if let Some(after) = after {
            validate_revision(after)?;
            argument.push_str(" --after ");
            argument.push_str(after);
        }
        self.execute_json("json_log", Some(&argument))
    }

    pub fn restore(&self, options: &RestoreOptions) -> Result<Value> {
        let mut parts = Vec::new();
        if let Some(source) = &options.source {
            validate_revision(source)?;
            parts.push(format!("--source {source}"));
        }
        if let Some(expected_head) = &options.expected_head {
            validate_revision(expected_head)?;
            parts.push(format!("--expected-head {expected_head}"));
        }
        if options.require_clean {
            parts.push("--require-clean".to_string());
        }
        parts.push("--".to_string());
        parts.push(quote_pragma_path(&options.path)?);
        self.execute_json("json_restore", Some(&parts.join(" ")))
    }

    pub fn configure_remote(&self, options: &RemoteConfigureOptions) -> Result<Value> {
        validate_remote_name(&options.name)?;
        validate_sdk_remote_url(&options.url)?;
        if let Some(token) = &options.bearer_token {
            self.set_http_bearer_token(&options.name, token.clone())?;
        }

        self.with_service(|service| {
            let remotes = execute_json(service, "json_remotes", None)?;
            let existing_url = remote_url(&remotes, &options.name)?;
            match existing_url {
                None => {
                    let argument = format!("{} {}", options.name, options.url);
                    execute_json(service, "json_remote_add", Some(&argument))?;
                }
                Some(existing) if existing == options.url => {}
                Some(_) if options.overwrite => {
                    let argument = format!("{} {}", options.name, options.url);
                    execute_json(service, "json_remote_set_url", Some(&argument))?;
                }
                Some(_) => {
                    return Err(SdkError::new(
                        SdkErrorCode::InvalidArgument,
                        format!(
                            "remote `{}` already exists with a different URL",
                            options.name
                        ),
                    ));
                }
            }

            if let Some(branch) = &options.upstream_branch {
                validate_branch_name(branch)?;
                let argument = format!("{branch} {}/{branch}", options.name);
                execute_json(service, "json_branch_upstream", Some(&argument))?;
            }
            execute_json(service, "json_remotes", None)
        })
    }

    pub fn push(&self, remote: Option<&str>, branch: Option<&str>) -> Result<Value> {
        let argument = remote_branch_argument(remote, branch)?;
        self.execute_json("json_push", argument.as_deref())
    }

    pub fn fetch(&self, remote: Option<&str>, branch: Option<&str>) -> Result<Value> {
        let argument = remote_branch_argument(remote, branch)?;
        self.execute_json("json_fetch", argument.as_deref())
    }

    pub fn pull(&self, remote: Option<&str>, branch: Option<&str>) -> Result<Value> {
        let argument = remote_branch_argument(remote, branch)?;
        self.execute_json("json_pull", argument.as_deref())
    }

    pub fn clone_repository(
        &self,
        remote_url: &str,
        branch: Option<&str>,
        bearer_token: Option<String>,
    ) -> Result<Value> {
        validate_sdk_remote_url(remote_url)?;
        if let Some(token) = bearer_token {
            self.set_http_bearer_token("origin", token)?;
        }
        let argument = match branch {
            Some(branch) => {
                validate_branch_name(branch)?;
                format!("{remote_url} {branch}")
            }
            None => remote_url.to_string(),
        };
        self.execute_json("json_clone", Some(&argument))
    }

    fn execute_json(&self, name: &str, argument: Option<&str>) -> Result<Value> {
        self.with_service(|service| execute_json(service, name, argument))
    }

    fn with_service<T>(
        &self,
        operation: impl FnOnce(&mut RepositoryCommandService) -> Result<T>,
    ) -> Result<T> {
        match self.lifecycle.load(Ordering::Acquire) {
            LIFECYCLE_CLOSED => return Err(session_closed_error()),
            LIFECYCLE_OPENING => return Err(session_opening_error()),
            LIFECYCLE_CLOSING => return Err(session_closing_error()),
            LIFECYCLE_OPEN => {}
            _ => unreachable!("invalid repository session lifecycle"),
        }

        let mut state = self.state.lock();
        if self.lifecycle.load(Ordering::Acquire) != LIFECYCLE_OPEN {
            return Err(session_closing_error());
        }
        let service = state.service.as_mut().ok_or_else(session_closed_error)?;
        operation(service).map_err(|error| self.redact_error(error))
    }

    fn command_error(&self, error: ErrCtx) -> SdkError {
        let message = self.credentials.redact(&error.to_string());
        let lowercase = message.to_ascii_lowercase();
        let code = if lowercase.contains("locked")
            || lowercase.contains("database lock")
            || lowercase.contains("already held")
        {
            SdkErrorCode::RepositoryBusy
        } else {
            SdkErrorCode::RepositoryCommand
        };
        SdkError::new(code, message)
    }

    fn redact_error(&self, error: SdkError) -> SdkError {
        SdkError::new(error.code, self.credentials.redact(&error.message))
    }
}

impl Drop for RepositorySession {
    fn drop(&mut self) {
        self.lifecycle.store(LIFECYCLE_CLOSING, Ordering::Release);
        self.state.get_mut().service = None;
        self.lifecycle.store(LIFECYCLE_CLOSED, Ordering::Release);
    }
}

fn execute_json(
    service: &mut RepositoryCommandService,
    name: &str,
    argument: Option<&str>,
) -> Result<Value> {
    let command = RepositoryCommand::parse(name, argument).map_err(repository_command_error)?;
    let output = service
        .execute(command)
        .map_err(repository_command_error)?
        .ok_or_else(|| {
            SdkError::new(
                SdkErrorCode::InvalidResponse,
                format!("repository command `{name}` returned no JSON"),
            )
        })?;
    serde_json::from_str(&output).map_err(|error| {
        SdkError::new(
            SdkErrorCode::InvalidResponse,
            format!("repository command `{name}` returned invalid JSON: {error}"),
        )
    })
}

fn repository_command_error(error: ErrCtx) -> SdkError {
    let message = error.to_string();
    let lowercase = message.to_ascii_lowercase();
    let code = if lowercase.contains("locked")
        || lowercase.contains("database lock")
        || lowercase.contains("already held")
    {
        SdkErrorCode::RepositoryBusy
    } else {
        SdkErrorCode::RepositoryCommand
    };
    SdkError::new(code, message)
}

fn credential_error(error: RemoteCredentialErr) -> SdkError {
    SdkError::new(SdkErrorCode::InvalidArgument, error.to_string())
}

fn lifecycle_from_raw(lifecycle: u8) -> SessionLifecycle {
    match lifecycle {
        LIFECYCLE_CLOSED => SessionLifecycle::Closed,
        LIFECYCLE_OPENING => SessionLifecycle::Opening,
        LIFECYCLE_OPEN => SessionLifecycle::Open,
        LIFECYCLE_CLOSING => SessionLifecycle::Closing,
        _ => unreachable!("invalid repository session lifecycle"),
    }
}

fn repository_session_target(target: &Path) -> PathBuf {
    if target
        .file_name()
        .is_some_and(|name| name == graft::repo::GRAFT_DIR)
    {
        return target.to_path_buf();
    }
    if target.is_dir() || !target.exists() {
        return target.join(graft::repo::GRAFT_DIR);
    }
    target.to_path_buf()
}

fn diff_argument(options: &DiffOptions) -> Result<Option<String>> {
    if options.staged && (options.root.is_some() || options.from.is_some() || options.to.is_some())
    {
        return Err(invalid_argument(
            "staged diff cannot be combined with revision targets",
        ));
    }
    if options.root.is_some() && (options.from.is_some() || options.to.is_some()) {
        return Err(invalid_argument(
            "root diff cannot be combined with from/to revisions",
        ));
    }
    if options.from.is_none() && options.to.is_some() {
        return Err(invalid_argument("diff `to` requires a `from` revision"));
    }

    let mut parts = Vec::new();
    if options.rows {
        parts.push("--rows".to_string());
    }
    if options.staged {
        parts.push("--staged".to_string());
    }
    if let Some(root) = &options.root {
        validate_revision(root)?;
        parts.push("--root".to_string());
        parts.push(root.clone());
    }
    if let Some(from) = &options.from {
        validate_revision(from)?;
        parts.push(from.clone());
        if let Some(to) = &options.to {
            validate_revision(to)?;
            parts.push(to.clone());
        }
    }
    if let Some(path) = &options.path {
        parts.push("--".to_string());
        parts.push(quote_pragma_path(path)?);
    }
    Ok((!parts.is_empty()).then(|| parts.join(" ")))
}

fn remote_branch_argument(remote: Option<&str>, branch: Option<&str>) -> Result<Option<String>> {
    if let Some(remote) = remote {
        validate_remote_name(remote)?;
    }
    if let Some(branch) = branch {
        validate_branch_name(branch)?;
    }
    Ok(match (remote, branch) {
        (None, None) => None,
        (Some(remote), None) => Some(remote.to_string()),
        (Some(remote), Some(branch)) => Some(format!("{remote} {branch}")),
        (None, Some(branch)) => Some(format!("origin {branch}")),
    })
}

fn remote_url<'a>(remotes: &'a Value, name: &str) -> Result<Option<&'a str>> {
    let entries = remotes
        .get("remotes")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            SdkError::new(
                SdkErrorCode::InvalidResponse,
                "remote list response does not contain `remotes`",
            )
        })?;
    Ok(entries.iter().find_map(|entry| {
        (entry.get("name").and_then(Value::as_str) == Some(name))
            .then(|| entry.get("url").and_then(Value::as_str))
            .flatten()
    }))
}

fn validate_sdk_remote_url(url: &str) -> Result<()> {
    if url.trim() != url
        || url.is_empty()
        || url
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
    {
        return Err(invalid_argument(
            "remote URL must be a non-empty URI without whitespace",
        ));
    }
    let http = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("graft+https://"))
        .or_else(|| url.strip_prefix("graft+http://"));
    if let Some(location) = http {
        let authority = location.split('/').next().unwrap_or_default();
        if authority.contains('@') || location.contains(['?', '#']) {
            return Err(invalid_argument(
                "SDK HTTP remote URLs must not contain credentials, query parameters, or fragments",
            ));
        }
    }
    Ok(())
}

fn validate_remote_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.starts_with('-')
        || name
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
        || name.contains(['/', '\\'])
    {
        return Err(invalid_argument("invalid repository remote name"));
    }
    Ok(())
}

fn validate_branch_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.starts_with('-')
        || name
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
    {
        return Err(invalid_argument("invalid repository branch name"));
    }
    Ok(())
}

fn validate_revision(revision: &str) -> Result<()> {
    if revision.is_empty()
        || revision
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
        || revision.starts_with('-')
    {
        return Err(invalid_argument("invalid repository revision"));
    }
    Ok(())
}

fn quote_pragma_path(path: &Path) -> Result<String> {
    let raw = path
        .to_str()
        .ok_or_else(|| invalid_argument("repository path is not valid UTF-8"))?;
    let escaped = raw.replace('\\', "\\\\").replace('"', "\\\"");
    Ok(format!("\"{escaped}\""))
}

fn invalid_argument(message: impl Into<String>) -> SdkError {
    SdkError::new(SdkErrorCode::InvalidArgument, message)
}

fn session_closed_error() -> SdkError {
    SdkError::new(SdkErrorCode::SessionClosed, "repository session is closed")
}

fn session_opening_error() -> SdkError {
    SdkError::new(
        SdkErrorCode::SessionOpening,
        "repository session is opening",
    )
}

fn session_closing_error() -> SdkError {
    SdkError::new(
        SdkErrorCode::SessionClosing,
        "repository session is closing",
    )
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        sync::{Arc, Barrier, mpsc},
        thread,
        time::Duration,
    };

    use rusqlite::Connection;
    use serde_json::json;

    use super::*;

    #[test]
    fn operation_materialization_contract_is_explicit() {
        assert!(RepositoryOperation::Restore.materializes_worktree());
        assert!(RepositoryOperation::Pull.materializes_worktree());
        assert!(RepositoryOperation::Clone.materializes_worktree());
        assert!(!RepositoryOperation::Init.materializes_worktree());
        assert!(!RepositoryOperation::Status.materializes_worktree());
        assert!(!RepositoryOperation::Diff.materializes_worktree());
        assert!(!RepositoryOperation::AddAll.materializes_worktree());
        assert!(!RepositoryOperation::Commit.materializes_worktree());
        assert!(!RepositoryOperation::History.materializes_worktree());
        assert!(!RepositoryOperation::RemoteConfigure.materializes_worktree());
        assert!(!RepositoryOperation::Push.materializes_worktree());
        assert!(!RepositoryOperation::Fetch.materializes_worktree());
    }

    #[test]
    fn session_reuses_runtime_and_reopens_after_close() {
        let directory = tempfile::tempdir().unwrap();
        let session = RepositorySession::new(directory.path());
        session.open().unwrap();
        session.init().unwrap();

        fs::write(directory.path().join("note.txt"), "one\n").unwrap();
        session.add_all().unwrap();
        session.commit("initial").unwrap();
        for _ in 0..10 {
            assert_eq!(session.status().unwrap()["dirty"], json!(false));
            session.diff(&DiffOptions::default()).unwrap();
        }

        session.close().unwrap();
        assert_eq!(session.lifecycle(), SessionLifecycle::Closed);
        assert_eq!(
            session.status().unwrap_err().code(),
            SdkErrorCode::SessionClosed
        );
        session.reopen().unwrap();
        assert_eq!(session.status().unwrap()["dirty"], json!(false));
    }

    #[test]
    fn second_repository_session_reports_busy_until_first_closes() {
        let directory = tempfile::tempdir().unwrap();
        let first = RepositorySession::new(directory.path());
        first.open().unwrap();
        first.init().unwrap();

        let second = RepositorySession::new(directory.path());
        let error = second.open().unwrap_err();
        assert_eq!(error.code(), SdkErrorCode::RepositoryBusy);

        first.close().unwrap();
        second.open().unwrap();
        second.close().unwrap();
    }

    #[test]
    fn same_session_serializes_concurrent_calls() {
        let directory = tempfile::tempdir().unwrap();
        let session = Arc::new(RepositorySession::new(directory.path()));
        session.open().unwrap();
        session.init().unwrap();
        fs::write(directory.path().join("note.txt"), "one\n").unwrap();
        session.add_all().unwrap();
        session.commit("initial").unwrap();

        let barrier = Arc::new(Barrier::new(9));
        let mut threads = Vec::new();
        for _ in 0..8 {
            let session = session.clone();
            let barrier = barrier.clone();
            threads.push(thread::spawn(move || {
                barrier.wait();
                for _ in 0..10 {
                    session.status().unwrap();
                    session.diff(&DiffOptions::default()).unwrap();
                }
            }));
        }
        barrier.wait();
        for thread in threads {
            thread.join().unwrap();
        }
    }

    #[test]
    fn different_repositories_run_on_independent_session_locks() {
        let first_directory = tempfile::tempdir().unwrap();
        let second_directory = tempfile::tempdir().unwrap();
        let first = Arc::new(RepositorySession::new(first_directory.path()));
        let second = Arc::new(RepositorySession::new(second_directory.path()));
        first.open().unwrap();
        second.open().unwrap();
        first.init().unwrap();
        second.init().unwrap();

        let barrier = Arc::new(Barrier::new(3));
        let handles = [first, second].map(|session| {
            let barrier = barrier.clone();
            thread::spawn(move || {
                barrier.wait();
                for _ in 0..20 {
                    session.status().unwrap();
                    session.diff(&DiffOptions::default()).unwrap();
                }
            })
        });
        barrier.wait();
        for handle in handles {
            handle.join().unwrap();
        }
    }

    #[test]
    fn close_rejects_queued_work_and_waits_for_in_flight_holder() {
        let directory = tempfile::tempdir().unwrap();
        let session = Arc::new(RepositorySession::new(directory.path()));
        session.open().unwrap();
        session.init().unwrap();

        let in_flight = session.state.lock();
        let (closed, received) = mpsc::channel();
        let closing_session = session.clone();
        let close = thread::spawn(move || {
            closing_session.close().unwrap();
            closed.send(()).unwrap();
        });
        while session.lifecycle() != SessionLifecycle::Closing {
            thread::yield_now();
        }

        assert_eq!(
            session.status().unwrap_err().code(),
            SdkErrorCode::SessionClosing
        );
        assert!(received.recv_timeout(Duration::from_millis(20)).is_err());
        drop(in_flight);
        received.recv_timeout(Duration::from_secs(1)).unwrap();
        close.join().unwrap();
        assert_eq!(session.lifecycle(), SessionLifecycle::Closed);
    }

    #[test]
    fn open_application_database_handle_does_not_block_non_materializing_calls() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("app.eidos");
        let database = Connection::open(&database_path).unwrap();
        database
            .execute("CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT)", [])
            .unwrap();
        database
            .execute("INSERT INTO items (name) VALUES ('one')", [])
            .unwrap();

        let session = RepositorySession::new(directory.path());
        session.open().unwrap();
        session.init().unwrap();
        session.add_all().unwrap();
        session.commit("initial").unwrap();
        assert_eq!(session.status().unwrap()["dirty"], json!(false));
        session
            .diff(&DiffOptions { rows: true, ..DiffOptions::default() })
            .unwrap();

        drop(database);
        fs::write(directory.path().join("note.txt"), "materialized later\n").unwrap();
        session.add_all().unwrap();
        session.commit("second").unwrap();
        session.close().unwrap();

        let reopened = Connection::open(&database_path).unwrap();
        assert_eq!(
            reopened
                .query_row("SELECT COUNT(*) FROM items", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            1
        );
    }

    #[test]
    fn credentials_are_not_accepted_in_http_remote_urls() {
        let session = RepositorySession::new(".");
        for url in [
            "https://token@example.com/org/repo",
            "https://example.com/org/repo?token=secret",
            "https://example.com/org/repo#secret",
        ] {
            let error = session
                .clone_repository(url, None, Some("secret".to_string()))
                .unwrap_err();
            assert_eq!(error.code(), SdkErrorCode::InvalidArgument);
            assert!(!error.to_string().contains("secret"));
        }
    }

    #[test]
    fn remote_and_branch_arguments_cannot_be_reinterpreted_as_flags() {
        let session = RepositorySession::new(".");
        for remote in ["--force", "-f"] {
            let error = session.push(Some(remote), None).unwrap_err();
            assert_eq!(error.code(), SdkErrorCode::InvalidArgument);
        }
        for branch in ["--force", "-f"] {
            let error = session.push(Some("origin"), Some(branch)).unwrap_err();
            assert_eq!(error.code(), SdkErrorCode::InvalidArgument);
        }
    }
}
