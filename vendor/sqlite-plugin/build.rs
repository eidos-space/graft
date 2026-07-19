extern crate bindgen;

use std::env;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=sqlite3/sqlite3.h");
    println!("cargo:rerun-if-changed=sqlite3/sqlite3ext.h");

    let vars = bindgen::Builder::default()
        .header("sqlite3/sqlite3ext.h")
        .allowlist_item("SQLITE_.*")
        .use_core()
        .default_macro_constant_type(bindgen::MacroTypeVariation::Signed)
        .generate()
        .expect("Unable to generate bindings");

    let bindings = bindgen::Builder::default()
        .header("sqlite3/sqlite3ext.h")
        .blocklist_item("SQLITE_.*")
        .use_core()
        .default_macro_constant_type(bindgen::MacroTypeVariation::Signed)
        .generate()
        .expect("Unable to generate bindings");

    let out_path = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR should be defined"));
    vars.write_to_file(out_path.join("vars.rs"))
        .expect("Couldn't write vars!");
    let bindings_path = out_path.join("bindings.rs");
    bindings
        .write_to_file(&bindings_path)
        .expect("Couldn't write bindings!");

    // libclang reports Emscripten C functions in a form bindgen currently
    // omits from its output. These declarations are the static SQLite entry
    // points used by SqliteApi and are provided by libsqlite3-sys at link time.
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("emscripten") {
        let mut bindings = OpenOptions::new()
            .append(true)
            .open(bindings_path)
            .expect("Couldn't reopen bindings!");
        bindings
            .write_all(
                br#"
unsafe extern "C" {
    pub fn sqlite3_vfs_register(
        arg1: *mut sqlite3_vfs,
        arg2: ::core::ffi::c_int,
    ) -> ::core::ffi::c_int;
    pub fn sqlite3_vfs_find(
        arg1: *const ::core::ffi::c_char,
    ) -> *mut sqlite3_vfs;
    pub fn sqlite3_mprintf(
        arg1: *const ::core::ffi::c_char,
        ...
    ) -> *mut ::core::ffi::c_char;
    pub fn sqlite3_log(
        arg1: ::core::ffi::c_int,
        arg2: *const ::core::ffi::c_char,
        ...
    );
    pub fn sqlite3_libversion_number() -> ::core::ffi::c_int;
}
"#,
            )
            .expect("Couldn't append Emscripten SQLite bindings!");
    }
}
