use super::*;
use crate::e2ee::{
    ChunkSize, DiscoveryBudget, EncryptedHttpClient, EncryptedListClass, EncryptionError,
    ObjectPublicationOutcome, ObjectSnapshot, PreparedEncryptedObject, RefObservation,
    RefPublicationOutcome, RefTarget, RoutingHint,
};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, ReadBuf};

/// Host-selected private staging and per-operation resource budgets.
#[derive(Clone)]
pub struct EncryptedTransferBudget {
    pub private_directory: std::path::PathBuf,
    pub max_buffered_plaintext: usize,
    pub max_ciphertext_spool: u64,
}

impl EncryptedTransferBudget {
    pub(super) fn validate(&self) -> Result<()> {
        let metadata = fs::symlink_metadata(&self.private_directory)?;
        if !metadata.is_dir() || self.max_buffered_plaintext == 0 || self.max_ciphertext_spool == 0
        {
            return Err(EncryptionError::TooLarge.into());
        }
        crate::e2ee::native_fs::validate_private_directory(&self.private_directory)?;
        Ok(())
    }
}

/// Built-in providers retain no client or credential store. Trusted custom
/// providers must avoid introducing an ownership cycle.
#[derive(Clone)]
pub(super) struct SessionRegistration {
    pub identity: Arc<()>,
    pub url: String,
    pub uuid: String,
    pub session: crate::e2ee::SecretSession,
    pub budget: EncryptedTransferBudget,
    pub journals: Option<Arc<dyn crate::e2ee::RefMutationJournalProvider>>,
    pub uploads: Option<Arc<dyn crate::e2ee::MultipartJournalProvider>>,
}

pub(super) fn uuid_text(repository: crate::e2ee::RepositoryId) -> String {
    let hex = repository
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

#[derive(Clone)]
pub(super) struct EncryptedRemote {
    client: EncryptedHttpClient,
    budget: EncryptedTransferBudget,
    journals: Option<Arc<dyn crate::e2ee::RefMutationJournalProvider>>,
    uploads: Option<Arc<dyn crate::e2ee::MultipartJournalProvider>>,
}

impl fmt::Debug for EncryptedRemote {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EncryptedRemote")
            .field("client", &self.client)
            .finish_non_exhaustive()
    }
}

impl EncryptedRemote {
    pub(super) fn staging_directory(&self) -> &std::path::Path {
        &self.budget.private_directory
    }

    pub(super) fn new(
        client: EncryptedHttpClient,
        budget: EncryptedTransferBudget,
        journals: Option<Arc<dyn crate::e2ee::RefMutationJournalProvider>>,
        uploads: Option<Arc<dyn crate::e2ee::MultipartJournalProvider>>,
    ) -> Result<Self> {
        budget.validate()?;
        Ok(Self { client, budget, journals, uploads })
    }

    pub(super) fn has_mutation_journals(&self) -> bool {
        self.journals.is_some()
    }

    pub(super) fn buffered_read_limit(&self) -> usize {
        self.budget.max_buffered_plaintext
    }

    pub(super) async fn download_file(&self, path: &str) -> Result<Option<std::fs::File>> {
        Ok(self
            .client
            .download_object(
                path,
                &self.budget.private_directory,
                self.budget.max_ciphertext_spool,
            )
            .await?
            .map(|object| object.into_file()))
    }

    pub(super) async fn fetch_bundle(
        &self,
        ref_path: &str,
        have: Option<&str>,
        root: &Path,
    ) -> Result<bool> {
        let operation = self.client.operation().await?;
        let have = have.map(parse_id).transpose()?;
        let Some(bundle) = self
            .client
            .fetch_bundle(
                ref_path,
                have,
                &self.budget.private_directory,
                self.budget.max_ciphertext_spool,
            )
            .await?
        else {
            return Ok(false);
        };
        let mut objects = BTreeMap::new();
        let mut hints = Vec::new();
        for entry in bundle.entries {
            match entry {
                crate::e2ee::EncryptedBundleEntry::Object { remote_key, value } => {
                    objects.insert(remote_key, value);
                }
                crate::e2ee::EncryptedBundleEntry::Hint(hint) => hints.push(hint),
            }
        }
        let mut verified = Vec::new();
        for hint in hints {
            let pack = objects
                .remove(hint.pack_key())
                .ok_or(EncryptionError::HintMismatch)?;
            let index = objects
                .remove(hint.index_key())
                .ok_or(EncryptionError::HintMismatch)?;
            let pack_path = pack.metadata().logical_key.clone();
            let index_path = index.metadata().logical_key.clone();
            let id = pack_path
                .strip_prefix("objects/pack/")
                .and_then(|path| path.strip_suffix(".pack"))
                .ok_or(EncryptionError::InvalidObjectKey)?;
            parse_id(id)?;
            if index_path != format!("objects/pack/{id}.idx") {
                return Err(EncryptionError::HintMismatch.into());
            }
            let mut pack = pack.into_file();
            let mut index = index.into_file();
            let topology = crate::repo::verified_pack_file_topology(
                id,
                &mut pack,
                &mut index,
                self.budget.max_buffered_plaintext,
            )?;
            hint.verify_topology(operation.root()?, &topology)
                .map_err(|_| EncryptionError::HintMismatch)?;
            use std::io::Seek;
            index.rewind()?;
            verified.push((pack_path, pack));
            verified.push((index_path, index));
        }
        if !objects.is_empty() {
            return Err(EncryptionError::HintMismatch.into());
        }
        // Nothing is exposed until every frame, object identity and hint passes.
        for (path, file) in verified {
            let destination = root.join(path);
            let parent = destination
                .parent()
                .ok_or(EncryptionError::InvalidObjectKey)?;
            tokio::fs::create_dir_all(parent).await?;
            let mut output = tokio::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(destination)
                .await?;
            let mut source = tokio::fs::File::from_std(file);
            tokio::select! {
                result = tokio::io::copy(&mut source, &mut output) => { result?; },
                _ = operation.cancelled() => return Err(EncryptionError::Cancelled.into()),
            }
        }
        let target = bundle
            .reference
            .target()
            .ok_or(EncryptionError::InvalidEncoding)?;
        let destination = root.join(ref_path);
        tokio::fs::create_dir_all(
            destination
                .parent()
                .ok_or(EncryptionError::InvalidObjectKey)?,
        )
        .await?;
        tokio::fs::write(destination, target_bytes(target)).await?;
        operation.check_active()?;
        Ok(true)
    }

