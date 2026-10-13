use super::*;
use sha2::{Digest, Sha256};
use std::io::Read;

// The payload budget bounds raw bytes. These separate discovery limits bound
// the compact identity/graph inventory without retaining every object's bytes.
const MAX_PUSH_OBJECTS: usize = 100_000;
const MAX_PUSH_PENDING: usize = 100_000;

fn too_large() -> RepoErr {
    RemoteErr::from(crate::e2ee::EncryptionError::TooLarge).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypted_push_canonical_reader_rejects_oversized_objects_before_decode() {
        let directory = tempfile::tempdir().unwrap();
        let repo = Repository::init(directory.path()).unwrap();
        let bytes = vec![b'x'; 1024 * 1024];
        let id = object::ObjectId::for_bytes(&bytes);
        let path = repo.object_store().path_for(&id);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
        assert!(matches!(
            repo.read_bounded_push_object(&id, 64 * 1024),
            Err(RepoErr::Remote(RemoteErr::Encryption(
                crate::e2ee::EncryptedHttpError::Encryption(crate::e2ee::EncryptionError::TooLarge)
            )))
        ));
    }
}

#[derive(Default)]
struct PushInventory {
    objects: BTreeSet<object::ObjectId>,
    external: BTreeMap<object::ObjectId, u64>,
    commits: usize,
}

impl Repository {
    pub(super) fn push_encrypted_commit_chain(
        &self,
        remote: &crate::remote::Remote,
        head: &str,
        stop_at: Option<&str>,
    ) -> Result<remote_objects::PreparedObjectPush> {
        remote.with_encrypted_operation(|| {
            let discovery_trace = crate::trace::PushTraceSpan::new("encrypted_push_discovery");
            let maximum = remote.buffered_read_limit().ok_or_else(too_large)?;
            let object_limit = maximum
                .checked_sub(REMOTE_OBJECT_PACK_MAGIC.len())
                .filter(|limit| *limit > 0)
                .ok_or_else(too_large)?;
            let head = object::ObjectId::from_str(head)?;
            if !matches!(
                self.read_bounded_push_object(&head, object_limit)?.1,
                object::Object::Commit(_)
            ) {
                return Err(RepoErr::CommitNotFound(head.to_string()));
            }
            let stop = stop_at.map(object::ObjectId::from_str).transpose()?;
            let inventory_trace =
                crate::trace::PushTraceSpan::new("encrypted_push_remote_inventory");
            let remote_objects = if let Some(stop) = stop
                .as_ref()
                .filter(|id| self.object_store().path_for(id).exists())
            {
                self.advertised_encrypted_push_objects(stop, object_limit)?
            } else if stop.is_none() {
                self.remote_object_ids(remote)?
            } else {
                BTreeSet::new()
            };
            inventory_trace.finish(&[]);
            // The authenticated remote head is an authoritative traversal
            // boundary. Do not rescan its entire ancestry for each small push.
            // An older merge parent outside this boundary can be republished
            // conservatively; untrusted routing hints never authorize skipping it.
            let stops = stop.into_iter().collect();
            let graph_trace = crate::trace::PushTraceSpan::new("encrypted_push_local_graph");
            let PushInventory { objects: ids, external, commits: count } =
                self.discover_encrypted_push_objects(head, &remote_objects, &stops, object_limit)?;
            graph_trace.finish(&[]);
            discovery_trace.finish(&[]);
            let probe_trace = crate::trace::PushTraceSpan::new("encrypted_payload_existence_probe");
            let external = self.missing_remote_large_file_payloads(remote, external)?;
            probe_trace.finish(&[]);
            for (id, size) in external {
                self.publish_encrypted_file_payload(remote, &id, size)?;
            }
            // Re-read canonical objects by identity. Each completed batch is
            // published and dropped before constructing another batch.
            let pack_trace = crate::trace::PushTraceSpan::new("encrypted_push_pack_prepare");
            let mut objects = BTreeMap::new();
            let mut commits = Vec::new();
            let mut size = REMOTE_OBJECT_PACK_MAGIC.len();
            for id in ids {
                let (bytes, value) = self.read_bounded_push_object(&id, object_limit)?;
                if !objects.is_empty() && (objects.len() == 512 || bytes.len() > maximum - size) {
                    let pack = self
                        .prepare_object_pack(
                            std::mem::take(&mut objects),
                            std::mem::take(&mut commits),
                        )?
                        .ok_or_else(too_large)?;
                    block_on_remote(remote.publish_encrypted_object_pack(&pack))?;
                    size = REMOTE_OBJECT_PACK_MAGIC.len();
                }
                size += bytes.len();
                if let object::Object::Commit(commit) = value {
                    commits
                        .push(RemoteObjectPackCommit { id: id.clone(), parents: commit.parents });
                }
                objects.insert(id, bytes);
            }
            let pack = self.prepare_object_pack(objects, commits)?;
            pack_trace.finish(&[]);
            Ok(remote_objects::PreparedObjectPush {
                commits: count,
                pack,
                bundle_objects: vec![],
            })
        })
    }

