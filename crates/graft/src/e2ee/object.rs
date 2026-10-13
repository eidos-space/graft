use super::{EncryptionError, PROFILE, RepositoryId, Result, RootKey, SUITE, codec, mac, random};
use chacha20poly1305::{
    XChaCha20Poly1305, XNonce,
    aead::{Aead, KeyInit, Payload},
};
use hkdf::Hkdf;
use sha2::{Digest, Sha256};
use std::ops::Range;
use zeroize::Zeroizing;

pub(crate) const PREFIX_LENGTH: usize = 72;
pub(crate) const MAX_HEADER_LENGTH: usize = 16 * 1024;
const TAG_LENGTH: u64 = 16;

#[derive(Clone, Copy, Debug, Default)]
pub enum ChunkSize {
    KiB16,
    #[default]
    KiB64,
    KiB256,
}

impl ChunkSize {
    fn bytes(self) -> u32 {
        match self {
            Self::KiB16 => 16 * 1024,
            Self::KiB64 => 64 * 1024,
            Self::KiB256 => 256 * 1024,
        }
    }

    fn parse(value: u32) -> Result<Self> {
        match value {
            16384 => Ok(Self::KiB16),
            65536 => Ok(Self::KiB64),
            262144 => Ok(Self::KiB256),
            _ => Err(EncryptionError::UnsupportedProfile),
        }
    }
}

struct ObjectIdentity {
    logical: String,
    class: &'static str,
    remote: String,
}

impl ObjectIdentity {
    fn new(root: &RootKey, logical: &str) -> Result<Self> {
        if logical.is_empty()
            || logical.len() > 4096
            || logical.contains('\\')
            || logical.chars().any(char::is_control)
            || logical
                .split('/')
                .any(|part| part.is_empty() || part == "." || part == "..")
        {
            return Err(EncryptionError::InvalidObjectKey);
        }
        let class = if logical.starts_with("objects/pack/") && logical.ends_with(".pack") {
            "packs"
        } else if logical.starts_with("objects/pack/") && logical.ends_with(".idx") {
            "indexes"
        } else if logical.starts_with("objects/") {
            "objects"
        } else if logical.starts_with("store/files/") {
            "files"
        } else if logical.starts_with("logs/")
            && logical.split('/').nth(2) == Some("commits")
            && logical.split('/').count() == 4
        {
            "commits"
        } else if logical.starts_with("segments/") {
            "segments"
        } else {
            return Err(EncryptionError::InvalidObjectKey);
        };
        let key = root.derive(b"name")?;
        let input = codec::encode(&[
            b"graft/name/v1",
            PROFILE,
            class.as_bytes(),
            logical.as_bytes(),
        ])?;
        let token = mac(key.as_ref(), &input);
        let hex: String = token.iter().map(|b| format!("{b:02x}")).collect();
        Ok(Self {
            logical: logical.to_owned(),
            class,
            remote: format!("e2ee/{class}/{hex}"),
        })
    }
}

struct ChunkContext {
    repository: RepositoryId,
    remote_key: String,
    upload_id: [u8; 32],
    header_digest: [u8; 32],
    cipher: XChaCha20Poly1305,
    length: u64,
    chunk_size: u32,
    count: u64,
    digest: [u8; 32],
    header_end: u64,
}

impl ChunkContext {
    fn chunk_length(&self, index: u64) -> Result<usize> {
        if index >= self.count {
            return Err(EncryptionError::InvalidChunk);
        }
        let start = index
            .checked_mul(u64::from(self.chunk_size))
            .ok_or(EncryptionError::TooLarge)?;
        let length = self
            .length
            .saturating_sub(start)
            .min(u64::from(self.chunk_size));
        Ok(length as usize)
    }

    fn aad(&self, index: u64) -> Result<Vec<u8>> {
        codec::encode(&[
            b"graft/object-chunk/v1",
            PROFILE,
            self.repository.as_bytes(),
            self.remote_key.as_bytes(),
            &self.upload_id,
            &self.header_digest,
            &index.to_be_bytes(),
        ])
    }

    fn total_bytes(&self) -> Result<u64> {
        self.count
            .checked_mul(TAG_LENGTH)
            .and_then(|n| n.checked_add(self.length))
            .and_then(|n| n.checked_add(self.header_end))
            .ok_or(EncryptionError::TooLarge)
    }
}

/// Sequential bounded-memory encryption of one already prepared immutable snapshot.
/// A caller must not publish its output until `finish` succeeds.
pub struct ObjectEncryptor {
    context: ChunkContext,
    header: Vec<u8>,
    next: u64,
    digest: Sha256,
}

