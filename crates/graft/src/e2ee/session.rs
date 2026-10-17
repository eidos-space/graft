use super::{
    CheckpointStore, ConfigCheckpoint, EncryptedConfig, EncryptedRef, EncryptionError, RefTarget,
    RememberedKeyProvider, RepositoryId, Result, RootKey, random, reference,
};
use parking_lot::Mutex;
use std::{
    collections::{BTreeMap, VecDeque},
    fmt,
    sync::Arc,
};
use tokio::sync::Notify;
use zeroize::Zeroizing;

// Wait asynchronously before allocating a blocking KDF task or its memory.
static UNLOCK_GATE: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

/// Exact host identity for future secure-storage providers. Never normalize it.
pub struct SessionBinding {
    domain: String,
    account: String,
    repository: RepositoryId,
}

impl SessionBinding {
    pub fn new(domain: String, account: String, repository: RepositoryId) -> Result<Self> {
        for value in [&domain, &account] {
            if value.is_empty() || value.len() > 4096 || value.chars().any(char::is_control) {
                return Err(EncryptionError::InvalidSessionBinding);
            }
        }
        Ok(Self { domain, account, repository })
    }

    pub fn domain(&self) -> &str {
        &self.domain
    }
    pub fn account(&self) -> &str {
        &self.account
    }
    pub fn repository(&self) -> RepositoryId {
        self.repository
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecretSessionState {
    Disconnected,
    Locked,
    Unlocking,
    Unlocked,
    Locking,
}

struct State {
    owners: usize,
    phase: SecretSessionState,
    close_target: SecretSessionState,
    generation: u64,
    paused: bool,
    root: Option<Arc<RootKey>>,
    checkpoint: Option<ConfigCheckpoint>,
    tasks: usize,
    discovery_headers: BTreeMap<(String, String), (Vec<u8>, u64)>,
    discovery_order: VecDeque<(String, String)>,
    discovery_bytes: usize,
    verified_graph_heads: BTreeMap<String, String>,
    verified_graph_order: VecDeque<String>,
}

struct Inner {
    creator_pid: u32,
    id: [u8; 32],
    binding: SessionBinding,
    state: Mutex<State>,
    checkpoint_gate: Mutex<()>,
    changed: Notify,
    checkpoint_store: Option<Arc<dyn CheckpointStore>>,
}

/// Shared native session. No key/password serialization or provider auto-reload.
/// Bound to its creating process; inherited handles after `fork` cannot admit work.
pub struct SecretSession {
    inner: Arc<Inner>,
}

impl Clone for SecretSession {
    fn clone(&self) -> Self {
        if self.inner.creator_pid != std::process::id() {
            return Self { inner: self.inner.clone() };
        }
        let mut state = self.inner.state.lock();
        state.owners = state.owners.checked_add(1).expect("session owner count");
        Self { inner: self.inner.clone() }
    }
}

impl Drop for SecretSession {
    fn drop(&mut self) {
        if self.inner.creator_pid != std::process::id() {
            return;
        }
        {
            let mut state = self.inner.state.lock();
            state.owners = state.owners.checked_sub(1).expect("session owner count");
            if state.owners == 0 {
                state.close_target = SecretSessionState::Disconnected;
                state.phase = SecretSessionState::Locking;
                complete_close(&mut state);
            }
        }
        self.inner.changed.notify_waiters();
    }
}

impl fmt::Debug for SecretSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecretSession")
            .field("repository", &self.inner.binding.repository)
            .field("state", &self.state())
            .field("secrets", &"[REDACTED]")
            .finish()
    }
}

impl SecretSession {
    pub fn new(binding: SessionBinding, checkpoint: Option<ConfigCheckpoint>) -> Result<Self> {
        Self::construct(binding, checkpoint, None)
    }

    /// Blocking host-private startup. The store distinguishes explicit new enrollment
    /// from lost existing trust and binds metadata to this exact account/domain/UUID.
    pub fn with_checkpoint_store(
        binding: SessionBinding,
        store: Arc<dyn CheckpointStore>,
    ) -> Result<Self> {
        let checkpoint = store.load(&binding)?;
        Self::construct(binding, checkpoint, Some(store))
    }

    fn construct(
        binding: SessionBinding,
        checkpoint: Option<ConfigCheckpoint>,
        checkpoint_store: Option<Arc<dyn CheckpointStore>>,
    ) -> Result<Self> {
        if checkpoint
            .as_ref()
            .is_some_and(|value| value.repository() != binding.repository)
        {
            return Err(EncryptionError::IdentityChanged);
        }
        Ok(Self {
            inner: Arc::new(Inner {
                creator_pid: std::process::id(),
                id: random::<32>()?,
                binding,
                state: Mutex::new(State {
                    owners: 1,
                    phase: SecretSessionState::Locked,
                    close_target: SecretSessionState::Locked,
                    generation: 0,
                    paused: false,
                    root: None,
                    checkpoint,
                    tasks: 0,
                    discovery_headers: BTreeMap::new(),
                    discovery_order: VecDeque::new(),
                    discovery_bytes: 0,
                    verified_graph_heads: BTreeMap::new(),
                    verified_graph_order: VecDeque::new(),
                }),
                changed: Notify::new(),
                checkpoint_gate: Mutex::new(()),
                checkpoint_store,
            }),
        })
    }

    pub fn state(&self) -> SecretSessionState {
        if self.inner.creator_pid != std::process::id() {
            return SecretSessionState::Disconnected;
        }
        self.inner.state.lock().phase
    }
    pub fn binding(&self) -> &SessionBinding {
        &self.inner.binding
    }
    pub fn checkpoint(&self) -> Option<ConfigCheckpoint> {
        if self.inner.creator_pid != std::process::id() {
            return None;
        }
        self.inner.state.lock().checkpoint.clone()
    }
    pub fn is_paused(&self) -> bool {
        if self.inner.creator_pid != std::process::id() {
            return true;
        }
        self.inner.state.lock().paused
    }

    /// KDF buffers and task ownership remain registered even if this future is dropped.
    pub async fn unlock(
        &self,
        config: EncryptedConfig,
        password: Zeroizing<Vec<u8>>,
    ) -> Result<()> {
        if config.repository() != self.inner.binding.repository {
            return Err(EncryptionError::IdentityChanged);
        }
        let input = self.begin_password_unlock(password)?;
        self.unlock_request(config, input).await
    }

    pub(crate) fn begin_password_unlock(
        &self,
        password: Zeroizing<Vec<u8>>,
    ) -> Result<PasswordUnlock> {
        if self.inner.creator_pid != std::process::id() {
            return Err(EncryptionError::Cancelled);
        }
        super::config::validate_password(&password)?;
        let ticket = self.start_unlock()?;
        Ok(PasswordUnlock { password, ticket })
    }

    /// Prepare or reopen a native generation-1 initialization journal. This is
    /// explicit trusted-host enrollment, not online unlock or remote publication.
    /// Admission happens synchronously; KDF, root generation and persistence stay
    /// owned by the unlock ticket even if the returned future is abandoned.
    #[cfg(unix)]
    pub fn prepare_native_initial_config(
        &self,
        password: Zeroizing<Vec<u8>>,
        directory: std::path::PathBuf,
        create: bool,
    ) -> Result<
        impl std::future::Future<Output = Result<Arc<super::FileInitializationJournal>>>
        + Send
        + 'static,
    > {
        self.prepare_native_initial_config_inner(password, directory, create, None)
    }

    /// Source-bound enrollment and recovery, including later confirmed stages.
    /// Reopening rejects a changed source before KDF admission completes.
    #[cfg(unix)]
    pub fn prepare_native_initial_config_with_intent(
        &self,
        password: Zeroizing<Vec<u8>>,
        directory: std::path::PathBuf,
        create: bool,
        intent: super::InitializationIntent,
    ) -> Result<
        impl std::future::Future<Output = Result<Arc<super::FileInitializationJournal>>>
        + Send
        + 'static,
    > {
        self.prepare_native_initial_config_inner(password, directory, create, Some(intent))
    }