    pub(super) async fn read_batches(&self, paths: &[String]) -> Result<Vec<Vec<String>>> {
        let operation = self.client.operation().await?;
        let maximum = self.budget.max_buffered_plaintext.min(64 * 1024 * 1024) as u64;
        let mut batches = Vec::new();
        let mut batch = Vec::new();
        let mut length = 0_u64;
        for path in paths {
            let key = crate::e2ee::object::remote_key(operation.root()?, path)?;
            let metadata = self
                .client
                .discover_object_cached(&key)
                .await?
                .ok_or(EncryptionError::InvalidEncoding)?;
            if metadata.logical_key != *path || metadata.plaintext_length > maximum {
                return Err(EncryptionError::TooLarge.into());
            }
            if !batch.is_empty()
                && (batch.len() == 256 || metadata.plaintext_length > maximum - length)
            {
                batches.push(batch);
                batch = Vec::new();
                length = 0;
            }
            batch.push(path.clone());
            length += metadata.plaintext_length;
        }
        if !batch.is_empty() {
            batches.push(batch);
        }
        operation.check_active()?;
        Ok(batches)
    }

    pub(super) async fn observe_ref(&self, path: &str) -> Result<(Option<Bytes>, RefObservation)> {
        let observed = self.client.read_ref(path).await?;
        let bytes = observed.target().map(target_bytes);
        Ok((bytes, observed))
    }

    pub(super) fn validate_ref(&self, path: &str, observed: &RefObservation) -> Result<()> {
        self.client
            .session()
            .begin_operation()?
            .validate_observation(path, observed)?;
        Ok(())
    }

    pub(super) async fn get_raw(&self, path: &str, maximum: usize) -> Result<Option<Bytes>> {
        if is_ref(path) {
            let bytes = self.observe_ref(path).await?.0;
            if bytes.as_ref().is_some_and(|bytes| bytes.len() > maximum) {
                return Err(EncryptionError::TooLarge.into());
            }
            return Ok(bytes);
        }
        let operation = self.client.operation().await?;
        let maximum = maximum.min(self.budget.max_buffered_plaintext);
        let Some(value) = self
            .client
            .download_object(path, &self.budget.private_directory, maximum as u64)
            .await?
        else {
            return Ok(None);
        };
        let mut file = tokio::fs::File::from_std(value.into_file());
        let mut bytes = Vec::new();
        tokio::select! {
            result = file.read_to_end(&mut bytes) => {result?;},
            _ = operation.cancelled() => return Err(EncryptionError::Cancelled.into()),
        }
        operation.check_active()?;
        Ok(Some(Bytes::from(bytes)))
    }

    pub(super) async fn get_range(&self, path: &str, range: Range<u64>) -> Result<Bytes> {
        if range
            .end
            .checked_sub(range.start)
            .is_none_or(|length| length > self.budget.max_buffered_plaintext as u64)
        {
            return Err(EncryptionError::TooLarge.into());
        }
        self.client
            .read_object_range(path, range)
            .await?
            .map(Bytes::from)
            .ok_or_else(|| RemoteErr::HttpStatus {
                status: 404,
                path: "encrypted object".into(),
                message: "object missing".into(),
            })
    }

