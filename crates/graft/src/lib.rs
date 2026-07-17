pub mod local {
    pub mod fjall_storage;
}

pub mod rt {
    pub mod runtime;

    mod action;
    mod task;
}

pub mod core;
pub mod err;
pub mod oracle;
#[cfg(not(target_arch = "wasm32"))]
pub mod remote;
#[cfg(target_arch = "wasm32")]
#[path = "remote_wasm.rs"]
pub mod remote;
pub mod repo;
pub mod setup;
pub mod snapshot;
pub mod volume;
pub mod volume_reader;
pub mod volume_writer;

#[cfg(any(test, feature = "testutil"))]
pub mod testutil;

#[cfg(feature = "precept")]
pub mod fault;

pub use err::{GraftErr, LogicalErr};
pub use rt::runtime::{CommitInfo, DiffResult as PageDiffResult};

// re-export static_assertions for macros
#[doc(hidden)]
pub use static_assertions;