impl ObjectEncryptor {
    pub fn new(
        root: &RootKey,
        logical: &str,
        length: u64,
        digest: [u8; 32],
        size: ChunkSize,
    ) -> Result<Self> {
        let identity = ObjectIdentity::new(root, logical)?;
        let upload_id = random::<32>()?;
        let nonce = random::<24>()?;
        let chunk_size = size.bytes();
        let count = length / u64::from(chunk_size)
            + u64::from(!length.is_multiple_of(u64::from(chunk_size)));
        let count = count.max(1);
        let plaintext = codec::encode(&[
            identity.logical.as_bytes(),
            identity.class.as_bytes(),
            &length.to_be_bytes(),
            &digest,
            &chunk_size.to_be_bytes(),
            &count.to_be_bytes(),
            &0_u32.to_be_bytes(),
        ])?;
        let encrypted_length = plaintext
            .len()
            .checked_add(TAG_LENGTH as usize)
            .filter(|n| *n <= MAX_HEADER_LENGTH)
            .ok_or(EncryptionError::TooLarge)?;
        let mut prefix = Vec::with_capacity(PREFIX_LENGTH);
        prefix.extend_from_slice(b"GRAFTEO1");
        prefix.extend_from_slice(&1_u16.to_be_bytes());
        prefix.extend_from_slice(&SUITE.to_be_bytes());
        prefix.extend_from_slice(&upload_id);
        prefix.extend_from_slice(&nonce);
        prefix.extend_from_slice(&(encrypted_length as u32).to_be_bytes());
        let aad = codec::encode(&[
            b"graft/object-header/v1",
            PROFILE,
            root.repository.as_bytes(),
            identity.class.as_bytes(),
            identity.remote.as_bytes(),
            &prefix,
        ])?;
        let key = upload_key(
            root,
            b"header",
            b"graft/upload-header/v1",
            &identity.remote,
            &upload_id,
        )?;
        let encrypted = cipher(key.as_ref())?
            .encrypt(
                XNonce::from_slice(&nonce),
                Payload { msg: &plaintext, aad: &aad },
            )
            .map_err(|_| EncryptionError::AuthenticationFailed)?;
        let payload_key = upload_key(
            root,
            b"payload",
            b"graft/upload-payload/v1",
            &identity.remote,
            &upload_id,
        )?;
        let context = ChunkContext {
            repository: root.repository,
            remote_key: identity.remote,
            upload_id,
            header_digest: Sha256::digest(&encrypted).into(),
            cipher: cipher(payload_key.as_ref())?,
            length,
            chunk_size,
            count,
            digest,
            header_end: (PREFIX_LENGTH + encrypted.len()) as u64,
        };
        context.total_bytes()?;
        prefix.extend_from_slice(&encrypted);
        Ok(Self {
            context,
            header: prefix,
            next: 0,
            digest: Sha256::new(),
        })
    }

    pub fn remote_key(&self) -> &str {
        &self.context.remote_key
    }
    pub fn header_bytes(&self) -> &[u8] {
        &self.header
    }
    pub fn chunk_size(&self) -> usize {
        self.context.chunk_size as usize
    }
    pub fn chunk_count(&self) -> u64 {
        self.context.count
    }
    pub fn ciphertext_length(&self) -> Result<u64> {
        self.context.total_bytes()
    }

    /// Consumes each nonce once; there is deliberately no arbitrary-index encryption API.
    pub fn encrypt_next(&mut self, plaintext: &[u8]) -> Result<Vec<u8>> {
        if plaintext.len() != self.context.chunk_length(self.next)? {
            return Err(EncryptionError::InvalidChunk);
        }
        let index = self.next;
        self.next += 1;
        self.digest.update(plaintext);
        let nonce = chunk_nonce(index);
        self.context
            .cipher
            .encrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: plaintext,
                    aad: &self.context.aad(index)?,
                },
            )
            .map_err(|_| EncryptionError::AuthenticationFailed)
    }

    pub fn finish(self) -> Result<()> {
        if self.next != self.context.count {
            return Err(EncryptionError::InvalidChunk);
        }
        let digest: [u8; 32] = self.digest.finalize().into();
        if digest != self.context.digest {
            return Err(EncryptionError::SnapshotMismatch);
        }
        Ok(())
    }
}

/// Authenticated header and chunk reader, including discovery from opaque remote keys.
pub struct ObjectReader {
    logical: String,
    context: ChunkContext,
}

impl ObjectReader {
    /// Reads only bounded header bytes. `total_bytes` is the full transport object size.
    pub fn open(root: &RootKey, logical: &str, header: &[u8], total_bytes: u64) -> Result<Self> {
        let remote = remote_key(root, logical)?;
        let reader = Self::discover(root, &remote, header, total_bytes)?;
        if reader.logical != logical {
            return Err(EncryptionError::AuthenticationFailed);
        }
        Ok(reader)
    }

