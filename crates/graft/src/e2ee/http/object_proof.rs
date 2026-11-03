use super::*;
use futures::TryStreamExt;

#[test]
fn cross_language_integrity_challenge_vector() {
    let repository = crate::e2ee::RepositoryId::from_bytes([
        0, 1, 2, 3, 4, 5, 0x46, 7, 0x88, 9, 10, 11, 12, 13, 14, 15,
    ])
    .unwrap();
    let key = format!("e2ee/files/{}", "a".repeat(64));
    let challenge = std::array::from_fn(|index| index as u8);
    let prefix = prefix(repository, &key, 5, &challenge);
    let mut hash = Sha256::new();
    hash.update(prefix);
    hash.update([0, 1, 2, 254, 255]);
    assert_eq!(
        format!("{:x}", hash.finalize()),
        "4f904ed57dc3ecc8393ec4494eaa74bc6682a63a067043e3a689490d45e0532a"
    );
}

pub(super) fn prefix(
    repository: super::super::RepositoryId,
    key: &str,
    size: u64,
    challenge: &[u8; 32],
) -> Vec<u8> {
    let mut bytes = b"GRAFT-E2EE-OBJECT-PROOF-V1\0".to_vec();
    bytes.extend_from_slice(repository.as_bytes());
    bytes.extend_from_slice(&(key.len() as u32).to_be_bytes());
    bytes.extend_from_slice(key.as_bytes());
    bytes.extend_from_slice(&size.to_be_bytes());
    bytes.extend_from_slice(challenge);
    bytes
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Proof {
    version: u8,
    uuid: String,
    key: String,
    ciphertext_bytes: u64,
    challenge: String,
    sha256: String,
}

impl EncryptedHttpClient {
    pub(super) async fn confirm_object_proof(
        &self,
        operation: &SessionOperation,
        retained: &PreparedEncryptedObject,
    ) -> Result<()> {
        let challenge = super::super::random::<32>()?;
        let key = retained.remote_key();
        let size = retained.ciphertext_length();
        let mut hash = Sha256::new();
        hash.update(prefix(
            operation.binding().repository(),
            key,
            size,
            &challenge,
        ));
        let (stream, _cancel) = retained.ciphertext_range_stream(0, size);
        futures::pin_mut!(stream);
        while let Some(bytes) = stream
            .try_next()
            .await
            .map_err(|error| EncryptionError::CheckpointUnavailable(error.kind()))?
        {
            operation.check_active()?;
            hash.update(bytes);
        }
        let expected = format!("{:x}", hash.finalize());
        operation.check_active()?;
        let request = self.credentials.authorize_encrypted_request(
            &self.remote_name,
            self.client
                .post(self.object_url("e2ee", &format!("object-proof/{key}"))?)
                .header("Graft-Protocol", "1")
                .header("Content-Type", "application/octet-stream")
                .header("Content-Length", 32)
                .header("X-Graft-Object-Bytes", size)
                .body(challenge.to_vec()),
        );
        let response = tokio::select! {
            response = request.send() => response.map_err(transport_error)?,
            _ = operation.cancelled() => return Err(EncryptionError::Cancelled.into()),
        };
        validate_protocol(&response)?;
        if response.status() != reqwest::StatusCode::OK {
            return Err(EncryptedHttpError::HttpStatus(response.status().as_u16()));
        }
        let bytes = read_body(operation, response, 4096).await?;
        let proof: Proof =
            serde_json::from_slice(&bytes).map_err(|_| EncryptedHttpError::ProtocolMismatch)?;
        if proof.version != 1
            || proof.uuid != super::multipart::uuid_text(operation.binding().repository())
            || proof.key != key
            || proof.ciphertext_bytes != size
            || proof.challenge != super::multipart::hex(&challenge)
            || proof.sha256 != expected
        {
            return Err(EncryptionError::ImmutableConflict.into());
        }
        operation.check_active()?;
        Ok(())
    }
}
