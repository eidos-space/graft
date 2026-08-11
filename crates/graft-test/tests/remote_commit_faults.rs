use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Mutex, MutexGuard},
};

use graft::{
    core::{PageIdx, page::Page},
    volume_reader::VolumeRead,
    volume_writer::VolumeWrite,
};
use graft_test::GraftTestRuntime;

// Precept faults are process-global. Serialise tests in this binary so one
// workload cannot consume or clear another workload's pending fault.
static FAULT_STATE_LOCK: Mutex<()> = Mutex::new(());

fn lock_fault_state() -> MutexGuard<'static, ()> {
    FAULT_STATE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[test]
fn test_skip_segment_cache() {
    let _fault_state = lock_fault_state();
    graft_test::ensure_test_env();

    let runtime = GraftTestRuntime::with_memory_remote();

    // setup RemoteCommit faults
    let fault = precept::fault::get_fault_by_name("RemoteCommit: skipping segment cache").unwrap();
    fault.set_pending(1);
    let fault = precept::fault::get_fault_by_name("RemoteCommit: before commit").unwrap();
    fault.set_pending(1);

    // write to a volume and push
    let vid = runtime.volume_open(None, None, None).unwrap().vid;
    let mut writer = runtime.volume_writer(vid.clone()).unwrap();
    writer
        .write_page(PageIdx::FIRST, Page::test_filled(123))
        .unwrap();
    writer.commit().unwrap();

    // push should panic right before commit
    let err = catch_unwind(AssertUnwindSafe(|| runtime.volume_push(vid.clone())))
        .expect_err("expected volume_push to panic");
    tracing::info!("caught panic as expected: {:?}", err);

    // read the volume to make sure our page is still there
    let reader = runtime.volume_reader(vid.clone()).unwrap();
    let page = reader.read_page(PageIdx::FIRST).unwrap();
    assert_eq!(page, Page::test_filled(123));

    // a subsequent push should succeed
    runtime.volume_push(vid.clone()).unwrap();
    let remote = runtime.volume_get(&vid).unwrap().remote;

    // make sure we can pull the page to a peer
    let peer = runtime.spawn_peer();
    let vid2 = peer.volume_open(None, None, Some(remote)).unwrap().vid;
    peer.volume_pull(vid2.clone()).unwrap();

    let reader = peer.volume_reader(vid2).unwrap();
    let page = reader.read_page(PageIdx::FIRST).unwrap();
    assert_eq!(page, Page::test_filled(123));
}
