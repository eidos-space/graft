use std::{path::PathBuf, sync::Arc};

use graft_sdk::{
    DiffOptions as CoreDiffOptions, RemoteConfigureOptions as CoreRemoteConfigureOptions,
    RepositoryOperation, RepositorySession as CoreRepositorySession,
    RestoreOptions as CoreRestoreOptions, SdkError,
};
use napi::{
    Env, Error, Result, Status, Task,
    bindgen_prelude::{AbortSignal, AsyncTask},
};
use napi_derive::napi;

#[napi(object)]
pub struct DiffOptions {
    pub rows: Option<bool>,
    pub staged: Option<bool>,
    pub root: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub path: Option<String>,
}

#[napi(object)]
pub struct RestoreOptions {
    pub source: Option<String>,
    pub expected_head: Option<String>,
    pub require_clean: Option<bool>,
    pub path: String,
}

#[napi(object)]
pub struct RemoteConfigureOptions {
    pub name: String,
    pub url: String,
    pub bearer_token: Option<String>,
    pub overwrite: Option<bool>,
    pub upstream_branch: Option<String>,
}

#[napi(object)]
pub struct CloneOptions {
    pub remote_url: String,
    pub branch: Option<String>,
    pub bearer_token: Option<String>,
}

enum JsonOperation {
    Init,
    Status,
    StatusIncremental,
    AddAll,
    Commit {
        message: String,
    },
    Diff {
        options: CoreDiffOptions,
    },
    History {
        limit: usize,
        after: Option<String>,
    },
    Restore {
        options: CoreRestoreOptions,
    },
    ConfigureRemote {
        options: CoreRemoteConfigureOptions,
    },
    Push {
        remote: Option<String>,
        branch: Option<String>,
    },
    Fetch {
        remote: Option<String>,
        branch: Option<String>,
    },
    Pull {
        remote: Option<String>,
        branch: Option<String>,
    },
    Clone {
        remote_url: String,
        branch: Option<String>,
        bearer_token: Option<String>,
    },
}

pub struct JsonTask {
    session: Arc<CoreRepositorySession>,
    operation: JsonOperation,
}

impl Task for JsonTask {
    type Output = String;
    type JsValue = String;

    fn compute(&mut self) -> Result<Self::Output> {
        if matches!(self.operation, JsonOperation::StatusIncremental) {
            let value = self.session.status_incremental().map_err(napi_error)?;
            return serde_json::to_string(&value)
                .map_err(|error| Error::new(Status::GenericFailure, error.to_string()));
        }
        let value = match &mut self.operation {
            JsonOperation::Init => self.session.init(),
            JsonOperation::Status => self.session.status(),
            JsonOperation::StatusIncremental => unreachable!("handled before JSON value dispatch"),
            JsonOperation::AddAll => self.session.add_all(),
            JsonOperation::Commit { message } => self.session.commit(message),
            JsonOperation::Diff { options } => self.session.diff(options),
            JsonOperation::History { limit, after } => {
                self.session.history(*limit, after.as_deref())
            }
            JsonOperation::Restore { options } => self.session.restore(options),
            JsonOperation::ConfigureRemote { options } => self.session.configure_remote(options),
            JsonOperation::Push { remote, branch } => {
                self.session.push(remote.as_deref(), branch.as_deref())
            }
            JsonOperation::Fetch { remote, branch } => {
                self.session.fetch(remote.as_deref(), branch.as_deref())
            }
            JsonOperation::Pull { remote, branch } => {
                self.session.pull(remote.as_deref(), branch.as_deref())
            }
            JsonOperation::Clone { remote_url, branch, bearer_token } => self
                .session
                .clone_repository(remote_url, branch.as_deref(), bearer_token.take()),
        }
        .map_err(napi_error)?;
        serde_json::to_string(&value)
            .map_err(|error| Error::new(Status::GenericFailure, error.to_string()))
    }

    fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
        Ok(output)
    }
}

enum LifecycleOperation {
    Open,
    Close,
    Reopen,
}

pub struct LifecycleTask {
    session: Arc<CoreRepositorySession>,
    operation: LifecycleOperation,
}

impl Task for LifecycleTask {
    type Output = String;
    type JsValue = String;

