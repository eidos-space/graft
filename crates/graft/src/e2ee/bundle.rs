use super::download::{CiphertextBody, finish_staging, private_staging_file, stream_object_chunks};
use super::{
    DownloadedEncryptedObject, EncryptedHttpError, EncryptedObjectMetadata, EncryptionError,
    ObjectReader, RoutingHint, SessionOperation, codec, hint, object,
};
use std::path::Path;

type Result<T> = std::result::Result<T, EncryptedHttpError>;
pub(super) const MAX_BUNDLE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_MANIFEST_BYTES: usize = 256 * 1024;
const MAX_FRAMES: usize = 256;
pub(super) const MAX_FETCH_BYTES: u64 = 48 * 1024 * 1024;
pub(super) const MAX_FETCH_FRAMES: usize = 384;

pub(crate) struct EncryptedFetchBundle {
    pub reference: super::RefObservation,
    pub entries: Vec<EncryptedBundleEntry>,
}

/// An authenticated envelope/full plaintext digest, or an authenticated public hint.
/// Repository identity and actual hint topology still require semantic verification.
pub enum EncryptedBundleEntry {
    Object {
        remote_key: String,
        value: DownloadedEncryptedObject,
    },
    Hint(RoutingHint),
}

impl EncryptedBundleEntry {
    pub fn remote_key(&self) -> &str {
        match self {
            Self::Object { remote_key, .. } => remote_key,
            Self::Hint(hint) => hint.remote_key(),
        }
    }
}

struct Frame {
    key: String,
    length: u64,
}

pub(super) fn prepare_keys(keys: &[String]) -> Result<Vec<String>> {
    if keys.is_empty() || keys.len() > MAX_FRAMES {
        return Err(EncryptionError::TooLarge.into());
    }
    let mut sorted = keys.to_vec();
    sorted.sort();
    for (index, key) in sorted.iter().enumerate() {
        if index > 0 && sorted[index - 1] == *key {
            return Err(EncryptionError::InvalidEncoding.into());
        }
        validate_key(key)?;
    }
    Ok(sorted)
}

fn validate_key(key: &str) -> Result<()> {
    if key.starts_with("e2ee/hints/") {
        hint::validate_remote_key(key)?;
    } else {
        object::opaque_class(key)?;
    }
    Ok(())
}

fn parse_manifest(bytes: &[u8], keys: &[String], total: u64) -> Result<Vec<Frame>> {
    let fields = codec::decode(bytes, 4, MAX_MANIFEST_BYTES)?;
    if fields[0] != 1_u16.to_be_bytes() || !fields[1].is_empty() || !fields[2].is_empty() {
        return Err(EncryptedHttpError::ProtocolMismatch);
    }
    let descriptors = codec::decode_bounded(fields[3], MAX_FRAMES, MAX_MANIFEST_BYTES)?;
    if descriptors.len() != keys.len() {
        return Err(EncryptedHttpError::ProtocolMismatch);
    }
    let mut expected = 12 + bytes.len() as u64;
    let mut frames = Vec::with_capacity(descriptors.len());
    for (descriptor, requested) in descriptors.iter().zip(keys) {
        let fields = codec::decode(descriptor, 2, MAX_MANIFEST_BYTES)?;
        if fields[0] != requested.as_bytes() {
            return Err(EncryptedHttpError::ProtocolMismatch);
        }
        let length = u64::from_be_bytes(
            fields[1]
                .try_into()
                .map_err(|_| EncryptionError::InvalidEncoding)?,
        );
        expected = expected
            .checked_add(12 + fields[0].len() as u64)
            .and_then(|n| n.checked_add(length))
            .filter(|n| *n <= MAX_BUNDLE_BYTES)
            .ok_or(EncryptionError::TooLarge)?;
        frames.push(Frame { key: requested.clone(), length });
    }
    if expected != total {
        return Err(EncryptedHttpError::ProtocolMismatch);
    }
    Ok(frames)
}

pub(super) async fn read(
    operation: &SessionOperation,
    response: reqwest::Response,
    keys: &[String],
    total: u64,
    directory: &Path,
    maximum_plaintext: u64,
) -> Result<Vec<EncryptedBundleEntry>> {
    let mut body = CiphertextBody::new(response, total);
    let prefix = body.read_exact(operation, 12).await?;
    if &prefix[..8] != b"GRAFTEB1" {
        return Err(EncryptedHttpError::ProtocolMismatch);
    }
    let length = u32::from_be_bytes(
        prefix[8..12]
            .try_into()
            .map_err(|_| EncryptionError::InvalidEncoding)?,
    ) as usize;
    if length > MAX_MANIFEST_BYTES {
        return Err(EncryptionError::TooLarge.into());
    }
    let manifest = body.read_exact(operation, length).await?;
    let frames = parse_manifest(&manifest, keys, total)?;
    read_frames(operation, body, frames, directory, maximum_plaintext).await
}

