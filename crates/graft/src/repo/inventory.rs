use super::*;

impl Repository {
    pub fn status(&self) -> Result<RepoStatus> {
        let config = self.config()?;
        let head = self.head()?;
        let upstream = head
            .branch_name()
            .map(|branch| self.branch_upstream(branch))
            .transpose()?
            .flatten();
        let head_target = self.head_target()?;
        let index = self.read_index()?;
        let branches = self.branches()?;
        let remotes = self.remotes()?;
        let upstream_status = self.upstream_status(head_target.as_deref(), upstream.as_ref())?;
        let ahead = upstream_status.as_ref().map_or(0, |status| status.ahead);
        let behind = upstream_status.as_ref().map_or(0, |status| status.behind);
        let merge_head = self.merge_head()?;
        let orig_head = self.orig_head()?;
        let staged_changes = self.staged_changes_for_index(&index)?;
        let conflicted_changes = self.conflicted_changes_for_index(&index);
        let unstaged_changes = self.unstaged_changes_for_index(&index)?;
        let unstaged: Vec<String> = unstaged_changes
            .iter()
            .map(|change| change.path.clone())
            .collect();
        let staged = index.staged_paths();
        let conflicted = index.conflicted_paths();
        let counts = RepoStatusCounts::from_status_parts(
            unstaged.len(),
            unstaged_changes.len(),
            staged.len(),
            staged_changes.len(),
            conflicted.len(),
            conflicted_changes.len(),
        );
        let has_unstaged_changes = counts.unstaged > 0;
        let has_staged_changes = counts.staged > 0;
        let has_conflicts = counts.conflicted > 0;
        let work_in_progress =
            has_unstaged_changes || has_staged_changes || has_conflicts || merge_head.is_some();
        let dirty = has_unstaged_changes;
        let paths = RepoStatus::status_paths_from_changes(
            &unstaged_changes,
            &staged_changes,
            &conflicted_changes,
        );

        Ok(RepoStatus {
            worktree: self.worktree.clone(),
            graft_dir: self.graft_dir.clone(),
            repository_format_version: config.core.repository_format_version,
            head,
            head_target,
            merge_head,
            orig_head,
            dirty,
            has_unstaged_changes,
            has_staged_changes,
            has_conflicts,
            work_in_progress,
            counts,
            paths,
            unstaged,
            unstaged_changes,
            staged,
            staged_changes,
            conflicted,
            conflicted_changes,
            branches,
            remotes,
            upstream,
            upstream_status,
            ahead,
            behind,
        })
    }

    pub fn audit_artifacts(&self) -> Result<RepoArtifactAudit> {
        let artifacts = self.index_artifacts()?;
        let mut audit = RepoArtifactAudit {
            artifacts: artifacts.len(),
            external_payloads: artifacts.values().filter(|state| state.is_large()).count(),
            issues: Vec::new(),
        };

        for (path, state) in artifacts {
            self.audit_artifact_state(&path, &state, &mut audit);
        }

        Ok(audit)
    }

    pub fn repair_artifacts_from_remote(&self, remote: &str) -> Result<RepoArtifactRepairOutcome> {
        validate_remote_name(remote)?;
        let before = self.audit_artifacts()?;
        let remote_store = self.remote_store(remote)?;
        let artifacts = self.index_artifacts()?;
        let mut pack_cache = RemoteObjectPackCache::default();
        let mut fetched_objects = BTreeSet::new();
        let mut fetched_external_payloads = BTreeSet::new();

        for state in artifacts.values() {
            self.repair_artifact_state_from_remote(
                &remote_store,
                state,
                &mut pack_cache,
                &mut fetched_objects,
                &mut fetched_external_payloads,
            )?;
        }

        let after = self.audit_artifacts()?;
        Ok(RepoArtifactRepairOutcome {
            remote: remote.to_string(),
            fetched_objects: fetched_objects.len(),
            fetched_external_payloads: fetched_external_payloads.len(),
            before,
            after,
        })
    }

    pub fn fetch_large_file_payloads(
        &self,
        remote: &str,
        rev: Option<&str>,
    ) -> Result<RepoLargeFileFetchOutcome> {
        validate_remote_name(remote)?;
        let target = self.resolve_revision(rev.unwrap_or("HEAD"))?;
        let commit = self.read_commit(&target)?;
        let remote_store = self.remote_store(remote)?;
        let mut files = BTreeMap::<object::ObjectId, RepoLargeFileFetchEntry>::new();

        for (path, state) in &commit.artifacts {
            let CommitArtifactState::LargeFile { content_hash, size, .. } = state else {
                continue;
            };
            let entry =
                files
                    .entry(content_hash.clone())
                    .or_insert_with(|| RepoLargeFileFetchEntry {
                        content_hash: content_hash.clone(),
                        size: *size,
                        store_path: large_file_content_relative_path(content_hash),
                        status: RepoLargeFileFetchStatus::Present,
                        paths: Vec::new(),
                    });
            if entry.size != *size {
                return Err(RepoErr::Object(object::ObjectErr::InvalidObject {
                    kind: "large-file-pointer",
                    message: "same content hash referenced with different sizes".to_string(),
                }));
            }
            entry.paths.push(path.clone());
        }

        let mut already_present_payloads = 0;
        let mut fetched_payloads = 0;
        let mut fetched_bytes = 0;
        for entry in files.values_mut() {
            let present = self.large_file_content_path(&entry.content_hash).exists();
            self.fetch_large_file_content(&remote_store, &entry.content_hash, entry.size)?;
            if present {
                already_present_payloads += 1;
                entry.status = RepoLargeFileFetchStatus::Present;
            } else {
                fetched_payloads += 1;
                fetched_bytes += entry.size;
                entry.status = RepoLargeFileFetchStatus::Fetched;
            }
        }

        Ok(RepoLargeFileFetchOutcome {
            remote: remote.to_string(),
            target,
            external_payloads: files.len(),
            already_present_payloads,
            fetched_payloads,
            fetched_bytes,
            files: files.into_values().collect(),
        })
    }

