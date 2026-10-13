use super::{
    EncryptionError, PROFILE, RepositoryId, Result, RootKey, SUITE, codec, mac, random, verify_mac,
};
use argon2::{Algorithm, Argon2, Block, Params, Version};
use chacha20poly1305::{
    XChaCha20Poly1305, XNonce,
    aead::{Aead, KeyInit, Payload},
};
use sha2::{Digest, Sha256};
use std::sync::Mutex;
use zeroize::Zeroizing;

const MAGIC: &[u8; 8] = b"GRAFTEC1";
const CONFIG_BYTES: usize = 206;
const WRAP_AAD_END: usize = 102;
const MAC_INPUT_END: usize = 174;
static KDF_LOCK: Mutex<()> = Mutex::new(());

/// Validated configuration structure; authenticity is checked separately.
#[derive(Clone, Debug)]
pub struct EncryptedConfig {
    bytes: [u8; CONFIG_BYTES],
    repository: RepositoryId,
    generation: u64,
}

impl EncryptedConfig {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != CONFIG_BYTES {
            return Err(EncryptionError::InvalidEncoding);
        }
        let mut cursor = codec::Cursor::new(bytes);
        if cursor.take(8)? != MAGIC {
            return Err(EncryptionError::InvalidEncoding);
        }
        if u16::from_be_bytes(cursor.array()?) != 1 || u16::from_be_bytes(cursor.array()?) != SUITE
        {
            return Err(EncryptionError::UnsupportedProfile);
        }
        let repository = RepositoryId::from_bytes(cursor.array()?)?;
        let generation = u64::from_be_bytes(cursor.array()?);
        if generation == 0 {
            return Err(EncryptionError::InvalidEncoding);
        }
        cursor.take(32)?;
        if u16::from_be_bytes(cursor.array()?) != 1 {
            return Err(EncryptionError::UnsupportedProfile);
        }
        cursor.take(32 + 24 + 48 + 32)?;
        cursor.finish()?;
        Ok(Self {
            bytes: bytes
                .try_into()
                .map_err(|_| EncryptionError::InvalidEncoding)?,
            repository,
            generation,
        })
    }

    pub fn create(root: &RootKey, password: &[u8]) -> Result<Self> {
        Self::wrap(root, password, 1)
    }

    fn wrap(root: &RootKey, password: &[u8], generation: u64) -> Result<Self> {
        validate_password(password)?;
        let salt = random::<32>()?;
        let nonce = random::<24>()?;
        let mut bytes = [0; CONFIG_BYTES];
        bytes[..8].copy_from_slice(MAGIC);
        bytes[8..10].copy_from_slice(&1_u16.to_be_bytes());
        bytes[10..12].copy_from_slice(&SUITE.to_be_bytes());
        bytes[12..28].copy_from_slice(root.repository.as_bytes());
        bytes[28..36].copy_from_slice(&generation.to_be_bytes());
        bytes[36..68].copy_from_slice(&random::<32>()?);
        bytes[68..70].copy_from_slice(&1_u16.to_be_bytes());
        bytes[70..102].copy_from_slice(&salt);
        bytes[102..126].copy_from_slice(&nonce);
        let aad = codec::encode(&[b"graft/config-wrap/v1", PROFILE, &bytes[..WRAP_AAD_END]])?;
        let wrap_key = password_key(password, &salt)?;
        let cipher = XChaCha20Poly1305::new_from_slice(wrap_key.as_ref())
            .map_err(|_| EncryptionError::InvalidEncoding)?;
        let wrapped = cipher
            .encrypt(
                XNonce::from_slice(&nonce),
                Payload { msg: root.secret.as_ref(), aad: &aad },
            )
            .map_err(|_| EncryptionError::AuthenticationFailed)?;
        bytes[126..174].copy_from_slice(&wrapped);
        let config_key = root.derive(b"config")?;
        let input = codec::encode(&[b"graft/config-mac/v1", PROFILE, &bytes[..MAC_INPUT_END]])?;
        bytes[174..].copy_from_slice(&mac(config_key.as_ref(), &input));
        Self::parse(&bytes)
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub fn repository(&self) -> RepositoryId {
        self.repository
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Pins identity before expensive KDF. A successful unwrap is not a proof of freshness.
    pub fn unlock(&self, password: &[u8], pinned: Option<RepositoryId>) -> Result<RootKey> {
        if pinned.is_some_and(|id| id != self.repository) {
            return Err(EncryptionError::IdentityChanged);
        }
        let key = password_key(password, &self.bytes[70..102])?;
        let cipher = XChaCha20Poly1305::new_from_slice(key.as_ref())
            .map_err(|_| EncryptionError::InvalidEncoding)?;
        let aad = codec::encode(&[
            b"graft/config-wrap/v1",
            PROFILE,
            &self.bytes[..WRAP_AAD_END],
        ])?;
        let plaintext = Zeroizing::new(
            cipher
                .decrypt(
                    XNonce::from_slice(&self.bytes[102..126]),
                    Payload { msg: &self.bytes[126..174], aad: &aad },
                )
                .map_err(|_| EncryptionError::UnlockFailed)?,
        );
        let mut secret = Zeroizing::new([0; 32]);
        if plaintext.len() != secret.len() {
            return Err(EncryptionError::UnlockFailed);
        }
        secret.copy_from_slice(&plaintext);
        let root = RootKey::import(self.repository, secret);
        self.authenticate(&root)
            .map_err(|_| EncryptionError::UnlockFailed)?;
        Ok(root)
    }

    pub fn authenticate(&self, root: &RootKey) -> Result<()> {
        if root.repository != self.repository {
            return Err(EncryptionError::IdentityChanged);
        }
        let key = root.derive(b"config")?;
        let input = codec::encode(&[
            b"graft/config-mac/v1",
            PROFILE,
            &self.bytes[..MAC_INPUT_END],
        ])?;
        verify_mac(key.as_ref(), &input, &self.bytes[MAC_INPUT_END..])
    }

    /// Builds a fixed candidate only. Hosts must authorize and CAS the old bytes before success.
    pub fn password_change_candidate(&self, root: &RootKey, password: &[u8]) -> Result<Self> {
        self.authenticate(root)?;
        let generation = self
            .generation
            .checked_add(1)
            .ok_or(EncryptionError::GenerationExhausted)?;
        Self::wrap(root, password, generation)
    }

    pub fn checkpoint(&self, root: &RootKey) -> Result<ConfigCheckpoint> {
        self.authenticate(root)?;
        Ok(ConfigCheckpoint {
            repository: self.repository,
            generation: self.generation,
            digest: Sha256::digest(self.bytes).into(),
        })
    }
}

/// Non-secret host-private observation; it is not a global freshness proof.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigCheckpoint {
    repository: RepositoryId,
    generation: u64,
    digest: [u8; 32],
}