    fn advertised_encrypted_push_objects(
        &self,
        head: &object::ObjectId,
        maximum: usize,
    ) -> Result<BTreeSet<object::ObjectId>> {
        let (_, object::Object::Commit(commit)) = self.read_bounded_push_object(head, maximum)?
        else {
            return Err(RepoErr::CommitNotFound(head.to_string()));
        };
        let (_, object::Object::Tree(tree)) =
            self.read_bounded_push_object(&commit.tree, maximum)?
        else {
            return Err(RemoteErr::from(crate::e2ee::EncryptionError::InvalidEncoding).into());
        };
        let mut ids = BTreeSet::from([head.clone(), commit.tree]);
        // The verified commit/tree already bind these immutable IDs. Reading
        // old blob contents again would turn a cached update into a full scan.
        ids.extend(tree.entries.into_iter().map(|entry| entry.oid));
        if ids.len() > MAX_PUSH_OBJECTS {
            return Err(too_large());
        }
        Ok(ids)
    }

    fn discover_encrypted_push_objects(
        &self,
        root: object::ObjectId,
        remote_objects: &BTreeSet<object::ObjectId>,
        stops: &BTreeSet<object::ObjectId>,
        maximum: usize,
    ) -> Result<PushInventory> {
        let mut pending = vec![root];
        let mut result = PushInventory::default();
        while let Some(id) = pending.pop() {
            cancellation_checkpoint()?;
            if remote_objects.contains(&id)
                || stops.contains(&id)
                || !result.objects.insert(id.clone())
            {
                continue;
            }
            if result.objects.len() > MAX_PUSH_OBJECTS {
                return Err(too_large());
            }
            let (_, value) = self.read_bounded_push_object(&id, maximum)?;
            match value {
                object::Object::Commit(commit) => {
                    result.commits += 1;
                    pending.push(commit.tree);
                    pending.extend(commit.parents);
                }
                object::Object::Tree(tree) => {
                    pending.extend(tree.entries.into_iter().map(|entry| entry.oid))
                }
                object::Object::Blob(object::BlobObject::LargeFilePointer(pointer)) => {
                    if result
                        .external
                        .insert(pointer.content_hash, pointer.size)
                        .is_some_and(|size| size != pointer.size)
                    {
                        return Err(RepoErr::Object(object::ObjectErr::InvalidObject {
                            kind: "large-file-pointer",
                            message: "same content hash referenced with different sizes".into(),
                        }));
                    }
                }
                object::Object::Blob(_) | object::Object::Tag(_) => {}
            }
            if pending.len() > MAX_PUSH_PENDING {
                return Err(too_large());
            }
        }
        Ok(result)
    }

