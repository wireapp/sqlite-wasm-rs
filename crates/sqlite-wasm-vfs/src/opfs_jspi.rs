//! Experimental, worker-free OPFS VFS using JSPI.
//!
//! Writes and truncations are accumulated in an owned, non-overlapping overlay.
//! Dirty ranges are coalesced before submission to one OPFS writable stream per
//! file; submission does not publish. Reads use the overlay plus a bounded cache
//! of the published file, and file size reports the overlay's logical size.
//! `xSync`, close and SQLite's cross-file journal/WAL phase boundaries publish
//! streams. OPFS writable
//! streams expose neither fsync nor directory fsync: this backend cannot promise
//! power-loss durability equivalent to a native SQLite filesystem, including
//! with synchronous=FULL.
//!
//! Use a dedicated directory, exclusively owned through a cooperative Web Lock.
//! Do not access it through another VFS or directly through OPFS while installed.
//! One connection per database is enforced. WAL is supported with an OPFS-backed
//! log and SQLite's heap-memory index: set `PRAGMA locking_mode=EXCLUSIVE` before
//! the first database access, including when reopening a WAL database. Shared-
//! memory WAL (NORMAL locking mode) is unsupported.
//!
//! All SQLite entry (even connections using other VFSes), file management and
//! lifecycle operations must use [`SqliteGuard`]. Enter from a
//! `#[wasm_bindgen(jspi)]` export whenever an operation can suspend. The guard is
//! non-reentrant and must be acquired before application connection mutexes.
//! JSPI suspends I/O and guard contention, not CPU-heavy query execution.

#![allow(deprecated)] // wasm-bindgen's JSPI API is experimental.

use async_lock::{Mutex, MutexGuard};
use js_sys::{
    futures::{future_to_promise, jspi_block_on_promise},
    Promise, Reflect, Uint8Array,
};
use rsqlite_vfs::{
    AccessMode, DeviceCharacteristics, FileKind, LockLevel, MemChunksFile, OpenAccess, OpenOptions,
    OpenRequest, OpenedFile, OsCallback, RegisterVfsError, SQLiteIoMethods, SQLiteVfs, SyncOptions,
    VfsError, VfsErrorCode, VfsFile, VfsRegistration, VfsResult, VfsStore,
};
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet, VecDeque},
    rc::Rc,
};
use wasm_bindgen::{prelude::wasm_bindgen, JsValue};

#[wasm_bindgen(module = "/src/opfs_jspi.js")]
extern "C" {
    #[wasm_bindgen(catch)]
    fn acquire(directory: &str) -> Result<Promise, JsValue>;
    #[wasm_bindgen(js_name = sleep)]
    fn sleep_js(milliseconds: f64) -> Promise;
    #[wasm_bindgen(catch)]
    fn release(lease: &JsValue) -> Result<Promise, JsValue>;
    #[wasm_bindgen(catch, js_name = open)]
    fn open_js(
        lease: &JsValue,
        name: &str,
        create: bool,
        exclusive: bool,
        role: &str,
        group: &str,
    ) -> Result<Promise, JsValue>;
    #[wasm_bindgen(catch)]
    fn exists(lease: &JsValue, name: &str) -> Result<Promise, JsValue>;
    #[wasm_bindgen(catch)]
    fn remove(lease: &JsValue, name: &str) -> Result<Promise, JsValue>;
    #[wasm_bindgen(catch, js_name = read)]
    fn read_js(handle: &JsValue, offset: f64, length: f64) -> Result<Promise, JsValue>;
    #[wasm_bindgen(catch, js_name = write)]
    fn write_js(handle: &JsValue, offset: f64, bytes: &Uint8Array) -> Result<Promise, JsValue>;
    #[wasm_bindgen(catch, js_name = truncate)]
    fn truncate_js(handle: &JsValue, length: f64) -> Result<Promise, JsValue>;
    #[wasm_bindgen(catch, js_name = size)]
    fn size_js(handle: &JsValue) -> Result<Promise, JsValue>;
    #[wasm_bindgen(catch, js_name = sync)]
    fn sync_js(handle: &JsValue) -> Result<Promise, JsValue>;
    #[wasm_bindgen(catch, js_name = close)]
    fn close_js(handle: &JsValue) -> Result<Promise, JsValue>;
    #[wasm_bindgen(js_name = scheduleSqliteCleanup)]
    fn schedule_cleanup(exports: &JsValue);
}

