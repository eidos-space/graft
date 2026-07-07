use super::*;

impl Repository {
    pub(super) fn write_artifact_state_from_path(
        &self,
        key: &str,
        path: &Path,
    ) -> Result<CommitArtifactState> {
        self.write_artifact_state_from_path_with_file_config(key, path, &self.file_config()?)
    }

    pub(super) fn write_artifact_state_from_path_with_file_config(
        &self,
        key: &str,
        path: &Path,
        config: &FileConfig,
    ) -> Result<CommitArtifactState> {
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.file_type().is_file() {
            return Err(RepoErr::Object(object::ObjectErr::InvalidObject {
                kind: "file",
                message: format!("path `{}` is not a regular file", path.display()),
            }));
        }

        let bytes = fs::read(path)?;
        let size = bytes.len() as u64;
        let kind = classify_artifact_bytes(&bytes);
        let object_kind = object_kind_from_repo_path_kind(kind);
        let content_hash = object::ObjectId::for_bytes(&bytes);
        if artifact_storage_for_path(key, kind, size, config) == RepoPathStorage::External {
            self.write_large_file_content(&content_hash, &bytes)?;
            let pointer = object::LargeFilePointerBlob {
                kind: object_kind,
                content_hash: content_hash.clone(),
                size,
            };
            let oid = self.object_store().write(&object::Object::Blob(
                object::BlobObject::LargeFilePointer(pointer),
            ))?;
            Ok(CommitArtifactState::LargeFile { kind, oid, content_hash, size })
        } else {
            let oid =
                self.object_store()
                    .write(&object::Object::Blob(object::BlobObject::File(
                        object::FileBlob { kind: object_kind, bytes },
                    )))?;
            Ok(CommitArtifactState::File { kind, oid, content_hash, size })
        }
    }

