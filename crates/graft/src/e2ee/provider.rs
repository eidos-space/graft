use super::{Result, RootKey, SessionBinding};

/// Trusted, blocking native provider. Ordinary plugins must not receive this API.
/// Implementations must use approved OS protection, bind the application and exact
/// session identity, and never fall back to plaintext storage. Absence is distinct
/// from unavailable, corrupt or unauthorized storage. Forget requires the host to
/// drain every session using the entry before deleting it.
pub trait RememberedKeyProvider: Send + Sync {
    fn load(&self, binding: &SessionBinding) -> Result<Option<RootKey>>;
    fn remember(&self, binding: &SessionBinding, root: &RootKey) -> Result<()>;
    fn forget(&self, binding: &SessionBinding) -> Result<()>;
}

/// Trusted native host authorization for one password-reset attempt. Implementations
/// must perform fresh OS authentication for the exact application/account/repository
/// binding. Account login, a remembered-key read, cached consent or a JS boolean
/// must not satisfy this contract. No root key is provided to the authorizer.
/// This blocking call is owned by the secret session's lock/drain boundary.
pub trait PasswordResetAuthorizer: Send + Sync {
    fn authorize(&self, binding: &SessionBinding) -> Result<()>;
}

#[cfg(all(target_os = "macos", feature = "macos-keychain"))]
mod macos {
    use super::*;
    use crate::e2ee::{EncryptionError, PROFILE, SUITE, codec};
    use security_framework::{
        access_control::{ProtectionMode, SecAccessControl},
        passwords::{
            PasswordOptions, delete_generic_password_options, generic_password,
            set_generic_password_options,
        },
    };
    use sha2::{Digest, Sha256};
    use zeroize::Zeroizing;

    // Stable Security.framework status; all other statuses fail closed.
    const ITEM_NOT_FOUND: i32 = -25300;
    const RECORD_LENGTH: usize = 72;

    /// Data Protection Keychain provider for an appropriately signed/entitled
    /// host. Does not request iCloud synchronization, weak fallback, user-presence
    /// authentication or hardware isolation. Fresh local authentication for
    /// password reset remains the host's separate responsibility.
    pub struct MacKeychainProvider {
        application: String,
        access_group: String,
    }