    fn compute(&mut self) -> Result<Self::Output> {
        match self.operation {
            LifecycleOperation::Open => self.session.open(),
            LifecycleOperation::Close => self.session.close(),
            LifecycleOperation::Reopen => self.session.reopen(),
        }
        .map_err(napi_error)?;
        Ok(lifecycle_label(self.session.lifecycle()).to_string())
    }

    fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
        Ok(output)
    }
}

#[napi(js_name = "RepositorySession")]
pub struct NodeRepositorySession {
    session: Arc<CoreRepositorySession>,
}

#[napi]
impl NodeRepositorySession {
    #[napi(constructor)]
    pub fn new(target: String) -> Self {
        Self {
            session: Arc::new(CoreRepositorySession::new(target)),
        }
    }

    #[napi(getter)]
    pub fn target(&self) -> String {
        self.session.target().to_string_lossy().into_owned()
    }

    #[napi(getter)]
    pub fn lifecycle(&self) -> &'static str {
        lifecycle_label(self.session.lifecycle())
    }

    #[napi]
    pub fn open(&self, signal: Option<AbortSignal>) -> AsyncTask<LifecycleTask> {
        lifecycle_task(self, LifecycleOperation::Open, signal)
    }

    #[napi]
    pub fn close(&self, signal: Option<AbortSignal>) -> AsyncTask<LifecycleTask> {
        lifecycle_task(self, LifecycleOperation::Close, signal)
    }

    #[napi]
    pub fn reopen(&self, signal: Option<AbortSignal>) -> AsyncTask<LifecycleTask> {
        lifecycle_task(self, LifecycleOperation::Reopen, signal)
    }

    #[napi]
    pub fn set_http_bearer_token(&self, remote_name: String, token: String) -> Result<()> {
        self.session
            .set_http_bearer_token(&remote_name, token)
            .map_err(napi_error)
    }

    #[napi]
    pub fn clear_http_bearer_token(&self, remote_name: String) -> Result<()> {
        self.session
            .clear_http_bearer_token(&remote_name)
            .map_err(napi_error)
    }

    #[napi]
    pub fn init(&self, signal: Option<AbortSignal>) -> AsyncTask<JsonTask> {
        json_task(self, JsonOperation::Init, signal)
    }

    #[napi]
    pub fn status(&self, signal: Option<AbortSignal>) -> AsyncTask<JsonTask> {
        json_task(self, JsonOperation::Status, signal)
    }

    #[napi]
    pub fn status_incremental(&self, signal: Option<AbortSignal>) -> AsyncTask<JsonTask> {
        json_task(self, JsonOperation::StatusIncremental, signal)
    }

    #[napi]
    pub fn add_all(&self, signal: Option<AbortSignal>) -> AsyncTask<JsonTask> {
        json_task(self, JsonOperation::AddAll, signal)
    }

    #[napi]
    pub fn commit(&self, message: String, signal: Option<AbortSignal>) -> AsyncTask<JsonTask> {
        json_task(self, JsonOperation::Commit { message }, signal)
    }

    #[napi]
    pub fn diff(
        &self,
        options: Option<DiffOptions>,
        signal: Option<AbortSignal>,
    ) -> AsyncTask<JsonTask> {
        let options = options.map_or_else(CoreDiffOptions::default, |options| CoreDiffOptions {
            rows: options.rows.unwrap_or(false),
            staged: options.staged.unwrap_or(false),
            root: options.root,
            from: options.from,
            to: options.to,
            path: options.path.map(PathBuf::from),
        });
        json_task(self, JsonOperation::Diff { options }, signal)
    }

    #[napi]
    pub fn history(
        &self,
        limit: Option<u32>,
        after: Option<String>,
        signal: Option<AbortSignal>,
    ) -> AsyncTask<JsonTask> {
        json_task(
            self,
            JsonOperation::History {
                limit: limit.unwrap_or(50) as usize,
                after,
            },
            signal,
        )
    }

    #[napi]
    pub fn restore(
        &self,
        options: RestoreOptions,
        signal: Option<AbortSignal>,
    ) -> AsyncTask<JsonTask> {
        json_task(
            self,
            JsonOperation::Restore {
                options: CoreRestoreOptions {
                    source: options.source,
                    expected_head: options.expected_head,
                    require_clean: options.require_clean.unwrap_or(false),
                    path: PathBuf::from(options.path),
                },
            },
            signal,
        )
    }

    #[napi]
    pub fn configure_remote(
        &self,
        options: RemoteConfigureOptions,
        signal: Option<AbortSignal>,
    ) -> AsyncTask<JsonTask> {
        json_task(
            self,
            JsonOperation::ConfigureRemote {
                options: CoreRemoteConfigureOptions {
                    name: options.name,
                    url: options.url,
                    bearer_token: options.bearer_token,
                    overwrite: options.overwrite.unwrap_or(false),
                    upstream_branch: options.upstream_branch,
                },
            },
            signal,
        )
    }

    #[napi]
    pub fn push(
        &self,
        remote: Option<String>,
        branch: Option<String>,
        signal: Option<AbortSignal>,
    ) -> AsyncTask<JsonTask> {
        json_task(self, JsonOperation::Push { remote, branch }, signal)
    }

    #[napi]
    pub fn fetch(
        &self,
        remote: Option<String>,
        branch: Option<String>,
        signal: Option<AbortSignal>,
    ) -> AsyncTask<JsonTask> {
        json_task(self, JsonOperation::Fetch { remote, branch }, signal)
    }

    #[napi]
    pub fn pull(
        &self,
        remote: Option<String>,
        branch: Option<String>,
        signal: Option<AbortSignal>,
    ) -> AsyncTask<JsonTask> {
        json_task(self, JsonOperation::Pull { remote, branch }, signal)
    }

    #[napi(js_name = "cloneRepository")]
    pub fn clone_repository(
        &self,
        options: CloneOptions,
        signal: Option<AbortSignal>,
    ) -> AsyncTask<JsonTask> {
        json_task(
            self,
            JsonOperation::Clone {
                remote_url: options.remote_url,
                branch: options.branch,
                bearer_token: options.bearer_token,
            },
            signal,
        )
    }
}