static SQLITE: Mutex<()> = Mutex::new(());
type Cleanup = Box<dyn FnOnce()>;
thread_local! {
    static CLEANUP: RefCell<VecDeque<Cleanup>> = RefCell::default();
}

/// Exclusive entry to SQLite for this entire Wasm instance.
///
/// Acquire this before any synchronous connection mutex and hold it around one
/// complete SQLite operation, including prepare, step and finalize. Also guard
/// connection/VFS open, close and lifecycle operations. Do not hold it across an
/// application transaction containing unrelated asynchronous work.
///
/// This guard is deliberately non-reentrant. Every acquisition path which can
/// contend must already be inside a JSPI-capable export; an uncontended lock does
/// not create a suspendable boundary.
pub struct SqliteGuard {
    _guard: MutexGuard<'static, ()>,
}

impl SqliteGuard {
    pub fn lock() -> Self {
        let guard = if let Some(guard) = SQLITE.try_lock() {
            guard
        } else {
            let slot = Rc::new(RefCell::new(None));
            let result = slot.clone();
            jspi_block_on_promise(&future_to_promise(async move {
                *result.borrow_mut() = Some(SQLITE.lock().await);
                Ok(JsValue::UNDEFINED)
            }))
            .expect("SQLite entry requires a JSPI boundary");
            let acquired = slot.borrow_mut().take().expect("SQLite lock acquired");
            acquired
        };
        let guard = Self { _guard: guard };
        // Pop before invoking: cleanup may suspend or enqueue more cleanup.
        while let Some(cleanup) = CLEANUP.with_borrow_mut(|queue| queue.pop_front()) {
            cleanup();
        }
        guard
    }
}

/// Defers SQLite cleanup from `Drop` or a JavaScript finalizer.
///
/// Ownership remains in the queue until a fresh `WebAssembly.promising` entry
/// obtains [`SqliteGuard`]. The closure already owns SQLite entry and must not
/// attempt to acquire this non-reentrant guard again.
pub fn defer(cleanup: impl FnOnce() + 'static) {
    CLEANUP.with_borrow_mut(|queue| queue.push_back(Box::new(cleanup)));
    schedule_cleanup(&wasm_bindgen::exports());
}

#[wasm_bindgen(jspi)]
#[allow(unreachable_pub)] // Called through the Wasm exports object by JavaScript.
pub fn sqlite_wasm_vfs_sqlite_cleanup() {
    let _sqlite = SqliteGuard::lock();
}

/// Runs one complete synchronous SQLite operation under [`SqliteGuard`].
///
/// This convenience API uses the same serialization domain as direct guard
/// acquisition and deferred cleanup. The closure may suspend through JSPI but
/// must finish before returning (no Futures or spawned operations).
pub fn with_sqlite<T>(operation: impl FnOnce() -> T) -> VfsResult<T> {
    let _sqlite = SqliteGuard::lock();
    Ok(operation())
}

fn error(code: VfsErrorCode, message: &str) -> VfsError {
    VfsError::new(code, message.to_owned().into())
}

fn settle(promise: Result<Promise, JsValue>, code: VfsErrorCode) -> VfsResult<JsValue> {
    promise
        .and_then(|promise| jspi_block_on_promise(&promise))
        .map_err(|value| {
            let name = Reflect::get(&value, &"name".into())
                .ok()
                .and_then(|x| x.as_string());
            let code = match name.as_deref() {
                Some("QuotaExceededError") => VfsErrorCode::Full,
                Some("NoModificationAllowedError") if code == VfsErrorCode::CantOpen => {
                    VfsErrorCode::Busy
                }
                Some("NotFoundError") if code == VfsErrorCode::IoDelete => {
                    VfsErrorCode::IoDeleteNoEntry
                }
                _ => code,
            };
            let message = Reflect::get(&value, &"message".into())
                .ok()
                .and_then(|x| x.as_string())
                .unwrap_or_else(|| format!("{value:?}"));
            error(
                code,
                &format!("OPFS {}: {message}", name.as_deref().unwrap_or("error")),
            )
        })
}

