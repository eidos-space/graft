use super::*;

impl Repository {
    pub fn read_object(&self, id: &str) -> Result<object::Object> {
        let id = object::ObjectId::from_str(id)?;
        Ok(self.object_store().read(&id)?)
    }

    pub fn remote_store(&self, remote: &str) -> Result<crate::remote::Remote> {
        validate_remote_name(remote)?;
        let config = self
            .config()?
            .remotes
            .get(remote)
            .cloned()
            .ok_or_else(|| RepoErr::RemoteNotFound(remote.to_string()))?;
        Ok(config.build()?)
    }

    pub(super) fn remote_branch_refs_from_store(
        &self,
        remote: &str,
        remote_store: &crate::remote::Remote,
    ) -> Result<Vec<RemoteBranchRef>> {
        validate_remote_name(remote)?;
        let prefix = "refs/heads/";
        let mut refs = BTreeMap::<String, String>::new();
        let mut paths = Vec::new();
        for path in block_on_remote(remote_store.list_raw(prefix))? {
            if path == prefix || path.ends_with('/') {
                continue;
            }
            let Some(branch) = path.strip_prefix(prefix) else {
                continue;
            };
            validate_ref_name(branch)?;
            let branch = branch.to_string();
            paths.push((path, branch));
        }

        let remote_refs = block_on_remote(async {
            stream::iter(paths)
                .map(|(path, branch)| async move {
                    let bytes = remote_store.get_raw(&path).await?;
                    Ok::<_, RemoteErr>((path, branch, bytes))
                })
                .buffer_unordered(REMOTE_REF_READ_CONCURRENCY)
                .try_collect::<Vec<_>>()
                .await
        })?;

        for (path, branch, bytes) in remote_refs {
            let Some(bytes) = bytes else {
                continue;
            };
            refs.insert(branch, parse_remote_ref(&path, bytes)?);
        }

        Ok(refs
            .into_iter()
            .map(|(branch, head)| RemoteBranchRef { remote: remote.to_string(), branch, head })
            .collect())
    }

    pub(super) fn set_remote_head_if_absent(
        &self,
        remote_store: &crate::remote::Remote,
        branch: &str,
    ) -> Result<()> {
        if branch != self.default_branch()? {
            return Ok(());
        }

        match block_on_remote(
            remote_store.put_raw_if_not_exists(HEAD_FILE, Head::branch(branch).serialize()),
        ) {
            Ok(()) => Ok(()),
            Err(RepoErr::Remote(err)) if err.precondition_failed() => Ok(()),
            Err(err) => Err(err),
        }
    }

    pub(super) fn remote_object_ids(
        &self,
        remote: &crate::remote::Remote,
    ) -> Result<BTreeSet<object::ObjectId>> {
        let mut objects = BTreeSet::new();
        for path in block_on_remote(remote.list_raw(DIR_OBJECTS))? {
            if let Some(id) = remote_loose_object_id(&path)? {
                objects.insert(id);
            }
        }

        for index in fetch_remote_object_pack_indexes(remote)? {
            for entry in index.objects {
                objects.insert(entry.id);
            }
        }

        Ok(objects)
    }

