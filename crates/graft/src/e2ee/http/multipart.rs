use super::*;

#[cfg(all(test, unix))]
#[path = "multipart_tests.rs"]
mod tests;

#[cfg(unix)]
#[derive(Clone, Copy)]
enum MultipartSource<'a> {
    Prepared(&'a PreparedEncryptedObject),
    Retained(&'a str),
}

impl EncryptedHttpClient {
    pub async fn requires_multipart(&self, length: u64) -> Result<bool> {
        let operation = self.operation().await?;
        let verified = self.verified.lock().await;
        operation.check_active()?;
        let limits = verified
            .as_ref()
            .ok_or(EncryptionError::StaleObservation)?
            .limits;
        let maximum = limits
            .max_request_bytes
            .unwrap_or(64 * 1024 * 1024)
            .min(64 * 1024 * 1024);
        if length <= maximum {
            return Ok(false);
        }
        if !limits.multipart_object {
            return Err(EncryptedHttpError::UnsupportedProfile);
        }
        Ok(true)
    }

    /// Durable exact-ciphertext multipart publication. Every completion (including
    /// 412 or response loss) authenticates all winning object chunks and SHA-256.
    #[cfg(unix)]
    pub async fn publish_object_multipart(
        &self,
        candidate: &PreparedEncryptedObject,
        provider: Arc<dyn crate::e2ee::MultipartJournalProvider>,
        verification_directory: &Path,
    ) -> Result<ObjectPublicationOutcome> {
        self.publish_multipart_inner(
            MultipartSource::Prepared(candidate),
            provider,
            verification_directory,
            false,
        )
        .await
    }

    /// Explicitly abort the recorded server session, durably retain that result,
    /// then send every part under a new server session using the same ciphertext.
    /// An unacknowledged abort never clears the previous session identity.
    #[cfg(unix)]
    pub async fn restart_object_multipart(
        &self,
        candidate: &PreparedEncryptedObject,
        provider: Arc<dyn crate::e2ee::MultipartJournalProvider>,
        verification_directory: &Path,
    ) -> Result<ObjectPublicationOutcome> {
        self.publish_multipart_inner(
            MultipartSource::Prepared(candidate),
            provider,
            verification_directory,
            true,
        )
        .await
    }

    /// Trusted-host recovery from an existing private slot. No plaintext snapshot
    /// or replacement ciphertext is prepared; an absent allocation fails closed.
    #[cfg(unix)]
    pub async fn resume_retained_object_multipart(
        &self,
        opaque_key: &str,
        provider: Arc<dyn crate::e2ee::MultipartJournalProvider>,
        verification_directory: &Path,
    ) -> Result<ObjectPublicationOutcome> {
        self.publish_multipart_inner(
            MultipartSource::Retained(opaque_key),
            provider,
            verification_directory,
            false,
        )
        .await
    }

    /// Explicitly abort and restart the exact retained upload without reconstructing
    /// plaintext or re-encrypting. Lost abort acknowledgements preserve the old pin.
    #[cfg(unix)]
    pub async fn restart_retained_object_multipart(
        &self,
        opaque_key: &str,
        provider: Arc<dyn crate::e2ee::MultipartJournalProvider>,
        verification_directory: &Path,
    ) -> Result<ObjectPublicationOutcome> {
        self.publish_multipart_inner(
            MultipartSource::Retained(opaque_key),
            provider,
            verification_directory,
            true,
        )
        .await
    }

    #[cfg(unix)]
    async fn publish_multipart_inner(
        &self,
        source: MultipartSource<'_>,
        provider: Arc<dyn crate::e2ee::MultipartJournalProvider>,
        verification_directory: &Path,
        restart: bool,
    ) -> Result<ObjectPublicationOutcome> {
        use crate::e2ee::FileMultipartJournal;
        let admission = self.session.begin_operation()?;
        if let MultipartSource::Prepared(candidate) = source {
            candidate.validate(&admission)?;
        } else if let MultipartSource::Retained(key) = source {
            crate::e2ee::object::opaque_class(key)?;
        }
        drop(admission);
        let operation = self.operation().await?;
        let (candidate, key) = match source {
            MultipartSource::Prepared(candidate) => {
                candidate.validate(&operation)?;
                (
                    Some(candidate.retained_copy()),
                    candidate.remote_key().to_owned(),
                )
            }
            MultipartSource::Retained(key) => {
                crate::e2ee::object::opaque_class(key)?;
                (None, key.to_owned())
            }
        };
        let limits = self
            .verified
            .lock()
            .await
            .as_ref()
            .ok_or(EncryptionError::StaleObservation)?
            .limits;
        let part_bytes = limits
            .multipart_part_bytes
            .ok_or(EncryptedHttpError::UnsupportedProfile)?;
        if !limits.multipart_object
            || part_bytes == 0
            || part_bytes > 64 * 1024 * 1024
            || limits
                .max_request_bytes
                .is_some_and(|maximum| part_bytes > maximum)
        {
            return Err(EncryptedHttpError::UnsupportedProfile);
        }
        let endpoint = self.base.to_string();
        let (operation, lease, retained) = tokio::task::spawn_blocking(move || {
            operation.check_active()?;
            let directory = if candidate.is_some() {
                provider.directory(operation.binding(), &key)?
            } else {
                provider.existing_directory(operation.binding(), &key)?
            };
            let mut lease = FileMultipartJournal::open(directory, operation.binding())?
                .acquire(operation.binding())?;
            let retained = if let Some(candidate) = candidate {
                lease.retain(&operation, &endpoint, &candidate)?
            } else {
                lease.restore(&operation, &endpoint, &key)?
            };
            operation.check_active()?;
            Ok::<_, EncryptionError>((operation, lease, retained))
        })
        .await
        .map_err(|_| EncryptionError::CheckpointUnavailable(std::io::ErrorKind::Other))??;
        let identity = lease
            .record
            .as_ref()
            .ok_or(EncryptionError::InvalidEncoding)?
            .identity
            .clone();
        if identity.total.div_ceil(part_bytes) > 10_000 {
            return Err(EncryptionError::TooLarge.into());
        }
        let (operation, mut lease) = if restart && retained.is_some() {
            let previous = lease
                .record
                .as_ref()
                .ok_or(EncryptionError::InvalidEncoding)?;
            if let Some(server_id) = previous.session_id.clone() {
                let request = self.credentials.authorize_encrypted_request(
                    &self.remote_name,
                    self.client
                        .delete(self.object_url("multipart-abort", &identity.key)?)
                        .header("Graft-Protocol", "1")
                        .header("Content-Length", 0)
                        .header("x-graft-upload-id", &server_id),
                );
                let response = tokio::select! {
                    response = request.send() => response.map_err(transport_error),
                    _ = operation.cancelled() => return unknown(EncryptionError::Cancelled.into()),
                };
                let response = match response {
                    Ok(response) => response,
                    Err(error) => return unknown(error),
                };
                if let Err(error) = validate_protocol(&response) {
                    return unknown(error);
                }
                if response.status() != reqwest::StatusCode::NO_CONTENT
                    || response.content_length().is_some_and(|length| length != 0)
                {
                    return unknown(EncryptedHttpError::HttpStatus(response.status().as_u16()));
                }
                let mut lease = lease;
                let record = lease
                    .record
                    .as_mut()
                    .ok_or(EncryptionError::InvalidEncoding)?;
                record.aborted_session_id = Some(server_id);
                record.session_id = None;
                record.part_bytes = None;
                match persist(operation, lease).await {
                    Ok(value) => value,
                    Err(error) => return unknown(error.into()),
                }
            } else if previous.aborted_session_id.is_some() {
                (operation, lease)
            } else {
                return Err(EncryptionError::StaleObservation.into());
            }
        } else {
            (operation, lease)
        };
        let mut cause = None;
        let mut created = false;
        if let Some(retained) = &retained {
            let uuid = uuid_text(operation.binding().repository());
            let request = self.credentials.authorize_encrypted_request(
                &self.remote_name,
                self.client
                    .post(self.object_url("multipart-start", &identity.key)?)
                    .header("Graft-Protocol", "1")
                    .header("Content-Length", 0)
                    .header("x-graft-object-bytes", identity.total)
                    .header("x-graft-e2ee-uuid", &uuid)
                    .header("x-graft-e2ee-upload-id", hex(&identity.upload_id))
                    .header("x-graft-e2ee-ciphertext-sha256", hex(&identity.digest)),
            );
            let response = tokio::select! {
                response = request.send() => response.map_err(transport_error),
                _ = operation.cancelled() => return unknown(EncryptionError::Cancelled.into()),
            };
            let response = match response {
                Ok(response) => response,
                Err(error) => return unknown(error),
            };
            if let Err(error) = validate_protocol(&response) {
                return unknown(error);
            }
            if response.status() == reqwest::StatusCode::PRECONDITION_FAILED {
                // The immutable winner is authenticated below, including full content.
            } else if response.status() != reqwest::StatusCode::OK {
                return unknown(EncryptedHttpError::HttpStatus(response.status().as_u16()));
            } else {
                let bytes = match read_body(&operation, response, 1024 * 1024).await {
                    Ok(bytes) => bytes,
                    Err(error) => return unknown(error),
                };
                let mut start: MultipartStart = match serde_json::from_slice(&bytes) {
                    Ok(start) => start,
                    Err(_) => return unknown(EncryptedHttpError::ProtocolMismatch),
                };
                if let Err(error) = validate_start(&start, &uuid, &identity, part_bytes) {
                    return unknown(error);
                }
                let previous = lease
                    .record
                    .as_ref()
                    .ok_or(EncryptionError::InvalidEncoding)?;
                if previous
                    .session_id
                    .as_ref()
                    .is_some_and(|id| id != &start.upload_id)
                    || previous.part_bytes.is_some_and(|bytes| bytes != part_bytes)
                    || previous
                        .aborted_session_id
                        .as_ref()
                        .is_some_and(|id| id == &start.upload_id)
                {
                    return unknown(EncryptedHttpError::ProtocolMismatch);
                }
                if previous.aborted_session_id.is_some() {
                    start.uploaded_parts.clear();
                }
                lease
                    .record
                    .as_mut()
                    .ok_or(EncryptionError::InvalidEncoding)?
                    .session_id = Some(start.upload_id.clone());
                lease
                    .record
                    .as_mut()
                    .ok_or(EncryptionError::InvalidEncoding)?
                    .part_bytes = Some(part_bytes);
                let (operation, lease) = match persist(operation, lease).await {
                    Ok(value) => value,
                    Err(error) => return unknown(error.into()),
                };
                return self
                    .send_multipart_parts(operation, lease, retained, start, verification_directory)
                    .await;
            }
        }
        self.confirm_multipart(
            operation,
            lease,
            verification_directory,
            &mut cause,
            &mut created,
        )
        .await
    }

    #[cfg(unix)]
    async fn send_multipart_parts(
        &self,
        operation: SessionOperation,
        lease: crate::e2ee::multipart_journal::Lease,
        retained: &PreparedEncryptedObject,
        start: MultipartStart,
        verification_directory: &Path,
    ) -> Result<ObjectPublicationOutcome> {
        let identity = lease
            .record
            .as_ref()
            .ok_or(EncryptionError::InvalidEncoding)?
            .identity
            .clone();
        let completed: HashSet<_> = start
            .uploaded_parts
            .iter()
            .map(|part| part.part_number)
            .collect();
        for index in 0..identity.total.div_ceil(start.part_bytes) {
            if let Err(error) = operation.check_active() {
                return unknown(error.into());
            }
            if completed.contains(&(index + 1)) {
                continue;
            }
            let begin = index * start.part_bytes;
            let end = (begin + start.part_bytes).min(identity.total);
            let (stream, _cancel) = retained.ciphertext_range_stream(begin, end);
            let request = self.credentials.authorize_encrypted_request(
                &self.remote_name,
                self.client
                    .put(self.object_url("multipart-part", &identity.key)?)
                    .header("Graft-Protocol", "1")
                    .header("Content-Type", "application/octet-stream")
                    .header("Content-Length", end - begin)
                    .header("x-graft-upload-id", &start.upload_id)
                    .header("x-graft-part-number", index + 1)
                    .body(reqwest::Body::wrap_stream(stream)),
            );
            let response = tokio::select! {
                response = request.send() => response.map_err(transport_error),
                _ = operation.cancelled() => return unknown(EncryptionError::Cancelled.into()),
            };
            let response = match response {
                Ok(response) => response,
                Err(error) => return unknown(error),
            };
            if let Err(error) = validate_protocol(&response) {
                return unknown(error);
            }
            if response.status() != reqwest::StatusCode::NO_CONTENT
                || response.content_length().is_some_and(|length| length != 0)
            {
                return unknown(EncryptedHttpError::HttpStatus(response.status().as_u16()));
            }
        }
        let request = self.credentials.authorize_encrypted_request(
            &self.remote_name,
            self.client
                .post(self.object_url("multipart-complete", &identity.key)?)
                .header("Graft-Protocol", "1")
                .header("Content-Length", 0)
                .header("x-graft-upload-id", &start.upload_id),
        );
        let response = tokio::select! {
            response = request.send() => response.map_err(transport_error),
            _ = operation.cancelled() => return unknown(EncryptionError::Cancelled.into()),
        };
        let mut created = false;
        let mut cause = match response {
            Ok(response) => {
                if let Err(error) = validate_protocol(&response) {
                    return unknown(error);
                }
                created = response.status() == reqwest::StatusCode::NO_CONTENT;
                if created && response.content_length().is_some_and(|length| length != 0) {
                    return unknown(EncryptedHttpError::ProtocolMismatch);
                }
                (!created).then(|| EncryptedHttpError::HttpStatus(response.status().as_u16()))
            }
            Err(error) => Some(error),
        };
        self.confirm_multipart(
            operation,
            lease,
            verification_directory,
            &mut cause,
            &mut created,
        )
        .await
    }

    #[cfg(unix)]
    async fn confirm_multipart(
        &self,
        operation: SessionOperation,
        mut lease: crate::e2ee::multipart_journal::Lease,
        directory: &Path,
        cause: &mut Option<EncryptedHttpError>,
        created: &mut bool,
    ) -> Result<ObjectPublicationOutcome> {
        let identity = &lease
            .record
            .as_ref()
            .ok_or(EncryptionError::InvalidEncoding)?
            .identity;
        let winner = match self
            .download_object(&identity.logical, directory, identity.plaintext_length)
            .await
        {
            Ok(Some(winner)) => winner,
            Ok(None) => {
                return Ok(ObjectPublicationOutcome::OutcomeUnknown { error: cause.take() });
            }
            Err(error) => return unknown(error),
        };
        if winner.metadata().logical_key != identity.logical
            || winner.metadata().plaintext_length != identity.plaintext_length
            || winner.metadata().plaintext_sha256 != identity.plaintext_digest
        {
            return unknown(EncryptionError::ImmutableConflict.into());
        }
        drop(winner);
        let persisted = tokio::task::spawn_blocking(move || {
            operation.check_active()?;
            lease.confirm()?;
            operation.check_active()
        })
        .await;
        match persisted {
            Ok(Ok(())) if *created => Ok(ObjectPublicationOutcome::Created),
            Ok(Ok(())) => Ok(ObjectPublicationOutcome::Reused),
            Ok(Err(error)) => unknown(error.into()),
            Err(_) => {
                unknown(EncryptionError::CheckpointUnavailable(std::io::ErrorKind::Other).into())
            }
        }
    }
}

#[cfg(unix)]
#[derive(Deserialize)]
struct MultipartStart {
    upload_id: String,
    total_bytes: u64,
    part_bytes: u64,
    #[serde(default)]
    uploaded_parts: Vec<MultipartPart>,
    encryption: MultipartIdentity,
}
#[cfg(unix)]
#[derive(Deserialize)]
struct MultipartPart {
    part_number: u64,
    bytes: u64,
}
#[cfg(unix)]
#[derive(Deserialize)]
struct MultipartIdentity {
    uuid: String,
    upload_id: String,
    ciphertext_sha256: String,
}

#[cfg(unix)]
fn validate_start(
    start: &MultipartStart,
    uuid: &str,
    identity: &crate::e2ee::multipart_journal::Identity,
    part_bytes: u64,
) -> Result<()> {
    if start.upload_id.is_empty()
        || start.upload_id.len() > 1024
        || !start
            .upload_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
        || start.total_bytes != identity.total
        || start.part_bytes != part_bytes
        || start.encryption.uuid != uuid
        || start.encryption.upload_id != hex(&identity.upload_id)
        || start.encryption.ciphertext_sha256 != hex(&identity.digest)
    {
        return Err(EncryptedHttpError::ProtocolMismatch);
    }
    let count = identity.total.div_ceil(part_bytes);
    let mut previous = 0;
    for part in &start.uploaded_parts {
        if part.part_number <= previous
            || part.part_number > count
            || part.bytes != (identity.total - (part.part_number - 1) * part_bytes).min(part_bytes)
        {
            return Err(EncryptedHttpError::ProtocolMismatch);
        }
        previous = part.part_number;
    }
    Ok(())
}

#[cfg(unix)]
async fn persist(
    operation: SessionOperation,
    lease: crate::e2ee::multipart_journal::Lease,
) -> crate::e2ee::Result<(SessionOperation, crate::e2ee::multipart_journal::Lease)> {
    tokio::task::spawn_blocking(move || {
        operation.check_active()?;
        lease.persist()?;
        operation.check_active()?;
        Ok((operation, lease))
    })
    .await
    .map_err(|_| EncryptionError::CheckpointUnavailable(std::io::ErrorKind::Other))?
}

#[cfg(unix)]
fn unknown(error: EncryptedHttpError) -> Result<ObjectPublicationOutcome> {
    Ok(ObjectPublicationOutcome::OutcomeUnknown { error: Some(error) })
}
#[cfg(unix)]
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
#[cfg(unix)]
fn uuid_text(repository: crate::e2ee::RepositoryId) -> String {
    let hex = hex(repository.as_bytes());
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}
