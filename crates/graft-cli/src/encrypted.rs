//! Session-only native CLI access to an explicitly enrolled encrypted remote.
use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use graft::{
    e2ee::{
        ConfigPublicationOutcome, EncryptedHttpClient, FileCheckpointStore,
        FileConfigMutationJournal, FileMultipartJournalProvider, FileRefMutationJournalProvider,
        ObjectPublicationOutcome, RepositoryId, SecretSession, SessionBinding,
    },
    remote::{EncryptedTransferBudget, RemoteConfig, RemoteCredentials},
    repo::Repository,
};
use graft_sdk::RepositorySession;
use serde_json::Value;
use std::{
    io::{IsTerminal, Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
};
use zeroize::Zeroizing;

mod initialization;
mod migration;
#[cfg(unix)]
mod terminal_password;
#[cfg(windows)]
#[path = "encrypted/terminal_password_windows.rs"]
mod terminal_password;

use graft::e2ee::native_fs as fs;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

#[derive(Args)]
pub struct EncryptedArgs {
    /// Absolute target worktree; clone requires an empty or absent target.
    #[arg(long)]
    worktree: PathBuf,
    /// Absolute private host-state directory outside every repository/worktree.
    #[arg(long)]
    state_dir: PathBuf,
    /// Explicit first enrollment of a new state directory. Never repairs lost trust.
    #[arg(long)]
    enroll: bool,
    /// Pinned encrypted HTTP endpoint, without credentials in its URL.
    #[arg(long)]
    url: String,
    /// Pinned canonical `UUIDv4` from the remote's account management API.
    #[arg(long)]
    uuid: String,
    /// Authenticated account subject associated with the supplied bearer token.
    #[arg(long)]
    account: String,
    /// Name of an environment variable containing the account bearer token.
    #[arg(long)]
    token_env: String,
    /// Read exact UTF-8 sync-password bytes from a non-terminal stdin until EOF.
    #[arg(long)]
    password_stdin: bool,
    /// Repository remote name; clone registers origin.
    #[arg(long, default_value = "origin")]
    remote: String,
    /// Selected branch; defaults to main.
    #[arg(long, default_value = "main")]
    branch: String,
    #[command(subcommand)]
    action: EncryptedAction,
}

#[derive(Subcommand)]
enum EncryptedAction {
    Clone,
    Fetch,
    Pull,
    Push,
    /// Change the wrapping password without replacing the repository root key.
    ChangePassword {
        /// Read exact new-password bytes from an inherited non-terminal descriptor >= 3.
        #[arg(long)]
        new_password_fd: Option<u32>,
    },
    /// Reconcile, then retry the exact retained password-change candidate if needed.
    ResumePasswordChange,
    /// Explicitly allocate configuration recovery for an older enrolled host directory.
    EnrollConfigRecovery,
    /// Explicitly allocate multipart recovery for an older enrolled host directory.
    EnrollMultipartRecovery,
    /// List retained uploads by opaque key without exposing logical paths or secrets.
    MultipartUploads,
    /// Reconcile/retry an existing retained upload with its original ciphertext.
    ResumeMultipart {
        #[arg(long)]
        key: String,
    },
    /// Explicitly abort its old server session and resend the retained ciphertext.
    RestartMultipart {
        #[arg(long)]
        key: String,
    },
    /// Freeze selected local history, explicit target mappings and one new root offline.
    PrepareMigration {
        #[arg(
            long,
            conflicts_with = "references",
            required_unless_present = "references"
        )]
        all_refs: bool,
        #[arg(
            long = "ref",
            conflicts_with = "all_refs",
            required_unless_present = "all_refs"
        )]
        references: Vec<String>,
        /// Repeat `SOURCE_FULL_REF=TARGET_FULL_REF` for every selected ref.
        #[arg(long = "map", required = true)]
        mappings: Vec<String>,
        /// Full target branch ref used for encrypted HEAD.
        #[arg(long)]
        target_head: String,
    },
    /// Reopen the exact prepared migration and publish its remaining phases.
    ResumeMigration,
    /// Recover all retained history into a fresh destination; does not cut over writers.
    VerifyMigration {
        #[arg(long)]
        verification_directory: PathBuf,
    },
    /// Fence local Graft history writers, independently verify, then switch the local remote.
    CutoverMigration {
        #[arg(long)]
        verification_directory: PathBuf,
    },
    /// Publish the selected committed local branch to a newly registered empty encrypted remote.
    Initialize {
        /// Absolute empty/absent worktree for independent validation before reporting connected.
        #[arg(long)]
        verification_directory: PathBuf,
    },
    /// Resume the exact retained root, configuration and initial source.
    ResumeInitialization {
        #[arg(long)]
        verification_directory: PathBuf,
    },
}