    pub(super) async fn read_bundle(&self, paths: &[String]) -> Result<BTreeMap<String, Bytes>> {
        let operation = self.client.operation().await?;
        let maximum = self.budget.max_buffered_plaintext.min(64 * 1024 * 1024);
        let mut objects = BTreeMap::new();
        let mut total = 0_usize;
        for paths in paths.chunks(256) {
            let mut expected = BTreeMap::new();
            for path in paths {
                if is_ref(path) {
                    return Err(EncryptionError::InvalidObjectKey.into());
                }
                expected.insert(
                    crate::e2ee::object::remote_key(operation.root()?, path)?,
                    path,
                );
            }
            let keys = expected.keys().cloned().collect::<Vec<_>>();
            let entries = self
                .client
                .read_bundle(
                    &keys,
                    &self.budget.private_directory,
                    (maximum - total) as u64,
                )
                .await?;
            for entry in entries {
                let crate::e2ee::EncryptedBundleEntry::Object { remote_key: key, value } = entry
                else {
                    return Err(EncryptionError::InvalidEncoding.into());
                };
                let path = expected
                    .remove(&key)
                    .ok_or(EncryptionError::InvalidEncoding)?;
                if value.metadata().logical_key != *path {
                    return Err(EncryptionError::InvalidEncoding.into());
                }
                let length = usize::try_from(value.metadata().plaintext_length)
                    .map_err(|_| EncryptionError::TooLarge)?;
                total = total.checked_add(length).ok_or(EncryptionError::TooLarge)?;
                if total > maximum {
                    return Err(EncryptionError::TooLarge.into());
                }
                let mut file = tokio::fs::File::from_std(value.into_file());
                let mut bytes = Vec::new();
                tokio::select! {
                    result = file.read_to_end(&mut bytes) => {result?;},
                    _ = operation.cancelled() => return Err(EncryptionError::Cancelled.into()),
                }
                if bytes.len() != length {
                    return Err(EncryptionError::InvalidEncoding.into());
                }
                objects.insert(path.clone(), Bytes::from(bytes));
            }
            if !expected.is_empty() {
                return Err(EncryptionError::InvalidEncoding.into());
            }
        }
        operation.check_active()?;
        Ok(objects)
    }

    pub(super) async fn has_raw(&self, path: &str) -> Result<bool> {
        if is_ref(path) {
            return Ok(self.observe_ref(path).await?.0.is_some());
        }
        let operation = self.client.operation().await?;
        let key = crate::e2ee::object::remote_key(operation.root()?, path)?;
        Ok(self.client.discover_object(&key).await?.is_some())
    }

    pub(super) async fn list_raw(&self, prefix: &str) -> Result<Vec<String>> {
        let operation = self.client.operation().await?;
        let mut paths = BTreeSet::new();
        for class in classes(prefix)? {
            for key in self
                .client
                .list_opaque(class, DiscoveryBudget::default())
                .await?
            {
                let logical =
                    if matches!(class, EncryptedListClass::Refs | EncryptedListClass::Head) {
                        self.client
                            .discover_ref(&key)
                            .await?
                            .map(|(logical, _)| logical)
                    } else {
                        self.client
                            .discover_object_cached(&key)
                            .await?
                            .map(|value| value.logical_key)
                    };
                if let Some(logical) = logical
                    && logical.starts_with(prefix)
                {
                    paths.insert(logical);
                }
            }
        }
        operation.check_active()?;
        Ok(paths.into_iter().collect())
    }

    pub(super) async fn put_chunks(&self, path: &str, chunks: Vec<Bytes>) -> Result<()> {
        let prepared = self.prepare_chunks(path, chunks).await?;
        self.publish_prepared(&prepared).await
    }

    async fn prepare_chunks(
        &self,
        path: &str,
        chunks: Vec<Bytes>,
    ) -> Result<PreparedEncryptedObject> {
        if is_ref(path) {
            return Err(EncryptionError::InvalidObjectKey.into());
        }
        let operation = self.client.operation().await?;
        let mut length = 0_u64;
        let mut digest = Sha256::new();
        for chunk in &chunks {
            length = length
                .checked_add(chunk.len() as u64)
                .ok_or(EncryptionError::TooLarge)?;
            digest.update(chunk);
        }
        let mut source = Chunks {
            chunks: chunks.into_iter(),
            current: Bytes::new(),
        };
        let prepared = PreparedEncryptedObject::prepare(
            &operation,
            ObjectSnapshot {
                logical_key: path.to_owned(),
                plaintext_length: length,
                plaintext_sha256: digest.finalize().into(),
                chunk_size: ChunkSize::KiB64,
            },
            &mut source,
            &self.budget.private_directory,
            self.budget.max_ciphertext_spool,
        )
        .await?;
        operation.check_active()?;
        Ok(prepared)
    }

    #[cfg(any(unix, windows))]
    pub(super) fn begin_operation(&self) -> Result<crate::e2ee::SessionOperation> {
        Ok(self.client.session().begin_operation()?)
    }

    /// The repository has verified source identity; preparation rechecks exact
    /// length/SHA while encrypting without retaining the full plaintext file.
    #[cfg(any(unix, windows))]
    pub(super) async fn put_verified_file(
        &self,
        path: &str,
        source: &std::path::Path,
        length: u64,
        digest: [u8; 32],
    ) -> Result<()> {
        let file = tokio::fs::File::open(source).await?;
        self.put_verified_handle(path, file.into_std().await, length, digest)
            .await
    }

    #[cfg(any(unix, windows))]
    pub(super) fn plaintext_spool(&self) -> Result<(std::fs::File, u64)> {
        self.budget.validate()?;
        Ok((
            crate::e2ee::download::private_staging_file(&self.budget.private_directory)?,
            self.budget.max_ciphertext_spool,
        ))
    }