#[napi]
pub fn operation_materializes_worktree(operation: String) -> Result<bool> {
    let operation = match operation.as_str() {
        "init" => RepositoryOperation::Init,
        "status" => RepositoryOperation::Status,
        "add_all" | "addAll" => RepositoryOperation::AddAll,
        "commit" => RepositoryOperation::Commit,
        "diff" => RepositoryOperation::Diff,
        "history" => RepositoryOperation::History,
        "restore" => RepositoryOperation::Restore,
        "remote_configure" | "configureRemote" => RepositoryOperation::RemoteConfigure,
        "push" => RepositoryOperation::Push,
        "fetch" => RepositoryOperation::Fetch,
        "pull" => RepositoryOperation::Pull,
        "clone" | "cloneRepository" => RepositoryOperation::Clone,
        _ => {
            return Err(Error::new(
                Status::InvalidArg,
                format!("unknown repository operation `{operation}`"),
            ));
        }
    };
    Ok(operation.materializes_worktree())
}

#[napi]
pub fn sdk_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

fn lifecycle_task(
    session: &NodeRepositorySession,
    operation: LifecycleOperation,
    signal: Option<AbortSignal>,
) -> AsyncTask<LifecycleTask> {
    AsyncTask::with_optional_signal(
        LifecycleTask {
            session: session.session.clone(),
            operation,
        },
        signal,
    )
}

fn json_task(
    session: &NodeRepositorySession,
    operation: JsonOperation,
    signal: Option<AbortSignal>,
) -> AsyncTask<JsonTask> {
    AsyncTask::with_optional_signal(
        JsonTask {
            session: session.session.clone(),
            operation,
        },
        signal,
    )
}

fn napi_error(error: SdkError) -> Error {
    Error::new(
        Status::GenericFailure,
        format!("[{}] {}", error.code().as_str(), error.message()),
    )
}

fn lifecycle_label(lifecycle: graft_sdk::SessionLifecycle) -> &'static str {
    match lifecycle {
        graft_sdk::SessionLifecycle::Closed => "closed",
        graft_sdk::SessionLifecycle::Opening => "opening",
        graft_sdk::SessionLifecycle::Open => "open",
        graft_sdk::SessionLifecycle::Closing => "closing",
    }
}