    fn read_bounded_push_object(
        &self,
        id: &object::ObjectId,
        maximum: usize,
    ) -> Result<(Vec<u8>, object::Object)> {
        cancellation_checkpoint()?;
        let mut bytes = Vec::new();
        let limit = (maximum as u64).checked_add(1).ok_or_else(too_large)?;
        fs::File::open(self.object_store().path_for(id))?
            .take(limit)
            .read_to_end(&mut bytes)?;
        if bytes.len() > maximum {
            return Err(too_large());
        }
        let actual = object::ObjectId::for_bytes(&bytes);
        if actual != *id {
            return Err(
                object::ObjectErr::ObjectIdMismatch { expected: id.clone(), actual }.into(),
            );
        }
        let value = object::Object::decode(&bytes)?;
        if value.canonical_bytes() != bytes {
            return Err(RemoteErr::from(crate::e2ee::EncryptionError::InvalidEncoding).into());
        }
        Ok((bytes, value))
    }

    fn publish_encrypted_file_payload(
        &self,
        remote: &crate::remote::Remote,
        id: &object::ObjectId,
        size: u64,
    ) -> Result<()> {
        let trace = crate::trace::PushTraceSpan::new("encrypted_payload_canonical_verify");
        let digest = self.verify_encrypted_file_payload(id, size)?;
        trace.finish(&[("plaintext_bytes", size)]);
        block_on_remote(remote.publish_verified_file(
            &large_file_content_relative_path(id),
            &self.large_file_content_path(id),
            size,
            digest,
        ))?;
        Ok(())
    }

    pub(super) fn verify_encrypted_file_payload(
        &self,
        id: &object::ObjectId,
        size: u64,
    ) -> Result<[u8; 32]> {
        let detail_trace = crate::trace::PushTraceSpan::new("encrypted_payload_verify_detail");
        let trace_enabled = crate::trace::enabled();
        let mut read_time = std::time::Duration::ZERO;
        let mut read_max = std::time::Duration::ZERO;
        let mut hash_time = std::time::Duration::ZERO;
        let mut hash_max = std::time::Duration::ZERO;
        let path = self.large_file_content_path(id);
        let mut file = fs::File::open(&path)?;
        let mut hash = blake3::Hasher::new();
        let mut sha = Sha256::new();
        let mut remaining = size;
        let mut buffer = [0_u8; 64 * 1024];
        while remaining > 0 {
            cancellation_checkpoint()?;
            let length = remaining.min(buffer.len() as u64) as usize;
            let started = trace_enabled.then(std::time::Instant::now);
            let read = file.read(&mut buffer[..length])?;
            if let Some(started) = started {
                let elapsed = started.elapsed();
                read_time += elapsed;
                read_max = read_max.max(elapsed);
            }
            if read == 0 {
                break;
            }
            let started = trace_enabled.then(std::time::Instant::now);
            hash.update(&buffer[..read]);
            sha.update(&buffer[..read]);
            if let Some(started) = started {
                let elapsed = started.elapsed();
                hash_time += elapsed;
                hash_max = hash_max.max(elapsed);
            }
            remaining -= read as u64;
        }
        if remaining != 0 || file.read(&mut buffer[..1])? != 0 || file.metadata()?.len() != size {
            return Err(RemoteErr::from(crate::e2ee::EncryptionError::SnapshotMismatch).into());
        }
        let actual = object::ObjectId::from_str(hash.finalize().to_hex().as_str())?;
        if actual != *id {
            return Err(
                object::ObjectErr::ObjectIdMismatch { expected: id.clone(), actual }.into(),
            );
        }
        let digest = sha.finalize().into();
        let micros =
            |duration: std::time::Duration| u64::try_from(duration.as_micros()).unwrap_or(u64::MAX);
        detail_trace.finish(&[
            ("read_sync_us", micros(read_time)),
            ("read_sync_max_us", micros(read_max)),
            ("hash_sync_us", micros(hash_time)),
            ("hash_sync_max_us", micros(hash_max)),
        ]);
        Ok(digest)
    }
}