    #[cfg(any(unix, windows))]
    pub(super) async fn put_verified_handle(
        &self,
        path: &str,
        file: std::fs::File,
        length: u64,
        digest: [u8; 32],
    ) -> Result<()> {
        if is_ref(path) {
            return Err(EncryptionError::InvalidObjectKey.into());
        }
        let operation = self.client.operation().await?;
        let mut file = tokio::fs::File::from_std(file);
        let trace = crate::trace::PushTraceSpan::new("encrypted_payload_ciphertext_prepare");
        let prepared = PreparedEncryptedObject::prepare(
            &operation,
            ObjectSnapshot {
                logical_key: path.to_owned(),
                plaintext_length: length,
                plaintext_sha256: digest,
                chunk_size: ChunkSize::KiB64,
            },
            &mut file,
            &self.budget.private_directory,
            self.budget.max_ciphertext_spool,
        )
        .await?;
        trace.finish(&[("plaintext_bytes", length)]);
        let trace = crate::trace::PushTraceSpan::new("encrypted_payload_immutable_publish");
        self.publish_prepared(&prepared).await?;
        trace.finish(&[("ciphertext_bytes", prepared.ciphertext_length())]);
        operation.check_active()?;
        Ok(())
    }

    async fn publish_prepared(&self, prepared: &PreparedEncryptedObject) -> Result<()> {
        if self
            .client
            .requires_multipart(prepared.ciphertext_length())
            .await?
        {
            #[cfg(any(unix, windows))]
            {
                let provider =
                    self.uploads
                        .as_ref()
                        .ok_or(EncryptionError::CheckpointUnavailable(
                            std::io::ErrorKind::Unsupported,
                        ))?;
                return match self
                    .client
                    .publish_object_multipart(
                        prepared,
                        provider.clone(),
                        &self.budget.private_directory,
                    )
                    .await?
                {
                    ObjectPublicationOutcome::Created | ObjectPublicationOutcome::Reused => Ok(()),
                    ObjectPublicationOutcome::Unconfirmed => {
                        Err(RemoteErr::EncryptedPublicationUnconfirmed)
                    }
                    ObjectPublicationOutcome::OutcomeUnknown { error } => {
                        Err(RemoteErr::EncryptedOutcomeUnknown { source: error })
                    }
                };
            }
            #[cfg(not(any(unix, windows)))]
            return Err(EncryptionError::UnsupportedProfile.into());
        }
        for _ in 0..3 {
            match self.client.publish_object(prepared).await? {
                ObjectPublicationOutcome::Created | ObjectPublicationOutcome::Reused => {
                    return Ok(());
                }
                ObjectPublicationOutcome::Unconfirmed => {}
                ObjectPublicationOutcome::OutcomeUnknown { error } => {
                    return Err(RemoteErr::EncryptedOutcomeUnknown { source: error });
                }
            }
        }
        Err(RemoteErr::EncryptedPublicationUnconfirmed)
    }

    pub(super) async fn create_ref(&self, path: &str, bytes: Bytes) -> Result<()> {
        let observed = self.client.read_ref(path).await?;
        if observed.expected_bytes().is_some() {
            return Err(RemoteErr::HttpStatus {
                status: 412,
                path: "encrypted ref".into(),
                message: "ref already exists".into(),
            });
        }
        self.publish_ref(path, &observed, Some(bytes)).await
    }

    pub(super) async fn publish_ref(
        &self,
        path: &str,
        observed: &RefObservation,
        replacement: Option<Bytes>,
    ) -> Result<()> {
        let target = replacement
            .as_ref()
            .map(|bytes| parse_target(path, bytes))
            .transpose()?;
        self.publish_typed_ref(path, observed, target).await
    }

    pub(super) fn tag_target(&self, path: &str, bytes: &[u8]) -> Result<(String, RefTarget)> {
        if !path.starts_with("refs/tags/") || crate::repo::validate_full_ref(path).is_err() {
            return Err(EncryptionError::InvalidObjectKey.into());
        }
        if bytes.len() > self.budget.max_buffered_plaintext.min(16 * 1024 * 1024) {
            return Err(EncryptionError::TooLarge.into());
        }
        let id = crate::repo::object::ObjectId::for_bytes(bytes);
        let target = match crate::repo::object::Object::decode(bytes)
            .map_err(|_| EncryptionError::InvalidEncoding)?
        {
            crate::repo::object::Object::Tag(_) => RefTarget::from_annotated_tag(bytes)?,
            crate::repo::object::Object::Commit(_) => RefTarget::Commit(parse_id(id.as_str())?),
            _ => return Err(EncryptionError::InvalidEncoding.into()),
        };
        Ok((
            crate::repo::object::LooseObjectStore::relative_path(&id),
            target,
        ))
    }

    pub(super) async fn publish_tag_ref(
        &self,
        path: &str,
        observed: &RefObservation,
        bytes: Bytes,
    ) -> Result<()> {
        let (object_path, target) = self.tag_target(path, &bytes)?;
        let operation = self.client.operation().await?;
        operation.validate_observation(path, observed)?;
        self.put_chunks(&object_path, vec![bytes]).await?;
        self.publish_typed_ref(path, observed, Some(target)).await?;
        operation.check_active()?;
        Ok(())
    }