fn new_password(descriptor: Option<u32>) -> Result<Zeroizing<Vec<u8>>> {
    if let Some(descriptor) = descriptor {
        if descriptor < 3 {
            bail!("new-password descriptor must be an inherited descriptor >= 3");
        }
        let input = inherited_password_input(descriptor)
            .map_err(|_| anyhow::anyhow!("protected new-password descriptor is unavailable"))?;
        if input.is_terminal() {
            bail!("new-password descriptor must not refer to a terminal");
        }
        return password(input);
    }
    let first = terminal_password::read_with_prompt("New sync password: ")?;
    let confirmation = terminal_password::read_with_prompt("Confirm new sync password: ")?;
    if first.as_slice() != confirmation.as_slice() {
        bail!("new sync-password confirmation does not match");
    }
    Ok(first)
}

#[cfg(unix)]
fn inherited_password_input(descriptor: u32) -> std::io::Result<std::fs::File> {
    std::fs::File::open(format!("/dev/fd/{descriptor}"))
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn inherited_password_input(descriptor: u32) -> std::io::Result<std::fs::File> {
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::{
        Foundation::{DUPLICATE_SAME_ACCESS, DuplicateHandle},
        System::Threading::GetCurrentProcess,
    };
    unsafe extern "C" {
        fn _get_osfhandle(descriptor: i32) -> isize;
    }
    let descriptor = i32::try_from(descriptor).map_err(|_| std::io::ErrorKind::InvalidInput)?;
    // Duplicate the inherited CRT descriptor; never close the host's original.
    unsafe {
        let handle = _get_osfhandle(descriptor);
        if handle == -1 {
            return Err(std::io::ErrorKind::InvalidInput.into());
        }
        let mut duplicate = std::ptr::null_mut();
        if DuplicateHandle(
            GetCurrentProcess(),
            handle as _,
            GetCurrentProcess(),
            &mut duplicate,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        ) == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        Ok(std::fs::File::from_raw_handle(duplicate))
    }
}

fn config_publication_result(outcome: ConfigPublicationOutcome) -> Result<Value> {
    match outcome {
        ConfigPublicationOutcome::Confirmed => {
            Ok(serde_json::json!({"operation":"change-password","status":"confirmed"}))
        }
        ConfigPublicationOutcome::Conflict => {
            Err(ConfigPublicationError(graft_sdk::SdkErrorCode::RepositoryStale).into())
        }
        ConfigPublicationOutcome::Unconfirmed => Err(ConfigPublicationError(
            graft_sdk::SdkErrorCode::RemotePublicationUnconfirmed,
        )
        .into()),
        ConfigPublicationOutcome::OutcomeUnknown { .. } => Err(ConfigPublicationError(
            graft_sdk::SdkErrorCode::RemotePublicationOutcomeUnknown,
        )
        .into()),
    }
}

#[derive(Debug)]
struct ConfigPublicationError(graft_sdk::SdkErrorCode);
impl std::fmt::Display for ConfigPublicationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("encrypted publication was not confirmed; retain recovery state and resume the operation")
    }
}
impl std::error::Error for ConfigPublicationError {}

fn password(mut input: impl Read) -> Result<Zeroizing<Vec<u8>>> {
    // Allocate the bounded input and overflow sentinel before reading secrets.
    let mut bytes = Zeroizing::new(Vec::with_capacity(1025));
    input
        .by_ref()
        .take(1025)
        .read_to_end(&mut bytes)
        .map_err(|_| anyhow::anyhow!("failed to read protected sync-password input"))?;
    if bytes.is_empty() || bytes.len() > 1024 || std::str::from_utf8(&bytes).is_err() {
        bail!("sync-password input must contain 1–1024 exact UTF-8 bytes");
    }
    Ok(bytes)
}