    /// Authenticate a listed opaque key and recover its logical identity from the header.
    pub fn discover(root: &RootKey, remote: &str, header: &[u8], total_bytes: u64) -> Result<Self> {
        if header.len() > PREFIX_LENGTH + MAX_HEADER_LENGTH {
            return Err(EncryptionError::TooLarge);
        }
        let class = opaque_class(remote)?;
        let authenticated = decrypt_header(root, remote, class, header)?;
        let fields = codec::decode(&authenticated.plaintext, 7, MAX_HEADER_LENGTH)?;
        let logical =
            std::str::from_utf8(fields[0]).map_err(|_| EncryptionError::InvalidEncoding)?;
        let identity = ObjectIdentity::new(root, logical)?;
        if identity.remote != remote || fields[1] != class.as_bytes() || identity.class != class {
            return Err(EncryptionError::AuthenticationFailed);
        }
        let length = u64::from_be_bytes(array(fields[2])?);
        let digest = array(fields[3])?;
        let size = ChunkSize::parse(u32::from_be_bytes(array(fields[4])?))?.bytes();
        let count = u64::from_be_bytes(array(fields[5])?);
        if u32::from_be_bytes(array(fields[6])?) != 0 {
            return Err(EncryptionError::UnsupportedProfile);
        }
        let expected =
            (length / u64::from(size) + u64::from(!length.is_multiple_of(u64::from(size)))).max(1);
        if count != expected {
            return Err(EncryptionError::InvalidEncoding);
        }
        let payload_key = upload_key(
            root,
            b"payload",
            b"graft/upload-payload/v1",
            &identity.remote,
            &authenticated.upload_id,
        )?;
        let context = ChunkContext {
            repository: root.repository,
            remote_key: identity.remote,
            upload_id: authenticated.upload_id,
            header_digest: authenticated.digest,
            cipher: cipher(payload_key.as_ref())?,
            length,
            chunk_size: size,
            count,
            digest,
            header_end: header.len() as u64,
        };
        if context.total_bytes()? != total_bytes {
            return Err(EncryptionError::InvalidEncoding);
        }
        Ok(Self { logical: identity.logical, context })
    }

    pub fn logical_key(&self) -> &str {
        &self.logical
    }

    pub(crate) fn header_length(prefix: &[u8]) -> Result<usize> {
        if prefix.len() != PREFIX_LENGTH || &prefix[..8] != b"GRAFTEO1" {
            return Err(EncryptionError::InvalidEncoding);
        }
        if prefix[8..10] != 1_u16.to_be_bytes() || prefix[10..12] != SUITE.to_be_bytes() {
            return Err(EncryptionError::UnsupportedProfile);
        }
        let length = u32::from_be_bytes(array(&prefix[68..72])?) as usize;
        if !(TAG_LENGTH as usize..=MAX_HEADER_LENGTH).contains(&length) {
            return Err(EncryptionError::TooLarge);
        }
        Ok(PREFIX_LENGTH + length)
    }

    pub(crate) fn chunk_size(&self) -> u64 {
        u64::from(self.context.chunk_size)
    }

    pub(crate) fn chunk_range(&self, index: u64) -> Result<Range<u64>> {
        let length = self.context.chunk_length(index)? as u64;
        let start = self
            .context
            .header_end
            .checked_add(
                index
                    .checked_mul(self.chunk_size() + TAG_LENGTH)
                    .ok_or(EncryptionError::TooLarge)?,
            )
            .ok_or(EncryptionError::TooLarge)?;
        Ok(start
            ..start
                .checked_add(length + TAG_LENGTH)
                .ok_or(EncryptionError::TooLarge)?)
    }

    pub fn plaintext_length(&self) -> u64 {
        self.context.length
    }
    pub fn plaintext_digest(&self) -> &[u8; 32] {
        &self.context.digest
    }
    pub fn chunk_count(&self) -> u64 {
        self.context.count
    }

    pub fn decrypt_chunk(&self, index: u64, ciphertext: &[u8]) -> Result<Vec<u8>> {
        if ciphertext.len() != self.context.chunk_length(index)? + TAG_LENGTH as usize {
            return Err(EncryptionError::InvalidChunk);
        }
        let nonce = chunk_nonce(index);
        self.context
            .cipher
            .decrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: ciphertext,
                    aad: &self.context.aad(index)?,
                },
            )
            .map_err(|_| EncryptionError::AuthenticationFailed)
    }

    /// Full covering ciphertext blocks, never an unauthenticated byte slice.
    pub fn ciphertext_range(&self, plaintext: Range<u64>) -> Result<Range<u64>> {
        if plaintext.start > plaintext.end || plaintext.end > self.context.length {
            return Err(EncryptionError::InvalidChunk);
        }
        if plaintext.is_empty() {
            return Ok(self.context.header_end..self.context.header_end);
        }
        let size = u64::from(self.context.chunk_size);
        let first = plaintext.start / size;
        let last = (plaintext.end - 1) / size;
        let start = self
            .context
            .header_end
            .checked_add(
                first
                    .checked_mul(size + TAG_LENGTH)
                    .ok_or(EncryptionError::TooLarge)?,
            )
            .ok_or(EncryptionError::TooLarge)?;
        let end = if last + 1 == self.context.count {
            self.context.total_bytes()?
        } else {
            self.context
                .header_end
                .checked_add(
                    (last + 1)
                        .checked_mul(size + TAG_LENGTH)
                        .ok_or(EncryptionError::TooLarge)?,
                )
                .ok_or(EncryptionError::TooLarge)?
        };
        Ok(start..end)
    }
}