    pub(super) async fn read_tag_object(
        &self,
        path: &str,
        observed: &RefObservation,
    ) -> Result<Option<Bytes>> {
        self.validate_ref(path, observed)?;
        if !path.starts_with("refs/tags/") {
            return Err(EncryptionError::InvalidObjectKey.into());
        }
        let operation = self.client.operation().await?;
        operation.validate_observation(path, observed)?;
        let Some(target) = observed.target() else {
            operation.check_active()?;
            return Ok(None);
        };
        let id = match target {
            RefTarget::AnnotatedTag { object, .. } | RefTarget::Commit(object) => {
                crate::repo::object::ObjectId::new(
                    object
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect::<String>(),
                )
                .map_err(|_| EncryptionError::InvalidEncoding)?
            }
            RefTarget::Symbolic(_) => return Err(EncryptionError::InvalidEncoding.into()),
        };
        let object_path = crate::repo::object::LooseObjectStore::relative_path(&id);
        let bytes = self
            .get_raw(&object_path, 16 * 1024 * 1024)
            .await?
            .ok_or(EncryptionError::SnapshotMismatch)?;
        let (_, actual) = self.tag_target(path, &bytes)?;
        if &actual != target {
            return Err(EncryptionError::AuthenticationFailed.into());
        }
        operation.check_active()?;
        Ok(Some(bytes))
    }

    pub(super) async fn publish_typed_ref(
        &self,
        path: &str,
        observed: &RefObservation,
        target: Option<RefTarget>,
    ) -> Result<()> {
        self.publish_typed_ref_with_bundle(path, observed, target, None)
            .await
    }

    async fn publish_typed_ref_with_bundle(
        &self,
        path: &str,
        observed: &RefObservation,
        target: Option<RefTarget>,
        bundle: Option<&crate::e2ee::PreparedReceiveBundle>,
    ) -> Result<()> {
        if self.journals.is_none() {
            return Err(
                EncryptionError::CheckpointUnavailable(std::io::ErrorKind::NotFound).into(),
            );
        }
        let operation = self.client.operation().await?;
        operation.validate_observation(path, observed)?;
        let journal = Some(self.ref_journal(observed).await?.ok_or(
            EncryptionError::CheckpointUnavailable(std::io::ErrorKind::NotFound),
        )?);
        let retained = if let Some(journal) = &journal {
            self.client.load_ref_mutation(path, journal.clone()).await?
        } else {
            None
        };
        let mut retry_ref_only = false;
        let retained = if let Some(pending) = retained {
            let same_target = pending.replacement_target(&operation)? == target;
            match self.client.reconcile_ref(&pending).await? {
                RefPublicationOutcome::Confirmed => {
                    self.retire(&pending, journal.as_ref().unwrap()).await?;
                    if same_target {
                        return Ok(());
                    }
                    None
                }
                RefPublicationOutcome::Unconfirmed => {
                    if !same_target || pending.expected_bytes() != observed.expected_bytes() {
                        return Err(EncryptionError::StaleObservation.into());
                    }
                    Some(pending)
                }
                RefPublicationOutcome::Conflict => {
                    return Err(RemoteErr::CompareAndSwap { path: "encrypted ref".into() });
                }
                RefPublicationOutcome::OutcomeUnknown { error: None } if same_target => {
                    // Readback authenticated a third value. Retry the original
                    // exact CAS to obtain mismatch evidence; never rebase this
                    // candidate on the newly observed value.
                    retry_ref_only = true;
                    Some(pending)
                }
                RefPublicationOutcome::OutcomeUnknown { error } => {
                    return Err(RemoteErr::EncryptedOutcomeUnknown { source: error });
                }
            }
        } else {
            None
        };
        let expected = operation.observe_ref(path, observed.expected_bytes())?;
        let candidate = if let Some(pending) = retained {
            pending
        } else {
            match target {
                Some(target) => {
                    self.client
                        .prepare_ref_update(path, &target, expected)
                        .await?
                }
                None => self.client.prepare_ref_delete(path, expected).await?,
            }
        };
        if retry_ref_only && let Some(bundle) = bundle {
            // A previous interrupted receive may not have uploaded every
            // dependency. Make them available before retrying its standalone CAS.
            self.publish_prepared_bundle(bundle).await?;
        }
        let mut aggregate_enabled = bundle.is_some() && !retry_ref_only;
        for _ in 0..3 {
            let aggregate_outcome = if let Some(bundle) = bundle.filter(|_| aggregate_enabled) {
                match self
                    .client
                    .receive_bundle(&candidate, bundle, journal.clone())
                    .await?
                {
                    crate::e2ee::ReceiveBundleOutcome::Publication(outcome) => Some(outcome),
                    crate::e2ee::ReceiveBundleOutcome::Fallback => {
                        self.publish_prepared_bundle(bundle).await?;
                        aggregate_enabled = false;
                        None
                    }
                }
            } else {
                None
            };
            let outcome = if let Some(outcome) = aggregate_outcome {
                outcome
            } else if let Some(journal) = &journal {
                self.client
                    .publish_ref_durable(&candidate, journal.clone())
                    .await?
            } else {
                return Err(
                    EncryptionError::CheckpointUnavailable(std::io::ErrorKind::NotFound).into(),
                );
            };
            match outcome {
                RefPublicationOutcome::Confirmed => {
                    if let Some(journal) = &journal {
                        self.retire(&candidate, journal).await?;
                    }
                    return Ok(());
                }
                RefPublicationOutcome::Unconfirmed => {}
                RefPublicationOutcome::Conflict => {
                    if let Some(journal) = &journal {
                        self.retire(&candidate, journal).await?;
                    }
                    return Err(RemoteErr::CompareAndSwap { path: "encrypted ref".into() });
                }
                RefPublicationOutcome::OutcomeUnknown { error } => {
                    return Err(RemoteErr::EncryptedOutcomeUnknown { source: error });
                }
            }
        }
        Err(RemoteErr::EncryptedPublicationUnconfirmed)
    }

