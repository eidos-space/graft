use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use super::{
    CommitFileState,
    object::{ObjectId, TreeEntryMode},
};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Index {
    #[serde(default)]
    pub entries: Vec<IndexEntry>,
}

impl Index {
    pub fn stage(&mut self, entry: IndexEntry) {
        if entry.stage == IndexStage::Normal {
            self.entries.retain(|existing| existing.path != entry.path);
        } else {
            self.entries
                .retain(|existing| !(existing.path == entry.path && existing.stage == entry.stage));
        }
        self.entries.push(entry);
        self.entries.sort_by(|a, b| {
            a.path
                .cmp(&b.path)
                .then_with(|| u8::from(a.stage).cmp(&u8::from(b.stage)))
        });
    }

    pub fn remove_path(&mut self, path: &str) {
        self.entries.retain(|entry| entry.path != path);
    }

    pub fn stage0_entries(&self) -> impl Iterator<Item = &IndexEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.stage == IndexStage::Normal)
    }

    pub fn has_staged_changes(&self) -> bool {
        self.stage0_entries().next().is_some()
    }

    pub fn has_conflicts(&self) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.stage != IndexStage::Normal)
    }

    pub fn staged_paths(&self) -> Vec<String> {
        self.stage0_entries()
            .map(|entry| entry.path.clone())
            .collect()
    }

    pub fn conflicted_paths(&self) -> Vec<String> {
        let mut paths = BTreeSet::new();
        for entry in &self.entries {
            if entry.stage != IndexStage::Normal {
                paths.insert(entry.path.clone());
            }
        }
        paths.into_iter().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexEntry {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<TreeEntryMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oid: Option<ObjectId>,
    pub stage: IndexStage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<CommitFileState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexStage {
    Normal,
    Base,
    Ours,
    Theirs,
}

impl From<IndexStage> for u8 {
    fn from(value: IndexStage) -> Self {
        match value {
            IndexStage::Normal => 0,
            IndexStage::Base => 1,
            IndexStage::Ours => 2,
            IndexStage::Theirs => 3,
        }
    }
}