// A flat, case-sensitive namespace, encoded injectively as portable OPFS names.
// Reserve room for SQLite's journal suffixes below the physical name limit.
fn filename(name: &str) -> VfsResult<String> {
    if name.is_empty()
        || name.len() > 120
        || name.contains(['\0', '/', '\\'])
        || matches!(name, "." | "..")
    {
        return Err(error(
            VfsErrorCode::CantOpen,
            "expected a flat filename of 1..120 UTF-8 bytes",
        ));
    }
    use std::fmt::Write;
    let mut encoded = String::from("f-");
    for byte in name.bytes() {
        write!(&mut encoded, "{byte:02x}").unwrap();
    }
    Ok(encoded)
}

fn offset(offset: u64, length: usize, code: VfsErrorCode) -> VfsResult<f64> {
    if offset
        .checked_add(length as u64)
        .map_or(true, |end| end > (1u64 << 53) - 1)
    {
        return Err(error(
            code,
            "offset exceeds JavaScript's safe integer range",
        ));
    }
    Ok(offset as f64)
}

struct Lease(JsValue);
impl Drop for Lease {
    fn drop(&mut self) {
        let _ = release(&self.0);
    }
}

struct State {
    lease: Lease,
    os: Box<dyn OsCallback>,
    open: RefCell<HashMap<String, usize>>,
    databases: RefCell<HashSet<String>>,
    file_count: Cell<usize>,
    last_error: RefCell<Option<VfsError>>,
}

/// Installation failure, including OPFS/runtime errors and registration errors.
#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    #[error(transparent)]
    Storage(#[from] VfsError),
    #[error(transparent)]
    Registration(#[from] RegisterVfsError),
}

/// Installed VFS owner. Dropping it intentionally leaves the VFS and lease alive;
/// call [`Self::uninstall`] after closing every connection to release ownership.
pub struct OpfsJspi {
    state: Rc<State>,
    registration: Option<VfsRegistration<Rc<State>>>,
}

/// Installs a VFS in a dedicated OPFS directory.
///
/// Serialize installation with all SQLite and VFS lifecycle operations. An
/// application queue or the optional [`with_sqlite`] helper can provide this.
///
/// Directory components must be normal names (no `.`, `..`, backslash or NUL).
/// Leading/repeated slashes are normalized. Existing registrations are rejected.
/// All operations require a JSPI-capable runtime and a secure context.
pub fn install<C: OsCallback + Default + 'static>(
    name: &str,
    directory: &str,
    default_vfs: bool,
) -> Result<OpfsJspi, InstallError> {
    if name.is_empty() {
        return Err(RegisterVfsError::EmptyName.into());
    }
    if name.contains('\0') {
        return Err(RegisterVfsError::ToCStr.into());
    }
    // Reject name conflicts before acquiring storage resources. The caller
    // serializes registry access with other SQLite users in this instance.
    if unsafe { rsqlite_vfs::registered_vfs(name)? }.is_some() {
        return Err(RegisterVfsError::NameConflict(name.to_owned()).into());
    }
    let parts: Vec<_> = directory.split('/').filter(|p| !p.is_empty()).collect();
    if parts.is_empty()
        || parts
            .iter()
            .any(|p| matches!(*p, "." | "..") || p.contains(['\0', '\\']))
    {
        return Err(error(VfsErrorCode::CantOpen, "invalid OPFS directory").into());
    }
    let lease = Lease(settle(acquire(&parts.join("/")), VfsErrorCode::CantOpen)?);
    let state = Rc::new(State {
        lease,
        os: Box::new(JspiOs(C::default())),
        file_count: Cell::new(0),
        open: RefCell::new(HashMap::new()),
        databases: RefCell::new(HashSet::new()),
        last_error: RefCell::new(None),
    });
    // Caller-serialized registration; matching store and callback types.
    let registration =
        match unsafe { rsqlite_vfs::register_vfs::<Io, Vfs>(name, state.clone(), default_vfs) } {
            Ok(registration) => registration,
            Err(error) => {
                // Wait for release before returning so a subsequent install can
                // immediately retry. Drop remains a non-suspending fallback.
                let _ = settle(release(&state.lease.0), VfsErrorCode::IoClose);
                return Err(error.into());
            }
        };
    Ok(OpfsJspi {
        state,
        registration: Some(registration),
    })
}