    pub(super) fn write_large_file_content(
        &self,
        id: &object::ObjectId,
        bytes: &[u8],
    ) -> Result<()> {
        let path = self.large_file_content_path(id);
        if path.exists() {
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        write_file_atomic(&path, bytes)?;
        Ok(())
    }

    pub(super) fn large_file_content_path(&self, id: &object::ObjectId) -> PathBuf {
        self.graft_dir.join(large_file_content_relative_path(id))
    }

    pub(super) fn read_large_file_content(
        &self,
        id: &object::ObjectId,
        size: u64,
    ) -> Result<Vec<u8>> {
        let bytes = fs::read(self.large_file_content_path(id))?;
        validate_large_file_content(id, size, &bytes)?;
        Ok(bytes)
    }

    pub(super) fn artifact_bytes(&self, state: &CommitArtifactState) -> Result<Vec<u8>> {
        match state {
            CommitArtifactState::File { oid, .. } => {
                let object = self.object_store().read(oid)?;
                let object::Object::Blob(object::BlobObject::File(blob)) = object else {
                    return Err(RepoErr::Object(object::ObjectErr::InvalidObject {
                        kind: "blob",
                        message: format!("artifact object {oid} is not a file blob"),
                    }));
                };
                Ok(blob.bytes)
            }
            CommitArtifactState::LargeFile { content_hash, size, .. } => {
                self.read_large_file_content(content_hash, *size)
            }
        }
    }

    pub(super) fn audit_artifact_state(
        &self,
        path: &str,
        state: &CommitArtifactState,
        audit: &mut RepoArtifactAudit,
    ) {
        match state {
            CommitArtifactState::File { kind, oid, content_hash, size } => {
                match self.object_store().read(oid) {
                    Ok(object::Object::Blob(object::BlobObject::File(blob))) => {
                        let actual_hash = object::ObjectId::for_bytes(&blob.bytes);
                        let actual_kind = repo_path_kind_from_object_kind(blob.kind);
                        if actual_kind != *kind
                            || &actual_hash != content_hash
                            || blob.bytes.len() as u64 != *size
                        {
                            audit.issues.push(RepoArtifactAuditIssue {
                                path: path.to_string(),
                                kind: RepoArtifactAuditIssueKind::InvalidObject,
                                oid: Some(oid.clone()),
                                content_hash: Some(content_hash.clone()),
                                message: format!(
                                    "file blob metadata mismatch: expected kind {kind}, {size} byte(s), and hash {content_hash}, got kind {actual_kind}, {} byte(s), and hash {actual_hash}",
                                    blob.bytes.len()
                                ),
                            });
                        }
                    }
                    Ok(_) => audit.issues.push(RepoArtifactAuditIssue {
                        path: path.to_string(),
                        kind: RepoArtifactAuditIssueKind::InvalidObject,
                        oid: Some(oid.clone()),
                        content_hash: Some(content_hash.clone()),
                        message: "artifact object is not a file blob".to_string(),
                    }),
                    Err(object::ObjectErr::Io(err))
                        if err.kind() == std::io::ErrorKind::NotFound =>
                    {
                        audit.issues.push(RepoArtifactAuditIssue {
                            path: path.to_string(),
                            kind: RepoArtifactAuditIssueKind::MissingObject,
                            oid: Some(oid.clone()),
                            content_hash: Some(content_hash.clone()),
                            message: format!("missing artifact object {oid}"),
                        });
                    }
                    Err(err) => audit.issues.push(RepoArtifactAuditIssue {
                        path: path.to_string(),
                        kind: RepoArtifactAuditIssueKind::InvalidObject,
                        oid: Some(oid.clone()),
                        content_hash: Some(content_hash.clone()),
                        message: err.to_string(),
                    }),
                }
            }
            CommitArtifactState::LargeFile { kind, oid, content_hash, size } => {
                match self.object_store().read(oid) {
                    Ok(object::Object::Blob(object::BlobObject::LargeFilePointer(pointer))) => {
                        let actual_kind = repo_path_kind_from_object_kind(pointer.kind);
                        if actual_kind != *kind
                            || &pointer.content_hash != content_hash
                            || pointer.size != *size
                        {
                            audit.issues.push(RepoArtifactAuditIssue {
                                path: path.to_string(),
                                kind: RepoArtifactAuditIssueKind::InvalidObject,
                                oid: Some(oid.clone()),
                                content_hash: Some(content_hash.clone()),
                                message: format!(
                                    "large file pointer mismatch: expected kind {kind}, {size} byte(s), and hash {content_hash}, got kind {actual_kind}, {} byte(s), and hash {}",
                                    pointer.size,
                                    pointer.content_hash
                                ),
                            });
                        }
                    }
                    Ok(_) => audit.issues.push(RepoArtifactAuditIssue {
                        path: path.to_string(),
                        kind: RepoArtifactAuditIssueKind::InvalidObject,
                        oid: Some(oid.clone()),
                        content_hash: Some(content_hash.clone()),
                        message: "artifact object is not a large file pointer".to_string(),
                    }),
                    Err(object::ObjectErr::Io(err))
                        if err.kind() == std::io::ErrorKind::NotFound =>
                    {
                        audit.issues.push(RepoArtifactAuditIssue {
                            path: path.to_string(),
                            kind: RepoArtifactAuditIssueKind::MissingObject,
                            oid: Some(oid.clone()),
                            content_hash: Some(content_hash.clone()),
                            message: format!("missing large file pointer object {oid}"),
                        });
                    }
                    Err(err) => audit.issues.push(RepoArtifactAuditIssue {
                        path: path.to_string(),
                        kind: RepoArtifactAuditIssueKind::InvalidObject,
                        oid: Some(oid.clone()),
                        content_hash: Some(content_hash.clone()),
                        message: err.to_string(),
                    }),
                }

                match fs::read(self.large_file_content_path(content_hash)) {
                    Ok(bytes) => {
                        if let Err(err) = validate_large_file_content(content_hash, *size, &bytes) {
                            audit.issues.push(RepoArtifactAuditIssue {
                                path: path.to_string(),
                                kind: RepoArtifactAuditIssueKind::InvalidExternalPayload,
                                oid: Some(oid.clone()),
                                content_hash: Some(content_hash.clone()),
                                message: err.to_string(),
                            });
                        }
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                        audit.issues.push(RepoArtifactAuditIssue {
                            path: path.to_string(),
                            kind: RepoArtifactAuditIssueKind::MissingExternalPayload,
                            oid: Some(oid.clone()),
                            content_hash: Some(content_hash.clone()),
                            message: format!("missing external payload {content_hash}"),
                        });
                    }
                    Err(err) => audit.issues.push(RepoArtifactAuditIssue {
                        path: path.to_string(),
                        kind: RepoArtifactAuditIssueKind::InvalidExternalPayload,
                        oid: Some(oid.clone()),
                        content_hash: Some(content_hash.clone()),
                        message: err.to_string(),
                    }),
                }
            }
        }
    }
}