    async fn publish_prepared_bundle(
        &self,
        bundle: &crate::e2ee::PreparedReceiveBundle,
    ) -> Result<()> {
        for object in &bundle.objects {
            self.publish_prepared(object).await?;
        }
        for hint in &bundle.hints {
            self.client.publish_hint(hint).await?;
        }
        Ok(())
    }

    async fn ref_journal(
        &self,
        observed: &RefObservation,
    ) -> Result<Option<Arc<dyn crate::e2ee::RefMutationJournal>>> {
        Ok(if let Some(provider) = &self.journals {
            let provider = provider.clone();
            let session = self.client.session().clone();
            let key = observed.remote_key().to_owned();
            let journal_operation = self.client.session().begin_operation()?;
            Some(
                tokio::task::spawn_blocking(move || {
                    journal_operation.check_active()?;
                    let journal = provider.journal(session.binding(), &key)?;
                    journal_operation.check_active()?;
                    Ok::<_, EncryptionError>(journal)
                })
                .await
                .map_err(|_| EncryptionError::CheckpointUnavailable(std::io::ErrorKind::Other))??,
            )
        } else {
            None
        })
    }

    pub(super) async fn reconcile_existing_ref(
        &self,
        path: &str,
        observed: &RefObservation,
    ) -> Result<()> {
        self.validate_ref(path, observed)?;
        let Some(journal) = self.ref_journal(observed).await? else {
            return Ok(());
        };
        let Some(candidate) = self.client.load_ref_mutation(path, journal.clone()).await? else {
            return Ok(());
        };
        match self.client.reconcile_ref(&candidate).await? {
            RefPublicationOutcome::Confirmed => self.retire(&candidate, &journal).await,
            RefPublicationOutcome::Unconfirmed => Err(RemoteErr::EncryptedPublicationUnconfirmed),
            RefPublicationOutcome::Conflict => {
                Err(RemoteErr::CompareAndSwap { path: "encrypted ref".into() })
            }
            RefPublicationOutcome::OutcomeUnknown { error } => {
                Err(RemoteErr::EncryptedOutcomeUnknown { source: error })
            }
        }
    }