fn repository_id(text: &str) -> Result<RepositoryId> {
    if text.len() != 36
        || text.bytes().enumerate().any(|(i, b)| {
            if [8, 13, 18, 23].contains(&i) {
                b != b'-'
            } else {
                !b.is_ascii_digit() && !(b'a'..=b'f').contains(&b)
            }
        })
    {
        bail!("encrypted remote UUID must be canonical lowercase UUIDv4");
    }
    let hex: Vec<u8> = text.bytes().filter(|b| *b != b'-').collect();
    let mut bytes = [0; 16];
    for (index, pair) in hex.chunks_exact(2).enumerate() {
        let digit = |b: u8| {
            if b.is_ascii_digit() {
                b - b'0'
            } else {
                b - b'a' + 10
            }
        };
        bytes[index] = (digit(pair[0]) << 4) | digit(pair[1]);
    }
    RepositoryId::from_bytes(bytes).map_err(anyhow::Error::from)
}

fn create_private(path: &Path) -> Result<()> {
    fs::create_private_directory(path).context("cannot allocate new encrypted host state")?;
    fs::sync_directory(&fs::open(path)?)?;
    if let Some(parent) = path.parent() {
        fs::sync_directory(&fs::open(parent)?)?;
    }
    Ok(())
}

fn enroll_config_recovery(
    state: &Path,
    binding: &SessionBinding,
) -> Result<FileConfigMutationJournal> {
    // Persist allocation evidence before creating the slot. A missing allocated slot
    // or interrupted enrollment must never look like a pre-feature host enrollment.
    let marker = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(state.join("config-recovery-enrolled"))
        .context(
            "configuration recovery was already allocated or allocation evidence is unavailable",
        )?;
    (&marker).write_all(b"graft-cli/config-recovery/v1\n")?;
    marker.sync_all()?;
    fs::sync_directory(&fs::open(state)?)?;
    let slot = state.join("config-recovery");
    create_private(&slot)?;
    Ok(FileConfigMutationJournal::create(&slot, binding)?)
}

fn enroll_multipart_recovery(
    state: &Path,
    binding: &SessionBinding,
) -> Result<FileMultipartJournalProvider> {
    let marker = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(state.join("multipart-recovery-enrolled"))
        .context(
            "multipart recovery was already allocated or allocation evidence is unavailable",
        )?;
    (&marker).write_all(b"graft-cli/multipart-recovery/v1\n")?;
    marker.sync_all()?;
    fs::sync_directory(&fs::open(state)?)?;
    let slot = state.join("multipart-recovery");
    create_private(&slot)?;
    Ok(FileMultipartJournalProvider::create(&slot, binding)?)
}

