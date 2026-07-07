use super::*;

static ASYNC_JOBS: OnceLock<AsyncJobRegistry> = OnceLock::new();

pub(super) fn async_jobs() -> &'static AsyncJobRegistry {
    ASYNC_JOBS.get_or_init(AsyncJobRegistry::default)
}

#[derive(Default)]
pub(super) struct AsyncJobRegistry {
    pub(super) jobs: Mutex<BTreeMap<String, AsyncJob>>,
}

impl AsyncJobRegistry {
    pub(super) fn spawn_fetch(
        &self,
        repo_file: PathBuf,
        remote: Option<String>,
        branch: Option<String>,
        refspec: Option<String>,
        all: bool,
        format: AsyncJobResultFormat,
    ) -> String {
        let id = format!("graft-job-{}", NEXT_JOB_ID.fetch_add(1, Ordering::Relaxed));
        self.jobs
            .lock()
            .insert(id.clone(), AsyncJob::running("fetch", format));

        let job_id = id.clone();
        std::thread::spawn(move || {
            let result = Repository::discover_for_file(&repo_file)
                .map_err(|err| err.to_string())
                .and_then(|repo| {
                    match format {
                        AsyncJobResultFormat::Text => {
                            run_repo_fetch(&repo, remote, branch, refspec, all)
                        }
                        AsyncJobResultFormat::Json => {
                            run_repo_fetch_json(&repo, remote, branch, refspec, all)
                        }
                    }
                    .map_err(|err| err.to_string())
                });
            async_jobs().finish(&job_id, result);
        });

        id
    }

    pub(super) fn finish(&self, id: &str, result: Result<String, String>) {
        let mut jobs = self.jobs.lock();
        if let Some(job) = jobs.get_mut(id) {
            match result {
                Ok(result) => job.finish(result),
                Err(error) => job.fail(error),
            }
        }
    }

    pub(super) fn status_json(&self, id: &str) -> Result<String, ErrCtx> {
        let jobs = self.jobs.lock();
        let job = jobs.get(id).ok_or_else(|| unknown_job(id))?;
        Ok(job.status_json(id))
    }

    pub(super) fn json_status(&self, id: &str) -> Result<String, ErrCtx> {
        let jobs = self.jobs.lock();
        let job = jobs.get(id).ok_or_else(|| unknown_job(id))?;
        Ok(job.json_status(id))
    }

    pub(super) fn result(&self, id: &str) -> Result<String, ErrCtx> {
        let jobs = self.jobs.lock();
        let job = jobs.get(id).ok_or_else(|| unknown_job(id))?;
        match job.state {
            AsyncJobState::Running => Err(ErrCtx::PragmaErr(
                format!("job `{id}` is still running").into(),
            )),
            AsyncJobState::Done => Ok(job.result.clone().unwrap_or_default()),
            AsyncJobState::Failed => Err(ErrCtx::PragmaErr(
                format!(
                    "job `{id}` failed: {}",
                    job.error.as_deref().unwrap_or("unknown error")
                )
                .into(),
            )),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) enum AsyncJobResultFormat {
    Text,
    Json,
}

impl AsyncJobResultFormat {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Json => "json",
        }
    }
}

pub(super) fn unknown_job(id: &str) -> ErrCtx {
    ErrCtx::PragmaErr(format!("unknown job `{id}`").into())
}

#[derive(Debug, Clone)]
pub(super) struct AsyncJob {
    pub(super) kind: &'static str,
    pub(super) format: AsyncJobResultFormat,
    pub(super) state: AsyncJobState,
    pub(super) result: Option<String>,
    pub(super) error: Option<String>,
}

impl AsyncJob {
    pub(super) fn running(kind: &'static str, format: AsyncJobResultFormat) -> Self {
        Self {
            kind,
            format,
            state: AsyncJobState::Running,
            result: None,
            error: None,
        }
    }

    pub(super) fn finish(&mut self, result: String) {
        self.state = AsyncJobState::Done;
        self.result = Some(result);
        self.error = None;
    }

    pub(super) fn fail(&mut self, error: String) {
        self.state = AsyncJobState::Failed;
        self.result = None;
        self.error = Some(error);
    }

    pub(super) fn status_json(&self, id: &str) -> String {
        serde_json::json!({
            "id": id,
            "kind": self.kind,
            "state": self.state.as_str(),
            "result": self.result,
            "error": self.error,
        })
        .to_string()
    }

    pub(super) fn json_status(&self, id: &str) -> String {
        let result = match (&self.result, self.format) {
            (Some(result), AsyncJobResultFormat::Json) => serde_json::from_str(result)
                .unwrap_or_else(|_| serde_json::Value::String(result.clone())),
            (Some(result), AsyncJobResultFormat::Text) => serde_json::Value::String(result.clone()),
            (None, _) => serde_json::Value::Null,
        };
        serde_json::json!({
            "id": id,
            "kind": self.kind,
            "state": self.state.as_str(),
            "result_format": self.format.as_str(),
            "result": result,
            "error": self.error,
        })
        .to_string()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AsyncJobState {
    Running,
    Done,
    Failed,
}

impl AsyncJobState {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }
}