    pub(super) async fn publish_bundle_and_ref(
        &self,
        objects: &[RemoteBundleObject],
        pack: Option<&RemoteObjectPack>,
        path: &str,
        observed: &RefObservation,
        replacement: Bytes,
    ) -> Result<()> {
        parse_target(path, &replacement)?;
        let packs = pack
            .map(|pack| {
                crate::repo::split_verified_pack(
                    &pack.id,
                    &pack.pack,
                    &pack.index,
                    self.budget.max_buffered_plaintext,
                )
            })
            .transpose()?
            .unwrap_or_default();
        let maximum_ciphertext_length = objects
            .iter()
            .map(|object| object.content_length as u64)
            .chain(packs.iter().flat_map(|verified| {
                let pack = verified.pack();
                [pack.pack.len() as u64, pack.index.len() as u64]
            }))
            .try_fold(0_u64, |total, length| {
                let tags = length
                    .max(1)
                    .div_ceil(64 * 1024)
                    .checked_mul(16)
                    .ok_or(EncryptionError::TooLarge)?;
                total
                    .checked_add(length)
                    .and_then(|total| total.checked_add(tags))
                    .and_then(|total| {
                        total.checked_add(
                            (crate::e2ee::object::PREFIX_LENGTH
                                + crate::e2ee::object::MAX_HEADER_LENGTH)
                                as u64,
                        )
                    })
                    .ok_or(EncryptionError::TooLarge)
            })?;
        if !path.starts_with("refs/heads/")
            || objects.len().saturating_add(packs.len().saturating_mul(3)) > 256
            || maximum_ciphertext_length > self.budget.max_ciphertext_spool.min(64 * 1024 * 1024)
        {
            for object in objects {
                self.put_chunks(&object.path, object.chunks.clone()).await?;
            }
            for pack in &packs {
                self.publish_bounded_pack(pack).await?;
            }
            return self.publish_ref(path, observed, Some(replacement)).await;
        }
        let operation = self.client.operation().await?;
        let mut bundle =
            crate::e2ee::PreparedReceiveBundle { objects: Vec::new(), hints: Vec::new() };
        let mut spool_length = 0_u64;
        for object in objects {
            let prepared = self
                .prepare_chunks(&object.path, object.chunks.clone())
                .await?;
            spool_length = spool_length
                .checked_add(prepared.ciphertext_length())
                .ok_or(EncryptionError::TooLarge)?;
            bundle.objects.push(prepared);
        }
        for verified in &packs {
            let pack = verified.pack();
            let topology = verified.topology();
            let pack_key = crate::e2ee::object::remote_key(operation.root()?, &pack.pack_path)?;
            let index_key = crate::e2ee::object::remote_key(operation.root()?, &pack.index_path)?;
            if !topology.is_empty()
                && topology
                    .iter()
                    .all(|commit| commit.parents.len() <= crate::e2ee::hint::MAX_PARENTS)
            {
                bundle.hints.push(RoutingHint::create(
                    operation.root()?,
                    &pack_key,
                    &index_key,
                    topology,
                )?);
            }
            for (path, bytes) in [
                (&pack.pack_path, pack.pack.clone()),
                (&pack.index_path, pack.index.clone()),
            ] {
                let prepared = self.prepare_chunks(path, vec![bytes]).await?;
                spool_length = spool_length
                    .checked_add(prepared.ciphertext_length())
                    .ok_or(EncryptionError::TooLarge)?;
                bundle.objects.push(prepared);
            }
        }
        if spool_length > self.budget.max_ciphertext_spool {
            // Prepared ciphertext can be published individually; no ref CAS has
            // been sent and no candidate nonce has been generated yet.
            for object in &bundle.objects {
                self.publish_prepared(object).await?;
            }
            for hint in &bundle.hints {
                self.client.publish_hint(hint).await?;
            }
            return self.publish_ref(path, observed, Some(replacement)).await;
        }
        self.publish_typed_ref_with_bundle(
            path,
            observed,
            Some(parse_target(path, &replacement)?),
            Some(&bundle),
        )
        .await
    }

    async fn retire(
        &self,
        candidate: &crate::e2ee::PreparedRefMutation,
        journal: &Arc<dyn crate::e2ee::RefMutationJournal>,
    ) -> Result<()> {
        self.client
            .retire_ref_mutation(candidate, journal.clone())
            .await
            .map_err(|error| RemoteErr::EncryptedOutcomeUnknown { source: Some(error) })
    }

    pub(super) async fn publish_pack(&self, pack: &RemoteObjectPack) -> Result<()> {
        let packs = crate::repo::split_verified_pack(
            &pack.id,
            &pack.pack,
            &pack.index,
            self.budget.max_buffered_plaintext,
        )?;
        for pack in packs {
            self.publish_bounded_pack(&pack).await?;
        }
        Ok(())
    }

    async fn publish_bounded_pack(&self, verified: &crate::repo::VerifiedPack) -> Result<()> {
        let operation = self.client.operation().await?;
        let pack = verified.pack();
        let topology = verified.topology();
        let pack_key = crate::e2ee::object::remote_key(operation.root()?, &pack.pack_path)?;
        let index_key = crate::e2ee::object::remote_key(operation.root()?, &pack.index_path)?;
        let hint = if topology.is_empty()
            || topology
                .iter()
                .any(|commit| commit.parents.len() > crate::e2ee::hint::MAX_PARENTS)
        {
            None
        } else {
            Some(RoutingHint::create(
                operation.root()?,
                &pack_key,
                &index_key,
                topology,
            )?)
        };
        self.put_chunks(&pack.pack_path, vec![pack.pack.clone()])
            .await?;
        self.put_chunks(&pack.index_path, vec![pack.index.clone()])
            .await?;
        if let Some(hint) = hint {
            self.client.publish_hint(&hint).await?;
        }
        operation.check_active()?;
        Ok(())
    }
}

fn is_ref(path: &str) -> bool {
    path == "HEAD" || path.starts_with("refs/")
}
fn target_bytes(target: &RefTarget) -> Bytes {
    match target {
        RefTarget::Commit(id) => Bytes::from(format!(
            "{}\n",
            id.iter().map(|b| format!("{b:02x}")).collect::<String>()
        )),
        RefTarget::AnnotatedTag { object, .. } => Bytes::from(format!(
            "{}\n",
            object
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        )),
        RefTarget::Symbolic(path) => Bytes::from(format!("ref: {path}\n")),
    }
}
fn parse_target(path: &str, bytes: &[u8]) -> Result<RefTarget> {
    // A lone tag ID cannot distinguish a lightweight tag from an annotated tag.
    // Tag writes require the typed client target and verified object dependency.
    if path.starts_with("refs/tags/") {
        return Err(EncryptionError::UnsupportedProfile.into());
    }
    let text = std::str::from_utf8(bytes).map_err(|_| EncryptionError::InvalidEncoding)?;
    let text = text.strip_suffix('\n').unwrap_or(text);
    if let Some(branch) = text.strip_prefix("ref: ") {
        if path != "HEAD" {
            return Err(EncryptionError::InvalidObjectKey.into());
        }
        crate::repo::validate_full_ref(branch).map_err(|_| EncryptionError::InvalidObjectKey)?;
        return Ok(RefTarget::Symbolic(branch.to_owned()));
    }
    Ok(RefTarget::Commit(parse_id(text)?))
}