    #[cfg(unix)]
    fn prepare_native_initial_config_inner(
        &self,
        password: Zeroizing<Vec<u8>>,
        directory: std::path::PathBuf,
        create: bool,
        intent: Option<super::InitializationIntent>,
    ) -> Result<
        impl std::future::Future<Output = Result<Arc<super::FileInitializationJournal>>>
        + Send
        + 'static,
    > {
        let input = self.begin_password_unlock(password)?;
        let session = self.clone();
        Ok(async move {
            input.ticket.check_task_active()?;
            let permit = tokio::select! {
                permit = UNLOCK_GATE.acquire() => permit.map_err(|_| EncryptionError::UnlockFailed)?,
                _ = input.ticket.wait_cancelled(SecretSessionState::Unlocking) => return Err(EncryptionError::Cancelled),
            };
            let inner = session.inner.clone();
            let result = tokio::task::spawn_blocking(move || {
                use super::InitializationJournal;
                let _permit = permit;
                if !input.ticket.is_unlocking() {
                    return Err(EncryptionError::Cancelled);
                }
                if create {
                    // A new local candidate cannot replace existing trust, even
                    // if another process advanced the durable checkpoint.
                    let has_checkpoint = inner.state.lock().checkpoint.is_some();
                    if has_checkpoint
                        || inner
                            .checkpoint_store
                            .as_ref()
                            .map(|store| store.load(&inner.binding))
                            .transpose()?
                            .flatten()
                            .is_some()
                    {
                        return Err(EncryptionError::StaleObservation);
                    }
                }
                let (journal, prepared_root) = if create {
                    let root = RootKey::generate(inner.binding.repository)?;
                    let config = EncryptedConfig::create(&root, &input.password)?;
                    // Validate the generated wrapper before admitting it to
                    // durable recovery. Only wrapped bytes reach this provider.
                    let verified =
                        config.unlock(&input.password, Some(inner.binding.repository))?;
                    config.authenticate(&verified)?;
                    config.authenticate(&root)?;
                    if !input.ticket.is_unlocking() {
                        return Err(EncryptionError::Cancelled);
                    }
                    (
                        Arc::new(match &intent {
                            Some(intent) => super::FileInitializationJournal::create_with_intent(
                                directory,
                                &inner.binding,
                                &config,
                                intent,
                            )?,
                            None => super::FileInitializationJournal::create(
                                directory,
                                &inner.binding,
                                &config,
                            )?,
                        }),
                        Some(verified),
                    )
                } else {
                    (
                        Arc::new(super::FileInitializationJournal::open(
                            directory,
                            &inner.binding,
                        )?),
                        None,
                    )
                };
                let record = journal.load(&inner.binding)?;
                if intent.is_none()
                    && !matches!(
                        record.stage,
                        super::InitializationStage::Prepared
                            | super::InitializationStage::ConfigPublished
                    )
                {
                    return Err(EncryptionError::StaleObservation);
                }
                if let Some(intent) = &intent
                    && record.intent.as_ref() != Some(intent)
                {
                    return Err(EncryptionError::StaleObservation);
                }
                let config = record.config;
                if config.generation() != 1 {
                    return Err(EncryptionError::InvalidEncoding);
                }
                let root = match prepared_root {
                    Some(root) => {
                        config.authenticate(&root)?;
                        root
                    }
                    None => config.unlock(&input.password, Some(inner.binding.repository))?,
                };
                drop(input.password);
                if !input.ticket.is_unlocking() {
                    return Err(EncryptionError::Cancelled);
                }
                let checkpoint = checkpoint_candidate(&inner, &config, &root)?;
                if let Some(store) = &inner.checkpoint_store {
                    store.advance(&inner.binding, &checkpoint)?;
                }
                Ok::<_, EncryptionError>((
                    config,
                    journal,
                    UnlockOutcome {
                        root: Some(Ok(root)),
                        ticket: input.ticket,
                        checkpoint_persisted: true,
                    },
                ))
            })
            .await
            .map_err(|_| EncryptionError::UnlockFailed)??;
            let (config, journal, outcome) = result;
            session.finish_unlock(&config, outcome)?;
            Ok(journal)
        })
    }

    /// Explicit trusted-host load. Never invoked by transport, lock, or reconnect.
    /// OS protection does not replace fresh local authentication for password reset.
    pub async fn unlock_remembered(
        &self,
        config: EncryptedConfig,
        provider: Arc<dyn RememberedKeyProvider>,
    ) -> Result<()> {
        if config.repository() != self.inner.binding.repository {
            return Err(EncryptionError::IdentityChanged);
        }
        let input = self.begin_remembered_unlock(provider)?;
        self.unlock_remembered_request(config, input).await
    }

    pub(crate) fn begin_remembered_unlock(
        &self,
        provider: Arc<dyn RememberedKeyProvider>,
    ) -> Result<RememberedUnlock> {
        Ok(RememberedUnlock { provider, ticket: self.start_unlock()? })
    }

    pub(crate) async fn unlock_remembered_request(
        &self,
        config: EncryptedConfig,
        input: RememberedUnlock,
    ) -> Result<()> {
        input.ticket.check_task_active()?;
        if !Arc::ptr_eq(&input.ticket.inner, &self.inner)
            || config.repository() != self.inner.binding.repository
        {
            return Err(EncryptionError::IdentityChanged);
        }
        let RememberedUnlock { provider, ticket } = input;
        let observed = config.clone();
        let inner = self.inner.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            let root = if ticket.is_unlocking() {
                provider
                    .load(&inner.binding)
                    .and_then(|root| root.ok_or(EncryptionError::RememberedKeyMissing))
            } else {
                Err(EncryptionError::Cancelled)
            };
            let root = root.and_then(|root| {
                if !ticket.is_unlocking() {
                    return Err(EncryptionError::Cancelled);
                }
                let checkpoint = checkpoint_candidate(&inner, &config, &root)?;
                if let Some(store) = &inner.checkpoint_store {
                    store.advance(&inner.binding, &checkpoint)?;
                }
                Ok(root)
            });
            UnlockOutcome {
                root: Some(root),
                ticket,
                checkpoint_persisted: true,
            }
        })
        .await
        .map_err(|_| EncryptionError::SecureStorageUnavailable)?;
        self.finish_unlock(&observed, outcome)
    }

    /// Explicit host-approved remembering of an already authenticated root.
    /// A cancelled caller or concurrent lock does not prove the OS write failed;
    /// the remembered entry may remain. Lock never loads that entry implicitly.
    pub async fn remember_key(&self, provider: Arc<dyn RememberedKeyProvider>) -> Result<()> {
        let operation = self.begin_operation()?;
        tokio::task::spawn_blocking(move || {
            operation.check_active()?;
            provider.remember(&operation.inner.binding, operation.root()?)?;
            operation.check_active()
        })
        .await
        .map_err(|_| EncryptionError::SecureStorageUnavailable)?
    }

    /// Disconnect/drain this secret session, then delete its remembered entry.
    /// Reconnect remains blocked until deletion exits, even if the caller aborts.
    /// The host must also drain any other session sharing this provider entry.
    pub async fn forget_key(&self, provider: Arc<dyn RememberedKeyProvider>) -> Result<()> {
        if self.inner.creator_pid != std::process::id() {
            return Err(EncryptionError::Cancelled);
        }
        self.disconnect().await;
        let ticket = {
            let mut state = self.inner.state.lock();
            if state.phase != SecretSessionState::Disconnected {
                return Err(EncryptionError::SessionBusy);
            }
            let generation = state
                .generation
                .checked_add(1)
                .ok_or(EncryptionError::GenerationExhausted)?;
            state.generation = generation;
            state.phase = SecretSessionState::Locking;
            state.close_target = SecretSessionState::Disconnected;
            state.tasks = 1;
            SessionOperation {
                inner: self.inner.clone(),
                generation,
                root: None,
            }
        };
        tokio::task::spawn_blocking(move || {
            let result = provider.forget(&ticket.inner.binding);
            drop(ticket);
            result
        })
        .await
        .map_err(|_| EncryptionError::SecureStorageUnavailable)?
    }

    pub(crate) async fn unlock_request(
        &self,
        config: EncryptedConfig,
        input: PasswordUnlock,
    ) -> Result<()> {
        input.ticket.check_task_active()?;
        if !Arc::ptr_eq(&input.ticket.inner, &self.inner)
            || config.repository() != self.inner.binding.repository
        {
            return Err(EncryptionError::IdentityChanged);
        }
        let permit = tokio::select! {
            permit = UNLOCK_GATE.acquire() => permit.map_err(|_| EncryptionError::UnlockFailed)?,
            _ = input.ticket.wait_cancelled(SecretSessionState::Unlocking) => return Err(EncryptionError::Cancelled),
        };
        let repository = self.inner.binding.repository;
        let inner = self.inner.clone();
        let observed = config.clone();
        let task = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let root = if input.ticket.is_unlocking() {
                config.unlock(&input.password, Some(repository))
            } else {
                Err(EncryptionError::Cancelled)
            };
            // Drop the password before relinquishing any task ownership.
            drop(input.password);
            let root = root.and_then(|root| {
                if !input.ticket.is_unlocking() {
                    return Err(EncryptionError::Cancelled);
                }
                if let Some(store) = &inner.checkpoint_store {
                    let checkpoint = checkpoint_candidate(&inner, &config, &root)?;
                    store.advance(&inner.binding, &checkpoint)?;
                }
                Ok(root)
            });
            UnlockOutcome {
                root: Some(root),
                ticket: input.ticket,
                checkpoint_persisted: true,
            }
        });
        let outcome = task.await.map_err(|_| EncryptionError::UnlockFailed)?;
        self.finish_unlock(&observed, outcome)
    }

    /// Explicit trusted-provider input. The session never loads a provider implicitly.
    pub fn unlock_with_root(&self, config: &EncryptedConfig, root: RootKey) -> Result<()> {
        let ticket = self.start_unlock()?;
        self.finish_unlock(
            config,
            UnlockOutcome {
                root: Some(Ok(root)),
                ticket,
                checkpoint_persisted: false,
            },
        )
    }

    fn start_unlock(&self) -> Result<SessionOperation> {
        if self.inner.creator_pid != std::process::id() {
            return Err(EncryptionError::Cancelled);
        }
        let mut state = self.inner.state.lock();
        match state.phase {
            SecretSessionState::Disconnected => return Err(EncryptionError::Disconnected),
            SecretSessionState::Locked => {}
            _ => return Err(EncryptionError::SessionBusy),
        }
        let generation = state
            .generation
            .checked_add(1)
            .ok_or(EncryptionError::GenerationExhausted)?;
        state.generation = generation;
        state.phase = SecretSessionState::Unlocking;
        state.tasks = 1;
        Ok(SessionOperation {
            inner: self.inner.clone(),
            generation,
            root: None,
        })
    }

    fn finish_unlock(&self, config: &EncryptedConfig, mut outcome: UnlockOutcome) -> Result<()> {
        outcome.ticket.check_task_active()?;
        let root = outcome.root.take().ok_or(EncryptionError::UnlockFailed)??;
        if root.repository() != self.inner.binding.repository {
            return Err(EncryptionError::IdentityChanged);
        }
        let checkpoint = checkpoint_candidate(&self.inner, config, &root)?;
        if !outcome.checkpoint_persisted
            && let Some(store) = &self.inner.checkpoint_store
        {
            store.advance(&self.inner.binding, &checkpoint)?;
        }
        let mut state = self.inner.state.lock();
        if state.phase != SecretSessionState::Unlocking
            || state.generation != outcome.ticket.generation
        {
            drop(root);
            return Err(EncryptionError::Cancelled);
        }
        if let Some(previous) = &state.checkpoint {
            previous.require_successor(&checkpoint)?;
        }
        if root.repository() != self.inner.binding.repository {
            return Err(EncryptionError::IdentityChanged);
        }
        state.checkpoint = Some(checkpoint);
        state.root = Some(Arc::new(root));
        state.phase = SecretSessionState::Unlocked;
        Ok(())
    }

    pub fn begin_operation(&self) -> Result<SessionOperation> {
        if self.inner.creator_pid != std::process::id() {
            return Err(EncryptionError::Cancelled);
        }
        let mut state = self.inner.state.lock();
        match state.phase {
            SecretSessionState::Unlocked => {}
            SecretSessionState::Disconnected => return Err(EncryptionError::Disconnected),
            _ => return Err(EncryptionError::Locked),
        }
        if state.paused {
            return Err(EncryptionError::Paused);
        }
        let root = state.root.as_ref().ok_or(EncryptionError::Locked)?.clone();
        state.tasks = state
            .tasks
            .checked_add(1)
            .ok_or(EncryptionError::TooLarge)?;
        Ok(SessionOperation {
            inner: self.inner.clone(),
            generation: state.generation,
            root: Some(root),
        })
    }

    /// Pause closes sync admission but preserves keys and existing observation identity.
    pub fn pause(&self, paused: bool) {
        if self.inner.creator_pid != std::process::id() {
            return;
        }
        self.inner.state.lock().paused = paused;
    }

    /// Close admission, signal task cancellation, then await actual key-owning task release.
    pub async fn lock(&self) {
        self.close(SecretSessionState::Locked).await;
    }
    pub async fn disconnect(&self) {
        self.close(SecretSessionState::Disconnected).await;
    }

    async fn close(&self, target: SecretSessionState) {
        if self.inner.creator_pid != std::process::id() {
            return;
        }
        {
            let mut state = self.inner.state.lock();
            if state.phase == SecretSessionState::Disconnected {
                return;
            }
            if target == SecretSessionState::Disconnected
                || state.phase != SecretSessionState::Locking
            {
                state.close_target = target;
            }
            state.phase = SecretSessionState::Locking;
            complete_close(&mut state);
        }
        self.inner.changed.notify_waiters();
        loop {
            let changed = self.inner.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.state() != SecretSessionState::Locking {
                return;
            }
            changed.await;
        }
    }

    /// Explicit reconnect; account authorization is the host's separate responsibility.
    pub fn reconnect(&self) -> Result<()> {
        if self.inner.creator_pid != std::process::id() {
            return Err(EncryptionError::Cancelled);
        }
        let mut state = self.inner.state.lock();
        if state.phase != SecretSessionState::Disconnected {
            return Err(EncryptionError::SessionBusy);
        }
        state.phase = SecretSessionState::Locked;
        state.close_target = SecretSessionState::Locked;
        state.paused = false;
        Ok(())
    }
}