impl ConfigCheckpoint {
    pub(crate) fn require_successor(&self, next: &Self) -> Result<()> {
        if self.repository != next.repository {
            return Err(EncryptionError::IdentityChanged);
        }
        if next.generation < self.generation
            || (next.generation == self.generation && next.digest != self.digest)
        {
            return Err(EncryptionError::ConfigRollbackDetected);
        }
        Ok(())
    }
    /// Host-private persistence; this format is not an HTTP protocol object.
    pub fn to_local_bytes(&self) -> [u8; 64] {
        let mut bytes = [0; 64];
        bytes[..8].copy_from_slice(b"GRAFTCP1");
        bytes[8..24].copy_from_slice(self.repository.as_bytes());
        bytes[24..32].copy_from_slice(&self.generation.to_be_bytes());
        bytes[32..].copy_from_slice(&self.digest);
        bytes
    }

    /// Local metadata is trusted, but malformed records are never accepted.
    pub fn from_local_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != 64 {
            return Err(EncryptionError::InvalidEncoding);
        }
        let mut cursor = codec::Cursor::new(bytes);
        if cursor.take(8)? != b"GRAFTCP1" {
            return Err(EncryptionError::InvalidEncoding);
        }
        let repository = RepositoryId::from_bytes(cursor.array()?)?;
        let generation = u64::from_be_bytes(cursor.array()?);
        if generation == 0 {
            return Err(EncryptionError::InvalidEncoding);
        }
        let digest = cursor.array()?;
        cursor.finish()?;
        Ok(Self { repository, generation, digest })
    }

    pub fn repository(&self) -> RepositoryId {
        self.repository
    }

    pub fn validate(&self, config: &EncryptedConfig, root: &RootKey) -> Result<Self> {
        config.authenticate(root)?;
        if self.repository != config.repository {
            return Err(EncryptionError::IdentityChanged);
        }
        let next = config.checkpoint(root)?;
        if next.generation < self.generation
            || (next.generation == self.generation && next.digest != self.digest)
        {
            return Err(EncryptionError::ConfigRollbackDetected);
        }
        Ok(next)
    }
}

pub(crate) fn validate_password(password: &[u8]) -> Result<()> {
    if password.is_empty() || password.len() > 1024 || std::str::from_utf8(password).is_err() {
        return Err(EncryptionError::InvalidPassword);
    }
    Ok(())
}

fn password_key(password: &[u8], salt: &[u8]) -> Result<Zeroizing<[u8; 32]>> {
    validate_password(password)?;
    let _guard = KDF_LOCK.lock().map_err(|_| EncryptionError::UnlockFailed)?;
    let params =
        Params::new(65536, 3, 4, Some(32)).map_err(|_| EncryptionError::UnsupportedProfile)?;
    // Argon2's allocating convenience method does not wipe its memory blocks.
    let mut memory = Zeroizing::new(vec![Block::default(); params.block_count()]);
    let mut key = Zeroizing::new([0; 32]);
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into_with_memory(password, salt, key.as_mut(), memory.as_mut_slice())
        .map_err(|_| EncryptionError::UnlockFailed)?;
    Ok(key)
}