    pub(super) fn fetch_packed_object_bytes(
        &self,
        remote: &crate::remote::Remote,
        id: &object::ObjectId,
        pack_cache: &mut RemoteObjectPackCache,
    ) -> Result<Bytes> {
        let hit = pack_cache.indexes(remote)?.iter().find_map(|index| {
            index
                .objects
                .iter()
                .find(|entry| &entry.id == id)
                .map(|entry| (index.pack.clone(), entry.offset, entry.len))
        });
        let Some((pack, offset, len)) = hit else {
            return Err(RepoErr::InvalidRemoteObject {
                path: object::LooseObjectStore::relative_path(id),
                message: "missing object".to_string(),
            });
        };
        let end = offset
            .checked_add(len)
            .ok_or_else(|| RepoErr::InvalidRemoteObject {
                path: pack.clone(),
                message: format!("pack entry for object {id} overflows u64 range"),
            })?;
        let pack_bytes = pack_cache.pack_bytes(remote, &pack)?;
        let offset = usize::try_from(offset).map_err(|_| RepoErr::InvalidRemoteObject {
            path: pack.clone(),
            message: format!("pack entry for object {id} offset does not fit in usize"),
        })?;
        let end = usize::try_from(end).map_err(|_| RepoErr::InvalidRemoteObject {
            path: pack.clone(),
            message: format!("pack entry for object {id} end does not fit in usize"),
        })?;
        if end > pack_bytes.len() {
            return Err(RepoErr::InvalidRemoteObject {
                path: pack,
                message: format!("pack entry for object {id} extends past pack length"),
            });
        }
        Ok(pack_bytes.slice(offset..end))
    }

    pub(super) fn fetch_commit_chain(
        &self,
        remote: &crate::remote::Remote,
        head: &str,
    ) -> Result<usize> {
        let mut count = 0;
        let mut stack = vec![head.to_string()];
        let mut seen = BTreeMap::<String, ()>::new();
        let mut pack_cache = RemoteObjectPackCache::default();
        while let Some(id) = stack.pop() {
            if seen.insert(id.clone(), ()).is_some() {
                continue;
            }

            let object_id = object::ObjectId::from_str(&id)?;
            let commit = match self.read_commit_object(&object_id)? {
                Some(commit) => commit,
                None => {
                    let object = self.fetch_remote_object(remote, &object_id, &mut pack_cache)?;
                    let object::Object::Commit(commit) = object else {
                        return Err(RepoErr::InvalidRemoteObject {
                            path: object::LooseObjectStore::relative_path(&object_id),
                            message: "expected commit object".to_string(),
                        });
                    };
                    count += 1;
                    commit
                }
            };

            self.fetch_object_graph(remote, &commit.tree, &mut pack_cache)?;
            for parent in commit.parents {
                stack.push(parent.to_string());
            }
        }
        Ok(count)
    }

    pub(super) fn push_commit_chain(
        &self,
        remote: &crate::remote::Remote,
        head: &str,
        stop_at: Option<&str>,
    ) -> Result<usize> {
        let remote_objects = if stop_at.is_some() {
            BTreeSet::new()
        } else {
            self.remote_object_ids(remote)?
        };
        let stop_commits = stop_at
            .map(|id| self.commit_ancestors_inclusive(id))
            .transpose()?
            .unwrap_or_default();
        let mut commits = Vec::new();
        let mut objects = BTreeMap::<object::ObjectId, Vec<u8>>::new();
        let mut external_payloads = BTreeMap::<object::ObjectId, u64>::new();
        let mut stack = vec![head.to_string()];
        let mut seen = BTreeMap::<String, ()>::new();
        while let Some(id) = stack.pop() {
            if seen.insert(id.clone(), ()).is_some() {
                continue;
            }
            if stop_commits.contains(&id) {
                continue;
            }
            let object_id = object::ObjectId::from_str(&id)?;
            if remote_objects.contains(&object_id) {
                continue;
            }

            let Some(bytes) = self.object_store().read_raw(&object_id)? else {
                return Err(RepoErr::CommitNotFound(id.clone()));
            };
            let object = object::Object::decode(&bytes)?;
            let actual = object::ObjectId::for_bytes(&bytes);
            if actual != object_id {
                return Err(RepoErr::Object(object::ObjectErr::ObjectIdMismatch {
                    expected: object_id,
                    actual,
                }));
            }
            let object::Object::Commit(commit) = object else {
                return Err(RepoErr::Object(object::ObjectErr::InvalidObject {
                    kind: "commit",
                    message: format!("object {id} is not a commit"),
                }));
            };
            let commit_id = object::ObjectId::from_str(&id)?;
            objects.insert(commit_id.clone(), bytes);
            self.collect_object_graph_for_pack(
                &commit.tree,
                &remote_objects,
                &mut objects,
                &mut external_payloads,
            )?;
            for parent in &commit.parents {
                stack.push(parent.to_string());
            }
            commits.push(commit_id);
        }

        let count = commits.len();
        self.push_large_file_contents(remote, external_payloads)?;
        self.push_object_pack(remote, objects)?;
        Ok(count)
    }

