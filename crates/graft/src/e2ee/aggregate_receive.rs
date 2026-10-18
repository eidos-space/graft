use super::*;
use futures::{Stream, StreamExt};
use std::pin::Pin;

pub(crate) struct PreparedReceiveBundle {
    pub objects: Vec<PreparedEncryptedObject>,
    pub hints: Vec<super::super::RoutingHint>,
}

pub(crate) enum ReceiveBundleOutcome {
    Publication(RefPublicationOutcome),
    /// No ref mutation was admitted, or an immutable collision left the exact
    /// old ref intact. Reuse authenticated encrypted primitives and this candidate.
    Fallback,
}

impl EncryptedHttpClient {
    pub(crate) async fn receive_bundle(
        &self,
        candidate: &PreparedRefMutation,
        bundle: &PreparedReceiveBundle,
        journal: Option<Arc<dyn super::super::RefMutationJournal>>,
    ) -> Result<ReceiveBundleOutcome> {
        let operation = self.operation().await?;
        candidate.validate(&operation)?;
        let limits = {
            let verified = tokio::select! {
                guard = self.verified.lock() => guard,
                _ = operation.cancelled() => return Err(EncryptionError::Cancelled.into()),
            };
            verified
                .as_ref()
                .ok_or(EncryptedHttpError::UnsupportedProfile)?
                .limits
        };
        if !limits.receive_bundle
            || !candidate.remote_key().starts_with("e2ee/refs/")
            || candidate.is_delete()
        {
            return Ok(ReceiveBundleOutcome::Fallback);
        }
        let count = bundle
            .objects
            .len()
            .checked_add(bundle.hints.len())
            .ok_or(EncryptionError::TooLarge)?;
        if count > 256 {
            return Ok(ReceiveBundleOutcome::Fallback);
        }
        let mut entries = std::collections::BTreeMap::new();
        for object in &bundle.objects {
            object.validate(&operation)?;
            if entries
                .insert(object.remote_key(), Entry::Object(object))
                .is_some()
            {
                return Err(EncryptionError::InvalidEncoding.into());
            }
        }
        for hint in &bundle.hints {
            super::super::RoutingHint::open(
                operation.root()?,
                hint.pack_key(),
                hint.index_key(),
                hint.as_bytes(),
            )?;
            if entries
                .insert(hint.remote_key(), Entry::Hint(hint))
                .is_some()
            {
                return Err(EncryptionError::InvalidEncoding.into());
            }
        }
        let descriptors = entries
            .iter()
            .map(|(key, entry)| {
                super::super::codec::encode(&[key.as_bytes(), &entry.length().to_be_bytes()])
            })
            .collect::<super::super::Result<Vec<_>>>()?;
        let descriptors = super::super::codec::encode(
            &descriptors.iter().map(Vec::as_slice).collect::<Vec<_>>(),
        )?;
        let manifest = super::super::codec::encode(&[
            &1_u16.to_be_bytes(),
            candidate.remote_key().as_bytes(),
            candidate
                .replacement_bytes()
                .ok_or(EncryptionError::InvalidEncoding)?,
            &descriptors,
        ])?;
        if manifest.len() > 256 * 1024 {
            return Ok(ReceiveBundleOutcome::Fallback);
        }
        let total =
            entries
                .iter()
                .try_fold(12 + manifest.len() as u64, |total, (key, entry)| {
                    total
                        .checked_add(12 + key.len() as u64)
                        .and_then(|total| total.checked_add(entry.length()))
                        .ok_or(EncryptionError::TooLarge)
                })?;
        if total > bundle::MAX_BUNDLE_BYTES
            || limits
                .max_request_bytes
                .is_some_and(|maximum| total > maximum)
        {
            return Ok(ReceiveBundleOutcome::Fallback);
        }
        // Bind exact retained ref bytes before the aggregate can dispatch CAS.
        {
            let journal = journal.ok_or(EncryptionError::CheckpointUnavailable(
                std::io::ErrorKind::NotFound,
            ))?;
            let journal_operation = self.session.begin_operation()?;
            let record = candidate.record(&journal_operation)?;
            let session = self.session.clone();
            tokio::task::spawn_blocking(move || {
                journal_operation.check_active()?;
                journal.bind(session.binding(), &record)?;
                journal_operation.check_active()
            })
            .await
            .map_err(|_| EncryptionError::CheckpointUnavailable(std::io::ErrorKind::Other))??;
        }
        let mut prefix = b"GRAFTEB1".to_vec();
        prefix.extend_from_slice(&(manifest.len() as u32).to_be_bytes());
        prefix.extend_from_slice(&manifest);
        type CiphertextStream = Pin<Box<dyn Stream<Item = std::io::Result<Vec<u8>>> + Send>>;
        let mut streams: Vec<CiphertextStream> =
            vec![Box::pin(futures::stream::once(async { Ok(prefix) }))];
        let mut cancellation = Vec::new();
        for (key, entry) in entries {
            let mut header = Vec::new();
            header.extend_from_slice(&(key.len() as u32).to_be_bytes());
            header.extend_from_slice(&entry.length().to_be_bytes());
            header.extend_from_slice(key.as_bytes());
            streams.push(Box::pin(futures::stream::once(async { Ok(header) })));
            match entry {
                Entry::Object(object) => {
                    let (stream, guard) = object.ciphertext_stream();
                    cancellation.push(guard);
                    streams.push(Box::pin(stream));
                }
                Entry::Hint(hint) => {
                    let bytes = hint.as_bytes().to_vec();
                    streams.push(Box::pin(futures::stream::once(async { Ok(bytes) })));
                }
            }
        }
        let body = reqwest::Body::wrap_stream(futures::stream::iter(streams).flatten());
        let mut url = self.base.clone();
        {
            let mut path = url
                .path_segments_mut()
                .map_err(|_| EncryptedHttpError::InvalidEndpoint)?;
            path.pop_if_empty().push("e2ee").push("receive-bundle");
            for segment in candidate.remote_key().split('/') {
                path.push(segment);
            }
        }
        let expected = candidate.expected_bytes();
        let hex = expected
            .unwrap_or_default()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let request = self.credentials.authorize_encrypted_request(
            &self.remote_name,
            self.client
                .post(url)
                .header("Graft-Protocol", "1")
                .header("Content-Type", "application/vnd.graft.e2ee-bundle")
                .header(
                    "x-graft-expected-present",
                    if expected.is_some() { "true" } else { "false" },
                )
                .header("x-graft-expected-hex", hex)
                .header("Content-Length", total)
                .body(body),
        );
        operation.check_active()?;
        let response = tokio::select! {
            result = request.send() => result.map_err(transport_error),
            _ = operation.cancelled() => return Ok(ReceiveBundleOutcome::Publication(EncryptionError::Cancelled.into())),
        };
        drop(cancellation);
        let (mismatch, collision, cause) = match response {
            Ok(response) => {
                if let Err(error) = validate_protocol(&response) {
                    return Ok(ReceiveBundleOutcome::Publication(
                        RefPublicationOutcome::OutcomeUnknown { error: Some(error) },
                    ));
                }
                let status = response.status().as_u16();
                if status == 204 {
                    if response.content_length().is_some_and(|length| length != 0) {
                        return Ok(ReceiveBundleOutcome::Publication(
                            RefPublicationOutcome::OutcomeUnknown {
                                error: Some(EncryptedHttpError::ProtocolMismatch),
                            },
                        ));
                    }
                    return Ok(ReceiveBundleOutcome::Publication(
                        RefPublicationOutcome::Confirmed,
                    ));
                }
                if matches!(status, 404 | 405 | 413 | 422) {
                    return Ok(ReceiveBundleOutcome::Fallback);
                }
                (
                    status == 409,
                    status == 412,
                    Some(EncryptedHttpError::HttpStatus(status)),
                )
            }
            Err(error) => (false, false, Some(error)),
        };
        let outcome = self
            .reconcile_ref_with(&operation, candidate, mismatch, cause)
            .await;
        if collision && matches!(outcome, RefPublicationOutcome::Unconfirmed) {
            Ok(ReceiveBundleOutcome::Fallback)
        } else {
            Ok(ReceiveBundleOutcome::Publication(outcome))
        }
    }
}

enum Entry<'a> {
    Object(&'a PreparedEncryptedObject),
    Hint(&'a super::super::RoutingHint),
}
impl Entry<'_> {
    fn length(&self) -> u64 {
        match self {
            Self::Object(object) => object.ciphertext_length(),
            Self::Hint(hint) => hint.as_bytes().len() as u64,
        }
    }
}