// Field order is deliberate: discarded blocking results wipe root before releasing the ticket.
pub(crate) struct PasswordUnlock {
    password: Zeroizing<Vec<u8>>,
    ticket: SessionOperation,
}

pub(crate) struct RememberedUnlock {
    provider: Arc<dyn RememberedKeyProvider>,
    ticket: SessionOperation,
}

impl RememberedUnlock {
    pub(crate) fn operation(&self) -> &SessionOperation {
        &self.ticket
    }
}

impl PasswordUnlock {
    pub(crate) fn operation(&self) -> &SessionOperation {
        &self.ticket
    }
}

struct UnlockOutcome {
    root: Option<Result<RootKey>>,
    ticket: SessionOperation,
    checkpoint_persisted: bool,
}

/// One admitted task. All derived-key work must retain this non-cloneable guard.
/// The capability is invalid outside the native process that admitted it.
pub struct SessionOperation {
    inner: Arc<Inner>,
    generation: u64,
    root: Option<Arc<RootKey>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct SessionEpoch {
    session: [u8; 32],
    generation: u64,
}

impl SessionOperation {
    /// Native-local completed graph receipts. They are never derived from a
    /// server hint and never replace authentication of newly fetched objects.
    pub(crate) fn cached_verified_graph_head(&self, scope: &str) -> Result<Option<String>> {
        self.check_active()?;
        let state = self.inner.state.lock();
        if state.generation != self.generation || state.phase != SecretSessionState::Unlocked {
            return Err(EncryptionError::Cancelled);
        }
        Ok(state.verified_graph_heads.get(scope).cloned())
    }

    pub(crate) fn remember_verified_graph_head(&self, scope: &str, head: &str) -> Result<()> {
        self.check_active()?;
        let mut state = self.inner.state.lock();
        if state.generation != self.generation || state.phase != SecretSessionState::Unlocked {
            return Err(EncryptionError::Cancelled);
        }
        if scope.len() > 8192 || head.len() != 64 {
            return Ok(());
        }
        state.verified_graph_order.retain(|key| key != scope);
        state.verified_graph_heads.remove(scope);
        while state.verified_graph_heads.len() >= 64 {
            let oldest = state
                .verified_graph_order
                .pop_front()
                .expect("bounded graph receipts");
            state.verified_graph_heads.remove(&oldest);
        }
        state.verified_graph_order.push_back(scope.into());
        state.verified_graph_heads.insert(scope.into(), head.into());
        Ok(())
    }

    /// Ciphertext-only discovery cache. Content/range readers must not use it.
    pub(crate) fn cached_discovery_header(
        &self,
        endpoint: &str,
        key: &str,
    ) -> Result<Option<(Vec<u8>, u64)>> {
        if self.inner.creator_pid != std::process::id() {
            return Err(EncryptionError::Cancelled);
        }
        let state = self.inner.state.lock();
        if state.generation != self.generation || state.phase != SecretSessionState::Unlocked {
            return Err(EncryptionError::Cancelled);
        }
        Ok(state
            .discovery_headers
            .get(&(endpoint.into(), key.into()))
            .cloned())
    }

    pub(crate) fn cache_discovery_header(
        &self,
        endpoint: &str,
        key: &str,
        header: Vec<u8>,
        total: u64,
    ) -> Result<()> {
        if self.inner.creator_pid != std::process::id() {
            return Err(EncryptionError::Cancelled);
        }
        const MAX_ENTRIES: usize = 1024;
        const MAX_BYTES: usize = 4 * 1024 * 1024;
        let size = endpoint
            .len()
            .saturating_mul(2)
            .saturating_add(key.len().saturating_mul(2))
            .saturating_add(header.len());
        let mut state = self.inner.state.lock();
        if state.generation != self.generation || state.phase != SecretSessionState::Unlocked {
            return Err(EncryptionError::Cancelled);
        }
        let identity = (endpoint.to_owned(), key.to_owned());
        if size > MAX_BYTES || state.discovery_headers.contains_key(&identity) {
            return Ok(());
        }
        while state.discovery_headers.len() >= MAX_ENTRIES
            || state.discovery_bytes.saturating_add(size) > MAX_BYTES
        {
            let oldest = state
                .discovery_order
                .pop_front()
                .expect("discovery cache accounting");
            let (bytes, _) = state
                .discovery_headers
                .remove(&oldest)
                .expect("discovery cache entry");
            state.discovery_bytes -= oldest.0.len() * 2 + oldest.1.len() * 2 + bytes.len();
        }
        state.discovery_bytes += size;
        state.discovery_order.push_back(identity.clone());
        state.discovery_headers.insert(identity, (header, total));
        Ok(())
    }