    pub(super) fn commit_ancestors_inclusive(&self, head: &str) -> Result<BTreeSet<String>> {
        let mut ancestors = BTreeSet::new();
        let mut stack = vec![head.to_string()];
        while let Some(id) = stack.pop() {
            if !ancestors.insert(id.clone()) {
                continue;
            }
            let commit = match self.read_commit(&id) {
                Ok(commit) => commit,
                Err(RepoErr::CommitNotFound(_)) => continue,
                Err(err) => return Err(err),
            };
            for parent in commit_parent_ids(&commit) {
                stack.push(parent);
            }
        }
        Ok(ancestors)
    }

    pub(super) fn collect_object_graph_for_pack(
        &self,
        id: &object::ObjectId,
        remote_objects: &BTreeSet<object::ObjectId>,
        objects: &mut BTreeMap<object::ObjectId, Vec<u8>>,
        external_payloads: &mut BTreeMap<object::ObjectId, u64>,
    ) -> Result<()> {
        if remote_objects.contains(id) || objects.contains_key(id) {
            return Ok(());
        }

        let Some(bytes) = self.object_store().read_raw(id)? else {
            return Err(RepoErr::Object(object::ObjectErr::InvalidObject {
                kind: "object",
                message: format!("missing local object {id}"),
            }));
        };
        let object = object::Object::decode(&bytes)?;
        let actual = object::ObjectId::for_bytes(&bytes);
        if actual != *id {
            return Err(RepoErr::Object(object::ObjectErr::ObjectIdMismatch {
                expected: id.clone(),
                actual,
            }));
        }

        objects.insert(id.clone(), bytes);
        match object {
            object::Object::Commit(commit) => {
                self.collect_object_graph_for_pack(
                    &commit.tree,
                    remote_objects,
                    objects,
                    external_payloads,
                )?;
            }
            object::Object::Tree(tree) => {
                for entry in tree.entries {
                    self.collect_object_graph_for_pack(
                        &entry.oid,
                        remote_objects,
                        objects,
                        external_payloads,
                    )?;
                }
            }
            object::Object::Blob(object::BlobObject::LargeFilePointer(pointer)) => {
                match external_payloads.insert(pointer.content_hash, pointer.size) {
                    Some(size) if size != pointer.size => {
                        return Err(RepoErr::Object(object::ObjectErr::InvalidObject {
                            kind: "large-file-pointer",
                            message: "same content hash referenced with different sizes"
                                .to_string(),
                        }));
                    }
                    _ => {}
                }
            }
            object::Object::Blob(_) | object::Object::Tag(_) => {}
        }
        Ok(())
    }

    pub(super) fn push_large_file_contents(
        &self,
        remote: &crate::remote::Remote,
        external_payloads: BTreeMap<object::ObjectId, u64>,
    ) -> Result<()> {
        for (id, size) in external_payloads {
            let bytes = self.read_large_file_content(&id, size)?;
            let path = large_file_content_relative_path(&id);
            match block_on_remote(remote.put_raw_if_not_exists(&path, bytes)) {
                Ok(()) => {}
                Err(RepoErr::Remote(err)) if err.precondition_failed() => {}
                Err(err) => return Err(err),
            }
        }
        Ok(())
    }

