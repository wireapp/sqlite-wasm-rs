//! SQLite VFS implementations for `wasm32-unknown-unknown`.
//!
//! Enable `sahpool` for the worker-only sync access handle pool, or `opfs-jspi`
//! for the experimental main-thread OPFS VFS using JavaScript Promise Integration.

#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]

/// Origin Private File System (OPFS) VFS implementation using `SyncAccessHandle`.
#[cfg(feature = "sahpool")]
pub mod sahpool;

#[cfg(all(feature = "opfs-jspi", target_feature = "atomics"))]
compile_error!("opfs-jspi does not support wasm threads/atomics");

#[cfg(all(feature = "opfs-jspi", not(target_feature = "atomics")))]
pub mod opfs_jspi;

#[cfg(test)]
wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_dedicated_worker);
