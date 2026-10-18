use super::{EncryptedHttpError, EncryptionError, ObjectReader, SessionOperation};
use bytes::Bytes;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use zeroize::Zeroizing;

type Result<T> = std::result::Result<T, EncryptedHttpError>;

/// Tighten the unnamed file before writing either plaintext or ciphertext.
/// Linux `O_TMPFILE` creation can otherwise inherit a permissive process umask.
pub(crate) fn private_staging_file(directory: &std::path::Path) -> Result<std::fs::File> {
    #[cfg(windows)]
    super::native_fs::validate_private_directory(directory).map_err(local_io_error)?;
    #[cfg(windows)]
    let file = super::native_fs::private_named_tempfile(directory)
        .map_err(local_io_error)?
        .into_file();
    #[cfg(not(windows))]
    let file = tempfile::tempfile_in(directory).map_err(local_io_error)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(local_io_error)?;
    }
    Ok(file)
}

/// Owns transport fragments, but assembles only one bounded AEAD block at a time.
pub(super) struct CiphertextBody {
    response: reqwest::Response,
    pending: Bytes,
    expected: u64,
    received: u64,
}

impl CiphertextBody {
    pub(super) fn new(response: reqwest::Response, expected: u64) -> Self {
        Self {
            response,
            pending: Bytes::new(),
            expected,
            received: 0,
        }
    }

    async fn next(&mut self, operation: &SessionOperation) -> Result<Option<Bytes>> {
        operation.check_active()?;
        let next = tokio::select! {
            next = self.response.chunk() => next.map_err(|error| EncryptedHttpError::Transport(error.without_url()))?,
            _ = operation.cancelled() => return Err(EncryptionError::Cancelled.into()),
        };
        if let Some(chunk) = &next {
            self.received = self
                .received
                .checked_add(chunk.len() as u64)
                .filter(|n| *n <= self.expected)
                .ok_or(EncryptedHttpError::ProtocolMismatch)?;
        }
        operation.check_active()?;
        Ok(next)
    }

    pub(super) async fn read_exact(
        &mut self,
        operation: &SessionOperation,
        length: usize,
    ) -> Result<Vec<u8>> {
        if length > 256 * 1024 + 16 {
            return Err(EncryptionError::InvalidChunk.into());
        }
        let mut block = Vec::with_capacity(length);
        let mut empty_chunks = 0;
        while block.len() < length {
            operation.check_active()?;
            if self.pending.is_empty() {
                self.pending = self
                    .next(operation)
                    .await?
                    .ok_or(EncryptedHttpError::ProtocolMismatch)?;
                if self.pending.is_empty() {
                    empty_chunks += 1;
                    if empty_chunks > 1024 {
                        return Err(EncryptedHttpError::ProtocolMismatch);
                    }
                    continue;
                }
            }
            let count = self.pending.len().min(length - block.len());
            block.extend_from_slice(&self.pending.split_to(count));
        }
        Ok(block)
    }

    pub(super) async fn finish(&mut self, operation: &SessionOperation) -> Result<()> {
        if !self.pending.is_empty()
            || self.received != self.expected
            || self.next(operation).await?.is_some()
        {
            return Err(EncryptedHttpError::ProtocolMismatch);
        }
        Ok(())
    }
}

pub(super) async fn stream_plaintext(
    operation: &SessionOperation,
    reader: &ObjectReader,
    body: &mut CiphertextBody,
    file: &mut tokio::fs::File,
) -> Result<()> {
    stream_object_chunks(operation, reader, body, file).await?;
    body.finish(operation).await
}

/// Read one exact object's payload inside a larger aggregate body.
pub(super) async fn stream_object_chunks(
    operation: &SessionOperation,
    reader: &ObjectReader,
    body: &mut CiphertextBody,
    file: &mut tokio::fs::File,
) -> Result<()> {
    let mut digest = Sha256::new();
    for index in 0..reader.chunk_count() {
        let range = reader.chunk_range(index)?;
        let length =
            usize::try_from(range.end - range.start).map_err(|_| EncryptionError::TooLarge)?;
        let ciphertext = body.read_exact(operation, length).await?;
        operation.check_active()?;
        let plaintext = Zeroizing::new(reader.decrypt_chunk(index, &ciphertext)?);
        digest.update(&plaintext);
        tokio::select! {
            result = file.write_all(&plaintext) => result.map_err(local_io_error)?,
            _ = operation.cancelled() => return Err(EncryptionError::Cancelled.into()),
        }
    }
    if <[u8; 32]>::from(digest.finalize()) != *reader.plaintext_digest() {
        return Err(EncryptionError::SnapshotMismatch.into());
    }
    operation.check_active()?;
    Ok(())
}

pub(super) async fn finish_staging(
    operation: &SessionOperation,
    mut file: tokio::fs::File,
) -> Result<std::fs::File> {
    tokio::select! {
        result = async {
            file.flush().await.map_err(local_io_error)?;
            file.seek(std::io::SeekFrom::Start(0)).await.map_err(local_io_error)?;
            Ok(file.into_std().await)
        } => result,
        _ = operation.cancelled() => Err(EncryptionError::Cancelled.into()),
    }
}

pub(super) fn local_io_error(error: std::io::Error) -> EncryptedHttpError {
    EncryptedHttpError::LocalIo(error.kind())
}
