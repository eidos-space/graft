use super::*;
use crate::e2ee::{CommitTopology, EncryptionError};

/// Verify a staged aggregate without retaining its complete plaintext pack.
pub(crate) fn verified_pack_file_topology(
    id: &str,
    pack: &mut std::fs::File,
    index_file: &mut std::fs::File,
    maximum_object: usize,
) -> crate::e2ee::Result<Vec<CommitTopology>> {
    use std::io::{Read, Seek, SeekFrom};
    let invalid = || EncryptionError::InvalidEncoding;
    pack.rewind().map_err(|_| invalid())?;
    let mut hasher = blake3::Hasher::new();
    std::io::copy(pack, &mut hasher).map_err(|_| invalid())?;
    if hasher.finalize().to_hex().as_str() != id {
        return Err(invalid());
    }
    pack.rewind().map_err(|_| invalid())?;
    let mut magic = vec![0; REMOTE_OBJECT_PACK_MAGIC.len()];
    pack.read_exact(&mut magic).map_err(|_| invalid())?;
    if magic != REMOTE_OBJECT_PACK_MAGIC {
        return Err(invalid());
    }
    index_file.rewind().map_err(|_| invalid())?;
    if index_file.metadata().map_err(|_| invalid())?.len() > maximum_object as u64 {
        return Err(EncryptionError::TooLarge);
    }
    // serde_json does not buffer a Reader; a File here performs a read syscall
    // for each byte of the index. Keep staged parsing bounded without paying
    // that cost for every commit in a long history.
    let index: RemoteObjectPackIndex =
        serde_json::from_reader(std::io::BufReader::new(index_file)).map_err(|_| invalid())?;
    let mut index =
        validate_remote_object_pack_index(&format!("{DIR_OBJECTS_PACK}/{id}.idx"), index)
            .map_err(|_| invalid())?;
    if index.pack != format!("{DIR_OBJECTS_PACK}/{id}.pack") {
        return Err(invalid());
    }
    index.objects.sort_by_key(|entry| entry.offset);
    let mut cursor = REMOTE_OBJECT_PACK_MAGIC.len() as u64;
    let mut commits = BTreeMap::new();
    for entry in index.objects {
        if entry.offset != cursor {
            return Err(invalid());
        }
        let length = usize::try_from(entry.len).map_err(|_| invalid())?;
        if length > maximum_object {
            return Err(EncryptionError::TooLarge);
        }
        let mut bytes = vec![0; length];
        pack.read_exact(&mut bytes).map_err(|_| invalid())?;
        let object = object::Object::decode(&bytes).map_err(|_| invalid())?;
        if object::ObjectId::for_bytes(&bytes) != entry.id || object.canonical_bytes() != bytes {
            return Err(invalid());
        }
        if let object::Object::Commit(commit) = object {
            commits.insert(entry.id, commit.parents);
        }
        cursor = cursor.checked_add(entry.len).ok_or_else(invalid)?;
    }
    if pack.seek(SeekFrom::End(0)).map_err(|_| invalid())? != cursor {
        return Err(invalid());
    }
    for advertised in index.commits {
        if commits.get(&advertised.id) != Some(&advertised.parents) {
            return Err(invalid());
        }
    }
    pack.rewind().map_err(|_| invalid())?;
    let raw_id = |id: &object::ObjectId| {
        blake3::Hash::from_hex(id.as_str())
            .map(|hash| *hash.as_bytes())
            .map_err(|_| invalid())
    };
    commits
        .into_iter()
        .map(|(id, parents)| {
            Ok(CommitTopology {
                commit: raw_id(&id)?,
                parents: parents
                    .iter()
                    .map(raw_id)
                    .collect::<crate::e2ee::Result<_>>()?,
            })
        })
        .collect()
}