    impl MacKeychainProvider {
        pub fn new(application: String, access_group: String) -> Result<Self> {
            for value in [&application, &access_group] {
                if value.is_empty()
                    || value.len() > 255
                    || !value.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_')
                    })
                {
                    return Err(EncryptionError::InvalidSessionBinding);
                }
            }
            let Some((team, bundle)) = access_group.split_once('.') else {
                return Err(EncryptionError::InvalidSessionBinding);
            };
            if team.len() != 10
                || !team
                    .bytes()
                    .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
                || bundle != application
            {
                return Err(EncryptionError::InvalidSessionBinding);
            }
            Ok(Self { application, access_group })
        }

        fn identity(&self, binding: &SessionBinding) -> Result<[u8; 32]> {
            Ok(Sha256::digest(codec::encode(&[
                b"graft/remembered-key/v1",
                PROFILE,
                &SUITE.to_be_bytes(),
                self.application.as_bytes(),
                self.access_group.as_bytes(),
                binding.domain().as_bytes(),
                binding.account().as_bytes(),
                binding.repository().as_bytes(),
            ])?)
            .into())
        }

        fn options(&self, identity: &[u8; 32]) -> PasswordOptions {
            let service = format!("{}.graft.e2ee-root.v1", self.application);
            let account = identity
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            let mut options = PasswordOptions::new_generic_password(&service, &account);
            options.use_protected_keychain();
            options.set_access_group(&self.access_group);
            options.set_access_synchronized(Some(false));
            options
        }

        fn decode(&self, binding: &SessionBinding, bytes: Zeroizing<Vec<u8>>) -> Result<RootKey> {
            if bytes.len() != RECORD_LENGTH || &bytes[..8] != b"GRAFTKR1" {
                return Err(EncryptionError::InvalidEncoding);
            }
            if bytes[8..40] != self.identity(binding)? {
                return Err(EncryptionError::IdentityChanged);
            }
            let mut secret = Zeroizing::new([0_u8; 32]);
            secret.copy_from_slice(&bytes[40..]);
            Ok(RootKey::import(binding.repository(), secret))
        }
    }

    impl RememberedKeyProvider for MacKeychainProvider {
        fn load(&self, binding: &SessionBinding) -> Result<Option<RootKey>> {
            match generic_password(self.options(&self.identity(binding)?)) {
                Ok(bytes) => self.decode(binding, Zeroizing::new(bytes)).map(Some),
                Err(error) if error.code() == ITEM_NOT_FOUND => Ok(None),
                Err(_) => Err(EncryptionError::SecureStorageUnavailable),
            }
        }

        fn remember(&self, binding: &SessionBinding, root: &RootKey) -> Result<()> {
            if root.repository() != binding.repository() {
                return Err(EncryptionError::IdentityChanged);
            }
            let identity = self.identity(binding)?;
            let mut options = self.options(&identity);
            let control = SecAccessControl::create_with_protection(
                Some(ProtectionMode::AccessibleWhenUnlockedThisDeviceOnly),
                0,
            )
            .map_err(|_| EncryptionError::SecureStorageUnavailable)?;
            options.set_access_control(control);
            let mut record = Zeroizing::new([0_u8; RECORD_LENGTH]);
            record[..8].copy_from_slice(b"GRAFTKR1");
            record[8..40].copy_from_slice(&identity);
            record[40..].copy_from_slice(root.secret.as_ref());
            set_generic_password_options(record.as_ref(), options)
                .map_err(|_| EncryptionError::SecureStorageUnavailable)
        }

        fn forget(&self, binding: &SessionBinding) -> Result<()> {
            match delete_generic_password_options(self.options(&self.identity(binding)?)) {
                Ok(()) => Ok(()),
                Err(error) if error.code() == ITEM_NOT_FOUND => Ok(()),
                Err(_) => Err(EncryptionError::SecureStorageUnavailable),
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::e2ee::tests::fixture_root;

        fn binding(account: &str) -> SessionBinding {
            SessionBinding::new(
                "provider-domain".into(),
                account.into(),
                fixture_root().repository(),
            )
            .unwrap()
        }

        #[test]
        fn provider_record_binds_application_group_account_domain_and_repository() {
            let provider = MacKeychainProvider::new(
                "org.graft.test".into(),
                "GRAFTTEST0.org.graft.test".into(),
            )
            .unwrap();
            let binding = binding("provider-account");
            let identity = provider.identity(&binding).unwrap();
            let mut record = Zeroizing::new(vec![0_u8; RECORD_LENGTH]);
            record[..8].copy_from_slice(b"GRAFTKR1");
            record[8..40].copy_from_slice(&identity);
            record[40..].copy_from_slice(fixture_root().secret.as_ref());
            let decoded = provider.decode(&binding, record.clone()).unwrap();
            assert_eq!(
                decoded.route_commit(&[7; 32]).unwrap(),
                fixture_root().route_commit(&[7; 32]).unwrap()
            );
            let changed_account = super::tests::binding("other-account");
            let changed_domain = SessionBinding::new(
                "other-domain".into(),
                "provider-account".into(),
                fixture_root().repository(),
            )
            .unwrap();
            let changed_uuid = SessionBinding::new(
                "provider-domain".into(),
                "provider-account".into(),
                crate::e2ee::RepositoryId::generate().unwrap(),
            )
            .unwrap();
            for changed in [changed_account, changed_domain, changed_uuid] {
                assert_ne!(provider.identity(&changed).unwrap(), identity);
                assert!(matches!(
                    provider.decode(&changed, record.clone()),
                    Err(EncryptionError::IdentityChanged)
                ));
            }
            for provider in [
                MacKeychainProvider::new(
                    "org.graft.other".into(),
                    "GRAFTTEST0.org.graft.other".into(),
                )
                .unwrap(),
                MacKeychainProvider::new(
                    "org.graft.test".into(),
                    "OTHERTST00.org.graft.test".into(),
                )
                .unwrap(),
            ] {
                assert!(matches!(
                    provider.decode(&binding, record.clone()),
                    Err(EncryptionError::IdentityChanged)
                ));
            }
            record.push(0);
            assert!(matches!(
                provider.decode(&binding, record),
                Err(EncryptionError::InvalidEncoding)
            ));
            for group in [
                "com.apple.token",
                "GRAFTTEST0.org.graft.other",
                "GRAFTTEST0.*",
            ] {
                assert!(matches!(
                    MacKeychainProvider::new("org.graft.test".into(), group.into()),
                    Err(EncryptionError::InvalidSessionBinding)
                ));
            }
        }

        #[test]
        #[ignore = "requires macOS security daemon; uses only an unentitled synthetic access group"]
        fn provider_unentitled_keychain_fails_closed_without_fallback() {
            let provider = MacKeychainProvider::new(
                "org.graft.synthetic-unentitled-test".into(),
                "GRAFTTEST0.org.graft.synthetic-unentitled-test".into(),
            )
            .unwrap();
            let binding = SessionBinding::new(
                "provider-domain".into(),
                "provider-account".into(),
                crate::e2ee::RepositoryId::generate().unwrap(),
            )
            .unwrap();
            let root = RootKey::generate(binding.repository()).unwrap();
            assert_eq!(
                provider.remember(&binding, &root),
                Err(EncryptionError::SecureStorageUnavailable)
            );
            assert!(matches!(
                provider.load(&binding),
                Err(EncryptionError::SecureStorageUnavailable)
            ));
            assert_eq!(
                provider.forget(&binding),
                Err(EncryptionError::SecureStorageUnavailable)
            );
        }
    }
}

#[cfg(all(target_os = "macos", feature = "macos-keychain"))]
pub use macos::MacKeychainProvider;
