use super::download::{finish_staging, local_io_error, private_staging_file};
use super::session::SessionEpoch;
use super::{ChunkSize, EncryptedHttpError, EncryptionError, ObjectEncryptor, SessionOperation};
use sha2::{Digest, Sha256};
use std::{
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use zeroize::Zeroizing;

type Result<T> = std::result::Result<T, EncryptedHttpError>;

/// The repository must supply the identity of a fixed, privately prepared snapshot.
/// Preparation verifies these bytes again before permitting any publication.
#[derive(Clone)]
pub struct ObjectSnapshot {
    pub logical_key: String,
    pub plaintext_length: u64,
    pub plaintext_sha256: [u8; 32],
    pub chunk_size: ChunkSize,
}

/// One fully checked ciphertext spool. It contains no retained encryption keys.
/// Failed preparation never exposes a partial spool. Retry this value unchanged.
pub struct PreparedEncryptedObject {
    pub(super) file: Arc<Mutex<std::fs::File>>,
    pub(super) epoch: SessionEpoch,
    pub(super) snapshot: ObjectSnapshot,
    pub(super) remote_key: String,
    pub(super) ciphertext_length: u64,
    pub(super) ciphertext_sha256: [u8; 32],
}

impl PreparedEncryptedObject {
    pub(super) fn retained_copy(&self) -> Self {
        Self {
            file: self.file.clone(),
            epoch: self.epoch,
            snapshot: self.snapshot.clone(),
            remote_key: self.remote_key.clone(),
            ciphertext_length: self.ciphertext_length,
            ciphertext_sha256: self.ciphertext_sha256,
        }
    }
    pub async fn prepare(
        operation: &SessionOperation,
        snapshot: ObjectSnapshot,
        source: &mut (impl AsyncRead + Unpin),
        directory: &Path,
        maximum_ciphertext: u64,
    ) -> Result<Self> {
        let prepare_trace = crate::trace::PushTraceSpan::new("encrypted_spool_prepare_total");
        let header_trace = crate::trace::PushTraceSpan::new("encrypted_spool_open_header");
        operation.check_active()?;
        let mut encryptor = ObjectEncryptor::new(
            operation.root()?,
            &snapshot.logical_key,
            snapshot.plaintext_length,
            snapshot.plaintext_sha256,
            snapshot.chunk_size,
        )?;
        let ciphertext_length = encryptor.ciphertext_length()?;
        if ciphertext_length > maximum_ciphertext {
            return Err(EncryptionError::TooLarge.into());
        }
        let remote_key = encryptor.remote_key().to_owned();
        let file = private_staging_file(directory)?;
        let mut file = tokio::fs::File::from_std(file);
        let mut digest = Sha256::new();
        write_ciphertext(operation, &mut file, encryptor.header_bytes(), &mut digest).await?;
        header_trace.finish(&[]);
        let stream_trace = crate::trace::PushTraceSpan::new("encrypted_spool_encrypt_stream");
        encrypt_snapshot(
            operation,
            &snapshot,
            source,
            &mut file,
            &mut encryptor,
            &mut digest,
        )
        .await?;
        encryptor.finish()?;
        stream_trace.finish(&[]);
        let finish_trace = crate::trace::PushTraceSpan::new("encrypted_spool_flush_seek");
        let file = finish_staging(operation, file).await?;
        finish_trace.finish(&[]);
        operation.check_active()?;
        let prepared = Self {
            file: Arc::new(Mutex::new(file)),
            epoch: operation.epoch()?,
            snapshot,
            remote_key,
            ciphertext_length,
            ciphertext_sha256: digest.finalize().into(),
        };
        prepare_trace.finish(&[]);
        Ok(prepared)
    }

    pub fn remote_key(&self) -> &str {
        &self.remote_key
    }
    pub fn ciphertext_length(&self) -> u64 {
        self.ciphertext_length
    }
    pub fn ciphertext_sha256(&self) -> &[u8; 32] {
        &self.ciphertext_sha256
    }

    pub(super) fn validate(&self, operation: &SessionOperation) -> Result<()> {
        if operation.epoch()? != self.epoch {
            return Err(EncryptionError::StaleObservation.into());
        }
        operation.check_active()?;
        Ok(())
    }

    pub(super) fn body(&self) -> (reqwest::Body, CancelUpload) {
        let (stream, cancelled) = self.ciphertext_stream();
        (reqwest::Body::wrap_stream(stream), cancelled)
    }

    pub(super) fn ciphertext_stream(
        &self,
    ) -> (
        impl futures::Stream<Item = std::io::Result<Vec<u8>>> + Send + 'static,
        CancelUpload,
    ) {
        self.ciphertext_range_stream(0, self.ciphertext_length)
    }

    pub(super) fn ciphertext_range_stream(
        &self,
        start: u64,
        end: u64,
    ) -> (
        impl futures::Stream<Item = std::io::Result<Vec<u8>>> + Send + 'static,
        CancelUpload,
    ) {
        let cancelled = Arc::new(AtomicBool::new(false));
        let state = (self.file.clone(), start, end, cancelled.clone());
        let stream =
            futures::stream::try_unfold(state, |(file, offset, length, cancelled)| async move {
                if offset == length {
                    return Ok(None);
                }
                if cancelled.load(Ordering::Acquire) {
                    return Err(std::io::ErrorKind::Interrupted.into());
                }
                let count = (length - offset).min(64 * 1024) as usize;
                let source = file.clone();
                let signal = cancelled.clone();
                let bytes = tokio::task::spawn_blocking(move || {
                    use std::io::{Read, Seek};
                    let mut file = source
                        .lock()
                        .map_err(|_| std::io::Error::other("ciphertext spool unavailable"))?;
                    if signal.load(Ordering::Acquire) {
                        return Err(std::io::ErrorKind::Interrupted.into());
                    }
                    file.seek(std::io::SeekFrom::Start(offset))?;
                    let mut bytes = vec![0; count];
                    file.read_exact(&mut bytes)?;
                    Ok::<_, std::io::Error>(bytes)
                })
                .await
                .map_err(|_| std::io::Error::other("ciphertext spool read failed"))??;
                if cancelled.load(Ordering::Acquire) {
                    return Err(std::io::ErrorKind::Interrupted.into());
                }
                Ok::<_, std::io::Error>(Some((
                    bytes,
                    (file, offset + count as u64, length, cancelled),
                )))
            });
        (stream, CancelUpload(cancelled))
    }
}

/// The body stream holds ciphertext only; its owner cancels it on return or abort.
pub(super) struct CancelUpload(Arc<AtomicBool>);
impl Drop for CancelUpload {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

pub enum ObjectPublicationOutcome {
    Created,
    /// Existing authenticated metadata matches; this does not prove full availability.
    Reused,
    Unconfirmed,
    OutcomeUnknown {
        error: Option<EncryptedHttpError>,
    },
}

async fn encrypt_snapshot(
    operation: &SessionOperation,
    snapshot: &ObjectSnapshot,
    source: &mut (impl AsyncRead + Unpin),
    file: &mut tokio::fs::File,
    encryptor: &mut ObjectEncryptor,
    digest: &mut Sha256,
) -> Result<()> {
    let detail_trace = crate::trace::PushTraceSpan::new("encrypted_spool_stream_detail");
    let trace_enabled = crate::trace::enabled();
    let mut read_time = std::time::Duration::ZERO;
    let mut read_max = std::time::Duration::ZERO;
    let mut encrypt_time = std::time::Duration::ZERO;
    let mut encrypt_max = std::time::Duration::ZERO;
    let mut write_time = std::time::Duration::ZERO;
    let mut write_max = std::time::Duration::ZERO;
    let mut remaining = snapshot.plaintext_length;
    let mut plaintext = Zeroizing::new(vec![0; encryptor.chunk_size()]);
    for _ in 0..encryptor.chunk_count() {
        let length = remaining.min(encryptor.chunk_size() as u64) as usize;
        let started = trace_enabled.then(std::time::Instant::now);
        tokio::select! {
            result = source.read_exact(&mut plaintext[..length]) => {
                result.map_err(|error| if error.kind() == std::io::ErrorKind::UnexpectedEof {
                    EncryptedHttpError::Encryption(EncryptionError::SnapshotMismatch)
                } else { local_io_error(error) })?;
            },
            _ = operation.cancelled() => return Err(EncryptionError::Cancelled.into()),
        }
        if let Some(started) = started {
            let elapsed = started.elapsed();
            read_time += elapsed;
            read_max = read_max.max(elapsed);
        }
        operation.check_active()?;
        let started = trace_enabled.then(std::time::Instant::now);
        let ciphertext = encryptor.encrypt_next(&plaintext[..length])?;
        plaintext[..length].fill(0);
        if let Some(started) = started {
            let elapsed = started.elapsed();
            encrypt_time += elapsed;
            encrypt_max = encrypt_max.max(elapsed);
        }
        let started = trace_enabled.then(std::time::Instant::now);
        write_ciphertext(operation, file, &ciphertext, digest).await?;
        if let Some(started) = started {
            let elapsed = started.elapsed();
            write_time += elapsed;
            write_max = write_max.max(elapsed);
        }
        remaining -= length as u64;
    }
    let started = trace_enabled.then(std::time::Instant::now);
    let mut extra = Zeroizing::new([0; 1]);
    let count = tokio::select! {
        result = source.read(extra.as_mut()) => result.map_err(local_io_error)?,
        _ = operation.cancelled() => return Err(EncryptionError::Cancelled.into()),
    };
    if count != 0 {
        return Err(EncryptionError::SnapshotMismatch.into());
    }
    let micros =
        |duration: std::time::Duration| u64::try_from(duration.as_micros()).unwrap_or(u64::MAX);
    detail_trace.finish(&[
        ("chunks", encryptor.chunk_count()),
        ("read_await_us", micros(read_time)),
        ("read_await_max_us", micros(read_max)),
        ("encrypt_sync_us", micros(encrypt_time)),
        ("encrypt_sync_max_us", micros(encrypt_max)),
        ("write_and_hash_await_us", micros(write_time)),
        ("write_and_hash_await_max_us", micros(write_max)),
        (
            "trailing_read_us",
            micros(started.map_or(std::time::Duration::ZERO, |start| start.elapsed())),
        ),
    ]);
    Ok(())
}

async fn write_ciphertext(
    operation: &SessionOperation,
    file: &mut tokio::fs::File,
    bytes: &[u8],
    digest: &mut Sha256,
) -> Result<()> {
    tokio::select! {
        result = file.write_all(bytes) => result.map_err(local_io_error)?,
        _ = operation.cancelled() => return Err(EncryptionError::Cancelled.into()),
    }
    digest.update(bytes);
    operation.check_active()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    fn private_tempdir() -> tempfile::TempDir {
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        crate::native_fs::set_permissions(
            directory.path(),
            crate::native_fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        directory
    }

    use super::*;
    use crate::e2ee::{
        EncryptedConfig, ObjectReader, SecretSession, SessionBinding, tests::fixture_root,
    };
    use std::io::{Read, Seek};

    struct StartedReader {
        source: tokio::io::DuplexStream,
        started: Option<tokio::sync::oneshot::Sender<()>>,
    }
    impl AsyncRead for StartedReader {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            if let Some(started) = self.started.take() {
                let _ = started.send(());
            }
            std::pin::Pin::new(&mut self.source).poll_read(cx, buf)
        }
    }

    fn session() -> SecretSession {
        let root = fixture_root();
        let config = EncryptedConfig::create(&root, b"test spool password").unwrap();
        let session = SecretSession::new(
            SessionBinding::new("native.test".into(), "account".into(), root.repository()).unwrap(),
            None,
        )
        .unwrap();
        session.unlock_with_root(&config, root).unwrap();
        session
    }
    fn snapshot(bytes: &[u8], size: ChunkSize) -> ObjectSnapshot {
        ObjectSnapshot {
            logical_key: "store/files/private-upload".into(),
            plaintext_length: bytes.len() as u64,
            plaintext_sha256: Sha256::digest(bytes).into(),
            chunk_size: size,
        }
    }

    #[tokio::test]
    async fn ciphertext_spool_roundtrips_empty_boundaries_and_all_sizes() {
        let session = session();
        let operation = session.begin_operation().unwrap();
        for size in [ChunkSize::KiB16, ChunkSize::KiB64, ChunkSize::KiB256] {
            for length in [0, 1, 262145] {
                let directory = private_tempdir();
                let bytes = vec![91; length];
                let prepared = PreparedEncryptedObject::prepare(
                    &operation,
                    snapshot(&bytes, size),
                    &mut bytes.as_slice(),
                    directory.path(),
                    400000,
                )
                .await
                .unwrap();
                let mut wire = Vec::new();
                let mut file = prepared.file.lock().unwrap();
                file.seek(std::io::SeekFrom::Start(0)).unwrap();
                file.read_to_end(&mut wire).unwrap();
                assert_eq!(wire.len() as u64, prepared.ciphertext_length());
                assert_eq!(
                    <[u8; 32]>::from(Sha256::digest(&wire)),
                    *prepared.ciphertext_sha256()
                );
                let end = ObjectReader::header_length(&wire[..72]).unwrap();
                let reader = ObjectReader::open(
                    &fixture_root(),
                    "store/files/private-upload",
                    &wire[..end],
                    wire.len() as u64,
                )
                .unwrap();
                let mut plaintext = Vec::new();
                for index in 0..reader.chunk_count() {
                    let range = reader.chunk_range(index).unwrap();
                    plaintext.extend(
                        reader
                            .decrypt_chunk(index, &wire[range.start as usize..range.end as usize])
                            .unwrap(),
                    );
                }
                assert_eq!(plaintext, bytes);
                assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    assert_eq!(file.metadata().unwrap().permissions().mode() & 0o077, 0);
                }
            }
        }
    }

    #[tokio::test]
    async fn ciphertext_spool_rejects_changed_truncated_extra_and_over_budget_sources() {
        let session = session();
        let operation = session.begin_operation().unwrap();
        for (source, budget, too_large) in [
            (b"evil".as_slice(), 10000, false),
            (b"da".as_slice(), 10000, false),
            (b"dataextra".as_slice(), 10000, false),
            (b"data".as_slice(), 1, true),
        ] {
            let directory = private_tempdir();
            let result = PreparedEncryptedObject::prepare(
                &operation,
                snapshot(b"data", ChunkSize::KiB64),
                &mut &*source,
                directory.path(),
                budget,
            )
            .await;
            assert!(
                matches!(result, Err(EncryptedHttpError::Encryption(error)) if error == if too_large {EncryptionError::TooLarge} else {EncryptionError::SnapshotMismatch})
            );
            assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
        }
    }

    #[tokio::test]
    async fn ciphertext_spool_lock_and_caller_abort_discard_partial_output() {
        for abort in [false, true] {
            let session = session();
            let operation = session.begin_operation().unwrap();
            let (mut writer, reader) = tokio::io::duplex(1);
            writer.write_all(b"d").await.unwrap();
            let (started, receiving) = tokio::sync::oneshot::channel();
            let mut reader = StartedReader { source: reader, started: Some(started) };
            let directory = private_tempdir();
            let path = directory.path().to_owned();
            let preparing = tokio::spawn(async move {
                PreparedEncryptedObject::prepare(
                    &operation,
                    snapshot(b"data", ChunkSize::KiB64),
                    &mut reader,
                    &path,
                    10000,
                )
                .await
            });
            tokio::time::timeout(std::time::Duration::from_secs(2), receiving)
                .await
                .unwrap()
                .unwrap();
            if abort {
                preparing.abort();
                assert!(matches!(preparing.await, Err(error) if error.is_cancelled()));
                tokio::time::timeout(std::time::Duration::from_secs(2), session.lock())
                    .await
                    .unwrap();
            } else {
                tokio::time::timeout(std::time::Duration::from_secs(2), session.lock())
                    .await
                    .unwrap();
                assert!(matches!(
                    preparing.await.unwrap(),
                    Err(EncryptedHttpError::Encryption(EncryptionError::Cancelled))
                ));
            }
            assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
        }
    }
}