impl OpfsJspi {
    /// Removes a closed file; SQLite sidecars are not removed automatically.
    pub fn remove(&self, name: &str) -> VfsResult<()> {
        if self.registration.is_none() {
            return Err(error(VfsErrorCode::Misuse, "VFS is uninstalled"));
        }
        Store::delete_file(&self.state, name, true)
    }

    /// Checks for a named file. Errors are not treated as absence.
    pub fn contains(&self, name: &str) -> VfsResult<bool> {
        if self.registration.is_none() {
            return Err(error(VfsErrorCode::Misuse, "VFS is uninstalled"));
        }
        Store::access(&self.state, name, AccessMode::Exists)
    }

    /// Unregisters and releases the directory lease. Close all connections first.
    ///
    /// # Safety
    /// No SQLite connections (including in-memory connections), saved VFS
    /// pointers or delegated callbacks may still use this registration. All
    /// SQLite entry must be serialized with this operation by the caller.
    pub unsafe fn uninstall(&mut self) -> Result<(), InstallError> {
        if self.state.file_count.get() != 0 {
            return Err(error(VfsErrorCode::Busy, "VFS has open files").into());
        }
        let Some(registration) = self.registration.take() else {
            return Ok(());
        };
        if let Err((registration, error)) = registration.unregister() {
            self.registration = Some(registration);
            return Err(error.into());
        }
        settle(release(&self.state.lease.0), VfsErrorCode::IoClose)?;
        Ok(())
    }
}

struct File {
    state: Rc<State>,
    name: Option<String>,
    handle: JsValue,
    memory: Option<MemChunksFile>,
    read_only: bool,
    lock: LockLevel,
    failed: bool,
    is_database: bool,
}
impl Drop for File {
    fn drop(&mut self) {
        self.state.file_count.set(self.state.file_count.get() - 1);
        if let Some(name) = &self.name {
            let mut open = self.state.open.borrow_mut();
            let count = open.get_mut(name).unwrap();
            *count -= 1;
            if *count == 0 {
                open.remove(name);
            }
            if self.is_database {
                self.state.databases.borrow_mut().remove(name);
            }
        }
    }
}
impl File {
    fn writable(&self) -> VfsResult<()> {
        if self.read_only {
            Err(error(VfsErrorCode::ReadOnly, "file is read-only"))
        } else if self.failed {
            Err(error(
                VfsErrorCode::IoWrite,
                "file has an uncertain write; close and reopen",
            ))
        } else {
            Ok(())
        }
    }