    pub(super) fn push_object_pack(
        &self,
        remote: &crate::remote::Remote,
        objects: BTreeMap<object::ObjectId, Vec<u8>>,
    ) -> Result<()> {
        if objects.is_empty() {
            return Ok(());
        }

        let mut pack = REMOTE_OBJECT_PACK_MAGIC.to_vec();
        let mut entries = Vec::with_capacity(objects.len());
        for (id, bytes) in objects {
            let offset = pack.len() as u64;
            let len = bytes.len() as u64;
            pack.extend_from_slice(&bytes);
            entries.push(RemoteObjectPackEntry { id, offset, len });
        }

        let pack_id = blake3::hash(&pack).to_hex().to_string();
        let pack_path = format!("{DIR_OBJECTS_PACK}/{pack_id}.pack");
        let index_path = format!("{DIR_OBJECTS_PACK}/{pack_id}.idx");
        let index = RemoteObjectPackIndex {
            version: REMOTE_OBJECT_PACK_VERSION,
            pack: pack_path.clone(),
            objects: entries,
        };
        let index_bytes =
            serde_json::to_vec(&index).map_err(|err| RepoErr::InvalidRemoteObject {
                path: index_path.clone(),
                message: format!("failed to encode pack index: {err}"),
            })?;

        match block_on_remote(remote.put_raw_if_not_exists(&pack_path, pack)) {
            Ok(()) => {}
            Err(RepoErr::Remote(err)) if err.precondition_failed() => {}
            Err(err) => return Err(err),
        }
        match block_on_remote(remote.put_raw_if_not_exists(&index_path, index_bytes)) {
            Ok(()) => {}
            Err(RepoErr::Remote(err)) if err.precondition_failed() => {}
            Err(err) => return Err(err),
        }
        Ok(())
    }

    pub(super) fn fetch_object_graph(
        &self,
        remote: &crate::remote::Remote,
        id: &object::ObjectId,
        pack_cache: &mut RemoteObjectPackCache,
    ) -> Result<()> {
        let object = match self.object_store().read_raw(id)? {
            Some(bytes) => {
                let object = object::Object::decode(&bytes)?;
                let actual = object::ObjectId::for_bytes(&bytes);
                if actual != *id {
                    return Err(RepoErr::Object(object::ObjectErr::ObjectIdMismatch {
                        expected: id.clone(),
                        actual,
                    }));
                }
                object
            }
            None => self.fetch_remote_object(remote, id, pack_cache)?,
        };

        match object {
            object::Object::Commit(commit) => {
                self.fetch_object_graph(remote, &commit.tree, pack_cache)?;
                for parent in commit.parents {
                    self.fetch_object_graph(remote, &parent, pack_cache)?;
                }
            }
            object::Object::Tree(tree) => {
                for entry in tree.entries {
                    self.fetch_object_graph(remote, &entry.oid, pack_cache)?;
                }
            }
            object::Object::Blob(object::BlobObject::LargeFilePointer(pointer)) => {
                self.fetch_large_file_content(remote, &pointer.content_hash, pointer.size)?;
            }
            object::Object::Blob(_) | object::Object::Tag(_) => {}
        }
        Ok(())
    }

    pub(super) fn fetch_remote_object(
        &self,
        remote: &crate::remote::Remote,
        id: &object::ObjectId,
        pack_cache: &mut RemoteObjectPackCache,
    ) -> Result<object::Object> {
        let path = object::LooseObjectStore::relative_path(id);
        let bytes = match block_on_remote(remote.get_raw(&path))? {
            Some(bytes) => bytes,
            None => self.fetch_packed_object_bytes(remote, id, pack_cache)?,
        };
        Ok(self.object_store().write_raw_validated(id, &bytes)?)
    }

    pub(super) fn fetch_large_file_content(
        &self,
        remote: &crate::remote::Remote,
        id: &object::ObjectId,
        size: u64,
    ) -> Result<()> {
        if self.large_file_content_path(id).exists() {
            self.read_large_file_content(id, size)?;
            return Ok(());
        }

        let path = large_file_content_relative_path(id);
        let bytes = block_on_remote(remote.get_raw(&path))?.ok_or_else(|| {
            RepoErr::InvalidRemoteObject {
                path: path.clone(),
                message: format!("missing external payload {id}"),
            }
        })?;
        let bytes = bytes.to_vec();
        validate_large_file_content(id, size, &bytes)?;
        self.write_large_file_content(id, &bytes)?;
        Ok(())
    }

