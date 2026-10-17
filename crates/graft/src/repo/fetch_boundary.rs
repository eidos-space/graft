use super::*;

/// A local completed fetch, bound to the live secret epoch and canonical local
/// store identity. Existing plaintext objects remain checked whenever read;
/// receipts do not promise repair of arbitrary out-of-band cache corruption.
#[derive(Default)]
pub(super) struct FetchBoundary {
    pub objects: BTreeMap<object::ObjectId, GraphRole>,
    budget: GraphBudget,
    #[cfg(all(feature = "e2ee", unix))]
    receipt: Option<(crate::e2ee::SessionOperation, String)>,
}

#[derive(Clone, Copy, Default)]
pub(super) struct GraphBudget(bool);

impl GraphBudget {
    pub fn new(remote: &Remote) -> Self {
        Self(remote.buffered_read_limit().is_some())
    }

    pub fn admit(self, retained: usize, additional: usize) -> Result<()> {
        if self.0
            && retained
                .checked_add(additional)
                .is_none_or(|count| count > 262_144)
        {
            #[cfg(all(feature = "e2ee", not(target_arch = "wasm32")))]
            return Err(RemoteErr::from(crate::e2ee::EncryptionError::TooLarge).into());
            #[cfg(not(all(feature = "e2ee", not(target_arch = "wasm32"))))]
            return Err(RepoErr::InvalidRemoteObject {
                path: "objects".into(),
                message: "object graph exceeds discovery budget".into(),
            });
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) enum GraphRole {
    Commit,
    Tree,
    File,
    Sqlite,
}

impl GraphRole {
    pub fn entry(mode: object::TreeEntryMode) -> Self {
        match mode {
            object::TreeEntryMode::Regular => Self::File,
            object::TreeEntryMode::SqliteDatabase => Self::Sqlite,
        }
    }

    pub fn validate(self, id: &object::ObjectId, value: &object::Object) -> Result<()> {
        let valid = matches!(
            (self, value),
            (Self::Commit, object::Object::Commit(_))
                | (Self::Tree, object::Object::Tree(_))
                | (
                    Self::File,
                    object::Object::Blob(
                        object::BlobObject::File(_) | object::BlobObject::LargeFilePointer(_)
                    )
                )
                | (
                    Self::Sqlite,
                    object::Object::Blob(object::BlobObject::SqliteSnapshot(_))
                )
        );
        if !valid {
            return Err(RepoErr::InvalidRemoteObject {
                path: id.to_string(),
                message: format!("object does not satisfy its {self:?} graph role"),
            });
        }
        Ok(())
    }
}

impl FetchBoundary {
    #[cfg(test)]
    pub fn from_objects(objects: BTreeMap<object::ObjectId, GraphRole>) -> Self {
        Self { objects, ..Self::default() }
    }

    pub fn contains(&self, id: &object::ObjectId, role: GraphRole) -> bool {
        self.objects.get(id) == Some(&role)
    }

    pub fn remember(&self, head: &str) -> Result<()> {
        #[cfg(all(feature = "e2ee", unix))]
        if let Some((operation, scope)) = &self.receipt {
            operation
                .remember_verified_graph_head(scope, head)
                .map_err(RemoteErr::from)?;
        }
        let _ = head;
        Ok(())
    }
}

impl Repository {
    pub(super) fn fetch_boundary(
        &self,
        remote: &Remote,
        name: &str,
        branch: &str,
        have: Option<&str>,
    ) -> Result<FetchBoundary> {
        #[cfg(all(feature = "e2ee", unix))]
        if remote.buffered_read_limit().is_some() {
            let operation = remote.begin_encrypted_operation()?;
            let directory = fs::canonicalize(&self.graft_dir)?;
            let metadata = fs::metadata(&directory)?;
            let config = self.config()?;
            let Some(RemoteConfig::EncryptedHttp { url, .. }) = config.remotes.get(name) else {
                return Ok(FetchBoundary::default());
            };
            let scope = format!(
                "{url}|{}|{}:{}|refs/heads/{branch}",
                directory.display(),
                metadata.dev(),
                metadata.ino()
            );
            let previous = operation
                .cached_verified_graph_head(&scope)
                .map_err(RemoteErr::from)?;
            let objects = if previous.as_deref() == have {
                have.and_then(|head| head.parse().ok())
                    .and_then(|id| self.verified_boundary_objects(&id).ok())
                    .unwrap_or_default()
            } else {
                BTreeMap::new()
            };
            return Ok(FetchBoundary {
                objects,
                budget: GraphBudget::new(remote),
                receipt: Some((operation, scope)),
            });
        }
        let _ = (remote, name, branch, have);
        Ok(FetchBoundary {
            budget: GraphBudget::new(remote),
            ..FetchBoundary::default()
        })
    }

    #[cfg(all(feature = "e2ee", unix))]
    fn verified_boundary_objects(
        &self,
        head: &object::ObjectId,
    ) -> Result<BTreeMap<object::ObjectId, GraphRole>> {
        let object::Object::Commit(commit) = self.object_store().read(head)? else {
            return Err(RepoErr::CommitNotFound(head.to_string()));
        };
        let object::Object::Tree(tree) = self.object_store().read(&commit.tree)? else {
            return Err(RepoErr::CommitNotFound(head.to_string()));
        };
        if tree.entries.len() > 100_000 {
            return Ok(BTreeMap::new());
        }
        let mut objects = BTreeMap::from([
            (head.clone(), GraphRole::Commit),
            (commit.tree, GraphRole::Tree),
        ]);
        for entry in tree.entries {
            let role = GraphRole::entry(entry.mode);
            if objects
                .insert(entry.oid, role)
                .is_some_and(|previous| previous != role)
            {
                return Ok(BTreeMap::new());
            }
        }
        Ok(objects)
    }

    pub(super) fn fetched_commit_ancestors(
        &self,
        head: &str,
        boundary: &FetchBoundary,
    ) -> Result<BTreeSet<String>> {
        let mut result = BTreeSet::new();
        let mut pending = vec![head.to_owned()];
        while let Some(id) = pending.pop() {
            if boundary.contains(&id.parse()?, GraphRole::Commit) || result.contains(&id) {
                continue;
            }
            boundary.budget.admit(result.len(), 1)?;
            result.insert(id.clone());
            let commit = self
                .read_commit_object(&id.parse()?)?
                .ok_or_else(|| RepoErr::CommitNotFound(id.clone()))?;
            boundary.budget.admit(pending.len(), commit.parents.len())?;
            pending.extend(commit.parents.into_iter().map(|parent| parent.to_string()));
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypted_graph_work_admits_boundary_and_rejects_growth_before_allocation() {
        let budget = GraphBudget(true);
        assert!(budget.admit(262_143, 1).is_ok());
        assert!(budget.admit(262_144, 1).is_err());
        assert!(budget.admit(1, 262_144).is_err());
        assert!(budget.admit(usize::MAX, 1).is_err());
        assert!(GraphBudget(false).admit(usize::MAX, 1).is_ok());
    }

    #[test]
    fn encrypted_ancestry_rejects_excess_pending_parents_before_traversal() {
        let directory = tempfile::tempdir().unwrap();
        let repo = Repository::init(directory.path()).unwrap();
        let path = directory.path().join("file.txt");
        fs::write(&path, b"base").unwrap();
        repo.stage_artifact_path(&path).unwrap();
        let first = repo.commit_staged("base").unwrap();
        let mut commit = repo
            .read_commit_object(&first.id.parse().unwrap())
            .unwrap()
            .unwrap();
        commit.parents = vec![first.id.parse().unwrap(); 262_145];
        let head = repo
            .object_store()
            .write(&object::Object::Commit(commit))
            .unwrap();
        let encrypted = FetchBoundary {
            budget: GraphBudget(true),
            ..FetchBoundary::default()
        };
        let error = repo
            .fetched_commit_ancestors(&head.to_string(), &encrypted)
            .unwrap_err();
        #[cfg(all(feature = "e2ee", not(target_arch = "wasm32")))]
        assert!(matches!(
            error,
            RepoErr::Remote(RemoteErr::Encryption(
                crate::e2ee::EncryptedHttpError::Encryption(crate::e2ee::EncryptionError::TooLarge)
            ))
        ));
        #[cfg(not(all(feature = "e2ee", not(target_arch = "wasm32"))))]
        assert!(matches!(error, RepoErr::InvalidRemoteObject { .. }));
        assert_eq!(
            repo.fetched_commit_ancestors(&head.to_string(), &FetchBoundary::default())
                .unwrap()
                .len(),
            2
        );
    }
}