    fn publish(&mut self, code: VfsErrorCode) -> VfsResult<()> {
        if self.memory.is_some() {
            return Ok(());
        }
        if self.failed {
            // close_js removes the poisoned JavaScript state from the lease even
            // though it rejects; ownership must not leak into a later reopen.
            let _ = settle(close_js(&self.handle), code);
            return Err(error(code, "previous write or publication failed"));
        }
        let result = settle(close_js(&self.handle), code).map(|_| ());
        self.failed |= result.is_err();
        result
    }
}
impl VfsFile for File {
    fn device_characteristics(&self) -> DeviceCharacteristics {
        DeviceCharacteristics::UNDELETABLE_WHEN_OPEN
    }
    fn read(&mut self, buf: &mut [u8], at: u64) -> VfsResult<usize> {
        if let Some(memory) = &mut self.memory {
            return memory.read(buf, at);
        }
        let at = offset(at, buf.len(), VfsErrorCode::IoRead)?;
        if buf.is_empty() {
            return Ok(0);
        }
        let bytes = Uint8Array::new(&settle(
            read_js(&self.handle, at, buf.len() as f64),
            VfsErrorCode::IoRead,
        )?);
        let count = bytes.length() as usize;
        if count > buf.len() {
            return Err(error(VfsErrorCode::IoRead, "invalid OPFS read length"));
        }
        bytes.copy_to(&mut buf[..count]);
        Ok(count)
    }
    fn write(&mut self, buf: &[u8], at: u64) -> VfsResult<()> {
        self.writable()?;
        if let Some(memory) = &mut self.memory {
            return memory.write(buf, at);
        }
        let at = offset(at, buf.len(), VfsErrorCode::IoWrite)?;
        if buf.is_empty() {
            return Ok(());
        }
        let bytes = Uint8Array::from(buf);
        let result = settle(write_js(&self.handle, at, &bytes), VfsErrorCode::IoWrite).map(|_| ());
        self.failed |= result.is_err();
        result
    }
    fn truncate(&mut self, size: u64) -> VfsResult<()> {
        self.writable()?;
        if let Some(memory) = &mut self.memory {
            return memory.truncate(size);
        }
        let size = offset(size, 0, VfsErrorCode::IoTruncate)?;
        let result = settle(truncate_js(&self.handle, size), VfsErrorCode::IoTruncate).map(|_| ());
        self.failed |= result.is_err();
        result
    }
    fn sync(&mut self, _options: SyncOptions) -> VfsResult<()> {
        if self.memory.is_some() || self.read_only {
            return Ok(());
        }
        if self.failed {
            return Err(error(VfsErrorCode::IoSync, "previous write failed"));
        }
        let result = settle(sync_js(&self.handle), VfsErrorCode::IoSync).map(|_| ());
        self.failed |= result.is_err();
        result
    }
    fn size(&self) -> VfsResult<u64> {
        if let Some(memory) = &self.memory {
            return memory.size();
        }
        let size = settle(size_js(&self.handle), VfsErrorCode::IoStat)?
            .as_f64()
            .unwrap_or(f64::NAN);
        if !size.is_finite()
            || size < 0.0
            || size.fract() != 0.0
            || size > ((1u64 << 53) - 1) as f64
        {
            return Err(error(VfsErrorCode::IoStat, "invalid OPFS file size"));
        }
        Ok(size as u64)
    }
    fn lock(&mut self, level: LockLevel) -> VfsResult<()> {
        self.lock = self.lock.max(level);
        Ok(())
    }
    fn unlock(&mut self, level: LockLevel) -> VfsResult<()> {
        self.lock = self.lock.min(level);
        Ok(())
    }
    fn check_reserved_lock(&self) -> VfsResult<bool> {
        Ok(self.lock >= LockLevel::Reserved)
    }
}

