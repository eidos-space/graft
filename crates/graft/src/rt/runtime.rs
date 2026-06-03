use std::{ops::RangeInclusive, sync::Arc, time::Duration};

use crate::core::{
    LogId, PageCount, PageIdx, SegmentId, VolumeId, checksum::Checksum, commit::Commit,
    logref::LogRef, lsn::{LSN, LSNRangeExt}, page::Page, pageset::PageSet,
};
use bytestring::ByteString;
use tracing::Instrument;
use tryiter::TryIteratorExt;

use crate::{
    GraftErr, LogicalErr,
    remote::Remote,
    rt::{
        action::{Action, FetchLog, FetchSegment, HydrateSnapshot, RemoteCommit},
        task::{autosync::AutosyncTask, supervise},
    },
    snapshot::Snapshot,
    volume::{Volume, VolumeStatus},
    volume_reader::VolumeReader,
    volume_writer::VolumeWriter,
};

use crate::local::fjall_storage::FjallStorage;

pub type Result<T> = std::result::Result<T, GraftErr>;

/// Commit metadata summary
#[derive(Debug, Clone)]
pub struct CommitInfo {
    pub lsn: LSN,
    pub page_count: PageCount,
    pub segment_id: Option<SegmentId>,
    pub is_checkpoint: bool,
    /// Number of pages changed in this commit
    pub changed_pages: usize,
}

/// Page-level diff result
#[derive(Debug, Clone)]
pub struct DiffResult {
    pub from_lsn: LSN,
    pub to_lsn: LSN,
    pub added_or_modified_pages: PageSet,
    pub page_count_delta: i64,
}

#[derive(Clone, Debug)]
pub struct Runtime {
    inner: Arc<RuntimeInner>,
}

#[derive(Debug)]
struct RuntimeInner {
    tokio: tokio::runtime::Handle,
    storage: Arc<FjallStorage>,
    remote: Arc<Remote>,
}

impl Runtime {
    /// Create a Graft `Runtime` wrapping the provided Tokio runtime handle.
    pub fn new(
        tokio_rt: tokio::runtime::Handle,
        remote: Arc<Remote>,
        storage: Arc<FjallStorage>,
        autosync: Option<Duration>,
    ) -> Runtime {
        // spin up background tasks as needed
        if let Some(interval) = autosync {
            let _guard = tokio_rt.enter();
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            tokio_rt.spawn(supervise(
                storage.clone(),
                remote.clone(),
                AutosyncTask::new(ticker),
            ));
        }
        Runtime {
            inner: Arc::new(RuntimeInner { tokio: tokio_rt, storage, remote }),
        }
    }

    pub(crate) fn storage(&self) -> &FjallStorage {
        &self.inner.storage
    }

    pub(crate) fn read_page(&self, snapshot: &Snapshot, pageidx: PageIdx) -> Result<Page> {
        let reader = self.storage().read();
        if let Some(commit) = reader.search_page(snapshot, pageidx)? {
            let idx = commit
                .segment_idx()
                .expect("BUG: commit claims to contain pageidx");

            if let Some(page) = reader.read_page(idx.sid().clone(), pageidx)? {
                return Ok(page);
            }

            // fallthrough to loading the page from the remote
            let range = idx
                .frame_for_pageidx(pageidx)
                .expect("BUG: no frame for pageidx");

            // fetch the segment frame containing the page
            self.run_action(FetchSegment { range })?;

            // now that we've fetched the segment, read the page again using a
            // fresh storage reader
            Ok(self
                .storage()
                .read()
                .read_page(idx.sid.clone(), pageidx)?
                .expect("BUG: page not found after fetching"))
        } else {
            Ok(Page::EMPTY)
        }
    }

    fn run_action<A: Action>(&self, action: A) -> Result<()> {
        let span = tracing::debug_span!("Action::run", ?action);

        self.inner.tokio.block_on(
            action
                .run(self.inner.storage.clone(), self.inner.remote.clone())
                .instrument(span),
        )
    }
}

// tag methods
impl Runtime {
    pub fn tag_iter(&self) -> impl Iterator<Item = Result<(ByteString, VolumeId)>> {
        self.storage().read().iter_tags().map_err(GraftErr::from)
    }

    pub fn tag_exists(&self, name: &str) -> Result<bool> {
        Ok(self.storage().read().tag_exists(name)?)
    }

    pub fn tag_get(&self, tag: &str) -> Result<Option<VolumeId>> {
        Ok(self.storage().read().get_tag(tag)?)
    }

