use super::*;

impl EncryptedHttpClient {
    /// A missing optional optimization permits encrypted primitive discovery.
    /// Integrity or framing errors are never converted into an absent bundle.
    pub(crate) async fn fetch_bundle(
        &self,
        logical_ref: &str,
        have: Option<[u8; 32]>,
        directory: &Path,
        maximum_plaintext: u64,
    ) -> Result<Option<bundle::EncryptedFetchBundle>> {
        let operation = self.operation().await?;
        let enabled = {
            let verified = tokio::select! {
                guard = self.verified.lock() => guard,
                _ = operation.cancelled() => return Err(EncryptionError::Cancelled.into()),
            };
            verified
                .as_ref()
                .is_some_and(|profile| profile.limits.fetch_bundle)
        };
        if !enabled {
            return Ok(None);
        }
        let remote_ref = reference::remote_key(operation.root()?, logical_ref)?;
        if !remote_ref.starts_with("e2ee/refs/") {
            return Err(EncryptionError::InvalidObjectKey.into());
        }
        let have = have
            .map(|id| operation.root()?.route_commit(&id))
            .transpose()?
            .map(|token| {
                token
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>()
            });
        let body = serde_json::to_vec(&serde_json::json!({"version":1,"have":have}))
            .map_err(|_| EncryptedHttpError::ProtocolMismatch)?;
        self.ensure_request_budget(&operation, body.len() as u64)
            .await?;
        let mut url = self.base.clone();
        {
            let mut path = url
                .path_segments_mut()
                .map_err(|_| EncryptedHttpError::InvalidEndpoint)?;
            path.pop_if_empty().push("e2ee").push("fetch-bundle");
            for segment in remote_ref.split('/') {
                path.push(segment);
            }
        }
        let request = self.credentials.authorize_encrypted_request(
            &self.remote_name,
            self.client
                .post(url)
                .header("Graft-Protocol", "1")
                .header("Content-Type", "application/json")
                .header("Accept-Encoding", "identity")
                .header("Content-Length", body.len())
                .body(body),
        );
        let response = tokio::select! {
            result = request.send() => result.map_err(transport_error)?,
            _ = operation.cancelled() => return Err(EncryptionError::Cancelled.into()),
        };
        operation.check_active()?;
        validate_protocol(&response)?;
        if matches!(response.status().as_u16(), 404 | 405 | 409 | 413 | 422) {
            return Ok(None);
        }
        if response.status() != reqwest::StatusCode::OK {
            return Err(EncryptedHttpError::HttpStatus(response.status().as_u16()));
        }
        let count = single_header(&response, "x-graft-bundle-objects")?;
        if count.is_empty() || !count.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(EncryptedHttpError::ProtocolMismatch);
        }
        let count = count
            .parse::<usize>()
            .map_err(|_| EncryptedHttpError::ProtocolMismatch)?;
        if count > bundle::MAX_FETCH_FRAMES {
            return Err(EncryptionError::TooLarge.into());
        }
        let total = validate_bundle_headers(&response, count)?;
        if total > bundle::MAX_FETCH_BYTES {
            return Err(EncryptionError::TooLarge.into());
        }
        bundle::read_fetch(
            &operation,
            response,
            logical_ref,
            total,
            count,
            directory,
            maximum_plaintext,
        )
        .await
        .map(Some)
    }
}