struct Store;
impl VfsStore for Store {
    type File = File;
    type AppData = Rc<State>;
    fn record_error(data: &Self::AppData, error: VfsError) {
        data.last_error.replace(Some(error));
    }
    fn last_error(data: &Self::AppData) -> Option<VfsError> {
        data.last_error.borrow().clone()
    }
    fn open_file(state: &Self::AppData, request: OpenRequest<'_>) -> VfsResult<OpenedFile<File>> {
        let options = request.options;
        let temporary = request.filename.is_none();
        let name = if temporary {
            None
        } else {
            Some(request.filename.unwrap().path().to_owned())
        };
        let handle = if let Some(name) = &name {
            let physical = filename(name)?;
            if options.kind() == Some(FileKind::MainDb) && state.open.borrow().contains_key(name) {
                return Err(error(VfsErrorCode::Busy, "database is already open"));
            }
            let group = match options.kind() {
                Some(FileKind::MainDb) => Some(name.as_str()),
                Some(FileKind::MainJournal) => name
                    .strip_suffix("-journal")
                    .filter(|base| state.databases.borrow().contains(*base)),
                Some(FileKind::Wal) => name
                    .strip_suffix("-wal")
                    .filter(|base| state.databases.borrow().contains(*base)),
                _ => None,
            };
            settle(
                open_js(
                    &state.lease.0,
                    &physical,
                    options.create(),
                    options.exclusive(),
                    match options.kind() {
                        Some(FileKind::MainDb) => "database",
                        Some(FileKind::MainJournal) => "journal",
                        Some(FileKind::SuperJournal) => "super-journal",
                        Some(FileKind::Wal) => "wal",
                        _ => "other",
                    },
                    group.unwrap_or_default(),
                ),
                VfsErrorCode::CantOpen,
            )?
        } else {
            JsValue::UNDEFINED
        };
        if let Some(name) = &name {
            *state.open.borrow_mut().entry(name.clone()).or_default() += 1;
            if options.kind() == Some(FileKind::MainDb) {
                state.databases.borrow_mut().insert(name.clone());
            }
        }
        state.file_count.set(state.file_count.get() + 1);
        Ok(OpenedFile {
            file: File {
                state: state.clone(),
                name,
                handle,
                memory: temporary.then(MemChunksFile::default),
                read_only: options.access() == OpenAccess::ReadOnly,
                lock: LockLevel::None,
                failed: false,
                is_database: options.kind() == Some(FileKind::MainDb),
            },
            access: options.access(),
        })
    }
    fn close_file(
        data: &Self::AppData,
        name: Option<&str>,
        mut file: File,
        options: OpenOptions,
    ) -> VfsResult<()> {
        // SQLite does not promise xSync immediately before every xClose. A
        // successful close must therefore publish the final batch.
        file.publish(VfsErrorCode::IoClose)?;
        drop(file);
        if options.delete_on_close() {
            if let Some(name) = name {
                return Self::delete_file(data, name, false);
            }
        }
        Ok(())
    }
    fn access(state: &Self::AppData, name: &str, _mode: AccessMode) -> VfsResult<bool> {
        Ok(settle(
            exists(&state.lease.0, &filename(name)?),
            VfsErrorCode::IoAccess,
        )?
        .as_bool()
        .unwrap_or(false))
    }
    fn full_pathname(_state: &Self::AppData, name: &str) -> VfsResult<String> {
        filename(name)?;
        if name.len() > 100 {
            return Err(error(
                VfsErrorCode::CantOpen,
                "database name exceeds 100 UTF-8 bytes",
            ));
        }
        Ok(name.to_owned())
    }
    fn delete_file(state: &Self::AppData, name: &str, _sync_dir: bool) -> VfsResult<()> {
        if state.open.borrow().contains_key(name) {
            return Err(error(VfsErrorCode::IoDelete, "file is open"));
        }
        settle(
            remove(&state.lease.0, &filename(name)?),
            VfsErrorCode::IoDelete,
        )
        .map(|_| ())
    }
}
struct Io;
impl SQLiteIoMethods for Io {
    type Store = Store;
}
struct Vfs;
impl SQLiteVfs<Io> for Vfs {
    type Os = dyn OsCallback;
    fn os(data: &Rc<State>) -> &Self::Os {
        &*data.os
    }
    const MAX_PATH_SIZE: std::os::raw::c_int = 121;
}

// The standard wasm host's sleep is a no-op without atomics. JSPI can instead
// yield to a timer, so SQLite busy timeouts do not spin on the main thread.
struct JspiOs<C>(C);
impl<C: OsCallback> OsCallback for JspiOs<C> {
    fn sleep(&self, duration: std::time::Duration) {
        jspi_block_on_promise(&sleep_js(duration.as_secs_f64() * 1000.0))
            .expect("JSPI timer failed: enter SQLite through a JSPI export");
    }
    fn random(&self, buf: &mut [u8]) -> usize {
        self.0.random(buf)
    }
    fn epoch_timestamp_in_ms(&self) -> VfsResult<i64> {
        self.0.epoch_timestamp_in_ms()
    }
}
