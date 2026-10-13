use super::{EncryptionError, PROFILE, Result, RootKey, SUITE, codec, mac, random};
use chacha20poly1305::{
    XChaCha20Poly1305, XNonce,
    aead::{Aead, KeyInit, Payload},
};
use zeroize::Zeroizing;

const PREFIX_LENGTH: usize = 69;
const MAX_LENGTH: usize = 16 * 1024;

/// Authoritative target hidden inside the authenticated ref envelope.
#[derive(Clone, PartialEq, Eq)]
pub enum RefTarget {
    Commit([u8; 32]),
    /// Exact annotated-tag object and its declared commit dependency.
    /// Repository consumers must verify the tag object before trusting that edge.
    AnnotatedTag {
        object: [u8; 32],
        commit: [u8; 32],
    },
    /// A normalized full branch ref, for example `refs/heads/main`.
    Symbolic(String),
}

impl RefTarget {
    /// Verify both authenticated tag identity and declared commit against content.
    pub fn verify_annotated_tag(&self, bytes: &[u8]) -> Result<()> {
        if Self::from_annotated_tag(bytes)? != *self {
            return Err(EncryptionError::AuthenticationFailed);
        }
        Ok(())
    }

    /// Derive the authoritative tag ID and commit edge from actual encoded bytes.
    /// This does not prove that the referenced commit's dependencies are available.
    pub fn from_annotated_tag(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > 16 * 1024 * 1024 {
            return Err(EncryptionError::TooLarge);
        }
        let crate::repo::object::Object::Tag(tag) = crate::repo::object::Object::decode(bytes)
            .map_err(|_| EncryptionError::InvalidEncoding)?
        else {
            return Err(EncryptionError::InvalidEncoding);
        };
        if tag.object_type != crate::repo::object::ObjectKind::Commit {
            return Err(EncryptionError::InvalidEncoding);
        }
        Ok(Self::AnnotatedTag {
            object: parse_commit(crate::repo::object::ObjectId::for_bytes(bytes).as_str())?,
            commit: parse_commit(tag.object.as_str())?,
        })
    }
}

/// Exact ciphertext suitable for transport CAS. No plaintext Debug output.
pub struct EncryptedRef {
    remote_key: String,
    bytes: Vec<u8>,
}

impl EncryptedRef {
    pub fn seal(root: &RootKey, logical_ref: &str, target: &RefTarget) -> Result<Self> {
        let remote_key = remote_key(root, logical_ref)?;
        let (kind, route, text) = target_fields(root, logical_ref, target)?;
        let mutation = random::<32>()?;
        let body = Zeroizing::new(codec::encode(&[
            logical_ref.as_bytes(),
            text.as_bytes(),
            &mutation,
        ])?);
        let nonce = random::<24>()?;
        let mut bytes = Vec::with_capacity(PREFIX_LENGTH + body.len() + 16);
        bytes.extend_from_slice(b"GRAFTER1");
        bytes.extend_from_slice(&1_u16.to_be_bytes());
        bytes.extend_from_slice(&SUITE.to_be_bytes());
        bytes.push(kind);
        bytes.extend_from_slice(&route);
        bytes.extend_from_slice(&nonce);
        let aad = aad(root, &remote_key, &bytes)?;
        let key = root.derive(b"ref")?;
        let cipher = XChaCha20Poly1305::new_from_slice(key.as_ref())
            .map_err(|_| EncryptionError::InvalidEncoding)?;
        let ciphertext = cipher
            .encrypt(
                XNonce::from_slice(&nonce),
                Payload { msg: &body, aad: &aad },
            )
            .map_err(|_| EncryptionError::AuthenticationFailed)?;
        bytes.extend_from_slice(&ciphertext);
        if bytes.len() > MAX_LENGTH {
            return Err(EncryptionError::TooLarge);
        }
        Ok(Self { remote_key, bytes })
    }

    /// Authenticate the exact observed bytes before decoding or trusting routing.
    /// The caller must retain these bytes for CAS rather than resealing the target.
    pub fn open(root: &RootKey, logical_ref: &str, bytes: &[u8]) -> Result<(Self, RefTarget)> {
        let remote_key = remote_key(root, logical_ref)?;
        let (reference, logical, target) = Self::discover(root, &remote_key, bytes)?;
        if logical != logical_ref {
            return Err(EncryptionError::InvalidEncoding);
        }
        Ok((reference, target))
    }

