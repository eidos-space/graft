//! Frozen migration inventory, verification and staged dependency publication.
use super::*;
use std::io::Read;

/// All includes local branches, tags and remote-tracking refs, plus HEAD.
/// Selected refs must be canonical full names; HEAD is pinned separately.
#[derive(Clone, PartialEq, Eq)]
pub enum MigrationRefSelection {
    All,
    Selected(Vec<String>),
}

#[derive(Clone, Copy)]
pub struct MigrationPlanningBudget {
    pub max_refs: usize,
    pub max_objects: usize,
    pub max_object_bytes: usize,
}

impl Default for MigrationPlanningBudget {
    fn default() -> Self {
        Self {
            max_refs: 100_000,
            max_objects: 1_000_000,
            max_object_bytes: 16 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Copy)]
pub struct MigrationVerificationBudget {
    pub max_payload_bytes: u64,
    pub max_storage_commits: usize,
    pub max_storage_pages: u64,
}

impl Default for MigrationVerificationBudget {
    fn default() -> Self {
        Self {
            max_payload_bytes: 8 * 1024 * 1024 * 1024,
            max_storage_commits: 1_000_000,
            max_storage_pages: 16_000_000,
        }
    }
}

/// Counts from one local verification pass, not a durable publication receipt.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct MigrationDependencyVerification {
    pub payloads: usize,
    pub payload_bytes: u64,
    pub snapshots: usize,
    pub storage_commits: usize,
    pub storage_pages: u64,
}

/// Resource limits for the native dependency-publication phase.
#[derive(Clone, Copy)]
pub struct MigrationPublicationBudget {
    pub verification: MigrationVerificationBudget,
    pub max_storage_batch_pages: u64,
    pub max_pack_bytes: usize,
}

impl Default for MigrationPublicationBudget {
    fn default() -> Self {
        Self {
            verification: MigrationVerificationBudget::default(),
            max_storage_batch_pages: 1024,
            max_pack_bytes: 4 * 1024 * 1024,
        }
    }
}

/// Logical source counts, not a durable migration or target-restoration receipt.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct MigrationDependencyPublication {
    pub repository_objects: usize,
    pub verified: MigrationDependencyVerification,
}

/// Private host metadata. No Debug/JSON implementation: names and IDs are not
/// default diagnostics. This proves repository-object closure, not `SQLite` storage
/// availability, payload availability, durable fencing or migration completion.
pub struct MigrationSourcePlan {
    source: PathBuf,
    max_refs: usize,
    max_object_bytes: usize,
    max_objects: usize,
    selection: MigrationRefSelection,
    head: Head,
    head_target: Option<String>,
    references: BTreeMap<String, Option<String>>,
    objects: BTreeSet<object::ObjectId>,
    snapshots: BTreeMap<object::ObjectId, object::SqliteSnapshotBlob>,
    external_payloads: BTreeMap<object::ObjectId, u64>,
}

impl MigrationSourcePlan {
    /// Canonical private bytes for a trusted host's immutable migration journal.
    /// Contains names and IDs; never send to remote, plugin RPC or diagnostics.
    pub fn encode_private(&self) -> Result<Vec<u8>> {
        if self.source.to_str().is_none() {
            return Err(invalid("migration source path is not UTF-8"));
        }
        serde_json::to_vec(&self.private_record())
            .map_err(|_| invalid("migration plan encoding failed"))
    }

    fn private_record(&self) -> SourcePlanRecord {
        let mut digest = blake3::Hasher::new();
        digest.update(b"graft/private-migration-object-closure/v1");
        for id in &self.objects {
            digest.update(id.as_str().as_bytes());
        }
        SourcePlanRecord {
            version: 1,
            source: self.source.to_string_lossy().into_owned(),
            all: matches!(self.selection, MigrationRefSelection::All),
            refs: self
                .references
                .iter()
                .map(|(name, target)| (name.clone(), target.clone()))
                .collect(),
            head: self.head.clone(),
            head_target: self.head_target.clone(),
            max_refs: self.max_refs,
            max_objects: self.max_objects,
            max_object_bytes: self.max_object_bytes,
            closure: *digest.finalize().as_bytes(),
        }
    }

    pub fn references(&self) -> &BTreeMap<String, Option<String>> {
        &self.references
    }
    pub fn head(&self) -> &Head {
        &self.head
    }
    pub fn head_target(&self) -> Option<&str> {
        self.head_target.as_deref()
    }
    pub fn objects(&self) -> &BTreeSet<object::ObjectId> {
        &self.objects
    }
    pub fn snapshots(&self) -> &BTreeMap<object::ObjectId, object::SqliteSnapshotBlob> {
        &self.snapshots
    }
    pub fn external_payloads(&self) -> &BTreeMap<object::ObjectId, u64> {
        &self.external_payloads
    }
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct SourcePlanRecord {
    version: u8,
    source: String,
    all: bool,
    refs: Vec<(String, Option<String>)>,
    head: Head,
    head_target: Option<String>,
    max_refs: usize,
    max_objects: usize,
    max_object_bytes: usize,
    closure: [u8; 32],
}

/// Private exact publication intent. An annotated tag's object ID is retained
/// separately from its commit dependency; neither is a publication receipt.
pub struct MigrationTargetRef {
    pub source_ref: String,
    pub target_ref: String,
    pub object: Option<object::ObjectId>,
    pub commit: Option<object::ObjectId>,
    pub annotated_tag: bool,
}

/// Validated explicit target namespace, still requiring enrollment, durable
/// publication progress and independent target restoration.
pub struct MigrationTargetPlan {
    source: Vec<u8>,
    refs: BTreeMap<String, MigrationTargetRef>,
    head: Head,
}

impl MigrationTargetPlan {
    /// Canonical private target intent, including the frozen source record.
    /// Never send these names/IDs to plugin RPC, diagnostics or the remote.
    pub fn encode_private(&self) -> Result<Vec<u8>> {
        let record = TargetPlanRecord {
            version: 1,
            source: serde_json::from_slice(&self.source)
                .map_err(|_| invalid("invalid frozen target source"))?,
            mappings: self
                .refs
                .values()
                .map(|value| (value.source_ref.clone(), value.target_ref.clone()))
                .collect(),
            head: self.head.clone(),
        };
        let bytes =
            serde_json::to_vec(&record).map_err(|_| invalid("target plan encoding failed"))?;
        if bytes.len() > 16 * 1024 * 1024 {
            return Err(failure(crate::e2ee::EncryptionError::TooLarge));
        }
        Ok(bytes)
    }