    pub(crate) fn epoch(&self) -> Result<SessionEpoch> {
        self.check_active()?;
        Ok(SessionEpoch {
            session: self.inner.id,
            generation: self.generation,
        })
    }

    fn is_unlocking(&self) -> bool {
        if self.inner.creator_pid != std::process::id() {
            return false;
        }
        let state = self.inner.state.lock();
        state.phase == SecretSessionState::Unlocking && state.generation == self.generation
    }

    pub(crate) fn root(&self) -> Result<&RootKey> {
        self.check_active()?;
        self.root.as_deref().ok_or(EncryptionError::Locked)
    }

    pub(crate) fn binding(&self) -> &SessionBinding {
        &self.inner.binding
    }

    pub fn check_active(&self) -> Result<()> {
        if self.inner.creator_pid != std::process::id() {
            return Err(EncryptionError::Cancelled);
        }
        let state = self.inner.state.lock();
        if state.generation != self.generation || state.phase != SecretSessionState::Unlocked {
            return Err(EncryptionError::Cancelled);
        }
        Ok(())
    }

    pub(crate) fn check_task_active(&self) -> Result<()> {
        if self.root.is_some() {
            self.check_active()
        } else if self.is_unlocking() {
            Ok(())
        } else {
            Err(EncryptionError::Cancelled)
        }
    }

    /// Network futures must select this signal and retain the guard until they exit.
    pub async fn cancelled(&self) {
        self.wait_cancelled(if self.root.is_some() {
            SecretSessionState::Unlocked
        } else {
            SecretSessionState::Unlocking
        })
        .await;
    }

    async fn wait_cancelled(&self, phase: SecretSessionState) {
        if self.inner.creator_pid != std::process::id() {
            return;
        }
        loop {
            let changed = self.inner.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let current = {
                let state = self.inner.state.lock();
                state.phase == phase && state.generation == self.generation
            };
            if !current {
                return;
            }
            changed.await;
        }
    }

    pub fn route_commit(&self, commit: &[u8; 32]) -> Result<[u8; 32]> {
        let route = self.root()?.route_commit(commit)?;
        self.check_active()?;
        Ok(route)
    }

    /// Observe actual transport bytes. Present values authenticate before becoming handles.
    pub fn observe_ref(&self, logical: &str, bytes: Option<&[u8]>) -> Result<RefObservation> {
        let root = self.root()?;
        let key = reference::remote_key(root, logical)?;
        let target = bytes
            .map(|value| EncryptedRef::open(root, logical, value).map(|(_, target)| target))
            .transpose()?;
        self.check_active()?;
        Ok(RefObservation {
            session: self.inner.id,
            generation: self.generation,
            key,
            bytes: bytes.map(Vec::from),
            target,
        })
    }

    pub fn discover_ref(&self, key: &str, bytes: &[u8]) -> Result<(String, RefObservation)> {
        let (_, logical, target) = EncryptedRef::discover(self.root()?, key, bytes)?;
        self.check_active()?;
        Ok((
            logical,
            RefObservation {
                session: self.inner.id,
                generation: self.generation,
                key: key.to_owned(),
                bytes: Some(bytes.to_vec()),
                target: Some(target),
            },
        ))
    }

    pub fn validate_observation(&self, logical: &str, observation: &RefObservation) -> Result<()> {
        let key = reference::remote_key(self.root()?, logical)?;
        if observation.session != self.inner.id
            || observation.generation != self.generation
            || observation.key != key
        {
            return Err(EncryptionError::StaleObservation);
        }
        Ok(())
    }

    pub fn seal_ref(
        &self,
        logical: &str,
        target: &RefTarget,
        expected: &RefObservation,
    ) -> Result<EncryptedRef> {
        self.validate_observation(logical, expected)?;
        let sealed = EncryptedRef::seal(self.root()?, logical, target)?;
        self.check_active()?;
        Ok(sealed)
    }

    /// Authentication and anti-rollback checks for a freshly read remote config.
    pub fn observe_config(&self, config: &EncryptedConfig) -> Result<ConfigCheckpoint> {
        self.check_active()?;
        let _checkpoint_guard = self.inner.checkpoint_gate.lock();
        let root = self.root()?;
        let checkpoint = checkpoint_candidate(&self.inner, config, root)?;
        if let Some(store) = &self.inner.checkpoint_store {
            store.advance(&self.inner.binding, &checkpoint)?;
        }
        let mut state = self.inner.state.lock();
        if state.phase != SecretSessionState::Unlocked || state.generation != self.generation {
            return Err(EncryptionError::Cancelled);
        }
        if let Some(previous) = &state.checkpoint {
            previous.require_successor(&checkpoint)?;
        }
        state.checkpoint = Some(checkpoint.clone());
        Ok(checkpoint)
    }

    /// Authenticate retained recovery metadata under this admitted root without
    /// observing it as the current remote config or advancing config trust.
    /// Online operations still require the current remote config/checkpoint gate.
    pub fn authenticate_retained_config(&self, config: &EncryptedConfig) -> Result<()> {
        self.check_active()?;
        config.authenticate(self.root()?)?;
        self.check_active()
    }

    /// Keep this non-cloneable key-owning task inside blocking persistence until
    /// it finishes, including when the async caller is cancelled or abandoned.
    pub(crate) async fn observe_config_durable(self, config: EncryptedConfig) -> Result<Self> {
        self.check_active()?;
        tokio::task::spawn_blocking(move || {
            self.observe_config(&config)?;
            Ok(self)
        })
        .await
        .map_err(|_| EncryptionError::CheckpointUnavailable(std::io::ErrorKind::Other))?
    }

