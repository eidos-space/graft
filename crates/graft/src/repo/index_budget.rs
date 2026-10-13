use super::*;

// Parsed discovery is separate from a transfer's raw plaintext buffer. Fixed
// caps retain room for 100,000 objects plus ordinary incremental history while
// bounding a malicious remote's aggregate metadata, including local caches.
pub(super) struct PackIndexBudget {
    pub maximum_index_bytes: Option<usize>,
    encoded: usize,
    entries: usize,
    parents: usize,
}

impl PackIndexBudget {
    pub fn new(maximum_index_bytes: Option<usize>, count: usize) -> Result<Self> {
        if maximum_index_bytes.is_some() && count > 4096 {
            return Err(too_large());
        }
        Ok(Self {
            maximum_index_bytes: maximum_index_bytes.map(|maximum| maximum.min(64 * 1024 * 1024)),
            encoded: 0,
            entries: 0,
            parents: 0,
        })
    }

    fn admit_size(&self, size: usize) -> Result<()> {
        if let Some(maximum) = self.maximum_index_bytes
            && (size > maximum
                || self
                    .encoded
                    .checked_add(size)
                    .is_none_or(|value| value > 64 * 1024 * 1024))
        {
            return Err(too_large());
        }
        Ok(())
    }

    pub fn decode(&mut self, path: &str, bytes: &[u8]) -> Result<RemoteObjectPackIndex> {
        self.admit_size(bytes.len())?;
        let index = decode_remote_object_pack_index(path, bytes)?;
        let entries = self
            .entries
            .checked_add(index.objects.len())
            .ok_or_else(too_large)?;
        let parents = index
            .commits
            .iter()
            .try_fold(self.parents, |count, commit| {
                count.checked_add(commit.parents.len())
            })
            .ok_or_else(too_large)?;
        if self.maximum_index_bytes.is_some() && (entries > 262_144 || parents > 1_048_576) {
            return Err(too_large());
        }
        self.encoded = self
            .encoded
            .checked_add(bytes.len())
            .ok_or_else(too_large)?;
        self.entries = entries;
        self.parents = parents;
        Ok(index)
    }

    pub fn cached(
        &mut self,
        remote_path: &str,
        cache: &Path,
    ) -> Result<Option<RemoteObjectPackIndex>> {
        let Some(maximum) = self.maximum_index_bytes else {
            return Ok(fs::read(cache)
                .ok()
                .and_then(|bytes| self.decode(remote_path, &bytes).ok()));
        };
        let metadata = match fs::symlink_metadata(cache) {
            Ok(value) => value,
            Err(_) => return Ok(None),
        };
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Ok(None);
        }
        let size = usize::try_from(metadata.len()).map_err(|_| too_large())?;
        self.admit_size(size)?;
        let file = match fs::File::open(cache) {
            Ok(value) => value,
            Err(_) => return Ok(None),
        };
        use std::io::Read;
        let mut bytes = Vec::with_capacity(size);
        file.take(maximum as u64 + 1).read_to_end(&mut bytes)?;
        self.admit_size(bytes.len())?;
        match self.decode(remote_path, &bytes) {
            Ok(index) => Ok(Some(index)),
            // Invalid local derived metadata is disposable; exceeding admission
            // is handled above, before decoding or falling back to the network.
            Err(RepoErr::InvalidRemoteObject { .. }) => Ok(None),
            Err(error) => Err(error),
        }
    }
}

fn too_large() -> RepoErr {
    #[cfg(all(feature = "e2ee", not(target_arch = "wasm32")))]
    {
        RemoteErr::from(crate::e2ee::EncryptionError::TooLarge).into()
    }
    #[cfg(not(all(feature = "e2ee", not(target_arch = "wasm32"))))]
    {
        RepoErr::InvalidRemoteObject {
            path: "objects/pack".into(),
            message: "pack index inventory exceeds budget".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cached_size_and_inventory_count_are_admitted_before_read_or_decode() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("oversized.idx");
        fs::File::create(&path).unwrap().set_len(65537).unwrap();
        let mut budget = PackIndexBudget::new(Some(65536), 1).unwrap();
        assert!(budget.cached("invalid-index-path", &path).is_err());
        assert!(PackIndexBudget::new(Some(65536), 4097).is_err());
        budget.encoded = 64 * 1024 * 1024;
        assert!(budget.admit_size(1).is_err());
        let mut budget = PackIndexBudget::new(Some(65536), 1).unwrap();
        budget.entries = 262_144;
        let pack = format!("objects/pack/{}.pack", "a".repeat(64));
        let bytes = serde_json::to_vec(&serde_json::json!({"version":1,"pack":pack,"objects":[{"id":"b".repeat(64),"offset":REMOTE_OBJECT_PACK_MAGIC.len(),"len":1}],"commits":[]})).unwrap();
        assert!(budget.decode("index", &bytes).is_err());
    }
    #[cfg(unix)]
    #[test]
    fn cached_symlinks_are_not_read() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("outside");
        fs::write(&target, b"not an index").unwrap();
        let link = directory.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(
            PackIndexBudget::new(Some(65536), 1)
                .unwrap()
                .cached("unused", &link)
                .unwrap()
                .is_none()
        );
        write_remote_object_pack_index_cache(&link, b"new cache").unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"not an index");
        assert_eq!(fs::read(&link).unwrap(), b"new cache");
    }
}
