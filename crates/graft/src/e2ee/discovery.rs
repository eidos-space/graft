use super::{EncryptedHttpError, EncryptionError, hint, object, reference};
use serde::Deserialize;
use std::collections::HashSet;

/// Only public storage classes may appear in discovery requests.
#[derive(Clone, Copy)]
pub enum EncryptedListClass {
    Objects,
    Packs,
    Indexes,
    Files,
    Commits,
    Segments,
    Refs,
    Head,
    Hints,
}

impl EncryptedListClass {
    pub(super) fn prefix(self) -> &'static str {
        match self {
            Self::Objects => "e2ee/objects/",
            Self::Packs => "e2ee/packs/",
            Self::Indexes => "e2ee/indexes/",
            Self::Files => "e2ee/files/",
            Self::Commits => "e2ee/commits/",
            Self::Segments => "e2ee/segments/",
            Self::Refs => "e2ee/refs/",
            Self::Head => "e2ee/head/",
            Self::Hints => "e2ee/hints/",
        }
    }
    fn validate(self, key: &str) -> super::Result<()> {
        if !key.starts_with(self.prefix()) {
            return Err(EncryptionError::InvalidObjectKey);
        }
        match self {
            Self::Refs | Self::Head => reference::validate_remote_key(key),
            Self::Hints => hint::validate_remote_key(key),
            _ => object::opaque_class(key).map(|_| ()),
        }
    }
}

/// Local work budgets, independent of any server claim of complete discovery.
pub struct DiscoveryBudget {
    pub max_objects: usize,
    pub max_pages: usize,
}
impl Default for DiscoveryBudget {
    fn default() -> Self {
        Self { max_objects: 100000, max_pages: 4096 }
    }
}

pub(super) struct Listing {
    class: EncryptedListClass,
    budget: DiscoveryBudget,
    pages: usize,
    keys: Vec<String>,
    cursors: HashSet<String>,
    cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Page {
    paths: Vec<String>,
    next_cursor: Option<String>,
}

impl Listing {
    pub(super) fn new(class: EncryptedListClass, budget: DiscoveryBudget) -> super::Result<Self> {
        if budget.max_objects == 0
            || budget.max_objects > 100000
            || budget.max_pages == 0
            || budget.max_pages > 4096
        {
            return Err(EncryptionError::TooLarge);
        }
        Ok(Self {
            class,
            budget,
            pages: 0,
            keys: Vec::new(),
            cursors: HashSet::new(),
            cursor: None,
        })
    }
    pub(super) fn query(&self) -> super::Result<(&'static str, Option<&str>, usize)> {
        if self.pages >= self.budget.max_pages || self.keys.len() >= self.budget.max_objects {
            return Err(EncryptionError::TooLarge);
        }
        Ok((
            self.class.prefix(),
            self.cursor.as_deref(),
            (self.budget.max_objects - self.keys.len()).min(256),
        ))
    }
    pub(super) fn accept(&mut self, bytes: &[u8]) -> Result<bool, EncryptedHttpError> {
        let page: Page =
            serde_json::from_slice(bytes).map_err(|_| EncryptedHttpError::ProtocolMismatch)?;
        let limit = self.query()?.2;
        if page.paths.len() > limit || (page.paths.is_empty() && page.next_cursor.is_some()) {
            return Err(EncryptedHttpError::ProtocolMismatch);
        }
        for key in page.paths {
            self.class.validate(&key)?;
            if self.keys.last().is_some_and(|previous| previous >= &key) {
                return Err(EncryptedHttpError::ProtocolMismatch);
            }
            self.keys.push(key);
        }
        if let Some(cursor) = &page.next_cursor
            && (cursor.is_empty()
                || cursor.len() > 4096
                || cursor.chars().any(char::is_control)
                || !self.cursors.insert(cursor.clone()))
        {
            return Err(EncryptedHttpError::ProtocolMismatch);
        }
        self.pages += 1;
        self.cursor = page.next_cursor;
        Ok(self.cursor.is_none())
    }
    pub(super) fn finish(self) -> Vec<String> {
        self.keys
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn listing(objects: usize, pages: usize) -> Listing {
        Listing::new(
            EncryptedListClass::Indexes,
            DiscoveryBudget { max_objects: objects, max_pages: pages },
        )
        .unwrap()
    }
    fn key(n: u8) -> String {
        format!("e2ee/indexes/{n:064x}")
    }
    fn page(paths: Vec<String>, cursor: Option<&str>) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({"paths":paths,"next_cursor":cursor})).unwrap()
    }
    #[test]
    fn opaque_listing_rejects_duplicates_regressions_cross_class_and_plaintext_keys() {
        for paths in [
            vec![key(1), key(1)],
            vec![key(2), key(1)],
            vec![format!("e2ee/refs/{:064x}", 1)],
            vec!["objects/pack/private.idx".into()],
            vec!["e2ee/indexes/bad".into()],
        ] {
            assert!(listing(10, 3).accept(&page(paths, None)).is_err());
        }
        let mut value = listing(10, 3);
        assert!(!value.accept(&page(vec![key(1)], Some("first"))).unwrap());
        assert!(value.accept(&page(vec![key(1)], None)).is_err());
    }
    #[test]
    fn opaque_listing_rejects_cursor_cycles_empty_progress_and_budget_exhaustion() {
        for cursor in ["", "\n"] {
            assert!(
                listing(10, 3)
                    .accept(&page(vec![key(1)], Some(cursor)))
                    .is_err()
            );
        }
        assert!(listing(10, 3).accept(&page(vec![], Some("next"))).is_err());
        let mut value = listing(10, 3);
        value.accept(&page(vec![key(1)], Some("cycle"))).unwrap();
        assert!(value.accept(&page(vec![key(2)], Some("cycle"))).is_err());
        let mut value = listing(1, 3);
        value.accept(&page(vec![key(1)], Some("next"))).unwrap();
        assert!(value.query().is_err());
        let mut value = listing(10, 1);
        value.accept(&page(vec![key(1)], Some("next"))).unwrap();
        assert!(value.query().is_err());
    }
    #[test]
    fn opaque_listing_rejects_malformed_and_oversized_pages_and_accepts_terminal_empty() {
        for json in [
            br#"{"paths":[],"paths":[]}"#.as_slice(),
            br#"{"paths":[],"extra":1}"#.as_slice(),
            br#"{"paths":null}"#.as_slice(),
        ] {
            assert!(listing(10, 3).accept(json).is_err());
        }
        assert!(
            listing(1, 3)
                .accept(&page(vec![key(1), key(2)], None))
                .is_err()
        );
        let mut value = listing(10, 3);
        assert!(value.accept(&page(Vec::new(), None)).unwrap());
        assert!(value.finish().is_empty());
        assert!(
            Listing::new(
                EncryptedListClass::Refs,
                DiscoveryBudget { max_objects: 100001, max_pages: 1 }
            )
            .is_err()
        );
    }
}