    /// Authenticate an opaque listed ref before recovering its logical identity.
    pub fn discover(
        root: &RootKey,
        remote: &str,
        bytes: &[u8],
    ) -> Result<(Self, String, RefTarget)> {
        validate_remote_key(remote)?;
        if bytes.len() > MAX_LENGTH {
            return Err(EncryptionError::TooLarge);
        }
        if bytes.len() < PREFIX_LENGTH + 16 {
            return Err(EncryptionError::InvalidEncoding);
        }
        let mut cursor = codec::Cursor::new(&bytes[..PREFIX_LENGTH]);
        if cursor.take(8)? != b"GRAFTER1" {
            return Err(EncryptionError::InvalidEncoding);
        }
        if u16::from_be_bytes(cursor.array()?) != 1 || u16::from_be_bytes(cursor.array()?) != SUITE
        {
            return Err(EncryptionError::UnsupportedProfile);
        }
        let kind = cursor.take(1)?[0];
        let route: [u8; 32] = cursor.array()?;
        let nonce: [u8; 24] = cursor.array()?;
        cursor.finish()?;
        let aad = aad(root, remote, &bytes[..PREFIX_LENGTH])?;
        let key = root.derive(b"ref")?;
        let cipher = XChaCha20Poly1305::new_from_slice(key.as_ref())
            .map_err(|_| EncryptionError::InvalidEncoding)?;
        let body = Zeroizing::new(
            cipher
                .decrypt(
                    XNonce::from_slice(&nonce),
                    Payload { msg: &bytes[PREFIX_LENGTH..], aad: &aad },
                )
                .map_err(|_| EncryptionError::AuthenticationFailed)?,
        );
        let fields = codec::decode(&body, 3, MAX_LENGTH)?;
        if fields[2].len() != 32 {
            return Err(EncryptionError::InvalidEncoding);
        }
        let logical_ref =
            std::str::from_utf8(fields[0]).map_err(|_| EncryptionError::InvalidEncoding)?;
        if remote_key(root, logical_ref)? != remote {
            return Err(EncryptionError::AuthenticationFailed);
        }
        let text = std::str::from_utf8(fields[1]).map_err(|_| EncryptionError::InvalidEncoding)?;
        let target = if let Some(reference) = text.strip_prefix("ref: ") {
            RefTarget::Symbolic(reference.to_owned())
        } else if let Some(tag) = text.strip_prefix("tag: ") {
            let (object, commit) = tag
                .split_once(" commit: ")
                .ok_or(EncryptionError::InvalidEncoding)?;
            RefTarget::AnnotatedTag {
                object: parse_commit(object)?,
                commit: parse_commit(commit)?,
            }
        } else {
            RefTarget::Commit(parse_commit(text)?)
        };
        let (expected_kind, expected_route, _) = target_fields(root, logical_ref, &target)?;
        if kind != expected_kind || route != expected_route {
            return Err(EncryptionError::AuthenticationFailed);
        }
        Ok((
            Self {
                remote_key: remote.to_owned(),
                bytes: bytes.to_vec(),
            },
            logical_ref.to_owned(),
            target,
        ))
    }

    pub fn remote_key(&self) -> &str {
        &self.remote_key
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

pub(crate) fn validate_remote_key(key: &str) -> Result<()> {
    let (class, token) = key
        .rsplit_once('/')
        .ok_or(EncryptionError::InvalidObjectKey)?;
    if class != "e2ee/refs" && class != "e2ee/head" {
        return Err(EncryptionError::InvalidObjectKey);
    }
    parse_commit(token).map_err(|_| EncryptionError::InvalidObjectKey)?;
    Ok(())
}

pub(crate) fn remote_key(root: &RootKey, logical: &str) -> Result<String> {
    if logical.len() > 4096 {
        return Err(EncryptionError::TooLarge);
    }
    let class = if logical == "HEAD" {
        "head"
    } else {
        crate::repo::validate_full_ref(logical).map_err(|_| EncryptionError::InvalidObjectKey)?;
        "refs"
    };
    let key = root.derive(b"name")?;
    let input = codec::encode(&[
        b"graft/name/v1",
        PROFILE,
        class.as_bytes(),
        logical.as_bytes(),
    ])?;
    Ok(format!("e2ee/{class}/{}", hex(&mac(key.as_ref(), &input))))
}

fn target_fields(
    root: &RootKey,
    logical: &str,
    target: &RefTarget,
) -> Result<(u8, [u8; 32], String)> {
    match target {
        RefTarget::Commit(commit) => Ok((1, root.route_commit(commit)?, hex(commit))),
        RefTarget::AnnotatedTag { object, commit } => {
            if !logical.starts_with("refs/tags/") {
                return Err(EncryptionError::InvalidObjectKey);
            }
            Ok((
                3,
                root.route_commit(commit)?,
                format!("tag: {} commit: {}", hex(object), hex(commit)),
            ))
        }
        RefTarget::Symbolic(reference) => {
            if logical != "HEAD" || !reference.starts_with("refs/heads/") {
                return Err(EncryptionError::InvalidObjectKey);
            }
            let key = remote_key(root, reference)?;
            let token = key
                .rsplit('/')
                .next()
                .ok_or(EncryptionError::InvalidEncoding)?;
            Ok((2, parse_commit(token)?, format!("ref: {reference}")))
        }
    }
}

fn aad(root: &RootKey, key: &str, prefix: &[u8]) -> Result<Vec<u8>> {
    codec::encode(&[
        b"graft/ref/v1",
        PROFILE,
        root.repository().as_bytes(),
        key.as_bytes(),
        prefix,
    ])
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub(super) fn parse_commit(text: &str) -> Result<[u8; 32]> {
    if text.len() != 64
        || !text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(EncryptionError::InvalidEncoding);
    }
    let mut bytes = [0; 32];
    for (index, pair) in text.as_bytes().chunks_exact(2).enumerate() {
        let digit = |b: u8| if b <= b'9' { b - b'0' } else { b - b'a' + 10 };
        bytes[index] = digit(pair[0]) * 16 + digit(pair[1]);
    }
    Ok(bytes)
}