struct AuthenticatedHeader {
    plaintext: Zeroizing<Vec<u8>>,
    upload_id: [u8; 32],
    digest: [u8; 32],
}

fn decrypt_header(
    root: &RootKey,
    remote: &str,
    class: &str,
    header: &[u8],
) -> Result<AuthenticatedHeader> {
    let mut cursor = codec::Cursor::new(header);
    if cursor.take(8)? != b"GRAFTEO1" {
        return Err(EncryptionError::InvalidEncoding);
    }
    if u16::from_be_bytes(cursor.array()?) != 1 || u16::from_be_bytes(cursor.array()?) != SUITE {
        return Err(EncryptionError::UnsupportedProfile);
    }
    let upload_id = cursor.array()?;
    let nonce: [u8; 24] = cursor.array()?;
    let length = u32::from_be_bytes(cursor.array()?) as usize;
    if !(TAG_LENGTH as usize..=MAX_HEADER_LENGTH).contains(&length) {
        return Err(EncryptionError::TooLarge);
    }
    let encrypted = cursor.take(length)?;
    cursor.finish()?;
    let aad = codec::encode(&[
        b"graft/object-header/v1",
        PROFILE,
        root.repository.as_bytes(),
        class.as_bytes(),
        remote.as_bytes(),
        &header[..PREFIX_LENGTH],
    ])?;
    let key = upload_key(
        root,
        b"header",
        b"graft/upload-header/v1",
        remote,
        &upload_id,
    )?;
    let plaintext = Zeroizing::new(
        cipher(key.as_ref())?
            .decrypt(
                XNonce::from_slice(&nonce),
                Payload { msg: encrypted, aad: &aad },
            )
            .map_err(|_| EncryptionError::AuthenticationFailed)?,
    );
    Ok(AuthenticatedHeader {
        plaintext,
        upload_id,
        digest: Sha256::digest(encrypted).into(),
    })
}

pub(crate) fn remote_key(root: &RootKey, logical: &str) -> Result<String> {
    Ok(ObjectIdentity::new(root, logical)?.remote)
}

pub(crate) fn opaque_class(remote: &str) -> Result<&str> {
    let mut parts = remote.split('/');
    if parts.next() != Some("e2ee") {
        return Err(EncryptionError::InvalidObjectKey);
    }
    let class = parts.next().ok_or(EncryptionError::InvalidObjectKey)?;
    let token = parts.next().ok_or(EncryptionError::InvalidObjectKey)?;
    if !matches!(
        class,
        "objects" | "packs" | "indexes" | "files" | "commits" | "segments"
    ) || token.len() != 64
        || !token
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        || parts.next().is_some()
    {
        return Err(EncryptionError::InvalidObjectKey);
    }
    Ok(class)
}

fn chunk_nonce(index: u64) -> [u8; 24] {
    let mut nonce = [0; 24];
    nonce[16..].copy_from_slice(&index.to_be_bytes());
    nonce
}

fn upload_key(
    root: &RootKey,
    purpose: &[u8],
    label: &[u8],
    remote: &str,
    upload_id: &[u8; 32],
) -> Result<Zeroizing<[u8; 32]>> {
    let base = root.derive(purpose)?;
    let info = codec::encode(&[
        label,
        PROFILE,
        root.repository.as_bytes(),
        remote.as_bytes(),
    ])?;
    let mut key = Zeroizing::new([0; 32]);
    Hkdf::<Sha256>::new(Some(upload_id), base.as_ref())
        .expand(&info, key.as_mut())
        .map_err(|_| EncryptionError::InvalidEncoding)?;
    Ok(key)
}

fn cipher(key: &[u8]) -> Result<XChaCha20Poly1305> {
    XChaCha20Poly1305::new_from_slice(key).map_err(|_| EncryptionError::InvalidEncoding)
}

fn array<const N: usize>(bytes: &[u8]) -> Result<[u8; N]> {
    bytes
        .try_into()
        .map_err(|_| EncryptionError::InvalidEncoding)
}