pub(crate) fn parse_id(text: &str) -> std::result::Result<[u8; 32], EncryptionError> {
    if text.len() != 64
        || !text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(EncryptionError::InvalidEncoding);
    }
    let mut bytes = [0; 32];
    for (index, pair) in text.as_bytes().chunks_exact(2).enumerate() {
        let digit = |b: u8| if b <= b'9' { b - b'0' } else { b - b'a' + 10 };
        bytes[index] = digit(pair[0]) * 16 + digit(pair[1]);
    }
    Ok(bytes)
}
fn classes(prefix: &str) -> Result<Vec<EncryptedListClass>> {
    use EncryptedListClass::*;
    let possibilities = [
        (Objects, "objects/"),
        (Packs, "objects/pack/"),
        (Indexes, "objects/pack/"),
        (Files, "store/files/"),
        (Commits, "logs/"),
        (Segments, "segments/"),
        (Refs, "refs/"),
        (Head, "HEAD"),
    ];
    let classes = possibilities
        .into_iter()
        .filter(|(_, path)| path.starts_with(prefix) || prefix.starts_with(path))
        .map(|(class, _)| class)
        .collect::<Vec<_>>();
    if classes.is_empty() {
        return Err(EncryptionError::InvalidObjectKey.into());
    }
    Ok(classes)
}

struct Chunks {
    chunks: std::vec::IntoIter<Bytes>,
    current: Bytes,
}
impl AsyncRead for Chunks {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        while self.current.is_empty() {
            let Some(bytes) = self.chunks.next() else {
                return std::task::Poll::Ready(Ok(()));
            };
            self.current = bytes;
        }
        let length = buffer.remaining().min(self.current.len());
        buffer.put_slice(&self.current.split_to(length));
        std::task::Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
mod ref_tests {
    use super::*;

    #[tokio::test]
    async fn ref_mutation_without_journal_fails_before_any_http_request() {
        use crate::e2ee::{EncryptedConfig, RepositoryId, RootKey, SecretSession, SessionBinding};
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        crate::native_fs::set_permissions(
            directory.path(),
            crate::native_fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        let root = RootKey::generate(RepositoryId::generate().unwrap()).unwrap();
        let config = EncryptedConfig::create(&root, b"journal regression password").unwrap();
        let session = SecretSession::new(
            SessionBinding::new(
                "journal-regression".into(),
                "account".into(),
                root.repository(),
            )
            .unwrap(),
            None,
        )
        .unwrap();
        session.unlock_with_root(&config, root).unwrap();
        let observed = session
            .begin_operation()
            .unwrap()
            .observe_ref("refs/heads/main", None)
            .unwrap();
        let remote = EncryptedRemote::new(
            EncryptedHttpClient::new(
                "http://127.0.0.1:1/repo",
                "origin".into(),
                RemoteCredentials::explicit(),
                session,
            )
            .unwrap(),
            EncryptedTransferBudget {
                private_directory: directory.path().to_owned(),
                max_buffered_plaintext: 1024,
                max_ciphertext_spool: 4096,
            },
            None,
            None,
        )
        .unwrap();
        let error = remote
            .publish_typed_ref(
                "refs/heads/main",
                &observed,
                Some(RefTarget::Commit([1; 32])),
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            RemoteErr::Encryption(crate::e2ee::EncryptedHttpError::Encryption(
                EncryptionError::CheckpointUnavailable(std::io::ErrorKind::NotFound)
            ))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn transfer_budget_rejects_symlinked_or_public_staging() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let directory = tempfile::tempdir().unwrap();
        let private = directory.path().join("private");
        fs::create_dir(&private).unwrap();
        fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).unwrap();
        let mut budget = EncryptedTransferBudget {
            private_directory: private.clone(),
            max_buffered_plaintext: 1024,
            max_ciphertext_spool: 4096,
        };
        budget.validate().unwrap();
        let alias = directory.path().join("alias");
        symlink(&private, &alias).unwrap();
        budget.private_directory = alias;
        assert!(budget.validate().is_err());
        budget.private_directory = private.clone();
        fs::set_permissions(&private, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(budget.validate().is_err());
    }

    #[test]
    fn annotated_tag_projection_preserves_object_and_ambiguous_writes_fail() {
        let tag = RefTarget::AnnotatedTag { object: [7; 32], commit: [9; 32] };
        let bytes = target_bytes(&tag);
        assert_eq!(bytes.as_ref(), format!("{}\n", "07".repeat(32)).as_bytes());
        assert!(matches!(
            parse_target("refs/tags/release", &bytes),
            Err(RemoteErr::Encryption(_))
        ));
        assert!(
            parse_target(
                "refs/heads/main",
                format!("{}\n", "09".repeat(32)).as_bytes()
            )
            .is_ok()
        );
    }
}
