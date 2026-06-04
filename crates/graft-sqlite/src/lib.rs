pub mod file;
pub mod json;
pub mod pragma;
pub mod row_level_diff;
pub mod sql_diff;
pub mod sqlite_parse;
pub mod vfs;

mod dbg;

#[cfg(feature = "register-static")]
pub mod register;

#[cfg(feature = "register-static")]
pub use register::register_static;