    pub fn references(&self) -> &BTreeMap<String, MigrationTargetRef> {
        &self.refs
    }
    pub fn head(&self) -> &Head {
        &self.head
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TargetPlanRecord {
    version: u8,
    source: SourcePlanRecord,
    mappings: Vec<(String, String)>,
    head: Head,
}

fn failure(error: crate::e2ee::EncryptionError) -> RepoErr {
    crate::remote::RemoteErr::from(error).into()
}
fn invalid(message: &'static str) -> RepoErr {
    object::ObjectErr::InvalidObject {
        kind: "migration-source",
        message: message.into(),
    }
    .into()
}
fn kind_mask(kind: object::ObjectKind) -> u8 {
    match kind {
        object::ObjectKind::Commit => 1,
        object::ObjectKind::Tree => 2,
        object::ObjectKind::Blob => 4,
        object::ObjectKind::Tag => 8,
    }
}

fn enqueue(
    id: object::ObjectId,
    allowed: u8,
    requirements: &mut BTreeMap<object::ObjectId, u8>,
    kinds: &BTreeMap<object::ObjectId, object::ObjectKind>,
    pending: &mut VecDeque<object::ObjectId>,
    limit: usize,
) -> Result<()> {
    if let Some(required) = requirements.get_mut(&id) {
        *required &= allowed;
        if *required == 0
            || kinds
                .get(&id)
                .is_some_and(|kind| kind_mask(*kind) & *required == 0)
        {
            return Err(invalid("object kind disagrees with a dependency"));
        }
        return Ok(());
    }
    if requirements.len() >= limit {
        return Err(failure(crate::e2ee::EncryptionError::TooLarge));
    }
    requirements.insert(id.clone(), allowed);
    pending.push_back(id);
    Ok(())
}

impl Repository {
    /// Restore the exact target mapping and HEAD, revalidating the frozen source.
    pub fn restore_migration_target_plan(&self, bytes: &[u8]) -> Result<MigrationTargetPlan> {
        if bytes.is_empty() || bytes.len() > 16 * 1024 * 1024 {
            return Err(failure(crate::e2ee::EncryptionError::TooLarge));
        }
        let record: TargetPlanRecord =
            serde_json::from_slice(bytes).map_err(|_| invalid("invalid private target plan"))?;
        if record.version != 1
            || serde_json::to_vec(&record).map_err(|_| invalid("invalid private target plan"))?
                != bytes
        {
            return Err(invalid("private target plan is not canonical"));
        }
        let source = self.restore_migration_source_plan(
            &serde_json::to_vec(&record.source)
                .map_err(|_| invalid("invalid private target source"))?,
        )?;
        let plan = self.plan_migration_target_refs(&source, record.mappings, record.head)?;
        // Mapping order is canonical target-ref order, not arbitrary caller order.
        if plan.encode_private()? != bytes {
            return Err(invalid("private target mapping is not canonical"));
        }
        Ok(plan)
    }

    /// Resolve every frozen source ref to an explicit target. No remote access,
    /// uploads, ref updates or implicit remote-tracking name conversion.
    pub fn plan_migration_target_refs(
        &self,
        source: &MigrationSourcePlan,
        mappings: Vec<(String, String)>,
        head: Head,
    ) -> Result<MigrationTargetPlan> {
        self.validate_migration_source_plan(source)?;
        if mappings.len() != source.references.len() {
            return Err(invalid(
                "target mapping must cover every selected source ref",
            ));
        }
        let mut names = BTreeSet::new();
        let mut refs = BTreeMap::new();
        for (from, to) in mappings {
            cancellation_checkpoint()?;
            if to.len() > 4096 {
                return Err(failure(crate::e2ee::EncryptionError::TooLarge));
            }
            validate_full_ref(&to).map_err(|_| invalid("invalid migration target ref"))?;
            if !names.insert(from.clone()) {
                return Err(invalid("duplicate migration source mapping"));
            }
            let target = source
                .references
                .get(&from)
                .ok_or_else(|| invalid("mapping is outside the frozen source"))?;
            let is_tag = from.starts_with("refs/tags/");
            if target.is_none() && !from.starts_with("refs/heads/") {
                return Err(invalid("only local branches may have an unborn target"));
            }
            if (is_tag && !to.starts_with("refs/tags/"))
                || (!is_tag && !to.starts_with("refs/heads/"))
            {
                return Err(invalid("target mapping changes the source ref kind"));
            }
            let object = target
                .as_deref()
                .map(object::ObjectId::from_str)
                .transpose()?;
            let (commit, annotated_tag) = match &object {
                None => (None, false),
                Some(id) => match self.read_migration_object(id, source.max_object_bytes)? {
                    object::Object::Commit(_) => (Some(id.clone()), false),
                    object::Object::Tag(tag)
                        if is_tag
                            && tag.object_type == object::ObjectKind::Commit
                            && source.objects.contains(&tag.object) =>
                    {
                        if self
                            .read_migration_object(&tag.object, source.max_object_bytes)?
                            .kind()
                            != object::ObjectKind::Commit
                        {
                            return Err(invalid("annotated tag does not target a commit"));
                        }
                        (Some(tag.object), true)
                    }
                    _ => return Err(invalid("migration ref has an invalid target kind")),
                },
            };
            let value = MigrationTargetRef {
                source_ref: from,
                target_ref: to.clone(),
                object,
                commit,
                annotated_tag,
            };
            if refs.insert(to, value).is_some() {
                return Err(invalid("target ref mapping collides"));
            }
        }
        // Ref files cannot simultaneously occupy a directory and its descendant.
        for target in refs.keys() {
            for (offset, _) in target.match_indices('/') {
                if refs.contains_key(&target[..offset]) {
                    return Err(invalid("target ref mapping has a file-directory collision"));
                }
            }
        }
        match &head {
            Head::Branch { name } => {
                if name.len() > 4085 {
                    return Err(failure(crate::e2ee::EncryptionError::TooLarge));
                }
                validate_ref_name(name).map_err(|_| invalid("invalid migration target HEAD"))?;
                if !refs.contains_key(&format!("refs/heads/{name}")) {
                    return Err(invalid("target HEAD branch is outside the mapped source"));
                }
            }
            Head::Detached { commit } => {
                let id = object::ObjectId::from_str(commit)?;
                if !source.objects.contains(&id)
                    || self
                        .read_migration_object(&id, source.max_object_bytes)?
                        .kind()
                        != object::ObjectKind::Commit
                {
                    return Err(invalid(
                        "target detached HEAD is outside the selected commit history",
                    ));
                }
            }
        }
        let source_bytes = source.encode_private()?;
        // Rehash the full retained closure, not only current ref targets.
        self.restore_migration_source_plan(&source_bytes)?;
        Ok(MigrationTargetPlan { source: source_bytes, refs, head })
    }

    pub fn validate_migration_target_plan(&self, plan: &MigrationTargetPlan) -> Result<()> {
        self.restore_migration_source_plan(&plan.source).map(|_| ())
    }

    /// Publish one explicitly mapped branch after complete dependency transfer.
    /// An occupied foreign target fails; the registered journal retains exact CAS
    /// intent for recovery. Does not advance migration stages or source refs.
    #[cfg(unix)]
    pub fn publish_migration_target_branch(
        &self,
        plan: &MigrationTargetPlan,
        remote: &crate::remote::Remote,
        target_ref: &str,
    ) -> Result<()> {
        remote.with_encrypted_operation(|| {
            self.validate_migration_target_plan(plan)?;
            self.publish_frozen_target_branch(plan, remote, target_ref)?;
            self.validate_migration_target_plan(plan)?;
            Ok(())
        })
    }

    #[cfg(unix)]
    fn publish_frozen_target_branch(
        &self,
        plan: &MigrationTargetPlan,
        remote: &crate::remote::Remote,
        target_ref: &str,
    ) -> Result<()> {
        let target = plan
            .refs
            .get(target_ref)
            .filter(|_| target_ref.starts_with("refs/heads/"))
            .ok_or_else(|| invalid("branch is not in the frozen target mapping"))?;
        if target.commit.is_some() {
            remote.require_encrypted_ref_journals()?;
        }
        let observed = block_on_remote(remote.observe_ref(target_ref))?;
        if let Some(commit) = &target.commit {
            let replacement = format!("{}\n", commit.as_str());
            if observed
                .bytes()
                .is_some_and(|bytes| bytes.as_ref() != replacement.as_bytes())
            {
                return Err(failure(crate::e2ee::EncryptionError::StaleObservation));
            }
            block_on_remote(remote.publish_observed_branch_ref(
                target_ref,
                &observed,
                commit.as_str(),
            ))?;
        } else if observed.bytes().is_some() {
            return Err(failure(crate::e2ee::EncryptionError::StaleObservation));
        }
        Ok(())
    }

    /// Publish one mapped tag with exact object and commit identities.
    /// Requires complete dependencies and registered durable ref recovery.
    #[cfg(unix)]
    pub fn publish_migration_target_tag(
        &self,
        plan: &MigrationTargetPlan,
        remote: &crate::remote::Remote,
        target_ref: &str,
    ) -> Result<()> {
        remote.with_encrypted_operation(|| {
            let source = self.restore_migration_source_plan(&plan.source)?;
            self.publish_frozen_target_tag(plan, &source, remote, target_ref)?;
            self.validate_migration_target_plan(plan)?;
            Ok(())
        })
    }

    #[cfg(unix)]
    fn publish_frozen_target_tag(
        &self,
        plan: &MigrationTargetPlan,
        source: &MigrationSourcePlan,
        remote: &crate::remote::Remote,
        target_ref: &str,
    ) -> Result<()> {
        let target = plan
            .refs
            .get(target_ref)
            .filter(|_| target_ref.starts_with("refs/tags/"))
            .ok_or_else(|| invalid("tag is not in the frozen target mapping"))?;
        let id = target
            .object
            .as_ref()
            .ok_or_else(|| invalid("mapped tag has no target object"))?;
        remote.require_encrypted_ref_journals()?;
        let bytes = self.read_verified_migration_object(source, id)?;
        let observed = block_on_remote(remote.observe_ref(target_ref))?;
        let replacement = format!("{}\n", id.as_str());
        if observed
            .bytes()
            .is_some_and(|value| value.as_ref() != replacement.as_bytes())
        {
            return Err(failure(crate::e2ee::EncryptionError::StaleObservation));
        }
        if observed.bytes().is_some() {
            let actual = block_on_remote(remote.read_observed_tag_object(target_ref, &observed))?
                .ok_or_else(|| failure(crate::e2ee::EncryptionError::SnapshotMismatch))?;
            if actual.as_ref() != bytes.as_slice() {
                return Err(failure(crate::e2ee::EncryptionError::SnapshotMismatch));
            }
        }
        block_on_remote(remote.publish_observed_tag_ref(
            target_ref,
            &observed,
            Bytes::from(bytes),
        ))?;
        Ok(())
    }

    /// Publish the retained HEAD after dependency/ref publication. A symbolic
    /// target is verified before and after CAS; no global target fence is implied.
    #[cfg(unix)]
    pub fn publish_migration_target_head(
        &self,
        plan: &MigrationTargetPlan,
        remote: &crate::remote::Remote,
    ) -> Result<()> {
        remote.with_encrypted_operation(|| {
            self.validate_migration_target_plan(plan)?;
            self.publish_frozen_target_head(plan, remote)?;
            self.validate_migration_target_plan(plan)?;
            Ok(())
        })
    }

    #[cfg(unix)]
    fn publish_frozen_target_head(
        &self,
        plan: &MigrationTargetPlan,
        remote: &crate::remote::Remote,
    ) -> Result<()> {
        remote.require_encrypted_ref_journals()?;
        self.verify_migration_head_branch(plan, remote)?;
        let observed = block_on_remote(remote.observe_ref(HEAD_FILE))?;
        let replacement = plan.head.serialize();
        if observed
            .bytes()
            .is_some_and(|bytes| bytes.as_ref() != replacement.as_bytes())
        {
            return Err(failure(crate::e2ee::EncryptionError::StaleObservation));
        }
        block_on_remote(remote.publish_observed_head_ref(&observed, &plan.head))?;
        self.verify_migration_head_branch(plan, remote)?;
        Ok(())
    }

    /// Publish every frozen mapped branch/tag, followed by HEAD. Each mutation
    /// has durable ref recovery; the returned count is not migration progress.
    #[cfg(unix)]
    pub fn publish_migration_target_refs(
        &self,
        plan: &MigrationTargetPlan,
        remote: &crate::remote::Remote,
    ) -> Result<usize> {
        remote.with_encrypted_operation(|| {
            let source = self.restore_migration_source_plan(&plan.source)?;
            remote.require_encrypted_ref_journals()?;
            for target_ref in plan.refs.keys() {
                cancellation_checkpoint()?;
                self.validate_migration_source_plan(&source)?;
                if target_ref.starts_with("refs/heads/") {
                    self.publish_frozen_target_branch(plan, remote, target_ref)?;
                } else if target_ref.starts_with("refs/tags/") {
                    self.publish_frozen_target_tag(plan, &source, remote, target_ref)?;
                } else {
                    return Err(invalid("unsupported migration target ref kind"));
                }
            }
            self.validate_migration_source_plan(&source)?;
            self.publish_frozen_target_head(plan, remote)?;
            self.validate_migration_target_plan(plan)?;
            Ok(plan.refs.len())
        })
    }

    #[cfg(unix)]
    fn verify_migration_head_branch(
        &self,
        plan: &MigrationTargetPlan,
        remote: &crate::remote::Remote,
    ) -> Result<()> {
        if let Head::Branch { name } = &plan.head {
            let target_ref = format!("refs/heads/{name}");
            let target = plan
                .refs
                .get(&target_ref)
                .ok_or_else(|| invalid("HEAD branch is not in the frozen target mapping"))?;
            let expected = target
                .commit
                .as_ref()
                .map(|id| format!("{}\n", id.as_str()));
            let observed = block_on_remote(remote.observe_ref(&target_ref))?;
            if observed.bytes().map(Bytes::as_ref)
                != expected.as_ref().map(|value| value.as_bytes())
            {
                return Err(failure(crate::e2ee::EncryptionError::StaleObservation));
            }
        }
        Ok(())
    }

    fn read_migration_object(
        &self,
        id: &object::ObjectId,
        max_bytes: usize,
    ) -> Result<object::Object> {
        let limit = u64::try_from(max_bytes)
            .ok()
            .and_then(|value| value.checked_add(1))
            .ok_or_else(|| failure(crate::e2ee::EncryptionError::TooLarge))?;
        let mut bytes = Vec::new();
        fs::File::open(self.object_store().path_for(id))?
            .take(limit)
            .read_to_end(&mut bytes)?;
        if bytes.len() > max_bytes {
            return Err(failure(crate::e2ee::EncryptionError::TooLarge));
        }
        let actual = object::ObjectId::for_bytes(&bytes);
        if &actual != id {
            return Err(
                object::ObjectErr::ObjectIdMismatch { expected: id.clone(), actual }.into(),
            );
        }
        Ok(object::Object::decode(&bytes)?)
    }

    /// Reopen the exact frozen source from trusted private journal bytes. A changed
    /// source cannot be adopted by rebuilding its current HEAD or selected inventory.
    pub fn restore_migration_source_plan(&self, bytes: &[u8]) -> Result<MigrationSourcePlan> {
        if bytes.is_empty() || bytes.len() > 16 * 1024 * 1024 {
            return Err(failure(crate::e2ee::EncryptionError::TooLarge));
        }
        let record: SourcePlanRecord =
            serde_json::from_slice(bytes).map_err(|_| invalid("invalid private migration plan"))?;
        if record.version != 1
            || serde_json::to_vec(&record).map_err(|_| invalid("invalid private migration plan"))?
                != bytes
        {
            return Err(invalid("private migration plan is not canonical"));
        }
        if fs::canonicalize(&self.graft_dir)?.to_str() != Some(record.source.as_str())
            || self.head()? != record.head
            || self.head_target()? != record.head_target
        {
            return Err(failure(crate::e2ee::EncryptionError::StaleObservation));
        }
        if record.refs.windows(2).any(|pair| pair[0].0 >= pair[1].0) {
            return Err(invalid("private migration refs are not unique and sorted"));
        }
        let selection = if record.all {
            MigrationRefSelection::All
        } else {
            if record.refs.iter().any(|(_, target)| target.is_none()) {
                return Err(invalid("private migration selection has an unborn ref"));
            }
            MigrationRefSelection::Selected(
                record.refs.iter().map(|(name, _)| name.clone()).collect(),
            )
        };
        let refs = self.migration_source_refs(&selection, record.max_refs)?;
        if refs
            .iter()
            .map(|(name, target)| (name.clone(), target.clone()))
            .collect::<Vec<_>>()
            != record.refs
        {
            return Err(failure(crate::e2ee::EncryptionError::StaleObservation));
        }
        let restored = self.plan_migration_source(
            selection,
            MigrationPlanningBudget {
                max_refs: record.max_refs,
                max_objects: record.max_objects,
                max_object_bytes: record.max_object_bytes,
            },
        )?;
        if restored.private_record() != record {
            return Err(failure(crate::e2ee::EncryptionError::StaleObservation));
        }
        Ok(restored)
    }

    /// Reverify planned repository objects and declared external files. Rechecks frozen
    /// refs before/after; subsequent publication must retain or reverify bytes.
    pub fn verify_migration_file_dependencies(
        &self,
        plan: &MigrationSourcePlan,
        max_bytes: u64,
    ) -> Result<u64> {
        self.verified_migration_file_digests(plan, max_bytes)
            .map(|(total, _)| total)
    }

    /// Restore every frozen object and external payload through the encrypted
    /// target into an independent, newly initialized repository. The caller must
    /// also verify `SQLite` storage; repository objects alone are not a receipt.
    #[cfg(unix)]
    pub fn hydrate_migration_verification_objects(
        &self,
        plan: &MigrationSourcePlan,
        remote: &crate::remote::Remote,
        max_payload_bytes: u64,
    ) -> Result<u64> {
        let verifier = fs::canonicalize(&self.graft_dir)?;
        if verifier == plan.source
            || verifier.starts_with(&plan.source)
            || plan.source.starts_with(&verifier)
        {
            return Err(failure(crate::e2ee::EncryptionError::InvalidSessionBinding));
        }
        let operation = Arc::new(remote.begin_encrypted_operation()?);
        let probe = operation.clone();
        with_secret_cancellation(Arc::new(move || probe.check_active().is_err()), || {
            let total = plan
                .external_payloads
                .values()
                .try_fold(0u64, |total, size| {
                    total
                        .checked_add(*size)
                        .filter(|total| *total <= max_payload_bytes)
                        .ok_or_else(|| failure(crate::e2ee::EncryptionError::TooLarge))
                })?;
            let mut cache = RemoteObjectPackCache::persistent(
                self.graft_dir.join(DIR_CACHE_REMOTE_OBJECT_PACK_INDEXES),
            );
            for id in &plan.objects {
                cancellation_checkpoint()?;
                // Always fetch from the remote, even if an object happens to be
                // present locally. Authenticated packs are only routing hints.
                self.fetch_remote_object(remote, id, &mut cache)?;
                if fs::metadata(self.object_store().path_for(id))?.len()
                    > plan.max_object_bytes as u64
                {
                    return Err(failure(crate::e2ee::EncryptionError::TooLarge));
                }
            }
            for (id, size) in &plan.external_payloads {
                cancellation_checkpoint()?;
                if self.large_file_content_path(id).exists() {
                    return Err(failure(crate::e2ee::EncryptionError::StaleObservation));
                }
                self.fetch_large_file_content(remote, id, *size)?;
            }
            operation.check_active().map_err(failure)?;
            Ok(total)
        })
    }

    /// Authenticate every selected branch/tag (including expected absence for
    /// an unborn branch), then the exact selected HEAD. Extra target refs do not
    /// change this explicit selection.
    #[cfg(unix)]
    pub fn verify_migration_target_references(
        plan: &MigrationTargetPlan,
        remote: &crate::remote::Remote,
    ) -> Result<()> {
        let operation = remote.begin_encrypted_operation()?;
        for target in plan.refs.values() {
            operation.check_active().map_err(failure)?;
            let observed = block_on_remote(remote.observe_ref(&target.target_ref))?;
            let expected = target.object.as_ref().map(|id| format!("{id}\n"));
            if observed.bytes().map(|bytes| bytes.as_ref())
                != expected.as_deref().map(str::as_bytes)
            {
                return Err(failure(crate::e2ee::EncryptionError::StaleObservation));
            }
        }
        let head = block_on_remote(remote.observe_ref(HEAD_FILE))?;
        if head.bytes().map(|bytes| bytes.as_ref()) != Some(plan.head.serialize().as_ref()) {
            return Err(failure(crate::e2ee::EncryptionError::StaleObservation));
        }
        operation.check_active().map_err(failure)
    }

    fn verified_migration_file_digests(
        &self,
        plan: &MigrationSourcePlan,
        max_bytes: u64,
    ) -> Result<(u64, BTreeMap<object::ObjectId, [u8; 32]>)> {
        use sha2::{Digest, Sha256};
        self.validate_migration_source_plan(plan)?;
        let limit = u64::try_from(plan.max_object_bytes)
            .ok()
            .and_then(|value| value.checked_add(1))
            .ok_or_else(|| failure(crate::e2ee::EncryptionError::TooLarge))?;
        for id in &plan.objects {
            cancellation_checkpoint()?;
            let mut bytes = Vec::new();
            fs::File::open(self.object_store().path_for(id))?
                .take(limit)
                .read_to_end(&mut bytes)?;
            if bytes.len() > plan.max_object_bytes {
                return Err(failure(crate::e2ee::EncryptionError::TooLarge));
            }
            if object::ObjectId::for_bytes(&bytes) != *id {
                return Err(invalid(
                    "planned repository object no longer matches its identity",
                ));
            }
        }
        let total = plan
            .external_payloads
            .values()
            .try_fold(0u64, |total, size| {
                total
                    .checked_add(*size)
                    .filter(|total| *total <= max_bytes)
                    .ok_or_else(|| failure(crate::e2ee::EncryptionError::TooLarge))
            })?;
        let mut digests = BTreeMap::new();
        let mut buffer = [0u8; 64 * 1024];
        for (expected, size) in &plan.external_payloads {
            cancellation_checkpoint()?;
            let path = self.large_file_content_path(expected);
            if !fs::symlink_metadata(&path)?.is_file() {
                return Err(invalid("external payload is not an ordinary file"));
            }
            let mut file = fs::File::open(path)?;
            if file.metadata()?.len() != *size {
                return Err(invalid(
                    "external payload length does not match its declaration",
                ));
            }
            let mut hasher = blake3::Hasher::new();
            let mut sha = Sha256::new();
            let mut remaining = *size;
            while remaining > 0 {
                cancellation_checkpoint()?;
                let length = usize::try_from(remaining.min(buffer.len() as u64))
                    .map_err(|_| failure(crate::e2ee::EncryptionError::TooLarge))?;
                let read = file.read(&mut buffer[..length])?;
                if read == 0 {
                    return Err(invalid(
                        "external payload was truncated during verification",
                    ));
                }
                hasher.update(&buffer[..read]);
                sha.update(&buffer[..read]);
                remaining -= read as u64;
            }
            if file.read(&mut buffer[..1])? != 0 || file.metadata()?.len() != *size {
                return Err(invalid("external payload grew during verification"));
            }
            if hasher.finalize().to_hex().as_str() != expected.as_str() {
                return Err(invalid(
                    "external payload content hash does not match its declaration",
                ));
            }
            digests.insert(expected.clone(), sha.finalize().into());
        }
        self.validate_migration_source_plan(plan)?;
        Ok((total, digests))
    }

    /// Publish only the frozen plan's external file payloads to an encrypted
    /// target. No refs/stages are advanced; storage/repository transfer remains
    /// a separate prerequisite of final migration publication.
    #[cfg(unix)]
    pub fn publish_migration_file_dependencies(
        &self,
        plan: &MigrationSourcePlan,
        remote: &crate::remote::Remote,
        max_bytes: u64,
    ) -> Result<u64> {
        let operation = Arc::new(remote.begin_encrypted_operation()?);
        let probe = operation.clone();
        with_secret_cancellation(Arc::new(move || probe.check_active().is_err()), || {
            let (total, digests) = self.verified_migration_file_digests(plan, max_bytes)?;
            for (id, digest) in digests {
                cancellation_checkpoint()?;
                block_on_remote(remote.publish_verified_file(
                    &large_file_content_relative_path(&id),
                    &self.large_file_content_path(&id),
                    plan.external_payloads[&id],
                    digest,
                ))?;
            }
            self.validate_migration_source_plan(plan)?;
            operation.check_active().map_err(failure)?;
            Ok(total)
        })
    }

    /// Publish the frozen repository-object closure in bounded packs with
    /// authenticated routing hints. Does not publish refs or storage/files.
    #[cfg(unix)]
    pub fn publish_migration_repository_objects(
        &self,
        plan: &MigrationSourcePlan,
        remote: &crate::remote::Remote,
        max_pack_bytes: usize,
    ) -> Result<usize> {
        let operation = Arc::new(remote.begin_encrypted_operation()?);
        let probe = operation.clone();
        with_secret_cancellation(Arc::new(move || probe.check_active().is_err()), || {
            self.validate_migration_source_plan(plan)?;
            let header = REMOTE_OBJECT_PACK_MAGIC.len();
            if max_pack_bytes <= header {
                return Err(failure(crate::e2ee::EncryptionError::TooLarge));
            }
            // Validate every object and its individual fit before networking.
            for id in &plan.objects {
                let bytes = self.read_verified_migration_object(plan, id)?;
                if bytes.len() > max_pack_bytes - header {
                    return Err(failure(crate::e2ee::EncryptionError::TooLarge));
                }
            }
            self.validate_migration_source_plan(plan)?;
            let mut objects = BTreeMap::new();
            let mut size = header;
            for id in &plan.objects {
                let bytes = self.read_verified_migration_object(plan, id)?;
                if !objects.is_empty()
                    && (objects.len() == 512 || bytes.len() > max_pack_bytes - size)
                {
                    let pack = self
                        .prepare_object_pack(std::mem::take(&mut objects), vec![])?
                        .ok_or_else(|| invalid("migration pack is empty"))?;
                    block_on_remote(remote.publish_encrypted_object_pack(&pack))?;
                    size = header;
                }
                size = size
                    .checked_add(bytes.len())
                    .filter(|size| *size <= max_pack_bytes)
                    .ok_or_else(|| failure(crate::e2ee::EncryptionError::TooLarge))?;
                objects.insert(id.clone(), bytes);
            }
            if let Some(pack) = self.prepare_object_pack(objects, vec![])? {
                block_on_remote(remote.publish_encrypted_object_pack(&pack))?;
            }
            self.validate_migration_source_plan(plan)?;
            operation.check_active().map_err(failure)?;
            Ok(plan.objects.len())
        })
    }

    #[cfg(unix)]
    fn read_verified_migration_object(
        &self,
        plan: &MigrationSourcePlan,
        id: &object::ObjectId,
    ) -> Result<Vec<u8>> {
        cancellation_checkpoint()?;
        let limit = u64::try_from(plan.max_object_bytes)
            .ok()
            .and_then(|limit| limit.checked_add(1))
            .ok_or_else(|| failure(crate::e2ee::EncryptionError::TooLarge))?;
        let mut bytes = Vec::new();
        fs::File::open(self.object_store().path_for(id))?
            .take(limit)
            .read_to_end(&mut bytes)?;
        if bytes.len() > plan.max_object_bytes {
            return Err(failure(crate::e2ee::EncryptionError::TooLarge));
        }
        if object::ObjectId::for_bytes(&bytes) != *id {
            return Err(invalid(
                "planned repository object no longer matches its identity",
            ));
        }
        Ok(bytes)
    }

    /// Freeze selected refs and verify every reachable local repository object.
    /// Does not hydrate storage, probe remotes, move refs or materialize files.
    pub fn plan_migration_source(
        &self,
        selection: MigrationRefSelection,
        budget: MigrationPlanningBudget,
    ) -> Result<MigrationSourcePlan> {
        if budget.max_refs == 0 || budget.max_objects == 0 || budget.max_object_bytes == 0 {
            return Err(failure(crate::e2ee::EncryptionError::TooLarge));
        }
        let references = self.migration_source_refs(&selection, budget.max_refs)?;
        let head = self.head()?;
        let head_target = self.head_target()?;
        let mut requirements = BTreeMap::new();
        let mut kinds = BTreeMap::new();
        let mut pending = VecDeque::new();
        for (reference, target) in &references {
            if let Some(target) = target {
                enqueue(
                    object::ObjectId::from_str(target)?,
                    if reference.starts_with("refs/tags/") {
                        1 | 8
                    } else {
                        1
                    },
                    &mut requirements,
                    &kinds,
                    &mut pending,
                    budget.max_objects,
                )?;
            }
        }
        // A selected-ref migration does not silently include unrelated HEAD history.
        if matches!(selection, MigrationRefSelection::All)
            && let Some(target) = &head_target
        {
            enqueue(
                object::ObjectId::from_str(target)?,
                1,
                &mut requirements,
                &kinds,
                &mut pending,
                budget.max_objects,
            )?;
        }
        let mut snapshots = BTreeMap::new();
        let mut external_payloads = BTreeMap::new();
        let read_limit = u64::try_from(budget.max_object_bytes)
            .ok()
            .and_then(|limit| limit.checked_add(1))
            .ok_or_else(|| failure(crate::e2ee::EncryptionError::TooLarge))?;
        while let Some(id) = pending.pop_front() {
            cancellation_checkpoint()?;
            let file = fs::File::open(self.object_store().path_for(&id))?;
            let mut bytes = Vec::new();
            file.take(read_limit).read_to_end(&mut bytes)?;
            if bytes.len() > budget.max_object_bytes {
                return Err(failure(crate::e2ee::EncryptionError::TooLarge));
            }
            let actual = object::ObjectId::for_bytes(&bytes);
            if actual != id {
                return Err(object::ObjectErr::ObjectIdMismatch { expected: id, actual }.into());
            }
            let value = object::Object::decode(&bytes)?;
            if kind_mask(value.kind()) & requirements[&id] == 0 {
                return Err(invalid("object kind disagrees with a dependency"));
            }
            kinds.insert(id.clone(), value.kind());
            match value {
                object::Object::Commit(commit) => {
                    enqueue(
                        commit.tree,
                        2,
                        &mut requirements,
                        &kinds,
                        &mut pending,
                        budget.max_objects,
                    )?;
                    for parent in commit.parents {
                        enqueue(
                            parent,
                            1,
                            &mut requirements,
                            &kinds,
                            &mut pending,
                            budget.max_objects,
                        )?;
                    }
                }
                object::Object::Tree(tree) => {
                    for entry in tree.entries {
                        enqueue(
                            entry.oid,
                            4,
                            &mut requirements,
                            &kinds,
                            &mut pending,
                            budget.max_objects,
                        )?;
                    }
                }
                object::Object::Tag(tag) => {
                    if tag.object_type != object::ObjectKind::Commit {
                        return Err(invalid("tag does not identify a commit"));
                    }
                    enqueue(
                        tag.object,
                        1,
                        &mut requirements,
                        &kinds,
                        &mut pending,
                        budget.max_objects,
                    )?;
                }
                object::Object::Blob(object::BlobObject::SqliteSnapshot(snapshot)) => {
                    snapshots.insert(id, snapshot);
                }
                object::Object::Blob(object::BlobObject::LargeFilePointer(pointer)) => {
                    if external_payloads
                        .insert(pointer.content_hash, pointer.size)
                        .is_some_and(|size| size != pointer.size)
                    {
                        return Err(invalid("payload has conflicting declared sizes"));
                    }
                }
                object::Object::Blob(object::BlobObject::File(_)) => {}
            }
        }
        let plan = MigrationSourcePlan {
            source: fs::canonicalize(&self.graft_dir)?,
            max_refs: budget.max_refs,
            max_object_bytes: budget.max_object_bytes,
            max_objects: budget.max_objects,
            selection,
            head,
            head_target,
            references,
            objects: kinds.into_keys().collect(),
            snapshots,
            external_payloads,
        };
        self.validate_migration_source_plan(&plan)?;
        Ok(plan)
    }

    /// Recheck frozen source refs before preparation/publication or final cutover.
    /// This is a change detector; it does not fence concurrent or offline writers.
    pub fn validate_migration_source_plan(&self, plan: &MigrationSourcePlan) -> Result<()> {
        cancellation_checkpoint()?;
        if fs::canonicalize(&self.graft_dir)? != plan.source
            || self.head()? != plan.head
            || self.head_target()? != plan.head_target
            || self.migration_source_refs(&plan.selection, plan.max_refs)? != plan.references
        {
            return Err(failure(crate::e2ee::EncryptionError::StaleObservation));
        }
        Ok(())
    }

    fn migration_source_refs(
        &self,
        selection: &MigrationRefSelection,
        limit: usize,
    ) -> Result<BTreeMap<String, Option<String>>> {
        let mut refs = BTreeMap::new();
        match selection {
            MigrationRefSelection::Selected(names) => {
                if names.is_empty() || names.len() > limit {
                    return Err(failure(crate::e2ee::EncryptionError::TooLarge));
                }
                for name in names {
                    validate_full_ref(name)?;
                    let target = self
                        .migration_ref_target(name)?
                        .ok_or_else(|| invalid("selected ref has no target"))?;
                    if refs.insert(name.clone(), Some(target)).is_some() {
                        return Err(invalid("duplicate selected ref"));
                    }
                }
            }
            MigrationRefSelection::All => {
                let mut directories = vec![self.graft_dir.join("refs")];
                let max_entries = limit
                    .checked_mul(4)
                    .and_then(|value| value.checked_add(3))
                    .ok_or_else(|| failure(crate::e2ee::EncryptionError::TooLarge))?;
                let mut entries = 0usize;
                while let Some(directory) = directories.pop() {
                    cancellation_checkpoint()?;
                    if !directory.try_exists()? {
                        continue;
                    }
                    if !fs::symlink_metadata(&directory)?.is_dir() {
                        return Err(invalid("ref directory is not an ordinary directory"));
                    }
                    for entry in fs::read_dir(&directory)? {
                        cancellation_checkpoint()?;
                        if entries >= max_entries {
                            return Err(failure(crate::e2ee::EncryptionError::TooLarge));
                        }
                        entries += 1;
                        let entry = entry?;
                        let kind = entry.file_type()?;
                        if kind.is_dir() {
                            if directories.len() >= limit {
                                return Err(failure(crate::e2ee::EncryptionError::TooLarge));
                            }
                            directories.push(entry.path());
                        } else if kind.is_file() {
                            if refs.len() >= limit {
                                return Err(failure(crate::e2ee::EncryptionError::TooLarge));
                            }
                            let path = entry.path();
                            let name = path
                                .strip_prefix(&self.graft_dir)
                                .map_err(|_| invalid("ref is outside source repository"))?
                                .to_str()
                                .ok_or_else(|| invalid("ref name is not UTF-8"))?
                                .replace('\\', "/");
                            validate_full_ref(&name)?;
                            refs.insert(name.clone(), self.migration_ref_target(&name)?);
                        } else {
                            return Err(invalid("ref is not an ordinary file or directory"));
                        }
                    }
                }
                // An unborn symbolic HEAD has no ref file yet, but its chosen
                // branch identity must survive an all-source migration.
                if let Head::Branch { name } = self.head()?
                    && self.head_target()?.is_none()
                {
                    let reference = format!("refs/heads/{name}");
                    if !refs.contains_key(&reference) {
                        if refs.len() >= limit {
                            return Err(failure(crate::e2ee::EncryptionError::TooLarge));
                        }
                        refs.insert(reference, None);
                    }
                }
            }
        }
        Ok(refs)
    }

    fn migration_ref_target(&self, name: &str) -> Result<Option<String>> {
        validate_full_ref(name)?;
        let path = self.graft_dir.join(name);
        let metadata = fs::symlink_metadata(&path)?;
        if !metadata.is_file() {
            return Err(invalid("ref is not an ordinary file"));
        }
        let file = fs::File::open(path)?;
        let mut bytes = Vec::new();
        file.take(4097).read_to_end(&mut bytes)?;
        if bytes.len() > 4096 {
            return Err(failure(crate::e2ee::EncryptionError::TooLarge));
        }
        let raw = std::str::from_utf8(&bytes).map_err(|_| invalid("ref is not UTF-8"))?;
        let target = raw.trim();
        Ok((!target.is_empty()).then(|| target.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(repo: &Repository, text: &str) -> String {
        let path = repo.worktree().join("migration.txt");
        fs::write(&path, text).unwrap();
        repo.stage_artifact_path(&path).unwrap();
        repo.commit_staged(text).unwrap().id
    }
    fn plan(repo: &Repository, selection: MigrationRefSelection) -> MigrationSourcePlan {
        repo.plan_migration_source(selection, MigrationPlanningBudget::default())
            .unwrap()
    }

    #[test]
    fn migration_target_private_record_restores_exact_mapping_and_rejects_malformed_intent() {
        let directory = tempfile::tempdir().unwrap();
        let repo = Repository::init(directory.path()).unwrap();
        let id = commit(&repo, "retained intent");
        let tag = repo
            .tag_create_annotated("release", Some(&id), "annotation")
            .unwrap();
        let source = plan(&repo, MigrationRefSelection::All);
        let target = repo
            .plan_migration_target_refs(
                &source,
                vec![
                    ("refs/tags/release".into(), "refs/tags/renamed".into()),
                    ("refs/heads/main".into(), "refs/heads/new-main".into()),
                ],
                Head::branch("new-main"),
            )
            .unwrap();
        let bytes = target.encode_private().unwrap();
        let restored = repo.restore_migration_target_plan(&bytes).unwrap();
        assert_eq!(restored.encode_private().unwrap(), bytes);
        assert_eq!(restored.head(), target.head());
        assert_eq!(
            restored.references()["refs/tags/renamed"]
                .object
                .as_ref()
                .unwrap()
                .as_str(),
            tag.object
        );
        let mut trailing = bytes.clone();
        trailing.push(b' ');
        assert!(repo.restore_migration_target_plan(&trailing).is_err());
        assert!(
            repo.restore_migration_target_plan(&bytes[..bytes.len() / 2])
                .is_err()
        );
        assert!(
            repo.restore_migration_target_plan(&source.encode_private().unwrap())
                .is_err()
        );
        for change in 0..5 {
            let mut record: TargetPlanRecord = serde_json::from_slice(&bytes).unwrap();
            match change {
                0 => record.version = 2,
                1 => record.mappings.reverse(),
                2 => record.mappings[1].1 = record.mappings[0].1.clone(),
                3 => record.head = Head::branch("outside"),
                _ => record.source.closure[0] ^= 1,
            }
            assert!(
                repo.restore_migration_target_plan(&serde_json::to_vec(&record).unwrap())
                    .is_err()
            );
        }
        commit(&repo, "later source");
        assert!(repo.restore_migration_target_plan(&bytes).is_err());
    }

    #[test]
    fn migration_target_plan_retains_unborn_branch_and_detached_selected_commit() {
        let directory = tempfile::tempdir().unwrap();
        let repo = Repository::init(directory.path()).unwrap();
        let empty = plan(&repo, MigrationRefSelection::All);
        let mapping = vec![("refs/heads/main".into(), "refs/heads/empty".into())];
        let target = repo
            .plan_migration_target_refs(&empty, mapping, Head::branch("empty"))
            .unwrap();
        assert!(target.references()["refs/heads/empty"].object.is_none());
        assert!(target.references()["refs/heads/empty"].commit.is_none());
        let id = commit(&repo, "tag history");
        repo.tag_create_annotated("only", Some(&id), "annotation")
            .unwrap();
        let selected = plan(
            &repo,
            MigrationRefSelection::Selected(vec!["refs/tags/only".into()]),
        );
        let target = repo
            .plan_migration_target_refs(
                &selected,
                vec![("refs/tags/only".into(), "refs/tags/renamed".into())],
                Head::Detached { commit: id.clone() },
            )
            .unwrap();
        assert_eq!(target.head(), &Head::Detached { commit: id });
        let long = vec![(
            "refs/tags/only".into(),
            format!("refs/tags/{}", "x".repeat(4096)),
        )];
        assert!(
            repo.plan_migration_target_refs(&selected, long, target.head().clone())
                .is_err()
        );
        let token = CancellationToken::new();
        token.cancel();
        assert!(
            with_cancellation(&token, || repo.plan_migration_target_refs(
                &selected,
                vec![("refs/tags/only".into(), "refs/tags/renamed".into())],
                target.head().clone()
            ))
            .is_err()
        );
    }

    #[test]
    fn migration_target_mapping_preserves_tags_and_rejects_collisions_and_unselected_head() {
        let directory = tempfile::tempdir().unwrap();
        let repo = Repository::init(directory.path()).unwrap();
        let base = commit(&repo, "base");
        repo.branch_create("side", Some(&base)).unwrap();
        repo.switch_branch("side").unwrap();
        let side = commit(&repo, "side");
        repo.switch_branch("main").unwrap();
        let tag = repo
            .tag_create_annotated("release", Some(&side), "keep annotation")
            .unwrap();
        repo.tag_create("light", Some(&base)).unwrap();
        repo.set_remote_tracking_ref("old", "archived", &side)
            .unwrap();
        let all = plan(&repo, MigrationRefSelection::All);
        let mappings: Vec<_> = all
            .references()
            .keys()
            .map(|name| {
                (
                    name.clone(),
                    if name.starts_with("refs/remotes/") {
                        "refs/heads/imported/archive".into()
                    } else {
                        name.clone()
                    },
                )
            })
            .collect();
        let target = repo
            .plan_migration_target_refs(&all, mappings.clone(), Head::branch("main"))
            .unwrap();
        let retained_tag = &target.references()["refs/tags/release"];
        assert_eq!(retained_tag.object.as_ref().unwrap().as_str(), tag.object);
        assert_eq!(retained_tag.commit.as_ref().unwrap().as_str(), side);
        assert!(retained_tag.annotated_tag);
        assert!(!target.references()["refs/tags/light"].annotated_tag);
        assert_eq!(
            target.references()["refs/heads/imported/archive"].source_ref,
            "refs/remotes/old/archived"
        );
        repo.validate_migration_target_plan(&target).unwrap();
        let mut missing = mappings.clone();
        missing.pop();
        assert!(
            repo.plan_migration_target_refs(&all, missing, Head::branch("main"))
                .is_err()
        );
        for replacement in [
            "refs/heads/main",
            "refs/heads/main/child",
            "refs/remotes/new/archive",
            "refs/tags/converted",
            "refs/heads/../escape",
        ] {
            let mut invalid_mapping = mappings.clone();
            invalid_mapping
                .iter_mut()
                .find(|(from, _)| from.starts_with("refs/remotes/"))
                .unwrap()
                .1 = replacement.into();
            assert!(
                repo.plan_migration_target_refs(&all, invalid_mapping, Head::branch("main"))
                    .is_err(),
                "{replacement}"
            );
        }
        let mut duplicate = mappings.clone();
        duplicate[1].0 = duplicate[0].0.clone();
        assert!(
            repo.plan_migration_target_refs(&all, duplicate, Head::branch("main"))
                .is_err()
        );
        assert!(
            repo.plan_migration_target_refs(&all, mappings.clone(), Head::branch("absent"))
                .is_err()
        );
        assert!(
            repo.plan_migration_target_refs(&all, mappings, Head::Detached { commit: tag.object })
                .is_err()
        );
        let selected = plan(
            &repo,
            MigrationRefSelection::Selected(vec!["refs/heads/main".into()]),
        );
        let mapping = vec![("refs/heads/main".into(), "refs/heads/renamed".into())];
        assert!(
            repo.plan_migration_target_refs(
                &selected,
                mapping.clone(),
                Head::Detached { commit: side }
            )
            .is_err()
        );
        let renamed = repo
            .plan_migration_target_refs(&selected, mapping, Head::branch("renamed"))
            .unwrap();
        assert_eq!(renamed.head(), &Head::branch("renamed"));
        commit(&repo, "changed source");
        assert!(repo.validate_migration_target_plan(&target).is_err());
        assert!(repo.validate_migration_target_plan(&renamed).is_err());
    }

    #[test]
    fn migration_private_plan_restores_exact_source_and_rejects_changes() {
        let directory = tempfile::tempdir().unwrap();
        let repo = Repository::init(directory.path()).unwrap();
        commit(&repo, "retained source");
        let frozen = plan(&repo, MigrationRefSelection::All);
        let bytes = frozen.encode_private().unwrap();
        let restored = repo.restore_migration_source_plan(&bytes).unwrap();
        assert_eq!(restored.encode_private().unwrap(), bytes);
        assert_eq!(restored.objects(), frozen.objects());
        let mut noncanonical = bytes.clone();
        noncanonical.push(b' ');
        assert!(repo.restore_migration_source_plan(&noncanonical).is_err());
        assert!(
            repo.restore_migration_source_plan(&bytes[..bytes.len() / 2])
                .is_err()
        );
        let other = tempfile::tempdir().unwrap();
        let other_repo = Repository::init(other.path()).unwrap();
        assert!(other_repo.restore_migration_source_plan(&bytes).is_err());
        let selected = plan(
            &repo,
            MigrationRefSelection::Selected(vec!["refs/heads/main".into()]),
        );
        let selected_bytes = selected.encode_private().unwrap();
        repo.branch_create("later-unselected", None).unwrap();
        assert!(repo.restore_migration_source_plan(&bytes).is_err());
        assert!(repo.restore_migration_source_plan(&selected_bytes).is_ok());
        commit(&repo, "later HEAD");
        assert!(repo.restore_migration_source_plan(&selected_bytes).is_err());
    }

    #[test]
    fn migration_source_preserves_tag_only_and_remote_only_history_and_selected_scope() {
        let directory = tempfile::tempdir().unwrap();
        let repo = Repository::init(directory.path()).unwrap();
        let base = commit(&repo, "base");
        let main = commit(&repo, "main");
        repo.branch_create("tag-only", Some(&base)).unwrap();
        repo.switch_branch("tag-only").unwrap();
        let side = commit(&repo, "tag-only commit");
        let tag = repo
            .tag_create_annotated("release", Some(&side), "retained annotation")
            .unwrap();
        repo.switch_branch("main").unwrap();
        repo.branch_delete("tag-only", true).unwrap();
        repo.branch_create("remote-only", Some(&base)).unwrap();
        repo.switch_branch("remote-only").unwrap();
        let remote = commit(&repo, "remote-only commit");
        repo.set_remote_tracking_ref("old", "archived", &remote)
            .unwrap();
        repo.switch_branch("main").unwrap();
        repo.branch_delete("remote-only", true).unwrap();
        let before_head = repo.head().unwrap();
        let before_file = fs::read(directory.path().join("migration.txt")).unwrap();
        let all = plan(&repo, MigrationRefSelection::All);
        for id in [&base, &main, &side, &remote, &tag.object] {
            assert!(
                all.objects()
                    .contains(&object::ObjectId::from_str(id).unwrap())
            );
        }
        assert_eq!(
            all.references()["refs/tags/release"].as_deref(),
            Some(tag.object.as_str())
        );
        assert_eq!(
            all.references()["refs/remotes/old/archived"].as_deref(),
            Some(remote.as_str())
        );
        let selected = plan(
            &repo,
            MigrationRefSelection::Selected(vec!["refs/heads/main".into()]),
        );
        assert!(
            selected
                .objects()
                .contains(&object::ObjectId::from_str(&base).unwrap())
        );
        for id in [&side, &remote, &tag.object] {
            assert!(
                !selected
                    .objects()
                    .contains(&object::ObjectId::from_str(id).unwrap())
            );
        }
        assert_eq!(repo.head().unwrap(), before_head);
        assert_eq!(
            fs::read(directory.path().join("migration.txt")).unwrap(),
            before_file
        );
        let tag_selected = plan(
            &repo,
            MigrationRefSelection::Selected(vec!["refs/tags/release".into()]),
        );
        assert!(
            tag_selected
                .objects()
                .contains(&object::ObjectId::from_str(&tag.object).unwrap())
        );
        assert!(
            !tag_selected
                .objects()
                .contains(&object::ObjectId::from_str(&main).unwrap())
        );
        repo.tag_delete("release").unwrap();
        let replaced = repo
            .tag_create_annotated("release", Some(&side), "changed annotation")
            .unwrap();
        assert_ne!(replaced.object, tag.object);
        assert_eq!(replaced.target, tag.target);
        assert!(repo.validate_migration_source_plan(&all).is_err());
        assert!(repo.validate_migration_source_plan(&tag_selected).is_err());
        repo.validate_migration_source_plan(&selected).unwrap();
        repo.tag_create("later", Some(&main)).unwrap();
        assert!(repo.validate_migration_source_plan(&all).is_err());
        repo.validate_migration_source_plan(&selected).unwrap();
        commit(&repo, "changed source");
        assert!(repo.validate_migration_source_plan(&selected).is_err());
    }

    #[test]
    fn migration_source_rejects_missing_and_corrupt_ancestor_objects_without_changing_worktree() {
        let directory = tempfile::tempdir().unwrap();
        let repo = Repository::init(directory.path()).unwrap();
        let ancestor = commit(&repo, "ancestor");
        commit(&repo, "current");
        let path = repo
            .object_store()
            .path_for(&object::ObjectId::from_str(&ancestor).unwrap());
        let bytes = fs::read(&path).unwrap();
        let head = repo.head_target().unwrap();
        let file = fs::read(directory.path().join("migration.txt")).unwrap();
        fs::remove_file(&path).unwrap();
        assert!(
            repo.plan_migration_source(
                MigrationRefSelection::All,
                MigrationPlanningBudget::default()
            )
            .is_err()
        );
        fs::write(&path, b"corrupt ancestor").unwrap();
        assert!(
            repo.plan_migration_source(
                MigrationRefSelection::All,
                MigrationPlanningBudget::default()
            )
            .is_err()
        );
        fs::write(&path, bytes).unwrap();
        plan(&repo, MigrationRefSelection::All);
        assert_eq!(repo.head_target().unwrap(), head);
        assert_eq!(
            fs::read(directory.path().join("migration.txt")).unwrap(),
            file
        );
    }

    #[test]
    fn migration_source_budgets_bad_selections_and_cancellation_fail_closed() {
        let directory = tempfile::tempdir().unwrap();
        let repo = Repository::init(directory.path()).unwrap();
        commit(&repo, "source");
        for refs in [
            vec![],
            vec!["main".into()],
            vec!["refs/heads/missing".into()],
            vec!["refs/heads/main".into(), "refs/heads/main".into()],
        ] {
            assert!(
                repo.plan_migration_source(
                    MigrationRefSelection::Selected(refs),
                    MigrationPlanningBudget::default()
                )
                .is_err()
            );
        }
        for budget in [
            MigrationPlanningBudget { max_refs: 0, ..Default::default() },
            MigrationPlanningBudget { max_objects: 1, ..Default::default() },
            MigrationPlanningBudget {
                max_object_bytes: 8,
                ..Default::default()
            },
        ] {
            assert!(
                repo.plan_migration_source(MigrationRefSelection::All, budget)
                    .is_err()
            );
        }
        let token = CancellationToken::new();
        token.cancel();
        assert!(matches!(
            with_cancellation(&token, || repo.plan_migration_source(
                MigrationRefSelection::All,
                MigrationPlanningBudget::default()
            )),
            Err(RepoErr::Cancelled)
        ));
    }

    #[test]
    #[cfg(unix)]
    fn migration_secret_cancellation_stops_traversal_and_restores_worker_scope() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let directory = tempfile::tempdir().unwrap();
        let repo = Repository::init(directory.path()).unwrap();
        commit(&repo, "cancellable source");
        let source = repo
            .plan_migration_source(
                MigrationRefSelection::All,
                MigrationPlanningBudget::default(),
            )
            .unwrap()
            .encode_private()
            .unwrap();
        let checks = Arc::new(AtomicUsize::new(0));
        let probe = checks.clone();
        assert!(matches!(
            with_secret_cancellation(
                Arc::new(move || probe.fetch_add(1, Ordering::SeqCst) >= 2),
                || { repo.restore_migration_source_plan(&source) }
            ),
            Err(RepoErr::Cancelled)
        ));
        assert!(checks.load(Ordering::SeqCst) >= 3);
        assert_eq!(
            repo.restore_migration_source_plan(&source)
                .unwrap()
                .encode_private()
                .unwrap(),
            source
        );
        with_secret_cancellation(Arc::new(|| true), || {
            assert!(matches!(cancellation_checkpoint(), Err(RepoErr::Cancelled)));
            with_secret_cancellation(Arc::new(|| false), || cancellation_checkpoint().unwrap());
            assert!(matches!(cancellation_checkpoint(), Err(RepoErr::Cancelled)));
        });
        cancellation_checkpoint().unwrap();
    }

    #[test]
    fn migration_source_rejects_valid_hash_objects_with_wrong_dependency_kinds() {
        let directory = tempfile::tempdir().unwrap();
        let repo = Repository::init(directory.path()).unwrap();
        let source = commit(&repo, "valid source");
        let object::Object::Commit(mut value) = repo.read_object(&source).unwrap() else {
            panic!("expected source commit");
        };
        let object::Object::Tree(tree) = repo.read_object(value.tree.as_str()).unwrap() else {
            panic!("expected source tree");
        };
        value.tree = tree.entries[0].oid.clone();
        let wrong = repo
            .object_store()
            .write(&object::Object::Commit(value))
            .unwrap();
        repo.write_ref("refs/heads/main", wrong.as_str()).unwrap();
        assert!(
            repo.plan_migration_source(
                MigrationRefSelection::All,
                MigrationPlanningBudget::default()
            )
            .is_err()
        );
        repo.write_ref("refs/heads/main", &source).unwrap();
        let tag = repo
            .tag_create_annotated("malformed", Some(&source), "annotation")
            .unwrap();
        let object::Object::Tag(mut value) = repo.read_object(&tag.object).unwrap() else {
            panic!("expected annotated tag");
        };
        value.object = tree.entries[0].oid.clone();
        let wrong = repo
            .object_store()
            .write(&object::Object::Tag(value))
            .unwrap();
        repo.write_ref("refs/tags/malformed", wrong.as_str())
            .unwrap();
        assert!(
            repo.plan_migration_source(
                MigrationRefSelection::All,
                MigrationPlanningBudget::default()
            )
            .is_err()
        );
    }
}
