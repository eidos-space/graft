use std::{
    borrow::Cow,
    fmt::Display,
    fs::OpenOptions,
    num::NonZero,
    os::raw::c_int,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use config::{Config, FileFormat};
use graft::{
    remote::RemoteConfig,
    rt::runtime::Runtime,
    setup::{GraftConfig, setup_graft, setup_graft_temporary},
};
use graft_sqlite::vfs::GraftVfs;
use graft_tracing::{SubscriberInitExt, TracingConsumer, setup_tracing_with_writer};
use serde::Deserialize;
use sqlite_plugin::{
    logger::{SqliteLogLevel, SqliteLogger},
    vars,
    vfs::{RegisterOpts, SqliteErr},
};

#[derive(Debug, Deserialize)]
pub struct ExtensionConfig {
    #[serde(default)]
    remote: RemoteConfig,

    #[serde(default)]
    data_dir: Option<PathBuf>,

    log_file: Option<PathBuf>,

    #[serde(default = "bool::default")]
    make_default: bool,

    /// if set, specifies the autosync interval in seconds
    #[serde(default = "Option::default")]
    autosync: Option<NonZero<u64>>,
}

impl ExtensionConfig {
    pub fn setup_runtime(&self) -> Result<Runtime, graft::setup::InitErr> {
        match &self.data_dir {
            Some(data_dir) => setup_graft(GraftConfig {
                remote: self.remote.clone(),
                data_dir: data_dir.clone(),
                autosync: self.autosync,
            }),
            None => setup_graft_temporary(self.remote.clone(), self.autosync),
        }
    }
}

fn setup_log_file(path: &Path) {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("failed to open log file");

    install_tracing(setup_tracing_with_writer(
        TracingConsumer::Tool,
        Mutex::new(file),
        None,
    ));

    tracing::info!("Log file opened");
}

fn install_tracing<S>(subscriber: S)
where
    S: tracing::Subscriber + Send + Sync + 'static,
{
    // A host process may load the extension for more than one SQLite connection.
    // Keep the first global subscriber instead of panicking on subsequent loads.
    let _ = subscriber.try_init();
}

#[allow(dead_code, reason = "msg is unused in static build")]
#[derive(Debug)]
struct InitErr(SqliteErr, Cow<'static, str>);

impl<T: Display> From<T> for InitErr {
    fn from(err: T) -> Self {
        InitErr(vars::SQLITE_INTERNAL, err.to_string().into())
    }
}

/// Write an error message to the `SQLite` error message pointer if it is not null.
#[cfg(feature = "dynamic")]
fn write_err_msg(
    p_api: *mut sqlite_plugin::sqlite3_api_routines,
    msg: &str,
    out: *mut *const std::ffi::c_char,
) -> Result<(), SqliteErr> {
    if !out.is_null() {
        // SAFETY: p_api must be aligned and valid
        // SAFETY: out is not null
        unsafe {
            let p_api = p_api.as_ref().ok_or(vars::SQLITE_INTERNAL)?;
            let api = sqlite_plugin::vfs::SqliteApi::new_dynamic(p_api)?;
            api.mprintf(msg, out)?;
        }
    }
    Ok(())
}

fn resolve_config() -> Result<ExtensionConfig, InitErr> {
    resolve_config_from_path(Path::new("graft.toml"))
}

fn resolve_config_from_path(path: &Path) -> Result<ExtensionConfig, InitErr> {
    // build the config
    let mut config = Config::builder();

    // add the config file if it exists
    if path.is_file() {
        let path = path
            .to_str()
            .ok_or_else(|| InitErr(vars::SQLITE_CANTOPEN, "config path is not UTF-8".into()))?;
        config = config.add_source(config::File::new(path, FileFormat::Toml).required(true));
    }

    Ok(config.build()?.try_deserialize()?)
}

fn setup_logger(logger: SqliteLogger) {
    #[derive(Clone)]
    struct Writer(Arc<Mutex<SqliteLogger>>);

    impl std::io::Write for Writer {
        #[inline]
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            let msg = str::from_utf8(data).map_err(std::io::Error::other)?;
            let logger = self.0.lock().expect("logger mutex poisoned");
            for line in msg.lines() {
                logger.log(SqliteLogLevel::Notice, line);
            }
            Ok(data.len())
        }

        #[inline]
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let writer = Writer(Arc::new(Mutex::new(logger)));
    let make_writer = move || writer.clone();

    install_tracing(setup_tracing_with_writer(
        TracingConsumer::Tool,
        make_writer,
        None,
    ));
}

#[cfg(feature = "dynamic")]
fn dynamic_init(p_api: *mut sqlite_plugin::sqlite3_api_routines) -> Result<(), InitErr> {
    let config = resolve_config()?;

    // initialize graft
    let runtime = config.setup_runtime()?;
    let vfs = GraftVfs::new(runtime);
    let opts = RegisterOpts { make_default: config.make_default };

    // Safety: `p_api` must be a valid, aligned pointer to a `sqlite3_api_routines` struct
    let result =
        unsafe { sqlite_plugin::vfs::register_dynamic(p_api, c"graft".to_owned(), vfs, opts) };
    let logger = result.map_err(|err| InitErr(err, "Failed to register Graft VFS".into()))?;

    if let Some(path) = &config.log_file {
        setup_log_file(path);
    } else {
        setup_logger(logger);
    }

    Ok(())
}

/// This function is automatically called by `SQLite` upon loading the extension
/// at runtime.
/// # Safety
/// This function must be called by sqlite's extension loading mechanism.
#[cfg(feature = "dynamic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_extension_init(
    _db: *mut std::ffi::c_void,
    pz_err_msg: *mut *const std::ffi::c_char,
    p_api: *mut sqlite_plugin::sqlite3_api_routines,
) -> c_int {
    match dynamic_init(p_api) {
        Ok(()) => sqlite_plugin::vars::SQLITE_OK_LOAD_PERMANENTLY,
        Err(err) => match write_err_msg(p_api, err.1.as_ref(), pz_err_msg) {
            Ok(()) => err.0,
            Err(e) => e,
        },
    }
}

#[cfg(feature = "static")]
fn graft_static_init_inner() -> Result<(), InitErr> {
    let config = resolve_config()?;

    // initialize graft
    let runtime = config.setup_runtime()?;
    let vfs = GraftVfs::new(runtime);
    let opts = RegisterOpts { make_default: config.make_default };

    // Safety: `p_api` must be a valid, aligned pointer to a `sqlite3_api_routines` struct
    let result = sqlite_plugin::vfs::register_static(c"graft".to_owned(), vfs, opts);
    let logger = result.map_err(|err| InitErr(err, "Failed to register Graft VFS".into()))?;

    if let Some(path) = &config.log_file {
        setup_log_file(path);
    } else {
        setup_logger(logger);
    }

    Ok(())
}

/// Register the Graft `SQLite` extension statically.
///
/// # Safety
///
/// This function is an FFI entry point for hosts that statically link the extension.
/// It must only be called in a process where `SQLite` extension initialization through
/// Graft's global registration path is appropriate.
#[cfg(feature = "static")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn graft_static_init() -> c_int {
    graft_static_init_inner().map_or_else(|err| err.0, |_| 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_loader_config_uses_temporary_base_storage() {
        let config =
            resolve_config_from_path(Path::new("this-loader-config-file-should-not-exist.toml"))
                .unwrap();

        assert!(config.data_dir.is_none());
    }

    #[test]
    fn explicit_loader_config_can_set_data_dir() {
        let root =
            std::env::temp_dir().join(format!("graft-ext-config-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let config_path = root.join("graft.toml");
        let data_dir = root.join("data");
        std::fs::write(
            &config_path,
            format!("data_dir = {:?}\n", data_dir.display().to_string()),
        )
        .unwrap();

        let config = resolve_config_from_path(&config_path).unwrap();
        assert_eq!(config.data_dir, Some(data_dir));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn tracing_initialization_is_idempotent() {
        install_tracing(setup_tracing_with_writer(
            TracingConsumer::Tool,
            std::io::sink,
            None,
        ));
        install_tracing(setup_tracing_with_writer(
            TracingConsumer::Tool,
            std::io::sink,
            None,
        ));
    }
}