pub fn run(args: EncryptedArgs) -> Result<Value> {
    if !args.worktree.is_absolute() || !args.state_dir.is_absolute() {
        bail!("encrypted worktree and host-state paths must be absolute");
    }
    if matches!(args.action, EncryptedAction::Clone) && args.remote != "origin" {
        bail!("encrypted clone registers the origin remote");
    }
    let uuid = repository_id(&args.uuid)?;
    // State binding deliberately includes the exact endpoint; changes need new enrollment.
    let binding = SessionBinding::new(
        format!("graft-cli:{}", args.url),
        args.account.clone(),
        uuid,
    )?;
    let remote = RemoteConfig::EncryptedHttp {
        url: args.url.clone(),
        uuid: args.uuid.clone(),
    };
    let multipart_recovery = matches!(
        args.action,
        EncryptedAction::MultipartUploads
            | EncryptedAction::ResumeMultipart { .. }
            | EncryptedAction::RestartMultipart { .. }
    );
    if multipart_recovery && args.enroll {
        bail!("multipart recovery requires an existing enrolled host-state directory");
    }
    if !matches!(args.action, EncryptedAction::Clone)
        && !migration::is_action(&args.action)
        && !multipart_recovery
    {
        let repository = Repository::discover(&args.worktree)?;
        let initial_registration = matches!(args.action, EncryptedAction::Initialize { .. })
            && !repository.config()?.remotes.contains_key(&args.remote);
        if !initial_registration && repository.remote_get_url(&args.remote)?.config != remote {
            bail!("persisted remote does not match the explicitly pinned encrypted remote");
        }
    }
    // Validate proposed placement before creating the state directory, unlocking or networking.
    let preflight = RepositorySession::new(&args.worktree);
    initialization::preflight(&args, &preflight)?;
    migration::preflight(&args, &preflight)?;
    if args.enroll {
        if args.state_dir.try_exists()? {
            bail!("first enrollment requires a new host-state directory");
        }
        preflight
            .validate_encrypted_host_directory_targets(std::slice::from_ref(&args.state_dir))?;
    } else {
        preflight.validate_encrypted_host_directories(std::slice::from_ref(&args.state_dir))?;
    }
    let sync_password = if args.password_stdin {
        if std::io::stdin().is_terminal() {
            bail!(
                "--password-stdin requires protected non-terminal input; terminal echo is refused"
            );
        }
        password(std::io::stdin().lock())?
    } else if matches!(
        args.action,
        EncryptedAction::Initialize { .. } | EncryptedAction::PrepareMigration { .. }
    ) {
        new_password(None)?
    } else {
        terminal_password::read()?
    };
    let token = Zeroizing::new(
        std::env::var(&args.token_env)
            .map_err(|_| anyhow::anyhow!("account bearer token is unavailable"))?,
    );
    if token.is_empty() {
        bail!("account bearer token is empty");
    }
    if args.enroll {
        create_private(&args.state_dir)?;
    }
    let checkpoint = args.state_dir.join("checkpoint");
    let journals = args.state_dir.join("ref-recovery");
    let uploads = args.state_dir.join("multipart-recovery");
    let staging = args.state_dir.join("staging");
    let config_recovery = args.state_dir.join("config-recovery");
    if args.enroll {
        for path in [&checkpoint, &journals, &staging] {
            create_private(path)?;
        }
    }
    preflight.validate_encrypted_host_directories(&[
        checkpoint.clone(),
        journals.clone(),
        staging.clone(),
    ])?;
    let checkpoint = if args.enroll {
        FileCheckpointStore::create(&checkpoint, &binding)?
    } else {
        FileCheckpointStore::open(&checkpoint, &binding)?
    };
    let journals = if args.enroll {
        FileRefMutationJournalProvider::create(&journals, &binding)?
    } else {
        FileRefMutationJournalProvider::open(&journals, &binding)?
    };
    let uploads = if args.enroll {
        preflight.validate_encrypted_host_directory_targets(std::slice::from_ref(&uploads))?;
        Some(enroll_multipart_recovery(&args.state_dir, &binding)?)
    } else if matches!(args.action, EncryptedAction::EnrollMultipartRecovery) {
        None
    } else {
        preflight.validate_encrypted_host_directories(std::slice::from_ref(&uploads))?;
        Some(FileMultipartJournalProvider::open(&uploads, &binding)?)
    };
    let config_journal = if args.enroll {
        preflight
            .validate_encrypted_host_directory_targets(std::slice::from_ref(&config_recovery))?;
        Some(Arc::new(enroll_config_recovery(&args.state_dir, &binding)?))
    } else if matches!(
        args.action,
        EncryptedAction::ChangePassword { .. } | EncryptedAction::ResumePasswordChange
    ) {
        preflight.validate_encrypted_host_directories(std::slice::from_ref(&config_recovery))?;
        Some(Arc::new(FileConfigMutationJournal::open(
            &config_recovery,
            &binding,
        )?))
    } else {
        None
    };
    let secret = SecretSession::with_checkpoint_store(binding, Arc::new(checkpoint))?;
    let credentials = RemoteCredentials::explicit();
    credentials.set_http_bearer_token(&args.remote, token.to_string())?;
    let client = EncryptedHttpClient::new(
        &args.url,
        args.remote.clone(),
        credentials.clone(),
        secret.clone(),
    )?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let outcome = (|| {
        if migration::is_action(&args.action) {
            return migration::run(
                &args,
                &secret,
                credentials,
                &runtime,
                (
                    Arc::new(journals),
                    Arc::new(uploads.context("multipart recovery is unavailable")?),
                ),
                sync_password,
            );
        }
        if matches!(
            args.action,
            EncryptedAction::Initialize { .. } | EncryptedAction::ResumeInitialization { .. }
        ) {
            return initialization::run(
                &args,
                &client,
                &secret,
                credentials,
                &runtime,
                (
                    Arc::new(journals),
                    Arc::new(uploads.context("multipart recovery is unavailable")?),
                ),
                sync_password,
            );
        }
        let current_password = matches!(args.action, EncryptedAction::ChangePassword { .. })
            .then(|| Zeroizing::new(sync_password.to_vec()));
        runtime.block_on(client.unlock(sync_password))?;
        match args.action {
            EncryptedAction::MultipartUploads => {
                let uploads = uploads.context("multipart recovery is unavailable")?;
                let retained = uploads.retained_uploads(secret.binding())?;
                return Ok(
                    serde_json::json!({"operation":"multipart-uploads","uploads":retained.into_iter().map(|upload| serde_json::json!({"key":upload.opaque_key,"ciphertext_bytes":upload.ciphertext_length,"server_session_recorded":upload.server_session_recorded})).collect::<Vec<_>>()}),
                );
            }
            EncryptedAction::ResumeMultipart { ref key }
            | EncryptedAction::RestartMultipart { ref key } => {
                let uploads = Arc::new(uploads.context("multipart recovery is unavailable")?);
                let restart = matches!(args.action, EncryptedAction::RestartMultipart { .. });
                let outcome = if restart {
                    runtime.block_on(
                        client.restart_retained_object_multipart(key, uploads, &staging),
                    )?
                } else {
                    runtime
                        .block_on(client.resume_retained_object_multipart(key, uploads, &staging))?
                };
                return match outcome {
                    ObjectPublicationOutcome::Created | ObjectPublicationOutcome::Reused => Ok(
                        serde_json::json!({"operation":if restart {"restart-multipart"} else {"resume-multipart"},"status":"confirmed","key":key}),
                    ),
                    ObjectPublicationOutcome::Unconfirmed => Err(ConfigPublicationError(
                        graft_sdk::SdkErrorCode::RemotePublicationUnconfirmed,
                    )
                    .into()),
                    ObjectPublicationOutcome::OutcomeUnknown { .. } => Err(ConfigPublicationError(
                        graft_sdk::SdkErrorCode::RemotePublicationOutcomeUnknown,
                    )
                    .into()),
                };
            }
            EncryptedAction::EnrollMultipartRecovery => {
                if !args.enroll {
                    let uploads = args.state_dir.join("multipart-recovery");
                    preflight.validate_encrypted_host_directory_targets(std::slice::from_ref(
                        &uploads,
                    ))?;
                    enroll_multipart_recovery(&args.state_dir, secret.binding())?;
                }
                return Ok(
                    serde_json::json!({"operation":"enroll-multipart-recovery","status":"confirmed"}),
                );
            }
            EncryptedAction::EnrollConfigRecovery => {
                if !args.enroll {
                    preflight.validate_encrypted_host_directory_targets(std::slice::from_ref(
                        &config_recovery,
                    ))?;
                    enroll_config_recovery(&args.state_dir, secret.binding())?;
                }
                return Ok(
                    serde_json::json!({"operation":"enroll-config-recovery","status":"confirmed"}),
                );
            }
            EncryptedAction::ChangePassword { new_password_fd } => {
                let journal = config_journal.context("configuration recovery is unavailable")?;
                runtime.block_on(client.ensure_no_pending_config_mutation(journal.clone()))?;
                let replacement = new_password(new_password_fd)?;
                let candidate = runtime.block_on(client.prepare_password_change(
                    current_password.context("current password is unavailable")?,
                    replacement,
                ))?;
                return config_publication_result(
                    runtime.block_on(client.publish_config_mutation(&candidate, journal))?,
                );
            }
            EncryptedAction::ResumePasswordChange => {
                let journal = config_journal.context("configuration recovery is unavailable")?;
                let candidate =
                    runtime.block_on(client.restore_config_mutation(journal.clone()))?;
                let outcome = runtime
                    .block_on(client.reconcile_config_mutation(&candidate, journal.clone()))?;
                let outcome = if matches!(outcome, ConfigPublicationOutcome::Unconfirmed) {
                    runtime.block_on(client.publish_config_mutation(&candidate, journal))?
                } else {
                    outcome
                };
                return config_publication_result(outcome);
            }
            _ => {}
        }
        credentials.set_encrypted_session_with_recovery(
            &args.remote,
            &remote,
            secret.clone(),
            EncryptedTransferBudget {
                private_directory: staging,
                max_buffered_plaintext: 4 * 1024 * 1024,
                max_ciphertext_spool: 8 * 1024 * 1024 * 1024,
            },
            Arc::new(journals),
            Arc::new(uploads.context("multipart recovery is unavailable")?),
        )?;
        let session =
            RepositorySession::new_with_remote_credentials(&args.worktree, None, credentials);
        session.open()?;
        let operation = match args.action {
            EncryptedAction::Clone => {
                session.clone_repository_with_config(remote, Some(&args.branch))
            }
            EncryptedAction::Fetch => session.fetch(Some(&args.remote), Some(&args.branch)),
            EncryptedAction::Pull => session.pull(Some(&args.remote), Some(&args.branch)),
            EncryptedAction::Push => session.push(Some(&args.remote), Some(&args.branch)),
            _ => unreachable!("configuration actions return before opening the repository session"),
        };
        let close = session.close();
        let value = operation?;
        close?;
        Ok(value)
    })();
    runtime.block_on(secret.disconnect());
    outcome
}