    pub(super) fn repair_artifact_state_from_remote(
        &self,
        remote: &crate::remote::Remote,
        state: &CommitArtifactState,
        pack_cache: &mut RemoteObjectPackCache,
        fetched_objects: &mut BTreeSet<object::ObjectId>,
        fetched_external_payloads: &mut BTreeSet<object::ObjectId>,
    ) -> Result<()> {
        let oid = state.oid();
        let missing_object = match self.object_store().read(oid) {
            Ok(_) => false,
            Err(object::ObjectErr::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => true,
            Err(_) => false,
        };
        if missing_object {
            let object = self.fetch_remote_object(remote, oid, pack_cache)?;
            validate_artifact_object_matches_state(state, &object)?;
            fetched_objects.insert(oid.clone());
        }

        if let CommitArtifactState::LargeFile { content_hash, size, .. } = state
            && !self.large_file_content_path(content_hash).exists()
        {
            self.fetch_large_file_content(remote, content_hash, *size)?;
            fetched_external_payloads.insert(content_hash.clone());
        }

        Ok(())
    }

    pub(super) fn referenced_large_file_payloads(&self) -> Result<BTreeSet<object::ObjectId>> {
        let mut referenced = BTreeSet::new();
        for entry in self.read_index()?.entries {
            if let Some(artifact) = entry.artifact {
                collect_large_file_payload_from_artifact(&artifact, &mut referenced);
            }
        }

        let mut starts = BTreeSet::new();
        if let Some(head) = self.head_target()? {
            starts.insert(head);
        }
        for branch in self.branches()? {
            if let Some(target) = branch.target {
                starts.insert(target);
            }
        }
        for branch in self.remote_tracking_branches()? {
            starts.insert(branch.head);
        }
        for tag in self.tags()? {
            starts.insert(tag.target);
        }

        let mut seen = BTreeSet::new();
        for start in starts {
            self.collect_reachable_large_file_payloads(&start, &mut seen, &mut referenced)?;
        }
        Ok(referenced)
    }

    pub(super) fn collect_reachable_large_file_payloads(
        &self,
        start: &str,
        seen: &mut BTreeSet<String>,
        referenced: &mut BTreeSet<object::ObjectId>,
    ) -> Result<()> {
        let mut stack = vec![start.to_string()];
        while let Some(id) = stack.pop() {
            if !seen.insert(id.clone()) {
                continue;
            }
            let commit = self.read_commit(&id)?;
            for artifact in commit.artifacts.values() {
                collect_large_file_payload_from_artifact(artifact, referenced);
            }
            for parent in commit_parent_ids(&commit) {
                stack.push(parent);
            }
        }
        Ok(())
    }

    pub(super) fn local_large_file_payloads(&self) -> Result<Vec<RepoLargeFilePruneEntry>> {
        let root = self.file_store_dir();
        if !root.exists() {
            return Ok(Vec::new());
        }

        let mut files = Vec::new();
        for dir in fs::read_dir(&root)? {
            let dir = dir?;
            if !dir.file_type()?.is_dir() {
                continue;
            }
            let fanout = dir.file_name().to_string_lossy().into_owned();
            if fanout.len() != 2 || !fanout.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                continue;
            }

            for file in fs::read_dir(dir.path())? {
                let file = file?;
                if !file.file_type()?.is_file() {
                    continue;
                }
                let suffix = file.file_name().to_string_lossy().into_owned();
                if suffix.len() != 62 || !suffix.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    continue;
                }
                let content_hash = object::ObjectId::from_str(&format!("{fanout}{suffix}"))?;
                let size = file.metadata()?.len();
                let path = large_file_content_relative_path(&content_hash);
                files.push(RepoLargeFilePruneEntry { content_hash, size, path });
            }
        }
        Ok(files)
    }
}
