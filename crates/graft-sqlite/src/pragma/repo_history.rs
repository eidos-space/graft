use super::*;

pub(super) fn run_repo_commit(
    runtime: &Runtime,
    file: &mut VolFile,
    message: String,
) -> Result<RepoCommitOutcome, ErrCtx> {
    if !file.is_idle() {
        return pragma_err!("cannot commit while there is an open transaction");
    }
    let repo = repo_for_file(file)?;
    let tables = staged_commit_table_summary(runtime, &repo)?;
    let commit = repo.commit_staged_with_table_summary(message, tables)?;
    let branch = repo.current_branch()?;
    Ok(RepoCommitOutcome { commit, branch })
}

pub(super) fn json_commit_summary(commit: CommitObject) -> JsonCommitSummary {
    let parents = if commit.parents.is_empty() {
        commit.parent.into_iter().collect()
    } else {
        commit.parents
    };
    JsonCommitSummary {
        id: commit.id,
        message: commit.message,
        parents,
    }
}

pub(super) fn json_commit_path_changes(
    commit: &CommitObject,
) -> Vec<crate::json::JsonRepoPathDiff> {
    commit
        .changes
        .iter()
        .map(|change| crate::json::JsonRepoPathDiff {
            path: change.path.clone(),
            change: repo_file_change_label(change.change).to_string(),
            kind: repo_tracked_path_kind_json_label(change.kind).to_string(),
            storage: repo_path_storage_json_label(change.storage).to_string(),
        })
        .collect()
}