/// Split only after the complete input has passed canonical identity validation.
/// Pack layout is disposable; every object retains its authoritative identity.
pub(crate) fn split_verified_pack(
    id: &str,
    pack: &[u8],
    index_bytes: &[u8],
    maximum: usize,
) -> crate::e2ee::Result<Vec<crate::remote::RemoteObjectPack>> {
    verified_pack_topology(id, pack, index_bytes)?;
    let mut index: RemoteObjectPackIndex =
        serde_json::from_slice(index_bytes).map_err(|_| EncryptionError::InvalidEncoding)?;
    index.objects.sort_by_key(|entry| entry.offset);
    let mut output = Vec::new();
    let mut bytes = REMOTE_OBJECT_PACK_MAGIC.to_vec();
    let mut entries = Vec::new();
    let mut commits = Vec::new();
    let mut hint_length = crate::e2ee::hint::EMPTY_HINT_LENGTH;
    // The pack path is always the same length. Include the optional commits
    // property even for a pack with no commits so this remains an upper bound.
    let index_overhead = serde_json::to_vec(&RemoteObjectPackIndex {
        version: REMOTE_OBJECT_PACK_VERSION,
        pack: format!("{DIR_OBJECTS_PACK}/{id}.pack"),
        objects: vec![],
        commits: vec![],
    })
    .map_err(|_| EncryptionError::InvalidEncoding)?
    .len()
        + b",\"commits\":[]".len();
    let mut index_length = index_overhead;
    for entry in index.objects {
        let start = usize::try_from(entry.offset).map_err(|_| EncryptionError::TooLarge)?;
        let length = usize::try_from(entry.len).map_err(|_| EncryptionError::TooLarge)?;
        let raw = &pack[start..start + length]; // Coverage was checked above.
        let commit =
            match object::Object::decode(raw).map_err(|_| EncryptionError::InvalidEncoding)? {
                object::Object::Commit(commit) => Some(RemoteObjectPackCommit {
                    id: entry.id.clone(),
                    parents: commit.parents,
                }),
                _ => None,
            };
        let hint_entry_length = commit
            .as_ref()
            .map_or(Some(0), |commit| {
                crate::e2ee::hint::entry_length(commit.parents.len())
            })
            .ok_or(EncryptionError::TooLarge)?;
        // The original offsets are no smaller than the repacked offsets.
        let entry_index_length = serde_json::to_vec(&entry)
            .map_err(|_| EncryptionError::InvalidEncoding)?
            .len()
            + 1;
        let commit_index_length = commit
            .as_ref()
            .map(|commit| serde_json::to_vec(commit).map(|bytes| bytes.len() + 1))
            .transpose()
            .map_err(|_| EncryptionError::InvalidEncoding)?
            .unwrap_or(0);
        let next_index_length = entry_index_length
            .checked_add(commit_index_length)
            .ok_or(EncryptionError::TooLarge)?;
        if !entries.is_empty()
            && (bytes.len().saturating_add(length) > maximum
                || index_length.saturating_add(next_index_length) > maximum
                || (commit.is_some() && commits.len() == crate::e2ee::hint::MAX_ENTRIES)
                || hint_length.saturating_add(hint_entry_length) > crate::e2ee::hint::MAX_LENGTH)
        {
            output.push(finish_pack(bytes, entries, commits)?);
            bytes = REMOTE_OBJECT_PACK_MAGIC.to_vec();
            entries = Vec::new();
            commits = Vec::new();
            hint_length = crate::e2ee::hint::EMPTY_HINT_LENGTH;
            index_length = index_overhead;
        }
        entries.push(RemoteObjectPackEntry {
            id: entry.id,
            offset: bytes.len() as u64,
            len: entry.len,
        });
        bytes.extend_from_slice(raw);
        if let Some(commit) = commit {
            commits.push(commit);
        }
        hint_length = hint_length.saturating_add(hint_entry_length);
        index_length = index_length.saturating_add(next_index_length);
    }
    if !entries.is_empty() {
        output.push(finish_pack(bytes, entries, commits)?);
    }
    Ok(output)
}

fn finish_pack(
    pack: Vec<u8>,
    objects: Vec<RemoteObjectPackEntry>,
    mut commits: Vec<RemoteObjectPackCommit>,
) -> crate::e2ee::Result<crate::remote::RemoteObjectPack> {
    let id = blake3::hash(&pack).to_hex().to_string();
    commits.sort_by(|a, b| a.id.cmp(&b.id));
    let index = RemoteObjectPackIndex {
        version: REMOTE_OBJECT_PACK_VERSION,
        pack: format!("{DIR_OBJECTS_PACK}/{id}.pack"),
        objects,
        commits,
    };
    let index = serde_json::to_vec(&index).map_err(|_| EncryptionError::InvalidEncoding)?;
    Ok(crate::remote::RemoteObjectPack::new(
        id,
        Bytes::from(pack),
        Bytes::from(index),
    ))
}