pub(super) async fn read_fetch(
    operation: &SessionOperation,
    response: reqwest::Response,
    logical_ref: &str,
    total: u64,
    count: usize,
    directory: &Path,
    maximum_plaintext: u64,
) -> Result<EncryptedFetchBundle> {
    let remote_ref = super::reference::remote_key(operation.root()?, logical_ref)?;
    let mut body = CiphertextBody::new(response, total);
    let prefix = body.read_exact(operation, 12).await?;
    if &prefix[..8] != b"GRAFTEB1" {
        return Err(EncryptedHttpError::ProtocolMismatch);
    }
    let length = u32::from_be_bytes(
        prefix[8..12]
            .try_into()
            .map_err(|_| EncryptionError::InvalidEncoding)?,
    ) as usize;
    if length > MAX_MANIFEST_BYTES {
        return Err(EncryptionError::TooLarge.into());
    }
    let manifest = body.read_exact(operation, length).await?;
    let fields = codec::decode(&manifest, 4, MAX_MANIFEST_BYTES)?;
    if fields[0] != 1_u16.to_be_bytes()
        || fields[1] != remote_ref.as_bytes()
        || fields[2].is_empty()
        || fields[2].len() > 16 * 1024
    {
        return Err(EncryptedHttpError::ProtocolMismatch);
    }
    let reference = operation.observe_ref(logical_ref, Some(fields[2]))?;
    if !matches!(
        reference.target(),
        Some(super::RefTarget::Commit(_) | super::RefTarget::AnnotatedTag { .. })
    ) {
        return Err(EncryptedHttpError::ProtocolMismatch);
    }
    let descriptors = codec::decode_bounded(fields[3], MAX_FETCH_FRAMES, MAX_MANIFEST_BYTES)?;
    if descriptors.len() != count {
        return Err(EncryptedHttpError::ProtocolMismatch);
    }
    let mut expected = 12 + length as u64;
    let mut frames: Vec<Frame> = Vec::with_capacity(count);
    for descriptor in descriptors {
        let fields = codec::decode(descriptor, 2, MAX_MANIFEST_BYTES)?;
        let key = std::str::from_utf8(fields[0]).map_err(|_| EncryptionError::InvalidEncoding)?;
        validate_key(key)?;
        if !["e2ee/packs/", "e2ee/indexes/", "e2ee/hints/"]
            .iter()
            .any(|prefix| key.starts_with(prefix))
            || frames.last().is_some_and(|last| last.key.as_str() >= key)
        {
            return Err(EncryptedHttpError::ProtocolMismatch);
        }
        let length = u64::from_be_bytes(
            fields[1]
                .try_into()
                .map_err(|_| EncryptionError::InvalidEncoding)?,
        );
        expected = expected
            .checked_add(12 + key.len() as u64)
            .and_then(|total| total.checked_add(length))
            .filter(|total| *total <= MAX_FETCH_BYTES)
            .ok_or(EncryptionError::TooLarge)?;
        frames.push(Frame { key: key.into(), length });
    }
    if expected != total {
        return Err(EncryptedHttpError::ProtocolMismatch);
    }
    let entries = read_frames(operation, body, frames, directory, maximum_plaintext).await?;
    Ok(EncryptedFetchBundle { reference, entries })
}

async fn read_frames(
    operation: &SessionOperation,
    mut body: CiphertextBody,
    frames: Vec<Frame>,
    directory: &Path,
    maximum_plaintext: u64,
) -> Result<Vec<EncryptedBundleEntry>> {
    let mut remaining = maximum_plaintext;
    let mut entries = Vec::with_capacity(frames.len());
    for frame in frames {
        validate_frame_header(operation, &mut body, &frame).await?;
        let entry = if frame.key.starts_with("e2ee/hints/") {
            read_hint(operation, &mut body, &frame).await?
        } else {
            read_object(operation, &mut body, &frame, directory, &mut remaining).await?
        };
        entries.push(entry);
    }
    body.finish(operation).await?;
    operation.check_active()?;
    Ok(entries)
}