pub fn error_output(error: &anyhow::Error) -> Value {
    let code = error
        .chain()
        .find_map(|cause| {
            if let Some(error) = cause.downcast_ref::<ConfigPublicationError>() {
                Some(error.0)
            } else if let Some(error) = cause.downcast_ref::<graft_sdk::SdkError>() {
                Some(error.code())
            } else if let Some(error) = cause.downcast_ref::<graft::e2ee::EncryptedHttpError>() {
                Some(graft_sdk::encrypted_http_error_code(error))
            } else if let Some(error) = cause.downcast_ref::<graft::e2ee::EncryptionError>() {
                Some(graft_sdk::encryption_error_code(error))
            } else if cause.is::<std::io::Error>() {
                Some(graft_sdk::SdkErrorCode::Encrypted(
                    graft_sdk::EncryptedErrorCode::LocalIo,
                ))
            } else {
                None
            }
        })
        .unwrap_or(graft_sdk::SdkErrorCode::InvalidArgument);
    serde_json::json!({"operation":"e2ee","error":{"code":code.as_str(),"message":error.to_string()}})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn allocated_multipart_recovery_cannot_be_recreated_after_slot_loss() {
        let fixture = tempfile::tempdir().unwrap();
        let host = fixture.path().join("host");
        create_private(&host).unwrap();
        let binding = SessionBinding::new(
            "cli".into(),
            "account".into(),
            repository_id("00010203-0405-4607-8809-0a0b0c0d0e0f").unwrap(),
        )
        .unwrap();
        enroll_multipart_recovery(&host, &binding).unwrap();
        assert!(enroll_multipart_recovery(&host, &binding).is_err());
        fs::rename(
            host.join("multipart-recovery"),
            fixture.path().join("retained-slot"),
        )
        .unwrap();
        assert!(enroll_multipart_recovery(&host, &binding).is_err());
        assert!(!host.join("multipart-recovery").exists());
    }
    #[test]
    fn allocated_configuration_recovery_cannot_be_recreated_after_slot_loss() {
        let fixture = tempfile::tempdir().unwrap();
        let host = fixture.path().join("host");
        create_private(&host).unwrap();
        let binding = SessionBinding::new(
            "cli".into(),
            "account".into(),
            repository_id("00010203-0405-4607-8809-0a0b0c0d0e0f").unwrap(),
        )
        .unwrap();
        enroll_config_recovery(&host, &binding).unwrap();
        assert!(enroll_config_recovery(&host, &binding).is_err());
        fs::rename(
            host.join("config-recovery"),
            fixture.path().join("retained-slot"),
        )
        .unwrap();
        assert!(enroll_config_recovery(&host, &binding).is_err());
        assert!(!host.join("config-recovery").exists());
    }
    #[test]
    fn config_publication_only_reports_success_after_confirmation() {
        assert_eq!(
            config_publication_result(ConfigPublicationOutcome::Confirmed).unwrap()["status"],
            "confirmed"
        );
        for (outcome, code) in [
            (
                ConfigPublicationOutcome::Unconfirmed,
                "GRAFT_SDK_REMOTE_PUBLICATION_UNCONFIRMED",
            ),
            (
                ConfigPublicationOutcome::OutcomeUnknown { error: None },
                "GRAFT_SDK_REMOTE_PUBLICATION_OUTCOME_UNKNOWN",
            ),
            (
                ConfigPublicationOutcome::Conflict,
                "GRAFT_SDK_REPOSITORY_STALE",
            ),
        ] {
            assert_eq!(
                error_output(&config_publication_result(outcome).unwrap_err())["error"]["code"],
                code
            );
        }
        for descriptor in 0..3 {
            assert!(new_password(Some(descriptor)).is_err());
        }
    }
    #[test]
    fn json_errors_preserve_typed_authentication_and_integrity_causes() {
        let integrity = anyhow::Error::new(graft::e2ee::EncryptionError::AuthenticationFailed);
        assert_eq!(
            error_output(&integrity)["error"]["code"],
            "GRAFT_SDK_E2EE_INTEGRITY_FAILED"
        );
        let account = anyhow::Error::new(graft::e2ee::EncryptedHttpError::HttpStatus(401));
        assert_eq!(
            error_output(&account)["error"]["code"],
            "GRAFT_SDK_E2EE_AUTHENTICATION_FAILED"
        );
    }
    #[test]
    fn protected_password_input_preserves_exact_bytes_and_rejects_invalid_input() {
        for value in [
            b" spaces \n".as_slice(),
            "密码\r\n".as_bytes(),
            &vec![b'x'; 1024],
        ] {
            assert_eq!(password(value).unwrap().as_slice(), value);
        }
        for value in [b"".as_slice(), b"\xff", &vec![b'x'; 1025]] {
            assert!(password(value).is_err());
        }
    }
    #[test]
    fn identity_requires_canonical_v4_uuid() {
        assert!(repository_id("00010203-0405-4607-8809-0a0b0c0d0e0f").is_ok());
        for value in [
            "00010203-0405-1607-8809-0a0b0c0d0e0f",
            "00010203-0405-4607-7809-0a0b0c0d0e0f",
            "00010203-0405-4607-8809-0A0B0C0D0E0F",
            "",
        ] {
            assert!(repository_id(value).is_err());
        }
    }

    #[test]
    fn new_host_state_placement_is_checked_without_allocation() {
        let fixture = tempfile::tempdir().unwrap();
        let worktree = fixture.path().join("future-worktree");
        let host = fixture.path().join("future-host");
        let session = RepositorySession::new(&worktree);
        session
            .validate_encrypted_host_directory_targets(std::slice::from_ref(&host))
            .unwrap();
        assert!(!host.exists());
        assert!(!worktree.exists());
        for invalid in [&worktree, &worktree.join("private"), fixture.path()] {
            assert!(
                session
                    .validate_encrypted_host_directory_targets(&[invalid.to_owned()])
                    .is_err()
            );
        }
        fs::create_dir(&worktree).unwrap();
        Repository::init(&worktree).unwrap();
        #[cfg(unix)]
        {
            let alias = fixture.path().join("alias");
            std::os::unix::fs::symlink(&worktree, &alias).unwrap();
            assert!(
                session
                    .validate_encrypted_host_directory_targets(&[alias.join("future-private")])
                    .is_err()
            );
        }
        let child = RepositorySession::new(worktree.join("future-child"));
        assert!(
            child
                .validate_encrypted_host_directory_targets(&[worktree.join("future-private")])
                .is_err()
        );
        assert!(!worktree.join("future-private").exists());
    }
}