    /// retrieves the `VolumeId` for a tag, replacing it with the provided `VolumeId`
    pub fn tag_replace(&self, tag: &str, vid: VolumeId) -> Result<Option<VolumeId>> {
        Ok(self.storage().read_write().tag_replace(tag, vid)?)
    }

    pub fn tag_delete(&self, tag: &str) -> Result<()> {
        Ok(self.storage().tag_delete(tag)?)
    }
}

// volume methods
impl Runtime {
    pub fn volume_iter(&self) -> impl Iterator<Item = Result<Volume>> {
        self.storage().read().iter_volumes().map_err(GraftErr::from)
    }

    pub fn volume_exists(&self, vid: &VolumeId) -> Result<bool> {
        Ok(self.storage().read().volume_exists(vid)?)
    }

    /// opens a volume. if any id is missing, it will be randomly
    /// generated. If the volume already exists, this function will fail if its
    /// remote Log doesn't match.
    pub fn volume_open(
        &self,
        vid: Option<VolumeId>,
        local: Option<LogId>,
        remote: Option<LogId>,
    ) -> Result<Volume> {
        Ok(self
            .storage()
            .read_write()
            .volume_open(vid, local, remote)?)
    }

    /// creates a new volume by forking an existing logref
    pub fn volume_from_logref(&self, logref: LogRef) -> Result<Option<Volume>> {
        Ok(self.storage().volume_from_logref(logref)?)
    }

    /// creates a new volume by forking an existing snapshot
    pub fn volume_from_snapshot(&self, snapshot: &Snapshot) -> Result<Volume> {
        Ok(self.storage().volume_from_snapshot(snapshot)?)
    }

    /// retrieves an existing volume. returns `LogicalErr::VolumeNotFound` if missing
    pub fn volume_get(&self, vid: &VolumeId) -> Result<Volume> {
        Ok(self.storage().read().volume(vid)?)
    }

    /// removes a volume but leaves the underlying logs in place
    pub fn volume_delete(&self, vid: &VolumeId) -> Result<()> {
        Ok(self.storage().volume_delete(vid)?)
    }

    /// fetches the latest changes to the remote and then pulls them into the volume
    pub fn volume_pull(&self, vid: VolumeId) -> Result<()> {
        let volume = self.inner.storage.read().volume(&vid)?;
        self.fetch_log(volume.remote, None)?;
        if volume.pending_commit.is_some() {
            self.storage().read_write().recover_pending_commit(&vid)?;
        }
        Ok(self
            .storage()
            .read_write()
            .sync_remote_to_local(volume.vid)?)
    }

    pub fn volume_push(&self, vid: VolumeId) -> Result<()> {
        self.run_action(RemoteCommit { vid })
    }

    pub fn volume_status(&self, vid: &VolumeId) -> Result<VolumeStatus> {
        let reader = self.storage().read();
        let volume = reader.volume(vid)?;
        let latest_local = reader.latest_lsn(&volume.local)?;
        let latest_remote = reader.latest_lsn(&volume.remote)?;
        Ok(volume.status(latest_local, latest_remote))
    }

    pub fn volume_snapshot(&self, vid: &VolumeId) -> Result<Snapshot> {
        Ok(self.storage().read().snapshot(vid)?)
    }

    pub fn volume_reader(&self, vid: VolumeId) -> Result<VolumeReader> {
        let snapshot = self.volume_snapshot(&vid)?;
        Ok(VolumeReader::new(self.clone(), vid, snapshot))
    }

    pub fn volume_writer(&self, vid: VolumeId) -> Result<VolumeWriter> {
        let snapshot = self.volume_snapshot(&vid)?;
        Ok(VolumeWriter::new(self.clone(), vid, snapshot))
    }
}

// log methods
impl Runtime {
    pub fn fetch_log(&self, log: LogId, max_lsn: Option<LSN>) -> Result<()> {
        self.run_action(FetchLog { log, max_lsn })
    }

    pub fn get_commit(&self, log: &LogId, lsn: LSN) -> Result<Option<Commit>> {
        Ok(self.storage().read().get_commit(log, lsn)?)
    }
}

// snapshot methods
impl Runtime {
    /// returns the total number of pages in the snapshot
    pub fn snapshot_pages(&self, snapshot: &Snapshot) -> Result<PageCount> {
        if let Some((log, lsn)) = snapshot.head() {
            Ok(self
                .storage()
                .read()
                .page_count(log, lsn)?
                .expect("BUG: missing head commit for snapshot"))
        } else {
            Ok(PageCount::ZERO)
        }
    }