    /// Retire only while both durable and in-memory trust still accept this read.
    /// Lock order is the local checkpoint gate, durable store, then journal.
    /// Session state is held only for admission checks, never for provider I/O,
    /// so locking can cancel admission while waiting for persistence to drain.
    /// Losing config changes do not advance either checkpoint.
    pub(crate) fn retire_config_mutation(
        &self,
        observed: &EncryptedConfig,
        journal: &dyn super::ConfigMutationJournal,
        record: &super::ConfigMutationRecord,
    ) -> Result<()> {
        self.check_active()?;
        let _checkpoint_guard = self.inner.checkpoint_gate.lock();
        let checkpoint = observed.checkpoint(self.root()?)?;
        let mut retire = || {
            {
                let state = self.inner.state.lock();
                if state.phase != SecretSessionState::Unlocked
                    || state.generation != self.generation
                {
                    return Err(EncryptionError::Cancelled);
                }
                if let Some(previous) = &state.checkpoint {
                    previous.require_successor(&checkpoint)?;
                }
            }
            journal.retire(&self.inner.binding, record)
        };
        match &self.inner.checkpoint_store {
            Some(store) => {
                store.with_validated_checkpoint(&self.inner.binding, &checkpoint, &mut retire)
            }
            None => retire(),
        }
    }
}

fn checkpoint_candidate(
    inner: &Inner,
    config: &EncryptedConfig,
    root: &RootKey,
) -> Result<ConfigCheckpoint> {
    let previous = inner.state.lock().checkpoint.clone();
    match previous {
        Some(previous) => previous.validate(config, root),
        None => config.checkpoint(root),
    }
}

impl Drop for SessionOperation {
    fn drop(&mut self) {
        // Wipe this task's key ownership before publishing a zero task count.
        self.root.take();
        if self.inner.creator_pid != std::process::id() {
            return;
        }
        {
            let mut state = self.inner.state.lock();
            state.tasks = state
                .tasks
                .checked_sub(1)
                .expect("registered encryption task");
            if state.tasks == 0 && state.phase == SecretSessionState::Unlocking {
                state.phase = SecretSessionState::Locked;
            }
            complete_close(&mut state);
        }
        self.inner.changed.notify_waiters();
    }
}

fn complete_close(state: &mut State) {
    if state.phase == SecretSessionState::Locking {
        state.discovery_headers.clear();
        state.discovery_order.clear();
        state.verified_graph_heads.clear();
        state.verified_graph_order.clear();
        state.discovery_bytes = 0;
    }
    if state.phase == SecretSessionState::Locking && state.tasks == 0 {
        state.root.take();
        state.phase = state.close_target;
    }
}

/// Exact mutable ciphertext observation; no root key and no plaintext Debug output.
pub struct RefObservation {
    session: [u8; 32],
    generation: u64,
    key: String,
    bytes: Option<Vec<u8>>,
    target: Option<RefTarget>,
}

impl RefObservation {
    pub fn remote_key(&self) -> &str {
        &self.key
    }
    pub fn expected_bytes(&self) -> Option<&[u8]> {
        self.bytes.as_deref()
    }
    pub fn target(&self) -> Option<&RefTarget> {
        self.target.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::e2ee::tests::fixture_root;
    use std::sync::{OnceLock, mpsc};
    use std::time::Duration;

    static CONFIG: OnceLock<EncryptedConfig> = OnceLock::new();

    fn config() -> EncryptedConfig {
        CONFIG
            .get_or_init(|| EncryptedConfig::create(&fixture_root(), b"session password").unwrap())
            .clone()
    }

    fn session() -> SecretSession {
        SecretSession::new(
            SessionBinding::new(
                "domain-secret".into(),
                "account-secret".into(),
                fixture_root().repository(),
            )
            .unwrap(),
            None,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn inherited_native_session_rejects_admission_before_touching_locks() {
        let mut session = session();
        Arc::get_mut(&mut session.inner).unwrap().creator_pid = std::process::id().wrapping_add(1);
        assert_eq!(session.state(), SecretSessionState::Disconnected);
        assert!(matches!(
            session.begin_operation(),
            Err(EncryptionError::Cancelled)
        ));
        assert!(matches!(
            session.start_unlock(),
            Err(EncryptionError::Cancelled)
        ));
        assert!(matches!(
            session.reconnect(),
            Err(EncryptionError::Cancelled)
        ));
        let ticket = SessionOperation {
            inner: session.inner.clone(),
            generation: 0,
            root: None,
        };
        assert!(matches!(
            ticket.check_active(),
            Err(EncryptionError::Cancelled)
        ));
        assert!(matches!(
            ticket.check_task_active(),
            Err(EncryptionError::Cancelled)
        ));
        let config = config();
        let checkpoint_guard = session.inner.checkpoint_gate.lock();
        let (send, receive) = mpsc::channel();
        let task = std::thread::spawn(move || send.send(ticket.observe_config(&config)).unwrap());
        assert!(matches!(
            receive.recv_timeout(Duration::from_secs(1)).unwrap(),
            Err(EncryptionError::Cancelled)
        ));
        drop(checkpoint_guard);
        task.join().unwrap();
        drop(session.clone());
        session.lock().await;
    }

    #[tokio::test]
    async fn completed_graph_receipts_are_bounded_and_cleared_on_lock() {
        let session = session();
        session.unlock_with_root(&config(), fixture_root()).unwrap();
        let operation = session.begin_operation().unwrap();
        for index in 0..65 {
            operation
                .remember_verified_graph_head(&format!("scope-{index}"), &"a".repeat(64))
                .unwrap();
        }
        assert!(
            operation
                .cached_verified_graph_head("scope-0")
                .unwrap()
                .is_none()
        );
        assert_eq!(
            operation.cached_verified_graph_head("scope-64").unwrap(),
            Some("a".repeat(64))
        );
        operation
            .remember_verified_graph_head(&"x".repeat(8193), &"b".repeat(64))
            .unwrap();
        assert_eq!(session.inner.state.lock().verified_graph_heads.len(), 64);
        drop(operation);
        session.lock().await;
        session.unlock_with_root(&config(), fixture_root()).unwrap();
        assert!(
            session
                .begin_operation()
                .unwrap()
                .cached_verified_graph_head("scope-64")
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn discovery_cache_is_bounded_endpoint_scoped_and_cleared_on_lock() {
        let session = session();
        session.unlock_with_root(&config(), fixture_root()).unwrap();
        let operation = session.begin_operation().unwrap();
        for index in 0..1025 {
            operation
                .cache_discovery_header(
                    "https://one.invalid/repo",
                    &format!("key-{index}"),
                    vec![1; 32],
                    64,
                )
                .unwrap();
        }
        assert!(
            operation
                .cached_discovery_header("https://one.invalid/repo", "key-0")
                .unwrap()
                .is_none()
        );
        assert!(
            operation
                .cached_discovery_header("https://one.invalid/repo", "key-1024")
                .unwrap()
                .is_some()
        );
        assert!(
            operation
                .cached_discovery_header("https://two.invalid/repo", "key-1024")
                .unwrap()
                .is_none()
        );
        for index in 0..8 {
            operation
                .cache_discovery_header(
                    "https://one.invalid/repo",
                    &format!("large-{index}"),
                    vec![2; 1024 * 1024],
                    2 * 1024 * 1024,
                )
                .unwrap();
        }
        {
            let state = session.inner.state.lock();
            assert!(state.discovery_bytes <= 4 * 1024 * 1024);
            assert!(state.discovery_headers.len() <= 1024);
        }
        drop(operation);
        session.lock().await;
        {
            let state = session.inner.state.lock();
            assert!(state.discovery_headers.is_empty());
            assert!(state.discovery_order.is_empty());
            assert_eq!(state.discovery_bytes, 0);
        }
        session.unlock_with_root(&config(), fixture_root()).unwrap();
        assert!(
            session
                .begin_operation()
                .unwrap()
                .cached_discovery_header("https://one.invalid/repo", "large-7")
                .unwrap()
                .is_none()
        );
    }

    struct FixtureProvider {
        saved: std::sync::atomic::AtomicBool,
        loads: std::sync::atomic::AtomicUsize,
    }

    #[test]
    fn retained_config_authentication_does_not_roll_back_current_trust() {
        let session = session();
        session.unlock_with_root(&config(), fixture_root()).unwrap();
        let operation = session.begin_operation().unwrap();
        let newer = config()
            .password_change_candidate(&fixture_root(), b"retained metadata current password")
            .unwrap();
        let checkpoint = operation.observe_config(&newer).unwrap();
        operation.authenticate_retained_config(&config()).unwrap();
        assert_eq!(session.checkpoint(), Some(checkpoint.clone()));
        let mut bytes = config().as_bytes().to_vec();
        *bytes.last_mut().unwrap() ^= 1;
        let corrupt = EncryptedConfig::parse(&bytes).unwrap();
        assert_eq!(
            operation.authenticate_retained_config(&corrupt),
            Err(EncryptionError::AuthenticationFailed)
        );
        assert_eq!(session.checkpoint(), Some(checkpoint));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn native_initial_config_enrollment_and_reopen_preserve_one_random_candidate() {
        use super::super::{FileCheckpointStore, FileInitializationJournal, InitializationJournal};
        use std::os::unix::fs::PermissionsExt;
        let checkpoint = tempfile::tempdir().unwrap();
        let recovery = tempfile::tempdir().unwrap();
        for directory in [&checkpoint, &recovery] {
            std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
                .unwrap();
        }
        let binding = || {
            SessionBinding::new(
                "initial.host".into(),
                "initial.account".into(),
                fixture_root().repository(),
            )
            .unwrap()
        };
        let store = Arc::new(FileCheckpointStore::create(checkpoint.path(), &binding()).unwrap());
        let session = SecretSession::with_checkpoint_store(binding(), store).unwrap();
        let password = || Zeroizing::new(b"native initial synthetic password".to_vec());
        let future = session
            .prepare_native_initial_config(password(), recovery.path().to_owned(), true)
            .unwrap();
        assert_eq!(session.state(), SecretSessionState::Unlocking);
        let journal = future.await.unwrap();
        assert_eq!(session.state(), SecretSessionState::Unlocked);
        let record = journal.load(session.binding()).unwrap();
        assert_eq!(record.stage, super::super::InitializationStage::Prepared);
        assert_eq!(record.config.generation(), 1);
        assert!(record.config.authenticate(&fixture_root()).is_err());
        let bytes = record.config.as_bytes().to_vec();
        assert_eq!(
            session.checkpoint(),
            Some(
                record
                    .config
                    .checkpoint(
                        &record
                            .config
                            .unlock(&password(), Some(binding().repository()))
                            .unwrap()
                    )
                    .unwrap()
            )
        );
        session.disconnect().await;
        drop(journal);
        let fresh = SecretSession::with_checkpoint_store(
            binding(),
            Arc::new(FileCheckpointStore::open(checkpoint.path(), &binding()).unwrap()),
        )
        .unwrap();
        let wrong = fresh
            .prepare_native_initial_config(
                Zeroizing::new(b"wrong initialization password".to_vec()),
                recovery.path().to_owned(),
                false,
            )
            .unwrap();
        assert!(matches!(wrong.await, Err(EncryptionError::UnlockFailed)));
        assert_eq!(fresh.state(), SecretSessionState::Locked);
        let reopened = fresh
            .prepare_native_initial_config(password(), recovery.path().to_owned(), false)
            .unwrap()
            .await
            .unwrap();
        assert_eq!(
            reopened.load(fresh.binding()).unwrap().config.as_bytes(),
            bytes
        );
        fresh.lock().await;
        assert!(matches!(
            fresh
                .prepare_native_initial_config(password(), recovery.path().to_owned(), true)
                .unwrap()
                .await,
            Err(EncryptionError::StaleObservation)
        ));
        assert_eq!(
            FileInitializationJournal::open(recovery.path(), fresh.binding())
                .unwrap()
                .load(fresh.binding())
                .unwrap()
                .config
                .as_bytes(),
            bytes
        );
        std::fs::remove_file(recovery.path().join("initialization")).unwrap();
        assert!(
            fresh
                .prepare_native_initial_config(password(), recovery.path().to_owned(), false)
                .unwrap()
                .await
                .is_err()
        );
        assert!(!recovery.path().join("initialization").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn native_source_bound_initialization_reopens_every_stage_without_replacing_intent() {
        use super::super::{
            FileCheckpointStore, InitializationIntent, InitializationJournal,
            InitializationStage as Stage,
        };
        use std::os::unix::fs::PermissionsExt;
        let checkpoint = tempfile::tempdir().unwrap();
        let recovery = tempfile::tempdir().unwrap();
        for path in [checkpoint.path(), recovery.path()] {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let binding = || {
            SessionBinding::new(
                "source-bound.host".into(),
                "account".into(),
                fixture_root().repository(),
            )
            .unwrap()
        };
        let store = FileCheckpointStore::create(checkpoint.path(), &binding()).unwrap();
        let secret = SecretSession::with_checkpoint_store(binding(), Arc::new(store)).unwrap();
        let password = || Zeroizing::new(b"source bound initial password".to_vec());
        let intent = InitializationIntent::new("main".into(), "a".repeat(64)).unwrap();
        let journal = secret
            .prepare_native_initial_config_with_intent(
                password(),
                recovery.path().to_owned(),
                true,
                intent.clone(),
            )
            .unwrap()
            .await
            .unwrap();
        let record = journal.load(secret.binding()).unwrap();
        assert_eq!(record.intent.unwrap().commit(), intent.commit());
        let exact = record.config.as_bytes().to_vec();
        for (expected, next) in [
            (Stage::Prepared, Stage::ConfigPublished),
            (Stage::ConfigPublished, Stage::ObjectsPublished),
            (Stage::ObjectsPublished, Stage::RefPublished),
            (Stage::RefPublished, Stage::Connected),
        ] {
            journal.advance(secret.binding(), expected, next).unwrap();
            secret.lock().await;
            let changed = InitializationIntent::new("main".into(), "b".repeat(64)).unwrap();
            assert!(matches!(
                secret
                    .prepare_native_initial_config_with_intent(
                        password(),
                        recovery.path().to_owned(),
                        false,
                        changed
                    )
                    .unwrap()
                    .await,
                Err(EncryptionError::StaleObservation)
            ));
            assert_eq!(secret.state(), SecretSessionState::Locked);
            let reopened = secret
                .prepare_native_initial_config_with_intent(
                    password(),
                    recovery.path().to_owned(),
                    false,
                    intent.clone(),
                )
                .unwrap()
                .await
                .unwrap();
            let record = reopened.load(secret.binding()).unwrap();
            assert_eq!(record.stage, next);
            assert_eq!(record.config.as_bytes(), exact);
            assert_eq!(record.intent.unwrap().commit(), intent.commit());
        }
        secret.disconnect().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn native_initial_config_abandoned_before_poll_releases_unlock_admission() {
        let session = session();
        let recovery = tempfile::tempdir().unwrap();
        let future = session
            .prepare_native_initial_config(
                Zeroizing::new(b"unused initialization password".to_vec()),
                recovery.path().to_owned(),
                true,
            )
            .unwrap();
        assert_eq!(session.state(), SecretSessionState::Unlocking);
        drop(future);
        session.lock().await;
        assert_eq!(session.state(), SecretSessionState::Locked);
        assert!(!recovery.path().join("initialization").exists());
    }
    impl RememberedKeyProvider for FixtureProvider {
        fn load(&self, _: &SessionBinding) -> Result<Option<RootKey>> {
            self.loads.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            Ok(self
                .saved
                .load(std::sync::atomic::Ordering::Acquire)
                .then(fixture_root))
        }
        fn remember(&self, _: &SessionBinding, _: &RootKey) -> Result<()> {
            self.saved.store(true, std::sync::atomic::Ordering::Release);
            Ok(())
        }
        fn forget(&self, _: &SessionBinding) -> Result<()> {
            self.saved
                .store(false, std::sync::atomic::Ordering::Release);
            Ok(())
        }
    }

    #[tokio::test]
    async fn provider_remember_load_and_forget_are_explicit_without_lock_or_reconnect_reload() {
        let session = session();
        let provider = Arc::new(FixtureProvider {
            saved: std::sync::atomic::AtomicBool::new(false),
            loads: std::sync::atomic::AtomicUsize::new(0),
        });
        assert_eq!(
            session.remember_key(provider.clone()).await,
            Err(EncryptionError::Locked)
        );
        session.unlock_with_root(&config(), fixture_root()).unwrap();
        session.remember_key(provider.clone()).await.unwrap();
        session.lock().await;
        assert_eq!(session.state(), SecretSessionState::Locked);
        assert_eq!(provider.loads.load(std::sync::atomic::Ordering::Acquire), 0);
        session
            .unlock_remembered(config(), provider.clone())
            .await
            .unwrap();
        assert_eq!(session.state(), SecretSessionState::Unlocked);
        session.disconnect().await;
        session.reconnect().unwrap();
        assert_eq!(session.state(), SecretSessionState::Locked);
        assert_eq!(provider.loads.load(std::sync::atomic::Ordering::Acquire), 1);
        session.forget_key(provider.clone()).await.unwrap();
        assert_eq!(session.state(), SecretSessionState::Disconnected);
        session.reconnect().unwrap();
        assert_eq!(
            session.unlock_remembered(config(), provider.clone()).await,
            Err(EncryptionError::RememberedKeyMissing)
        );
        assert_eq!(session.state(), SecretSessionState::Locked);
    }

    struct UnavailableProvider;
    impl RememberedKeyProvider for UnavailableProvider {
        fn load(&self, _: &SessionBinding) -> Result<Option<RootKey>> {
            Err(EncryptionError::SecureStorageUnavailable)
        }
        fn remember(&self, _: &SessionBinding, _: &RootKey) -> Result<()> {
            Err(EncryptionError::SecureStorageUnavailable)
        }
        fn forget(&self, _: &SessionBinding) -> Result<()> {
            Err(EncryptionError::SecureStorageUnavailable)
        }
    }

    #[tokio::test]
    async fn provider_unavailable_storage_never_admits_root_or_updates_checkpoint() {
        let session = session();
        assert_eq!(
            session
                .unlock_remembered(config(), Arc::new(UnavailableProvider))
                .await,
            Err(EncryptionError::SecureStorageUnavailable)
        );
        assert_eq!(session.state(), SecretSessionState::Locked);
        assert!(session.checkpoint().is_none());
        assert!(matches!(
            session.begin_operation(),
            Err(EncryptionError::Locked)
        ));
        assert_eq!(
            session.forget_key(Arc::new(UnavailableProvider)).await,
            Err(EncryptionError::SecureStorageUnavailable)
        );
        assert_eq!(session.state(), SecretSessionState::Disconnected);
    }

    struct BlockingProvider {
        started: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
        release: std::sync::Mutex<mpsc::Receiver<()>>,
    }
    impl BlockingProvider {
        fn wait(&self) -> Result<()> {
            self.started
                .lock()
                .unwrap()
                .take()
                .unwrap()
                .send(())
                .unwrap();
            self.release
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(5))
                .map_err(|_| EncryptionError::SecureStorageUnavailable)
        }
    }
    impl RememberedKeyProvider for BlockingProvider {
        fn load(&self, _: &SessionBinding) -> Result<Option<RootKey>> {
            self.wait()?;
            Ok(Some(fixture_root()))
        }
        fn remember(&self, _: &SessionBinding, _: &RootKey) -> Result<()> {
            self.wait()
        }
        fn forget(&self, _: &SessionBinding) -> Result<()> {
            self.wait()
        }
    }

    #[tokio::test]
    async fn provider_forget_blocks_reconnect_and_drains_after_caller_abort() {
        let session = session();
        session.unlock_with_root(&config(), fixture_root()).unwrap();
        let operation = session.begin_operation().unwrap();
        let (started_tx, mut started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let provider = Arc::new(BlockingProvider {
            started: std::sync::Mutex::new(Some(started_tx)),
            release: std::sync::Mutex::new(release_rx),
        });
        let caller_session = session.clone();
        let caller = tokio::spawn(async move { caller_session.forget_key(provider).await });
        tokio::time::timeout(Duration::from_secs(1), async {
            while session.state() != SecretSessionState::Locking {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        // Deletion cannot start while a previous operation still owns the root.
        assert!(matches!(
            started_rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        operation.check_active().unwrap_err();
        drop(operation);
        tokio::time::timeout(Duration::from_secs(2), started_rx)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(session.state(), SecretSessionState::Locking);
        assert_eq!(session.reconnect(), Err(EncryptionError::SessionBusy));
        assert!(matches!(
            session.begin_operation(),
            Err(EncryptionError::Locked)
        ));
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        let closing_session = session.clone();
        let closing = tokio::spawn(async move { closing_session.lock().await });
        tokio::task::yield_now().await;
        assert!(!closing.is_finished());
        assert_eq!(session.reconnect(), Err(EncryptionError::SessionBusy));
        release_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), closing)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(session.state(), SecretSessionState::Disconnected);
        session.reconnect().unwrap();
        assert_eq!(session.state(), SecretSessionState::Locked);
    }

    #[tokio::test]
    async fn provider_lock_drains_loading_and_remembering_after_caller_abort_or_cancellation() {
        for (load, abort) in [(true, true), (true, false), (false, true), (false, false)] {
            let observed = config();
            let session = session();
            if !load {
                session.unlock_with_root(&observed, fixture_root()).unwrap();
            }
            let (started_tx, started_rx) = tokio::sync::oneshot::channel();
            let (release_tx, release_rx) = mpsc::channel();
            let provider = Arc::new(BlockingProvider {
                started: std::sync::Mutex::new(Some(started_tx)),
                release: std::sync::Mutex::new(release_rx),
            });
            let caller_session = session.clone();
            let caller = tokio::spawn(async move {
                if load {
                    caller_session.unlock_remembered(observed, provider).await
                } else {
                    caller_session.remember_key(provider).await
                }
            });
            tokio::time::timeout(Duration::from_secs(2), started_rx)
                .await
                .unwrap()
                .unwrap();
            if abort {
                caller.abort();
            }
            let locking_session = session.clone();
            let locking = tokio::spawn(async move { locking_session.lock().await });
            tokio::time::timeout(Duration::from_secs(1), async {
                while session.state() != SecretSessionState::Locking {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert!(!locking.is_finished());
            assert!(matches!(
                session.begin_operation(),
                Err(EncryptionError::Locked)
            ));
            release_tx.send(()).unwrap();
            if abort {
                assert!(caller.await.unwrap_err().is_cancelled());
            } else {
                assert_eq!(caller.await.unwrap(), Err(EncryptionError::Cancelled));
            }
            tokio::time::timeout(Duration::from_secs(2), locking)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(session.state(), SecretSessionState::Locked);
            if load {
                assert!(session.checkpoint().is_none());
            }
        }
    }

    #[tokio::test]
    async fn config_retirement_serializes_checkpoint_changes_without_blocking_lock_drain() {
        use crate::e2ee::{ConfigMutationJournal, ConfigMutationRecord};

        struct BlockingJournal(BlockingProvider);
        impl ConfigMutationJournal for BlockingJournal {
            fn load(&self, _: &SessionBinding) -> Result<Option<ConfigMutationRecord>> {
                Ok(None)
            }
            fn bind(&self, _: &SessionBinding, _: &ConfigMutationRecord) -> Result<()> {
                Ok(())
            }
            fn retire(&self, _: &SessionBinding, _: &ConfigMutationRecord) -> Result<()> {
                self.0.wait()
            }
        }

        let next = config()
            .password_change_candidate(&fixture_root(), b"retirement checkpoint password")
            .unwrap();
        let record = ConfigMutationRecord::from_local_bytes(
            &crate::e2ee::codec::encode(&[config().as_bytes(), next.as_bytes()]).unwrap(),
        )
        .unwrap();
        for abort in [false, true] {
            let session = session();
            session.unlock_with_root(&config(), fixture_root()).unwrap();
            let (started_tx, started_rx) = tokio::sync::oneshot::channel();
            let (release_tx, release_rx) = mpsc::channel();
            let journal = BlockingJournal(BlockingProvider {
                started: std::sync::Mutex::new(Some(started_tx)),
                release: std::sync::Mutex::new(release_rx),
            });
            let operation = session.begin_operation().unwrap();
            let observed = next.clone();
            let retained = record.clone();
            let caller = tokio::spawn(async move {
                tokio::task::spawn_blocking(move || {
                    operation.retire_config_mutation(&observed, &journal, &retained)?;
                    operation.check_active()
                })
                .await
                .unwrap()
            });
            tokio::time::timeout(Duration::from_secs(2), started_rx)
                .await
                .unwrap()
                .unwrap();
            let peer = session.begin_operation().unwrap();
            let observing = tokio::spawn(peer.observe_config_durable(next.clone()));
            let closing_session = session.clone();
            let closing = tokio::spawn(async move { closing_session.lock().await });
            tokio::time::timeout(Duration::from_secs(2), async {
                while session.state() != SecretSessionState::Locking {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert!(!closing.is_finished());
            assert!(!observing.is_finished());
            assert_eq!(
                session.checkpoint(),
                Some(config().checkpoint(&fixture_root()).unwrap())
            );
            if abort {
                caller.abort();
            }
            release_tx.send(()).unwrap();
            if abort {
                assert!(caller.await.unwrap_err().is_cancelled());
            } else {
                assert_eq!(caller.await.unwrap(), Err(EncryptionError::Cancelled));
            }
            assert!(matches!(
                observing.await.unwrap(),
                Err(EncryptionError::Cancelled)
            ));
            tokio::time::timeout(Duration::from_secs(2), closing)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(session.state(), SecretSessionState::Locked);
        }
    }

    struct BlockingCheckpointStore {
        block: std::sync::atomic::AtomicBool,
        started: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
        release: std::sync::Mutex<mpsc::Receiver<()>>,
    }

    impl CheckpointStore for BlockingCheckpointStore {
        fn load(&self, _: &SessionBinding) -> Result<Option<ConfigCheckpoint>> {
            Ok(None)
        }

        fn advance(&self, _: &SessionBinding, _: &ConfigCheckpoint) -> Result<()> {
            if self.block.load(std::sync::atomic::Ordering::Acquire) {
                self.started
                    .lock()
                    .unwrap()
                    .take()
                    .unwrap()
                    .send(())
                    .unwrap();
                self.release
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(5))
                    .map_err(|_| {
                        EncryptionError::CheckpointUnavailable(std::io::ErrorKind::TimedOut)
                    })?;
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn aborted_checkpoint_caller_keeps_lock_pending_until_key_owner_exits() {
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let store = Arc::new(BlockingCheckpointStore {
            block: std::sync::atomic::AtomicBool::new(false),
            started: std::sync::Mutex::new(Some(started_tx)),
            release: std::sync::Mutex::new(release_rx),
        });
        let binding = SessionBinding::new(
            "domain-secret".into(),
            "account-secret".into(),
            fixture_root().repository(),
        )
        .unwrap();
        let session = SecretSession::with_checkpoint_store(binding, store.clone()).unwrap();
        session.unlock_with_root(&config(), fixture_root()).unwrap();
        let previous = session.checkpoint();
        let next = config()
            .password_change_candidate(&fixture_root(), b"checkpoint next password")
            .unwrap();
        store
            .block
            .store(true, std::sync::atomic::Ordering::Release);
        let operation = session.begin_operation().unwrap();
        let caller = tokio::spawn(async move { operation.observe_config_durable(next).await });
        tokio::time::timeout(Duration::from_secs(2), started_rx)
            .await
            .unwrap()
            .unwrap();
        caller.abort();
        assert!(matches!(caller.await, Err(error) if error.is_cancelled()));
        let locking_session = session.clone();
        let locking = tokio::spawn(async move { locking_session.lock().await });
        tokio::time::timeout(Duration::from_secs(1), async {
            while session.state() != SecretSessionState::Locking {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(!locking.is_finished());
        assert_eq!(session.checkpoint(), previous);
        assert!(matches!(
            session.begin_operation(),
            Err(EncryptionError::Locked)
        ));
        release_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), locking)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(session.state(), SecretSessionState::Locked);
        assert_eq!(session.checkpoint(), previous);
    }

    #[cfg(unix)]
    #[test]
    fn native_initial_config_aborted_persistence_keeps_lock_pending_until_root_exits() {
        // This scenario times persistence/lock drain, not the entire test
        // process's global KDF admission queue. Run its real Argon2 enrollment
        // in an isolated process while preserving every original deadline.
        assert!(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "e2ee::session::tests::native_initial_config_aborted_persistence_child",
                    "--ignored",
                    "--test-threads=1",
                ])
                .status()
                .unwrap()
                .success()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    #[ignore = "isolated native persistence fixture"]
    async fn native_initial_config_aborted_persistence_child() {
        use super::super::{FileInitializationJournal, InitializationJournal};
        use std::os::unix::fs::PermissionsExt;
        let recovery = tempfile::tempdir().unwrap();
        std::fs::set_permissions(recovery.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let store = Arc::new(BlockingCheckpointStore {
            block: std::sync::atomic::AtomicBool::new(true),
            started: std::sync::Mutex::new(Some(started_tx)),
            release: std::sync::Mutex::new(release_rx),
        });
        let session = SecretSession::with_checkpoint_store(
            SessionBinding::new(
                "initial.host".into(),
                "initial.account".into(),
                fixture_root().repository(),
            )
            .unwrap(),
            store,
        )
        .unwrap();
        let future = session
            .prepare_native_initial_config(
                Zeroizing::new(b"abandoned initial synthetic password".to_vec()),
                recovery.path().to_owned(),
                true,
            )
            .unwrap();
        let caller = tokio::spawn(future);
        tokio::time::timeout(Duration::from_secs(30), started_rx)
            .await
            .unwrap()
            .unwrap();
        caller.abort();
        assert!(matches!(caller.await, Err(error) if error.is_cancelled()));
        let closing = session.clone();
        let locking = tokio::spawn(async move { closing.lock().await });
        tokio::time::timeout(Duration::from_secs(1), async {
            while session.state() != SecretSessionState::Locking {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(!locking.is_finished());
        assert!(session.checkpoint().is_none());
        release_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), locking)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(session.state(), SecretSessionState::Locked);
        assert!(session.checkpoint().is_none());
        // The already-written wrapped candidate remains recoverable, but no
        // abandoned completion has admitted its root into the locked session.
        let config = FileInitializationJournal::open(recovery.path(), session.binding())
            .unwrap()
            .load(session.binding())
            .unwrap()
            .config;
        assert!(
            config
                .unlock(
                    b"abandoned initial synthetic password",
                    Some(session.binding().repository())
                )
                .is_ok()
        );
    }

    #[tokio::test]
    async fn lock_closes_admission_waits_for_tasks_and_invalidates_observations() {
        let session = session();
        session.unlock_with_root(&config(), fixture_root()).unwrap();
        let operation = session.begin_operation().unwrap();
        let bytes = EncryptedRef::seal(
            &fixture_root(),
            "refs/heads/main",
            &RefTarget::Commit([7; 32]),
        )
        .unwrap();
        let observed = operation
            .observe_ref("refs/heads/main", Some(bytes.as_bytes()))
            .unwrap();
        assert_eq!(observed.expected_bytes(), Some(bytes.as_bytes()));
        let lock_session = session.clone();
        let locking = tokio::spawn(async move {
            lock_session.lock().await;
        });
        tokio::time::timeout(Duration::from_secs(1), operation.cancelled())
            .await
            .unwrap();
        assert_eq!(session.state(), SecretSessionState::Locking);
        assert!(!locking.is_finished());
        assert!(matches!(
            session.begin_operation(),
            Err(EncryptionError::Locked)
        ));
        assert!(matches!(
            operation.route_commit(&[7; 32]),
            Err(EncryptionError::Cancelled)
        ));
        drop(operation);
        tokio::time::timeout(Duration::from_secs(1), locking)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(session.state(), SecretSessionState::Locked);
        session.unlock_with_root(&config(), fixture_root()).unwrap();
        let operation = session.begin_operation().unwrap();
        assert_eq!(
            operation.validate_observation("refs/heads/main", &observed),
            Err(EncryptionError::StaleObservation)
        );
    }

    #[tokio::test]
    async fn pause_and_password_changes_preserve_observations_but_account_sessions_do_not() {
        let session = session();
        session.unlock_with_root(&config(), fixture_root()).unwrap();
        let operation = session.begin_operation().unwrap();
        let observed = operation.observe_ref("refs/heads/main", None).unwrap();
        assert!(observed.expected_bytes().is_none());
        session.pause(true);
        assert!(matches!(
            session.begin_operation(),
            Err(EncryptionError::Paused)
        ));
        assert!(
            operation
                .validate_observation("refs/heads/main", &observed)
                .is_ok()
        );
        session.pause(false);
        let next = config()
            .password_change_candidate(&fixture_root(), b"new session password")
            .unwrap();
        let checkpoint = operation.observe_config(&next).unwrap();
        assert_eq!(session.checkpoint(), Some(checkpoint.clone()));
        assert!(
            operation
                .validate_observation("refs/heads/main", &observed)
                .is_ok()
        );
        assert_eq!(
            operation.observe_config(&config()),
            Err(EncryptionError::ConfigRollbackDetected)
        );
        assert_eq!(
            operation.validate_observation("refs/heads/other", &observed),
            Err(EncryptionError::StaleObservation)
        );
        let second = super::tests::session();
        second.unlock_with_root(&config(), fixture_root()).unwrap();
        let other = second.begin_operation().unwrap();
        assert_eq!(
            other.validate_observation("refs/heads/main", &observed),
            Err(EncryptionError::StaleObservation)
        );
        let sealed = operation
            .seal_ref("refs/heads/main", &RefTarget::Commit([7; 32]), &observed)
            .unwrap();
        assert_eq!(sealed.remote_key(), observed.remote_key());
        drop(operation);
        session.disconnect().await;
        assert_eq!(session.state(), SecretSessionState::Disconnected);
        assert!(matches!(
            session.begin_operation(),
            Err(EncryptionError::Disconnected)
        ));
        assert!(matches!(
            session.unlock_with_root(&next, fixture_root()),
            Err(EncryptionError::Disconnected)
        ));
        session.reconnect().unwrap();
        assert_eq!(session.state(), SecretSessionState::Locked);
        session.unlock_with_root(&next, fixture_root()).unwrap();
        assert_eq!(session.checkpoint(), Some(checkpoint));
        let debug = format!("{session:?}");
        assert!(!debug.contains("domain-secret") && !debug.contains("account-secret"));
        assert!(debug.contains("[REDACTED]"));
    }

    #[tokio::test]
    async fn abandoned_lock_waiter_and_last_session_owner_still_revoke_tasks() {
        let session = session();
        session.unlock_with_root(&config(), fixture_root()).unwrap();
        let operation = session.begin_operation().unwrap();
        let lock_session = session.clone();
        let locking = tokio::spawn(async move {
            lock_session.lock().await;
        });
        operation.cancelled().await;
        locking.abort();
        let _ = locking.await;
        assert_eq!(session.state(), SecretSessionState::Locking);
        drop(operation);
        assert_eq!(session.state(), SecretSessionState::Locked);
        session.unlock_with_root(&config(), fixture_root()).unwrap();
        let operation = session.begin_operation().unwrap();
        drop(session);
        operation.cancelled().await;
        assert_eq!(operation.check_active(), Err(EncryptionError::Cancelled));
        assert_eq!(
            operation.inner.state.lock().phase,
            SecretSessionState::Locking
        );
        let inner = operation.inner.clone();
        drop(operation);
        assert_eq!(inner.state.lock().phase, SecretSessionState::Disconnected);
        assert!(inner.state.lock().root.is_none());
    }

    #[tokio::test]
    async fn dropped_blocking_unlock_output_retains_task_ownership_until_keys_are_released() {
        let session = session();
        let ticket = session.start_unlock().unwrap();
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let worker = tokio::task::spawn_blocking(move || {
            let root = fixture_root();
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            UnlockOutcome {
                root: Some(Ok(root)),
                ticket,
                checkpoint_persisted: false,
            }
        });
        tokio::task::spawn_blocking(move || started_rx.recv().unwrap())
            .await
            .unwrap();
        drop(worker); // Detached output must release root before its task ticket.
        let lock_session = session.clone();
        let locking = tokio::spawn(async move {
            lock_session.lock().await;
        });
        tokio::task::yield_now().await;
        assert_eq!(session.state(), SecretSessionState::Locking);
        assert!(!locking.is_finished());
        release_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), locking)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(session.state(), SecretSessionState::Locked);
        assert!(session.inner.state.lock().root.is_none());
    }

    #[tokio::test]
    async fn password_unlock_handles_authentication_failure_without_leaving_busy_state() {
        let session = session();
        assert_eq!(
            session
                .unlock(config(), Zeroizing::new(b"wrong password".to_vec()))
                .await,
            Err(EncryptionError::UnlockFailed)
        );
        assert_eq!(session.state(), SecretSessionState::Locked);
        session
            .unlock(config(), Zeroizing::new(b"session password".to_vec()))
            .await
            .unwrap();
        assert_eq!(session.state(), SecretSessionState::Unlocked);
        session.lock().await;
        assert_eq!(session.state(), SecretSessionState::Locked);
    }

    #[test]
    fn local_checkpoint_encoding_is_exact_and_remains_bound_to_repository() {
        let config = config();
        let checkpoint = config.checkpoint(&fixture_root()).unwrap();
        let bytes = checkpoint.to_local_bytes();
        let decoded = ConfigCheckpoint::from_local_bytes(&bytes).unwrap();
        assert_eq!(decoded, checkpoint);
        assert_eq!(
            decoded.validate(&config, &fixture_root()).unwrap(),
            checkpoint
        );
        for offset in 0..bytes.len() {
            assert!(ConfigCheckpoint::from_local_bytes(&bytes[..offset]).is_err());
        }
        let mut trailing = bytes.to_vec();
        trailing.push(0);
        assert!(ConfigCheckpoint::from_local_bytes(&trailing).is_err());
        let mut zero_generation = bytes;
        zero_generation[24..32].fill(0);
        assert!(ConfigCheckpoint::from_local_bytes(&zero_generation).is_err());
        let repository = RepositoryId::generate().unwrap();
        assert!(matches!(
            SecretSession::new(
                SessionBinding::new("host".into(), "account".into(), repository).unwrap(),
                Some(checkpoint)
            ),
            Err(EncryptionError::IdentityChanged)
        ));
        assert!(SessionBinding::new("".into(), "account".into(), repository).is_err());
        assert!(SessionBinding::new("host".into(), "account\n".into(), repository).is_err());
        static_assertions::assert_not_impl_any!(SessionOperation: Clone, serde::Serialize);
        static_assertions::assert_not_impl_any!(SecretSession: serde::Serialize);
        static_assertions::assert_not_impl_any!(RefObservation: Clone, serde::Serialize, fmt::Debug);
    }
}
