//! Bounded ciphertext inventory shared by every encrypted backend instance in
//! one synchronous SDK push. Temporary objects retain their epoch validation.
use crate::e2ee::{PreparedEncryptedObject, RoutingHint};
use std::{
    cell::RefCell,
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

pub(super) const MAX_PLAN_BYTES: u64 = 2 * 1024 * 1024 * 1024;
pub(super) const MAX_PLAN_OBJECTS: usize = 256;

pub(super) struct Pending {
    pub objects: Vec<PreparedEncryptedObject>,
    pub hints: Vec<RoutingHint>,
    pub hint_bytes: usize,
    pub trace: Option<crate::trace::PushTraceSpan>,
    identities: BTreeMap<String, usize>,
    pub bytes: u64,
    pub sealed: bool,
    pub disabled: bool,
}

thread_local! {
    static ACTIVE: RefCell<Option<Arc<Mutex<Pending>>>> = const { RefCell::new(None) };
}

pub(super) fn current() -> Option<Arc<Mutex<Pending>>> {
    ACTIVE.with(|active| active.borrow().clone())
}

pub(super) fn with_plan<T>(operation: impl FnOnce() -> T) -> T {
    struct Scope(Option<Arc<Mutex<Pending>>>);
    impl Drop for Scope {
        fn drop(&mut self) {
            ACTIVE.with(|active| {
                active.replace(self.0.take());
            });
        }
    }
    let previous = ACTIVE.with(|active| {
        active.replace(Some(Arc::new(Mutex::new(Pending {
            objects: Vec::new(),
            hints: Vec::new(),
            hint_bytes: 0,
            trace: Some(crate::trace::PushTraceSpan::new(
                "encrypted_upload_plan_prepare",
            )),
            identities: BTreeMap::new(),
            bytes: 0,
            sealed: false,
            disabled: false,
        }))))
    });
    let _scope = Scope(previous);
    operation()
}

pub(super) enum Admission {
    Retained,
    Full,
    Disabled,
}

impl Pending {
    pub fn admit_hint(&mut self, hint: &RoutingHint) -> Admission {
        if self.disabled || self.sealed {
            return Admission::Disabled;
        }
        let Some(bytes) = self.hint_bytes.checked_add(hint.as_bytes().len()) else {
            return Admission::Full;
        };
        if bytes > 1024 * 1024 || self.hints.len() == MAX_PLAN_OBJECTS {
            return Admission::Full;
        }
        self.hint_bytes = bytes;
        self.hints.push(hint.clone());
        Admission::Retained
    }

    pub fn admit(&mut self, object: &PreparedEncryptedObject) -> super::Result<Admission> {
        if self.disabled || self.sealed {
            return Ok(Admission::Disabled);
        }
        if let Some(index) = self.identities.get(object.remote_key()) {
            if !self.objects[*index].same_plaintext(object) {
                return Err(crate::e2ee::EncryptionError::ImmutableConflict.into());
            }
            return Ok(Admission::Retained);
        }
        let Some(bytes) = self.bytes.checked_add(object.ciphertext_length()) else {
            return Ok(Admission::Full);
        };
        if bytes > MAX_PLAN_BYTES || self.objects.len() == MAX_PLAN_OBJECTS {
            return Ok(Admission::Full);
        }
        self.identities
            .insert(object.remote_key().to_owned(), self.objects.len());
        self.objects.push(object.retained_copy());
        self.bytes = bytes;
        Ok(Admission::Retained)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::e2ee::{
        ChunkSize, EncryptedConfig, ObjectSnapshot, RepositoryId, RootKey, SecretSession,
        SessionBinding,
    };
    use sha2::{Digest, Sha256};

    #[tokio::test]
    async fn inventory_deduplicates_and_rejects_conflicting_or_over_budget_objects() {
        let directory = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt;
        crate::native_fs::set_permissions(
            directory.path(),
            crate::native_fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        let root = RootKey::generate(RepositoryId::generate().unwrap()).unwrap();
        let config = EncryptedConfig::create(&root, b"synthetic plan test").unwrap();
        let session = SecretSession::new(
            SessionBinding::new("plan.test".into(), "test".into(), root.repository()).unwrap(),
            None,
        )
        .unwrap();
        session.unlock_with_root(&config, root).unwrap();
        let operation = session.begin_operation().unwrap();
        let mut objects = Vec::new();
        for (path, value) in [
            ("segments/one", b"first".as_slice()),
            ("segments/two", b"second".as_slice()),
            ("segments/one", b"different".as_slice()),
        ] {
            let snapshot = ObjectSnapshot {
                logical_key: path.into(),
                plaintext_length: value.len() as u64,
                plaintext_sha256: Sha256::digest(value).into(),
                chunk_size: ChunkSize::KiB64,
            };
            let mut source = value;
            objects.push(
                PreparedEncryptedObject::prepare(
                    &operation,
                    snapshot,
                    &mut source,
                    directory.path(),
                    4096,
                )
                .await
                .unwrap(),
            );
        }
        with_plan(|| {
            let plan = current().unwrap();
            let mut plan = plan.lock().unwrap();
            assert!(matches!(
                plan.admit(&objects[0]).unwrap(),
                Admission::Retained
            ));
            assert!(matches!(
                plan.admit(&objects[0]).unwrap(),
                Admission::Retained
            ));
            assert_eq!(plan.objects.len(), 1);
            assert_eq!(plan.bytes, objects[0].ciphertext_length());
            assert!(plan.admit(&objects[2]).is_err());
            plan.bytes = MAX_PLAN_BYTES;
            assert!(matches!(plan.admit(&objects[1]).unwrap(), Admission::Full));
            assert_eq!(plan.objects.len(), 1);
            plan.bytes = 0;
            plan.objects
                .resize_with(MAX_PLAN_OBJECTS, || objects[0].retained_copy());
            assert!(matches!(plan.admit(&objects[1]).unwrap(), Admission::Full));
            plan.disabled = true;
            assert!(matches!(
                plan.admit(&objects[1]).unwrap(),
                Admission::Disabled
            ));
        });
        assert!(current().is_none());
    }

    #[test]
    fn nested_scope_isolates_inventory_and_unwind_restores_it() {
        let result = std::panic::catch_unwind(|| {
            with_plan(|| {
                let outer = current().unwrap();
                with_plan(|| assert!(!Arc::ptr_eq(&outer, &current().unwrap())));
                assert!(Arc::ptr_eq(&outer, &current().unwrap()));
                panic!("synthetic preparation failure");
            })
        });
        assert!(result.is_err());
        assert!(current().is_none());
    }
}