    pub fn snapshot_is_latest(&self, vid: &VolumeId, snapshot: &Snapshot) -> Result<bool> {
        Ok(self.storage().read().is_latest_snapshot(vid, snapshot)?)
    }

    /// returns the checksum of the snapshot
    pub fn snapshot_checksum(&self, snapshot: &Snapshot) -> Result<Checksum> {
        Ok(self.storage().read().checksum(snapshot)?)
    }

    pub fn snapshot_missing_pages(&self, snapshot: &Snapshot) -> Result<PageSet> {
        let missing_frames = self.storage().read().find_missing_frames(snapshot)?;
        // merge missing_frames into a single PageSet
        Ok(missing_frames
            .into_iter()
            .fold(PageSet::EMPTY, |mut pageset, frame| {
                pageset |= frame.pageset;
                pageset
            }))
    }

    pub fn snapshot_hydrate(&self, snapshot: Snapshot) -> Result<()> {
        self.run_action(HydrateSnapshot { snapshot })
    }
}

// Version control methods (Git-like operations)
impl Runtime {
    /// Get the commit history for a Volume (similar to git log).
    /// Returns commits from newest to oldest.
    pub fn volume_log(&self, vid: &VolumeId) -> Result<Vec<CommitInfo>> {
        let reader = self.storage().read();
        let volume = reader.volume(vid)?;

        let mut commits = Vec::new();
        let latest = reader.latest_lsn(&volume.local)?;

        if let Some(head) = latest {
            for lsn in (LSN::FIRST.to_u64()..=head.to_u64()).rev() {
                let lsn = LSN::new(lsn);
                if let Some(commit) = reader.get_commit(&volume.local, lsn)? {
                    commits.push(CommitInfo {
                        lsn,
                        page_count: commit.page_count,
                        segment_id: commit.segment_id().cloned(),
                        is_checkpoint: commit.is_checkpoint(),
                        changed_pages: commit
                            .segment_idx()
                            .map_or(0, |idx| idx.pageset.cardinality().to_usize()),
                    });
                }
            }
        }

        Ok(commits)
    }

    /// Checkout to a specific historical commit (similar to git checkout <commit>).
    ///
    /// Creates a new Volume whose Local Log contains all history up to the
    /// target commit. The original Volume is left unchanged (safe operation).
    ///
    /// Returns the new Volume
    pub fn volume_checkout(&self, vid: &VolumeId, target_lsn: LSN) -> Result<Volume> {
        let reader = self.storage().read();
        let volume = reader.volume(vid)?;

        // Verify target LSN exists
        let target_commit = reader
            .get_commit(&volume.local, target_lsn)?
            .ok_or_else(|| LogicalErr::VolumeNotFound(vid.clone()))?;

        // Create LogRef pointing to the historical commit
        let logref = LogRef::new(volume.local, target_lsn);

        // Create new Volume from this historical point (copies commits to new Log)
        drop(reader); // release read lock
        let new_volume = self
            .storage()
            .volume_from_logref(logref)?
            .ok_or_else(|| LogicalErr::VolumeNotFound(vid.clone()))?;

        tracing::debug!(
            "volume_checkout: {} LSN {} (page_count={}) -> new Volume {} (local_log={})",
            vid,
            target_lsn,
            target_commit.page_count,
            new_volume.vid,
            new_volume.local
        );

        Ok(new_volume)
    }

    /// Compare differences between two commits (similar to git diff).
    /// Returns which pages changed.
    pub fn diff_commits(
        &self,
        log: &LogId,
        from_lsn: LSN,
        to_lsn: LSN,
    ) -> Result<DiffResult> {
        let reader = self.storage().read();

        let from_commit = reader
            .get_commit(log, from_lsn)?
            .ok_or(LogicalErr::VolumeNotFound(VolumeId::EMPTY))?;
        let to_commit = reader
            .get_commit(log, to_lsn)?
            .ok_or(LogicalErr::VolumeNotFound(VolumeId::EMPTY))?;

        // Collect all changed pages between from..to
        let mut changed_pages = PageSet::EMPTY;

        // Ensure from < to
        let range: RangeInclusive<LSN> = if from_lsn < to_lsn {
            from_lsn.next()..=to_lsn
        } else {
            to_lsn.next()..=from_lsn
        };

        // Iterate over all commits in the range
        for lsn in range.iter() {
            if let Some(commit) = reader.get_commit(log, lsn)?
                && let Some(idx) = commit.segment_idx() {
                    changed_pages |= idx.pageset.clone();
                }
        }

        let page_count_delta = if to_lsn > from_lsn {
            to_commit.page_count.to_u32() as i64 - from_commit.page_count.to_u32() as i64
        } else {
            from_commit.page_count.to_u32() as i64 - to_commit.page_count.to_u32() as i64
        };

        Ok(DiffResult {
            from_lsn,
            to_lsn,
            added_or_modified_pages: changed_pages,
            page_count_delta,
        })
    }

