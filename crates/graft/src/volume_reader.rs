use std::{
    borrow::Cow,
    collections::BTreeSet,
    sync::{Arc, OnceLock},
};

use crate::core::{PageCount, PageIdx, SegmentId, VolumeId, page::Page};

use crate::{GraftErr, rt::runtime::Runtime, snapshot::Snapshot, volume_writer::VolumeWriter};

pub(crate) type PageOriginManifest = Vec<Option<SegmentId>>;

/// A type which can read from a Volume
pub trait VolumeRead {
    fn snapshot(&self) -> &Snapshot;
    fn page_count(&self) -> PageCount;
    fn read_page(&self, pageidx: PageIdx) -> Result<Page, GraftErr>;
}

#[derive(Debug, Clone)]
pub struct VolumeReader {
    runtime: Runtime,
    vid: VolumeId,
    snapshot: Snapshot,
    page_origins: Arc<OnceLock<PageOriginManifest>>,
}

impl VolumeReader {
    pub(crate) fn new(runtime: Runtime, vid: VolumeId, snapshot: Snapshot) -> Self {
        Self {
            runtime,
            vid,
            snapshot,
            page_origins: Arc::new(OnceLock::new()),
        }
    }

    fn page_origins(&self) -> Result<&PageOriginManifest, GraftErr> {
        if self.page_origins.get().is_none() {
            let origins = self.runtime.snapshot_page_origins(&self.snapshot)?;
            let _ = self.page_origins.set(origins);
        }
        Ok(self
            .page_origins
            .get()
            .expect("page origin manifest initialized"))
    }

    /// Returns pages whose immutable storage origin differs between two snapshots.
    ///
    /// The manifest is dense and cached by each reader. Reusing one Base reader for both sides of
    /// a three-way merge therefore constructs Base origins once instead of rebuilding a balanced
    /// tree for every candidate and row-diff pass.
    pub fn changed_page_candidates(&self, other: &Self) -> Result<BTreeSet<u32>, GraftErr> {
        let from = self.page_origins()?;
        let to = other.page_origins()?;
        let page_count = from.len().max(to.len());
        Ok((0..page_count)
            .filter(|&index| from.get(index) != to.get(index))
            .map(|index| index as u32 + 1)
            .collect())
    }
}

impl From<VolumeReader> for VolumeWriter {
    fn from(reader: VolumeReader) -> Self {
        Self::new(reader.runtime, reader.vid, reader.snapshot)
    }
}

impl VolumeRead for VolumeReader {
    fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }

    fn page_count(&self) -> PageCount {
        self.snapshot.page_count
    }

    fn read_page(&self, pageidx: PageIdx) -> Result<Page, GraftErr> {
        if !self.snapshot.page_count.contains(pageidx) {
            return Ok(Page::EMPTY);
        }
        let Some(segment) = self
            .page_origins()?
            .get(pageidx.to_u32() as usize - 1)
            .and_then(Option::as_ref)
        else {
            return Ok(Page::EMPTY);
        };
        self.runtime
            .read_page_from_origin(&self.snapshot, pageidx, segment)
    }
}

pub enum VolumeReadRef<'a> {
    Reader(Cow<'a, VolumeReader>),
    Writer(&'a VolumeWriter),
}

impl VolumeRead for VolumeReadRef<'_> {
    fn snapshot(&self) -> &Snapshot {
        match self {
            VolumeReadRef::Reader(r) => r.snapshot(),
            VolumeReadRef::Writer(w) => w.snapshot(),
        }
    }

    fn page_count(&self) -> PageCount {
        match self {
            VolumeReadRef::Reader(r) => r.page_count(),
            VolumeReadRef::Writer(w) => w.page_count(),
        }
    }

    fn read_page(&self, pageidx: PageIdx) -> Result<Page, GraftErr> {
        match self {
            VolumeReadRef::Reader(r) => r.read_page(pageidx),
            VolumeReadRef::Writer(w) => w.read_page(pageidx),
        }
    }
}