async fn validate_frame_header(
    operation: &SessionOperation,
    body: &mut CiphertextBody,
    frame: &Frame,
) -> Result<()> {
    let header = body.read_exact(operation, 12).await?;
    let length = u32::from_be_bytes(
        header[..4]
            .try_into()
            .map_err(|_| EncryptionError::InvalidEncoding)?,
    ) as usize;
    let object_length = u64::from_be_bytes(
        header[4..]
            .try_into()
            .map_err(|_| EncryptionError::InvalidEncoding)?,
    );
    if length != frame.key.len() || object_length != frame.length {
        return Err(EncryptedHttpError::ProtocolMismatch);
    }
    if body.read_exact(operation, length).await? != frame.key.as_bytes() {
        return Err(EncryptedHttpError::ProtocolMismatch);
    }
    Ok(())
}

async fn read_hint(
    operation: &SessionOperation,
    body: &mut CiphertextBody,
    frame: &Frame,
) -> Result<EncryptedBundleEntry> {
    if frame.length > hint::MAX_LENGTH as u64 {
        return Err(EncryptionError::TooLarge.into());
    }
    let mut bytes = Vec::with_capacity(frame.length as usize);
    while bytes.len() < frame.length as usize {
        let length = (frame.length as usize - bytes.len()).min(MAX_MANIFEST_BYTES);
        bytes.extend(body.read_exact(operation, length).await?);
    }
    let hint = RoutingHint::discover(operation.root()?, &frame.key, &bytes)?;
    operation.check_active()?;
    Ok(EncryptedBundleEntry::Hint(hint))
}

async fn read_object(
    operation: &SessionOperation,
    body: &mut CiphertextBody,
    frame: &Frame,
    directory: &Path,
    remaining: &mut u64,
) -> Result<EncryptedBundleEntry> {
    if frame.length < object::PREFIX_LENGTH as u64 + 32 {
        return Err(EncryptionError::InvalidEncoding.into());
    }
    let mut header = body.read_exact(operation, object::PREFIX_LENGTH).await?;
    let length = ObjectReader::header_length(&header)?;
    if frame.length < length as u64 + 16 {
        return Err(EncryptionError::InvalidEncoding.into());
    }
    header.extend(
        body.read_exact(operation, length - object::PREFIX_LENGTH)
            .await?,
    );
    let reader = ObjectReader::discover(operation.root()?, &frame.key, &header, frame.length)?;
    *remaining = remaining
        .checked_sub(reader.plaintext_length())
        .ok_or(EncryptionError::TooLarge)?;
    let file = private_staging_file(directory)?;
    let mut file = tokio::fs::File::from_std(file);
    stream_object_chunks(operation, &reader, body, &mut file).await?;
    let file = finish_staging(operation, file).await?;
    let value = DownloadedEncryptedObject {
        file,
        metadata: EncryptedObjectMetadata {
            logical_key: reader.logical_key().to_owned(),
            plaintext_length: reader.plaintext_length(),
            plaintext_sha256: *reader.plaintext_digest(),
        },
    };
    operation.check_active()?;
    Ok(EncryptedBundleEntry::Object { remote_key: frame.key.clone(), value })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_parser_matches_independent_python_framing_vector() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../tests/fixtures/e2ee/read-bundle.json"
        ))
        .unwrap();
        let hex = fixture["manifest_hex"].as_str().unwrap();
        let manifest: Vec<u8> = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect();
        let keys = vec![fixture["key"].as_str().unwrap().to_owned()];
        let length = fixture["wire_hex"].as_str().unwrap().len() as u64 / 2;
        let frames = parse_manifest(&manifest, &keys, length).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].key, keys[0]);
        assert_eq!(frames[0].length, 104);
        assert!(parse_manifest(&manifest, &keys, length + 1).is_err());
        assert!(parse_manifest(&manifest, &[], length).is_err());
        let mut changed = keys;
        let end = changed[0].len();
        changed[0].replace_range(end - 1.., "b");
        assert!(parse_manifest(&manifest, &changed, length).is_err());
    }

    #[test]
    fn read_keys_reject_mutable_plaintext_unknown_duplicate_and_oversized_requests() {
        let valid = format!("e2ee/packs/{}", "a".repeat(64));
        let hint = format!("e2ee/hints/{}", "b".repeat(64));
        assert_eq!(
            prepare_keys(&[valid.clone(), hint.clone()]).unwrap(),
            vec![hint, valid.clone()]
        );
        for key in [
            "e2ee/config".to_owned(),
            format!("e2ee/refs/{}", "a".repeat(64)),
            "objects/private".to_owned(),
            format!("{valid}/extra"),
            valid.to_uppercase(),
        ] {
            assert!(prepare_keys(&[key]).is_err());
        }
        assert!(prepare_keys(&[]).is_err());
        assert!(prepare_keys(&[valid.clone(), valid.clone()]).is_err());
        assert!(prepare_keys(&vec![valid; 257]).is_err());
    }
}