    /// Soft reset: point the current tag at a historical version (dangerous operation).
    ///
    /// Creates a new Volume from the specified LSN, then updates the tag to
    /// point to it. The original volume data is preserved (until deleted).
    pub fn volume_reset_to(&self, tag: &str, target_lsn: LSN) -> Result<Volume> {
        // Get current Volume
        let current_vid = self
            .tag_get(tag)?
            .ok_or(LogicalErr::VolumeNotFound(VolumeId::EMPTY))?;

        // Checkout to historical version
        let new_volume = self.volume_checkout(&current_vid, target_lsn)?;

        // Update tag to point to the new Volume
        self.tag_replace(tag, new_volume.vid.clone())?;

        tracing::info!(
            "volume_reset_to: tag '{}' now points to Volume {} (LSN {})",
            tag,
            new_volume.vid,
            target_lsn
        );

        Ok(new_volume)
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use crate::core::{LogId, PageIdx, lsn::LSN, page::Page};
    use test_log::test;
    use tokio::time::sleep;

    use crate::{
        local::fjall_storage::FjallStorage, remote::RemoteConfig, rt::runtime::Runtime,
        volume_reader::VolumeRead, volume_writer::VolumeWrite,
    };

    #[test]
    fn runtime_sanity() {
        let tokio_rt = tokio::runtime::Builder::new_current_thread()
            .start_paused(true)
            .enable_all()
            .build()
            .unwrap();

        let remote = Arc::new(RemoteConfig::Memory.build().unwrap());
        let storage = Arc::new(FjallStorage::open_temporary().unwrap());
        let runtime = Runtime::new(
            tokio_rt.handle().clone(),
            remote.clone(),
            storage,
            Some(Duration::from_secs(1)),
        );

        let remote_log = LogId::random();
        let vid = runtime
            .volume_open(None, None, Some(remote_log.clone()))
            .unwrap()
            .vid;

        assert_eq!(runtime.volume_status(&vid).unwrap().to_string(), "_ r_",);

        // sanity check volume writer semantics
        let mut writer = runtime.volume_writer(vid.clone()).unwrap();
        for i in [1u8, 2, 5, 9] {
            let pageidx = PageIdx::must_new(i as u32);
            let page = Page::test_filled(i);
            writer.write_page(pageidx, page.clone()).unwrap();
            assert_eq!(writer.read_page(pageidx).unwrap(), page);
        }
        writer.commit().unwrap();

        assert_eq!(runtime.volume_status(&vid).unwrap().to_string(), "+1 r_",);

        // sanity check volume reader semantics
        let reader = runtime.volume_reader(vid.clone()).unwrap();
        tracing::info!("got snapshot {:?}", reader.snapshot());
        for i in [1u8, 2, 5, 9] {
            let pageidx = PageIdx::must_new(i as u32);
            let page = Page::test_filled(i);
            assert!(
                reader.read_page(pageidx).unwrap().into_bytes() == page.into_bytes(),
                "pages aren't equal"
            );
        }

        // create a second runtime connected to the same remote
        let storage = Arc::new(FjallStorage::open_temporary().unwrap());
        let runtime_2 = Runtime::new(
            tokio_rt.handle().clone(),
            remote.clone(),
            storage,
            Some(Duration::from_secs(1)),
        );

        // open the same remote log in the second runtime
        let vid_2 = runtime_2
            .volume_open(None, None, Some(remote_log))
            .unwrap()
            .vid;

        // let both runtimes run for a little while
        tokio_rt.block_on(async {
            // this sleep lets tokio advance time, allowing the runtime to flush all its jobs
            sleep(Duration::from_secs(5)).await;
            let tree = remote.testonly_format_tree().await;
            tracing::info!("remote tree\n{tree}")
        });

        assert_eq!(runtime.volume_status(&vid).unwrap().to_string(), "1 r1",);
        assert_eq!(runtime_2.volume_status(&vid_2).unwrap().to_string(), "_ r1",);

        // sanity check volume reader semantics in the second runtime
        let reader_2 = runtime_2.volume_reader(vid_2.clone()).unwrap();
        let task = tokio_rt.spawn_blocking(move || {
            for i in [1u8, 2, 5, 9] {
                let pageidx = PageIdx::must_new(i as u32);
                tracing::info!("checking page {pageidx}");
                let expected = Page::test_filled(i);
                let actual = reader_2.read_page(pageidx).unwrap();
                assert_eq!(expected, actual, "read unexpected page contents");
            }
        });
        tokio_rt.block_on(task).unwrap();

        // now write to the second volume, and sync back to the first
        let mut writer_2 = runtime_2.volume_writer(vid_2.clone()).unwrap();
        for i in [3u8, 4, 5, 7] {
            let pageidx = PageIdx::must_new(i as u32);
            let page = Page::test_filled(i + 10);
            writer_2.write_page(pageidx, page.clone()).unwrap();
            assert_eq!(writer_2.read_page(pageidx).unwrap(), page);
        }
        writer_2.commit().unwrap();

        // let both runtimes run for a little while
        tokio_rt.block_on(async {
            // this sleep lets tokio advance time, allowing the runtime to flush all its jobs
            sleep(Duration::from_secs(5)).await;
            let tree = remote.testonly_format_tree().await;
            tracing::info!("remote tree\n{tree}")
        });

        assert_eq!(runtime.volume_status(&vid).unwrap().to_string(), "1 r2",);
        assert_eq!(runtime_2.volume_status(&vid_2).unwrap().to_string(), "1 r2",);

        // sanity check volume reader semantics in the first runtime
        let reader = runtime.volume_reader(vid.clone()).unwrap();
        let task = tokio_rt.spawn_blocking(move || {
            for i in [3u8, 4, 5, 7] {
                let pageidx = PageIdx::must_new(i as u32);
                tracing::info!("checking page {pageidx}");
                let expected = Page::test_filled(i + 10);
                let actual = reader.read_page(pageidx).unwrap();
                assert_eq!(expected, actual, "read unexpected page contents");
            }
        });
        tokio_rt.block_on(task).unwrap();
    }

    #[test]
    fn checkout_performance() {
        use crate::lsn;
        use std::time::Instant;

        let tokio_rt = tokio::runtime::Builder::new_current_thread()
            .start_paused(true)
            .enable_all()
            .build()
            .unwrap();

        let remote = Arc::new(RemoteConfig::Memory.build().unwrap());
        let storage = Arc::new(FjallStorage::open_temporary().unwrap());
        let runtime = Runtime::new(
            tokio_rt.handle().clone(),
            remote.clone(),
            storage,
            None,
        );

        // Create Volume and write multiple commits
        let volume = runtime.volume_open(None, None, None).unwrap();
        let vid = volume.vid;

        // Create 100 commits
        for i in 1..=100 {
            let mut writer = runtime.volume_writer(vid.clone()).unwrap();
            let pageidx = PageIdx::must_new(i);
            let page = Page::test_filled(i as u8);
            writer.write_page(pageidx, page).unwrap();
            writer.commit().unwrap();
        }

        // Verify 100 commits
        let log = runtime.volume_log(&vid).unwrap();
        assert_eq!(log.len(), 100);
        println!("Created {} commits", log.len());

        // Test checkout performance (checkout to LSN 50)
        let start = Instant::now();
        let new_volume = runtime.volume_checkout(&vid, lsn!(50)).unwrap();
        let elapsed = start.elapsed();

        println!("volume_checkout to LSN 50:");
        println!("  Time: {:?}", elapsed);
        println!("  Original Volume: {} ({} commits)", vid, log.len());
        println!(
            "  New Volume: {} ({} commits)",
            new_volume.vid,
            runtime.volume_log(&new_volume.vid).unwrap().len()
        );

        // Verify new Volume has 50 commits
        let new_log = runtime.volume_log(&new_volume.vid).unwrap();
        assert_eq!(new_log.len(), 50);

        // Verify original Volume still exists and unchanged
        let old_log = runtime.volume_log(&vid).unwrap();
        assert_eq!(old_log.len(), 100);

        // Page data sharing verification: new Volume should read pages
        let reader = runtime.volume_reader(new_volume.vid).unwrap();
        let page = reader.read_page(PageIdx::must_new(1)).unwrap();
        assert_eq!(page, Page::test_filled(1));

        println!("✓ Checkout successful - pages are accessible");
        println!("✓ Original volume unchanged");
        println!("✓ New volume shares page data (no duplication)");
    }

    #[test]
    fn volume_log_empty_volume() {
        let tokio_rt = tokio::runtime::Builder::new_current_thread()
            .start_paused(true)
            .enable_all()
            .build()
            .unwrap();

        let remote = Arc::new(RemoteConfig::Memory.build().unwrap());
        let storage = Arc::new(FjallStorage::open_temporary().unwrap());
        let runtime = Runtime::new(
            tokio_rt.handle().clone(),
            remote.clone(),
            storage,
            None,
        );

        let volume = runtime.volume_open(None, None, None).unwrap();
        let log = runtime.volume_log(&volume.vid).unwrap();
        assert!(log.is_empty(), "New volume should have no commits");
    }

    #[test]
    fn volume_log_multiple_commits() {
        let tokio_rt = tokio::runtime::Builder::new_current_thread()
            .start_paused(true)
            .enable_all()
            .build()
            .unwrap();

        let remote = Arc::new(RemoteConfig::Memory.build().unwrap());
        let storage = Arc::new(FjallStorage::open_temporary().unwrap());
        let runtime = Runtime::new(
            tokio_rt.handle().clone(),
            remote.clone(),
            storage,
            None,
        );

        let volume = runtime.volume_open(None, None, None).unwrap();
        let vid = volume.vid;

        // Write 5 commits, each with different pages
        for i in 1..=5 {
            let mut writer = runtime.volume_writer(vid.clone()).unwrap();
            let pageidx = PageIdx::must_new(i);
            let page = Page::test_filled(i as u8);
            writer.write_page(pageidx, page).unwrap();
            writer.commit().unwrap();
        }

        let log = runtime.volume_log(&vid).unwrap();
        assert_eq!(log.len(), 5, "Should have 5 commits");

        // Commits should be in reverse order (newest first)
        assert_eq!(log[0].lsn.to_u64(), 5);
        assert_eq!(log[1].lsn.to_u64(), 4);
        assert_eq!(log[4].lsn.to_u64(), 1);

        // Page counts should increase
        assert_eq!(log[0].page_count.to_u32(), 5);
        assert_eq!(log[4].page_count.to_u32(), 1);
    }

    #[test]
    fn volume_checkout_can_read_historical_pages() {
        let tokio_rt = tokio::runtime::Builder::new_current_thread()
            .start_paused(true)
            .enable_all()
            .build()
            .unwrap();

        let remote = Arc::new(RemoteConfig::Memory.build().unwrap());
        let storage = Arc::new(FjallStorage::open_temporary().unwrap());
        let runtime = Runtime::new(
            tokio_rt.handle().clone(),
            remote.clone(),
            storage,
            None,
        );

        let volume = runtime.volume_open(None, None, None).unwrap();
        let vid = volume.vid;

        // Write 3 commits to create history
        for i in 1..=3 {
            let mut writer = runtime.volume_writer(vid.clone()).unwrap();
            let pageidx = PageIdx::must_new(i);
            let page = Page::test_filled(i as u8);
            writer.write_page(pageidx, page).unwrap();
            writer.commit().unwrap();
        }

        // Checkout to LSN 2 - should have only 2 pages in the snapshot
        let new_vol = runtime.volume_checkout(&vid, crate::lsn!(2)).unwrap();
        let reader = runtime.volume_reader(new_vol.vid.clone()).unwrap();

        // Page from commit 1 should be accessible
        let page1 = reader.read_page(PageIdx::must_new(1)).unwrap();
        assert_eq!(page1, Page::test_filled(1));

        // Snapshot page count should be 2 (commits 1 and 2 each wrote 1 page)
        assert_eq!(reader.page_count().to_usize(), 2);

        // New volume log should have exactly 2 commits
        let new_log = runtime.volume_log(&new_vol.vid).unwrap();
        assert_eq!(new_log.len(), 2);

        // Cleanup
        runtime.volume_delete(&new_vol.vid).unwrap();
    }

    #[test]
    fn volume_checkout_middle_of_history() {
        let tokio_rt = tokio::runtime::Builder::new_current_thread()
            .start_paused(true)
            .enable_all()
            .build()
            .unwrap();

        let remote = Arc::new(RemoteConfig::Memory.build().unwrap());
        let storage = Arc::new(FjallStorage::open_temporary().unwrap());
        let runtime = Runtime::new(
            tokio_rt.handle().clone(),
            remote.clone(),
            storage,
            None,
        );

        let volume = runtime.volume_open(None, None, None).unwrap();
        let vid = volume.vid;

        // Write 10 commits
        for i in 1..=10 {
            let mut writer = runtime.volume_writer(vid.clone()).unwrap();
            let pageidx = PageIdx::must_new(i);
            let page = Page::test_filled(i as u8);
            writer.write_page(pageidx, page).unwrap();
            writer.commit().unwrap();
        }

        // Checkout to middle (LSN 5)
        let mid_vol = runtime.volume_checkout(&vid, crate::lsn!(5)).unwrap();
        let mid_log = runtime.volume_log(&mid_vol.vid).unwrap();

        assert_eq!(mid_log.len(), 5);
        assert_eq!(mid_log[0].lsn.to_u64(), 5);
        assert_eq!(mid_log[4].lsn.to_u64(), 1);

        // Original volume still has all 10 commits
        let orig_log = runtime.volume_log(&vid).unwrap();
        assert_eq!(orig_log.len(), 10);

        runtime.volume_delete(&mid_vol.vid).unwrap();
    }

    #[test]
    fn volume_reset_to_basic() {
        let tokio_rt = tokio::runtime::Builder::new_current_thread()
            .start_paused(true)
            .enable_all()
            .build()
            .unwrap();

        let remote = Arc::new(RemoteConfig::Memory.build().unwrap());
        let storage = Arc::new(FjallStorage::open_temporary().unwrap());
        let runtime = Runtime::new(
            tokio_rt.handle().clone(),
            remote.clone(),
            storage,
            None,
        );

        let volume = runtime.volume_open(None, None, None).unwrap();
        let vid = volume.vid;

        // Write 5 commits
        for i in 1..=5 {
            let mut writer = runtime.volume_writer(vid.clone()).unwrap();
            writer
                .write_page(PageIdx::must_new(i), Page::test_filled(i as u8))
                .unwrap();
            writer.commit().unwrap();
        }

        // Set tag
        runtime.tag_replace("test-reset", vid.clone()).unwrap();

        // Reset to LSN 3
        let new_vol = runtime.volume_reset_to("test-reset", crate::lsn!(3)).unwrap();

        let new_log = runtime.volume_log(&new_vol.vid).unwrap();
        assert_eq!(new_log.len(), 3);
        assert_eq!(new_log[0].lsn.to_u64(), 3);

        // Tag now points to new volume
        let tagged_vid = runtime.tag_get("test-reset").unwrap().unwrap();
        assert_eq!(tagged_vid, new_vol.vid);

        // Old volume still exists
        let old_log = runtime.volume_log(&vid).unwrap();
        assert_eq!(old_log.len(), 5);

        // Cleanup
        runtime.volume_delete(&new_vol.vid).unwrap();
        runtime.volume_delete(&vid).unwrap();
    }

    #[test]
    fn diff_commits_basic() {
        let tokio_rt = tokio::runtime::Builder::new_current_thread()
            .start_paused(true)
            .enable_all()
            .build()
            .unwrap();

        let remote = Arc::new(RemoteConfig::Memory.build().unwrap());
        let storage = Arc::new(FjallStorage::open_temporary().unwrap());
        let runtime = Runtime::new(
            tokio_rt.handle().clone(),
            remote.clone(),
            storage,
            None,
        );

        let volume = runtime.volume_open(None, None, None).unwrap();
        let vid = volume.vid;
        let local = volume.local.clone();

        // Write 3 commits, each writing one page
        for i in 1..=3 {
            let mut writer = runtime.volume_writer(vid.clone()).unwrap();
            writer
                .write_page(PageIdx::must_new(i), Page::test_filled(i as u8))
                .unwrap();
            writer.commit().unwrap();
        }

        // Diff from LSN 1 to LSN 3
        let diff = runtime
            .diff_commits(&local, crate::lsn!(1), crate::lsn!(3))
            .unwrap();

        assert_eq!(diff.from_lsn, crate::lsn!(1));
        assert_eq!(diff.to_lsn, crate::lsn!(3));
        assert_eq!(diff.page_count_delta, 2); // 3 pages - 1 page
        // Pages 2 and 3 were added/modified between LSN 1 and 3
        assert!(diff.added_or_modified_pages.cardinality().to_usize() >= 2);
    }

    #[test]
    fn diff_commits_no_changes() {
        let tokio_rt = tokio::runtime::Builder::new_current_thread()
            .start_paused(true)
            .enable_all()
            .build()
            .unwrap();

        let remote = Arc::new(RemoteConfig::Memory.build().unwrap());
        let storage = Arc::new(FjallStorage::open_temporary().unwrap());
        let runtime = Runtime::new(
            tokio_rt.handle().clone(),
            remote.clone(),
            storage,
            None,
        );

        let volume = runtime.volume_open(None, None, None).unwrap();
        let vid = volume.vid;
        let local = volume.local.clone();

        // Write 1 commit
        let mut writer = runtime.volume_writer(vid.clone()).unwrap();
        writer
            .write_page(PageIdx::must_new(1), Page::test_filled(1))
            .unwrap();
        writer.commit().unwrap();

        // Diff from LSN 1 to LSN 1 (same commit) - no pages changed
        let diff = runtime
            .diff_commits(&local, crate::lsn!(1), crate::lsn!(1))
            .unwrap();
        // When from==to, range is empty, so no changed pages
        assert_eq!(diff.added_or_modified_pages.cardinality().to_usize(), 0);
    }

    #[test]
    fn commit_info_fields() {
        let tokio_rt = tokio::runtime::Builder::new_current_thread()
            .start_paused(true)
            .enable_all()
            .build()
            .unwrap();

        let remote = Arc::new(RemoteConfig::Memory.build().unwrap());
        let storage = Arc::new(FjallStorage::open_temporary().unwrap());
        let runtime = Runtime::new(
            tokio_rt.handle().clone(),
            remote.clone(),
            storage,
            None,
        );

        let volume = runtime.volume_open(None, None, None).unwrap();
        let vid = volume.vid;

        // Write some pages
        let mut writer = runtime.volume_writer(vid.clone()).unwrap();
        writer
            .write_page(PageIdx::must_new(1), Page::test_filled(1))
            .unwrap();
        writer.commit().unwrap();

        let log = runtime.volume_log(&vid).unwrap();
        assert!(!log.is_empty(), "Should have at least one commit");

        let info = &log[0];
        assert_eq!(info.lsn.to_u64(), 1);
        assert!(info.page_count.to_u32() >= 1);
    }

    /// Performance benchmark (not a correctness test; informational only).
    /// Run with: cargo test -p graft -- perf_assessment --nocapture
    #[test]
    fn perf_assessment() {
        use std::time::Instant;

        let tokio_rt = tokio::runtime::Builder::new_current_thread()
            .start_paused(true)
            .enable_all()
            .build()
            .unwrap();

        let remote = Arc::new(RemoteConfig::Memory.build().unwrap());
        let storage = Arc::new(FjallStorage::open_temporary().unwrap());
        let runtime = Runtime::new(
            tokio_rt.handle().clone(), remote.clone(), storage, None,
        );

        let volume = runtime.volume_open(None, None, None).unwrap();
        let vid = volume.vid;

        // Setup: write N pages, each in its own commit
        let n = 200;
        let t0 = Instant::now();
        for i in 1..=n {
            let mut writer = runtime.volume_writer(vid.clone()).unwrap();
            writer
                .write_page(PageIdx::must_new(i), Page::test_filled(i as u8))
                .unwrap();
            writer.commit().unwrap();
        }
        let setup = t0.elapsed();
        println!("--- setup: {n} commits in {setup:?}");

        // volume_log
        let t0 = Instant::now();
        let log = runtime.volume_log(&vid).unwrap();
        let vlog = t0.elapsed();
        println!("volume_log: {n} commits → {:.0?} ({:.0} µs/commit)",
            vlog, vlog.as_micros() as f64 / n as f64);

        // volume_checkout to various sizes
        for target in [10u64, 50, 100, 200] {
            let t0 = Instant::now();
            let co = runtime.volume_checkout(&vid, LSN::new(target)).unwrap();
            let dur = t0.elapsed();
            let co_log = runtime.volume_log(&co.vid).unwrap();
            println!("checkout LSN {target}: {:.0?} (result has {} commits)", dur, co_log.len());
            runtime.volume_delete(&co.vid).unwrap();
        }

        // diff_commits over various ranges
        for (from, to) in [(1u64, 10u64), (1, 50), (1, 100), (1, 200), (100, 200)] {
            let t0 = Instant::now();
            let diff = runtime
                .diff_commits(&volume.local, LSN::new(from), LSN::new(to))
                .unwrap();
            let dur = t0.elapsed();
            let range = to - from + 1;
            println!("diff {from}..{to} ({} commits): {:.0?} ({} changed pages)",
                range, dur, diff.added_or_modified_pages.cardinality());
        }
    }
}