/// Recover routing facts from canonical objects, never from index advertisements.
/// Require complete pack coverage so no hidden commit can escape hint generation.
pub(crate) fn verified_pack_topology(
    id: &str,
    pack: &[u8],
    index_bytes: &[u8],
) -> crate::e2ee::Result<Vec<CommitTopology>> {
    let invalid = || EncryptionError::InvalidEncoding;
    if !pack.starts_with(REMOTE_OBJECT_PACK_MAGIC) || blake3::hash(pack).to_hex().as_str() != id {
        return Err(invalid());
    }
    let index_path = format!("{DIR_OBJECTS_PACK}/{id}.idx");
    let index = decode_remote_object_pack_index(&index_path, index_bytes).map_err(|_| invalid())?;
    if index.pack != format!("{DIR_OBJECTS_PACK}/{id}.pack") {
        return Err(invalid());
    }
    let mut entries = index.objects.iter().collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.offset);
    let mut cursor = REMOTE_OBJECT_PACK_MAGIC.len();
    let mut commits = BTreeMap::new();
    for entry in entries {
        let offset = usize::try_from(entry.offset).map_err(|_| invalid())?;
        let length = usize::try_from(entry.len).map_err(|_| invalid())?;
        let end = offset.checked_add(length).ok_or_else(invalid)?;
        if offset != cursor {
            return Err(invalid());
        }
        let raw = pack.get(offset..end).ok_or_else(invalid)?;
        let decoded = object::Object::decode(raw).map_err(|_| invalid())?;
        if object::ObjectId::for_bytes(raw) != entry.id || decoded.canonical_bytes() != raw {
            return Err(invalid());
        }
        if let object::Object::Commit(commit) = decoded {
            commits.insert(entry.id.clone(), commit.parents);
        }
        cursor = end;
    }
    if cursor != pack.len() {
        return Err(invalid());
    }
    // Older indexes may omit advertisements. Every present advertisement must
    // match the object, including parent order; omitted commits are recovered.
    for advertised in index.commits {
        if commits.get(&advertised.id) != Some(&advertised.parents) {
            return Err(invalid());
        }
    }
    let raw_id = |id: &object::ObjectId| {
        blake3::Hash::from_hex(id.as_str())
            .map(|hash| *hash.as_bytes())
            .map_err(|_| invalid())
    };
    commits
        .into_iter()
        .map(|(id, parents)| {
            Ok(CommitTopology {
                commit: raw_id(&id)?,
                parents: parents
                    .iter()
                    .map(raw_id)
                    .collect::<crate::e2ee::Result<_>>()?,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypted_pack_splits_long_and_merge_heavy_history_without_changing_objects() {
        for (count, parent_count) in [(4097, 1), (1000, 32)] {
            let directory = tempfile::tempdir().unwrap();
            let repo = Repository::init(directory.path()).unwrap();
            let tree = object::Object::Tree(object::TreeObject::new(vec![]).unwrap());
            let tree_id = object::ObjectId::for_bytes(&tree.canonical_bytes());
            let mut objects = BTreeMap::from([(tree_id.clone(), tree.canonical_bytes())]);
            for index in 0..count {
                let signature =
                    object::Signature::new("Test", "test@example.invalid", index, "+0000");
                let commit = object::Object::Commit(object::CommitObject {
                    tree: tree_id.clone(),
                    parents: (0..parent_count)
                        .map(|parent| object::ObjectId::for_bytes(&[parent]))
                        .collect(),
                    author: signature.clone(),
                    committer: signature,
                    repo_format_version: 1,
                    tables: vec![],
                    path_changes: None,
                    message: format!("Commit {index}"),
                });
                objects.insert(
                    object::ObjectId::for_bytes(&commit.canonical_bytes()),
                    commit.canonical_bytes(),
                );
            }
            let expected = objects.clone();
            let pack = repo.prepare_object_pack(objects, vec![]).unwrap().unwrap();
            let (id, bytes, index) = pack.encrypted_test_parts();
            let parts = split_verified_pack(id, bytes, index, 64 * 1024 * 1024).unwrap();
            assert_eq!(parts.len(), 2);
            let root =
                crate::e2ee::RootKey::generate(crate::e2ee::RepositoryId::generate().unwrap())
                    .unwrap();
            let mut actual = BTreeMap::new();
            let mut total_commits = 0;
            for part in parts {
                let (id, bytes, index_bytes) = part.encrypted_test_parts();
                let topology = verified_pack_topology(id, bytes, index_bytes).unwrap();
                total_commits += topology.len();
                let pack_key =
                    crate::e2ee::object::remote_key(&root, &format!("objects/pack/{id}.pack"))
                        .unwrap();
                let index_key =
                    crate::e2ee::object::remote_key(&root, &format!("objects/pack/{id}.idx"))
                        .unwrap();
                let hint =
                    crate::e2ee::RoutingHint::create(&root, &pack_key, &index_key, &topology)
                        .unwrap();
                let expected_length = crate::e2ee::hint::EMPTY_HINT_LENGTH
                    + topology
                        .iter()
                        .map(|entry| crate::e2ee::hint::entry_length(entry.parents.len()).unwrap())
                        .sum::<usize>();
                assert_eq!(hint.as_bytes().len(), expected_length);
                let index: RemoteObjectPackIndex = serde_json::from_slice(index_bytes).unwrap();
                for entry in index.objects {
                    let start = entry.offset as usize;
                    assert!(
                        actual
                            .insert(entry.id, bytes[start..start + entry.len as usize].to_vec())
                            .is_none()
                    );
                }
            }
            assert_eq!(total_commits, count as usize);
            assert_eq!(actual, expected);
        }
    }

    fn fixture() -> (String, Vec<u8>, RemoteObjectPackIndex, Vec<CommitTopology>) {
        let directory = tempfile::tempdir().unwrap();
        let repo = Repository::init(directory.path()).unwrap();
        repo.mark_dirty_path(directory.path().join("app.db"))
            .unwrap();
        let first = repo.commit("first").unwrap();
        repo.mark_dirty_path(directory.path().join("app.db"))
            .unwrap();
        let second = repo.commit("second").unwrap();
        let mut objects = BTreeMap::new();
        repo.collect_object_graph_for_pack(
            &object::ObjectId::from_str(&first.id).unwrap(),
            &BTreeSet::new(),
            &mut objects,
            &mut BTreeMap::new(),
        )
        .unwrap();
        repo.collect_object_graph_for_pack(
            &object::ObjectId::from_str(&second.id).unwrap(),
            &BTreeSet::new(),
            &mut objects,
            &mut BTreeMap::new(),
        )
        .unwrap();
        let pack = repo.prepare_object_pack(objects, vec![]).unwrap().unwrap();
        let (id, bytes, index) = pack.encrypted_test_parts();
        let topology = verified_pack_topology(id, bytes, index).unwrap();
        assert_eq!(topology.len(), 2);
        let child = topology
            .iter()
            .find(|entry| entry.commit == *blake3::Hash::from_hex(&second.id).unwrap().as_bytes())
            .unwrap();
        assert_eq!(
            child.parents,
            vec![*blake3::Hash::from_hex(&first.id).unwrap().as_bytes()]
        );
        (
            id.to_owned(),
            bytes.to_vec(),
            serde_json::from_slice(index).unwrap(),
            topology,
        )
    }

    fn verify(
        id: &str,
        pack: &[u8],
        index: &RemoteObjectPackIndex,
    ) -> crate::e2ee::Result<Vec<CommitTopology>> {
        verified_pack_topology(id, pack, &serde_json::to_vec(index).unwrap())
    }

    #[test]
    fn encrypted_staged_pack_matches_memory_verification_and_rejects_invalid_index() {
        use std::io::{Seek, Write};
        let (id, bytes, index, expected) = fixture();
        let index_bytes = serde_json::to_vec(&index).unwrap();
        let mut pack = tempfile::tempfile().unwrap();
        pack.write_all(&bytes).unwrap();
        let mut index_file = tempfile::tempfile().unwrap();
        index_file.write_all(&index_bytes).unwrap();
        let actual = verified_pack_file_topology(&id, &mut pack, &mut index_file, 4 << 20).unwrap();
        let topology = |entries: Vec<CommitTopology>| {
            entries
                .into_iter()
                .map(|entry| (entry.commit, entry.parents))
                .collect::<Vec<_>>()
        };
        assert_eq!(topology(actual), topology(expected));
        assert_eq!(pack.stream_position().unwrap(), 0);
        assert!(matches!(
            verified_pack_file_topology(&id, &mut pack, &mut index_file, index_bytes.len() - 1),
            Err(EncryptionError::TooLarge)
        ));
        // Buffered read-ahead must not hide additional JSON after the index.
        index_file.seek(std::io::SeekFrom::End(0)).unwrap();
        index_file.write_all(b"{}").unwrap();
        assert!(verified_pack_file_topology(&id, &mut pack, &mut index_file, 4 << 20).is_err());
    }

    #[test]
    fn encrypted_pack_recovers_real_history_without_index_advertisements() {
        let (id, pack, mut index, expected) = fixture();
        for entry in &expected {
            index.commits.push(RemoteObjectPackCommit {
                id: object::ObjectId::new(blake3::Hash::from(entry.commit).to_hex().to_string())
                    .unwrap(),
                parents: entry
                    .parents
                    .iter()
                    .map(|parent| {
                        object::ObjectId::new(blake3::Hash::from(*parent).to_hex().to_string())
                            .unwrap()
                    })
                    .collect(),
            });
        }
        assert_eq!(verify(&id, &pack, &index).unwrap().len(), expected.len());
        index.commits.pop();
        assert_eq!(verify(&id, &pack, &index).unwrap().len(), expected.len());
        index.objects.reverse();
        assert_eq!(verify(&id, &pack, &index).unwrap().len(), expected.len());
    }

    #[test]
    fn encrypted_pack_rejects_false_commit_and_parent_advertisements() {
        let (id, pack, mut index, expected) = fixture();
        let child = expected
            .iter()
            .find(|entry| !entry.parents.is_empty())
            .unwrap();
        index.commits.push(RemoteObjectPackCommit {
            id: object::ObjectId::new(blake3::Hash::from(child.commit).to_hex().to_string())
                .unwrap(),
            parents: vec![],
        });
        assert!(verify(&id, &pack, &index).is_err());
        let non_commit = index
            .objects
            .iter()
            .find(|entry| {
                !expected.iter().any(|commit| {
                    entry.id.as_str() == blake3::Hash::from(commit.commit).to_hex().as_str()
                })
            })
            .unwrap()
            .id
            .clone();
        index.commits = vec![RemoteObjectPackCommit { id: non_commit, parents: vec![] }];
        assert!(verify(&id, &pack, &index).is_err());
    }

    #[test]
    fn encrypted_pack_rejects_wrong_identity_ranges_and_unindexed_bytes() {
        let (id, pack, index, _) = fixture();
        assert!(verify(&"00".repeat(32), &pack, &index).is_err());
        let mut changed = index.clone();
        changed.pack = format!("objects/pack/{}.pack", "00".repeat(32));
        assert!(verify(&id, &pack, &changed).is_err());
        changed = index.clone();
        changed.objects[0].id = object::ObjectId::new("00".repeat(32)).unwrap();
        assert!(verify(&id, &pack, &changed).is_err());
        changed = index.clone();
        changed.objects[1].offset = changed.objects[0].offset;
        assert!(verify(&id, &pack, &changed).is_err());
        changed = index.clone();
        changed.objects[0].len = u64::MAX;
        assert!(verify(&id, &pack, &changed).is_err());
        changed = index.clone();
        changed.objects.remove(0);
        assert!(verify(&id, &pack, &changed).is_err());
        let mut corrupted = pack.clone();
        *corrupted.last_mut().unwrap() ^= 1;
        let new_id = blake3::hash(&corrupted).to_hex().to_string();
        changed = index.clone();
        changed.pack = format!("objects/pack/{new_id}.pack");
        assert!(verify(&new_id, &corrupted, &changed).is_err());
        let mut trailing = pack;
        trailing.push(0);
        let new_id = blake3::hash(&trailing).to_hex().to_string();
        changed = index;
        changed.pack = format!("objects/pack/{new_id}.pack");
        assert!(verify(&new_id, &trailing, &changed).is_err());
    }
}