    pub fn large_file_payloads_status(
        &self,
        rev: Option<&str>,
    ) -> Result<RepoLargeFileStatusOutcome> {
        let target = self.resolve_revision(rev.unwrap_or("HEAD"))?;
        let commit = self.read_commit(&target)?;
        let mut files = BTreeMap::<object::ObjectId, RepoLargeFileStatusEntry>::new();

        for (path, state) in &commit.artifacts {
            let CommitArtifactState::LargeFile { content_hash, size, .. } = state else {
                continue;
            };
            let entry =
                files
                    .entry(content_hash.clone())
                    .or_insert_with(|| RepoLargeFileStatusEntry {
                        content_hash: content_hash.clone(),
                        size: *size,
                        store_path: large_file_content_relative_path(content_hash),
                        status: RepoLargeFileStatusState::Missing,
                        message: None,
                        paths: Vec::new(),
                    });
            if entry.size != *size {
                return Err(RepoErr::Object(object::ObjectErr::InvalidObject {
                    kind: "large-file-pointer",
                    message: "same content hash referenced with different sizes".to_string(),
                }));
            }
            entry.paths.push(path.clone());
        }

        let mut present_payloads = 0;
        let mut missing_payloads = 0;
        let mut invalid_payloads = 0;
        let mut present_bytes = 0;
        let mut missing_bytes = 0;
        let mut invalid_bytes = 0;
        for entry in files.values_mut() {
            match fs::read(self.large_file_content_path(&entry.content_hash)) {
                Ok(bytes) => {
                    if let Err(err) =
                        validate_large_file_content(&entry.content_hash, entry.size, &bytes)
                    {
                        entry.status = RepoLargeFileStatusState::Invalid;
                        entry.message = Some(err.to_string());
                        invalid_payloads += 1;
                        invalid_bytes += entry.size;
                    } else {
                        entry.status = RepoLargeFileStatusState::Present;
                        present_payloads += 1;
                        present_bytes += entry.size;
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    entry.status = RepoLargeFileStatusState::Missing;
                    entry.message =
                        Some(format!("missing external payload {}", entry.content_hash));
                    missing_payloads += 1;
                    missing_bytes += entry.size;
                }
                Err(err) => return Err(err.into()),
            }
        }

        Ok(RepoLargeFileStatusOutcome {
            target,
            external_payloads: files.len(),
            present_payloads,
            missing_payloads,
            invalid_payloads,
            present_bytes,
            missing_bytes,
            invalid_bytes,
            files: files.into_values().collect(),
        })
    }

    pub fn prune_large_file_payloads(&self, dry_run: bool) -> Result<RepoLargeFilePruneOutcome> {
        let referenced = self.referenced_large_file_payloads()?;
        let mut files = Vec::new();
        for payload in self.local_large_file_payloads()? {
            if referenced.contains(&payload.content_hash) {
                continue;
            }
            files.push(payload);
        }

        files.sort_by(|left, right| left.content_hash.cmp(&right.content_hash));
        let candidate_payloads = files.len();
        let candidate_bytes = files.iter().map(|file| file.size).sum();
        let mut pruned_payloads = 0;
        let mut pruned_bytes = 0;
        if !dry_run {
            for file in &files {
                let path = self.graft_dir.join(&file.path);
                fs::remove_file(&path)?;
                remove_empty_parent_dirs(path.parent(), &self.file_store_dir())?;
                pruned_payloads += 1;
                pruned_bytes += file.size;
            }
        }

        Ok(RepoLargeFilePruneOutcome {
            dry_run,
            referenced_payloads: referenced.len(),
            candidate_payloads,
            candidate_bytes,
            pruned_payloads,
            pruned_bytes,
            files,
        })
    }

    pub fn tracked_paths(&self) -> Result<Vec<RepoTrackedPath>> {
        let files = self.index_files()?;
        let artifacts = self.index_artifacts()?;
        let mut paths = Vec::with_capacity(files.len() + artifacts.len());

        for (path, file) in files {
            paths.push(RepoTrackedPath {
                path,
                kind: RepoTrackedPathKind::SqliteDatabase,
                storage: RepoPathStorage::SqliteSnapshot,
                size: None,
                page_count: Some(file.snapshot.page_count),
            });
        }
        for (path, artifact) in artifacts {
            let kind = artifact_tracked_path_kind(&artifact);
            paths.push(RepoTrackedPath {
                path,
                kind,
                storage: artifact_tracked_path_storage(&artifact),
                size: Some(artifact.size()),
                page_count: None,
            });
        }

        paths.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(paths)
    }

    pub fn untracked_paths(&self) -> Result<Vec<RepoTrackedPath>> {
        let index = self.read_index()?;
        self.untracked_paths_for_index(&index)
    }

    pub fn tracked_path_details(&self) -> Result<Vec<RepoTrackedPathDetail>> {
        let files = self.index_files()?;
        let artifacts = self.index_artifacts()?;
        let mut paths = Vec::with_capacity(files.len() + artifacts.len());

        for (path, file) in files {
            paths.push(RepoTrackedPathDetail {
                path,
                kind: RepoTrackedPathKind::SqliteDatabase,
                storage: RepoPathStorage::SqliteSnapshot,
                size: None,
                page_count: Some(file.snapshot.page_count),
                oid: None,
                content_hash: None,
                object_present: None,
                external_payload_present: None,
            });
        }
        for (path, artifact) in artifacts {
            let kind = artifact_tracked_path_kind(&artifact);
            let external_payload_present = match &artifact {
                CommitArtifactState::LargeFile { content_hash, .. } => {
                    Some(self.large_file_content_path(content_hash).exists())
                }
                CommitArtifactState::File { .. } => None,
            };
            paths.push(RepoTrackedPathDetail {
                path,
                kind,
                storage: artifact_tracked_path_storage(&artifact),
                size: Some(artifact.size()),
                page_count: None,
                oid: Some(artifact.oid().clone()),
                content_hash: Some(artifact.content_hash().clone()),
                object_present: Some(self.object_store().path_for(artifact.oid()).exists()),
                external_payload_present,
            });
        }

        paths.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(paths)
    }

    pub fn tracked_path_entries(&self) -> Result<Vec<RepoTrackedPathEntry>> {
        let index = self.read_index()?;
        let mut normal_entries = BTreeMap::<String, RepoTrackedPathEntry>::new();

        for (path, file) in self.head_files()? {
            normal_entries.insert(
                path.clone(),
                tracked_file_entry(path, index::IndexStage::Normal, &file),
            );
        }
        for (path, artifact) in self.head_artifacts()? {
            normal_entries.insert(
                path.clone(),
                tracked_artifact_entry(path, index::IndexStage::Normal, &artifact),
            );
        }

        for path in index.conflicted_paths() {
            normal_entries.remove(&path);
        }

        for entry in index.stage0_entries() {
            if let Some(entry) = tracked_index_entry(entry) {
                normal_entries.insert(entry.path.clone(), entry);
            } else {
                normal_entries.remove(&entry.path);
            }
        }

        let mut entries = normal_entries.into_values().collect::<Vec<_>>();
        entries.extend(
            index
                .entries
                .iter()
                .filter(|entry| entry.stage != index::IndexStage::Normal)
                .filter_map(tracked_index_entry),
        );
        entries.sort_by(|left, right| {
            left.path
                .cmp(&right.path)
                .then_with(|| u8::from(left.stage).cmp(&u8::from(right.stage)))
        });
        Ok(entries)
    }

    pub(super) fn upstream_status(
        &self,
        local: Option<&str>,
        upstream: Option<&BranchUpstream>,
    ) -> Result<Option<RepoUpstreamStatus>> {
        let Some(local) = local else {
            return Ok(None);
        };
        let Some(upstream) = upstream else {
            return Ok(None);
        };
        let Some(remote_target) = self.remote_tracking_ref(&upstream.remote, &upstream.branch)?
        else {
            return Ok(None);
        };

        let local_reachable = self.reachable_commits(local)?;
        let remote_reachable = self.reachable_commits(&remote_target)?;
        let ahead = local_reachable.difference(&remote_reachable).count();
        let behind = remote_reachable.difference(&local_reachable).count();
        let state = match (ahead, behind) {
            (0, 0) => RepoUpstreamState::UpToDate,
            (_, 0) => RepoUpstreamState::Ahead,
            (0, _) => RepoUpstreamState::Behind,
            _ => RepoUpstreamState::Diverged,
        };

        Ok(Some(RepoUpstreamStatus {
            remote: upstream.remote.clone(),
            branch: upstream.branch.clone(),
            local: local.to_string(),
            remote_target,
            ahead,
            behind,
            state,
        }))
    }

    pub(super) fn reachable_commits(&self, start: &str) -> Result<BTreeSet<String>> {
        let mut reachable = BTreeSet::new();
        let mut stack = vec![start.to_string()];
        while let Some(id) = stack.pop() {
            if !reachable.insert(id.clone()) {
                continue;
            }
            for parent in commit_parent_ids(&self.read_commit(&id)?) {
                stack.push(parent);
            }
        }
        Ok(reachable)
    }
}
