//! Main-thread JSPI example and browser integration smoke test.
#![allow(deprecated)]
use sqlite_wasm_rs as ffi;
use sqlite_wasm_vfs::opfs_jspi::{self, defer, SqliteGuard};
use std::{
    cell::RefCell,
    ffi::{CStr, CString},
    ptr,
};
use wasm_bindgen::prelude::*;

fn js_error(error: impl std::fmt::Display) -> JsValue {
    JsValue::from_str(&error.to_string())
}
struct Db(*mut ffi::sqlite3);
impl Db {
    fn open(name: &str) -> Result<Self, JsValue> {
        let name = CString::new(name).unwrap();
        let mut db = ptr::null_mut();
        let rc = unsafe {
            ffi::sqlite3_open_v2(
                name.as_ptr(),
                &mut db,
                ffi::SQLITE_OPEN_READWRITE | ffi::SQLITE_OPEN_CREATE,
                c"jspi-example".as_ptr(),
            )
        };
        if rc != ffi::SQLITE_OK {
            let message = if db.is_null() {
                String::new()
            } else {
                unsafe { CStr::from_ptr(ffi::sqlite3_errmsg(db)) }
                    .to_string_lossy()
                    .into_owned()
            };
            unsafe {
                ffi::sqlite3_close(db);
            }
            return Err(js_error(format!("open returned {rc}: {message}")));
        }
        let db = Self(db);
        // Also required before accessing an existing WAL database on reopen.
        db.exec("PRAGMA locking_mode=EXCLUSIVE")?;
        Ok(db)
    }
    fn exec(&self, sql: &str) -> Result<(), JsValue> {
        let sql = CString::new(sql).unwrap();
        let rc = unsafe {
            ffi::sqlite3_exec(self.0, sql.as_ptr(), None, ptr::null_mut(), ptr::null_mut())
        };
        if rc != ffi::SQLITE_OK {
            return Err(js_error(
                unsafe { CStr::from_ptr(ffi::sqlite3_errmsg(self.0)) }.to_string_lossy(),
            ));
        }
        Ok(())
    }
    fn scalar(&self, sql: &str) -> Result<String, JsValue> {
        let sql = CString::new(sql).unwrap();
        let mut stmt = ptr::null_mut();
        let rc = unsafe {
            ffi::sqlite3_prepare_v2(self.0, sql.as_ptr(), -1, &mut stmt, ptr::null_mut())
        };
        if rc != ffi::SQLITE_OK {
            return Err(js_error(format!("prepare returned {rc}")));
        }
        let rc = unsafe { ffi::sqlite3_step(stmt) };
        let result = if rc == ffi::SQLITE_ROW {
            Ok(
                unsafe { CStr::from_ptr(ffi::sqlite3_column_text(stmt, 0).cast()) }
                    .to_string_lossy()
                    .into_owned(),
            )
        } else {
            Err(js_error(format!("step returned {rc}")))
        };
        unsafe {
            ffi::sqlite3_finalize(stmt);
        }
        result
    }
}
impl Drop for Db {
    fn drop(&mut self) {
        unsafe {
            ffi::sqlite3_close(self.0);
        }
    }
}

/// Called sequentially by index.html on Window. No library guard is needed:
/// the caller awaits this export before starting another SQLite operation.
#[wasm_bindgen(jspi)]
pub fn run_tests() -> Result<String, JsValue> {
    let _sqlite = SqliteGuard::lock();
    let mut vfs = opfs_jspi::install::<ffi::WasmOsCallback>(
        "jspi-example",
        "sqlite-wasm-jspi-example",
        false,
    )
    .map_err(js_error)?;
    file_contract_tests();
    assert!(Db::open("../invalid").is_err());
    for mode in ["DELETE", "TRUNCATE", "PERSIST", "WAL"] {
        let name = format!("{mode}.db");
        let db = Db::open(&name)?;
        assert!(
            Db::open(&name).is_err(),
            "duplicate database connection must fail"
        );
        assert!(opfs_jspi::install::<ffi::WasmOsCallback>(
            "conflicting-vfs",
            "sqlite-wasm-jspi-example",
            false
        )
        .is_err());
        assert_eq!(
            db.scalar(&format!("PRAGMA journal_mode={mode}"))?,
            mode.to_lowercase()
        );
        db.exec("PRAGMA synchronous=FULL; DROP TABLE IF EXISTS data; CREATE TABLE data(value TEXT); INSERT INTO data VALUES ('committed');")?;
        db.exec("BEGIN; UPDATE data SET value='rolled back'; ROLLBACK;")?;
        assert_eq!(db.scalar("SELECT value FROM data")?, "committed");
        db.exec("BEGIN; UPDATE data SET value='persisted'; COMMIT;")?;
        assert_eq!(db.scalar("PRAGMA integrity_check")?, "ok");
        if mode == "WAL" {
            assert!(vfs.contains(&format!("{name}-wal")).map_err(js_error)?);
            assert!(!vfs.contains(&format!("{name}-shm")).map_err(js_error)?);
            assert_eq!(db.scalar("PRAGMA wal_checkpoint(TRUNCATE)")?, "0");
            db.exec("UPDATE data SET value='persisted after checkpoint'")?;
        }
        drop(db);
        let db = Db::open(&name)?;
        assert_eq!(
            db.scalar("SELECT value FROM data")?,
            if mode == "WAL" {
                "persisted after checkpoint"
            } else {
                "persisted"
            }
        );
        assert_eq!(db.scalar("PRAGMA integrity_check")?, "ok");
        drop(db);
        assert!(vfs.contains(&name).map_err(js_error)?);
        vfs.remove(&name).map_err(js_error)?;
        for suffix in ["-journal", "-wal"] {
            let name = format!("{name}{suffix}");
            if vfs.contains(&name).map_err(js_error)? {
                vfs.remove(&name).map_err(js_error)?;
            }
        }
    }
    let db = Db::open("attached-main.db")?;
    db.exec(
        "PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL;
        ATTACH DATABASE 'attached-aux.db' AS aux;
        CREATE TABLE data(value); CREATE TABLE aux.data(value);
        BEGIN; INSERT INTO data VALUES ('main'); INSERT INTO aux.data VALUES ('aux'); COMMIT;
        DETACH DATABASE aux;",
    )?;
    drop(db);
    let db = Db::open("attached-main.db")?;
    assert_eq!(db.scalar("SELECT value FROM data")?, "main");
    drop(db);
    let db = Db::open("attached-aux.db")?;
    assert_eq!(db.scalar("SELECT value FROM data")?, "aux");
    drop(db);
    for name in ["attached-main.db", "attached-aux.db"] {
        vfs.remove(name).map_err(js_error)?;
    }
    let db = Db::open("failure.db")?;
    db.exec("DROP TABLE IF EXISTS data; CREATE TABLE data(value); INSERT INTO data VALUES ('before failure');")?;
    fail_database_close();
    assert!(db.exec("UPDATE data SET value='failed commit'").is_err());
    drop(db);
    let db = Db::open("failure.db")?;
    assert_eq!(db.scalar("SELECT value FROM data")?, "before failure");
    assert_eq!(db.scalar("PRAGMA integrity_check")?, "ok");
    drop(db);
    vfs.remove("failure.db").map_err(js_error)?;
    unsafe {
        vfs.uninstall().map_err(js_error)?;
    }
    // Lock release is asynchronous; the next acquire is ordered behind it.
    let mut vfs = opfs_jspi::install::<ffi::WasmOsCallback>(
        "jspi-example",
        "sqlite-wasm-jspi-example",
        false,
    )
    .map_err(js_error)?;
    unsafe {
        vfs.uninstall().map_err(js_error)?;
    }
    Ok("PASS: indexed byte I/O, sparse/overlap/truncation, create/write/truncate/close errors, scoped and super-journal barriers, read-only, delete-on-close, main-thread SQLite, DELETE/TRUNCATE/PERSIST/WAL, checkpoint, rollback, reopen, integrity, duplicate opens, directory lease, uninstall/reinstall".into())
}

#[wasm_bindgen(module = "/test-hooks.js")]
extern "C" {
    #[wasm_bindgen(js_name = failNextWrite)]
    fn fail_next_write();
    #[wasm_bindgen(js_name = failFileCreation)]
    fn fail_file_creation(name: &str);
    #[wasm_bindgen(js_name = failDatabaseClose)]
    fn fail_database_close();
    #[wasm_bindgen(js_name = failNextStreamWrite)]
    fn fail_next_stream_write();
    #[wasm_bindgen(js_name = failNextStreamTruncate)]
    fn fail_next_stream_truncate();
    #[wasm_bindgen(js_name = crashAfterDatabaseWrite)]
    fn crash_after_database_write();
    #[wasm_bindgen(js_name = crashDuringWalCheckpoint)]
    fn crash_during_wal_checkpoint();
    #[wasm_bindgen(js_name = delay)]
    fn delay(milliseconds: f64) -> js_sys::Promise;
}

thread_local! {
    static GUARD_ORDER: RefCell<Vec<String>> = RefCell::default();
    static GUARD_VFS: RefCell<Option<opfs_jspi::OpfsJspi>> = RefCell::default();
    static CLEANUP_ORDER: RefCell<Vec<&'static str>> = RefCell::default();
    static DEFERRED_DB: RefCell<Option<Db>> = RefCell::default();
    static DEFERRED_VFS: RefCell<Option<opfs_jspi::OpfsJspi>> = RefCell::default();
}

#[wasm_bindgen(jspi)]
pub fn prepare_guard_connections() -> Result<(), JsValue> {
    let _sqlite = SqliteGuard::lock();
    let vfs =
        opfs_jspi::install::<ffi::WasmOsCallback>("jspi-example", "sqlite-wasm-jspi-guard", false)
            .map_err(js_error)?;
    GUARD_VFS.with_borrow_mut(|slot| *slot = Some(vfs));
    Ok(())
}

/// Two concurrently invoked promising exports must enter in FIFO order while
/// the first is suspended, rather than blocking the browser thread or failing.
#[wasm_bindgen(jspi)]
pub fn guarded_wait(id: &str, milliseconds: f64) -> Result<(), JsValue> {
    let _sqlite = SqliteGuard::lock();
    let db = Db::open(&format!("guard-{id}.db"))?;
    db.exec("DROP TABLE IF EXISTS data; CREATE TABLE data(value); INSERT INTO data VALUES (1)")?;
    GUARD_ORDER.with_borrow_mut(|order| order.push(format!("enter-{id}")));
    js_sys::futures::jspi_block_on_promise(&delay(milliseconds))?;
    assert_eq!(db.scalar("SELECT value FROM data")?, "1");
    GUARD_ORDER.with_borrow_mut(|order| order.push(format!("exit-{id}")));
    Ok(())
}

#[wasm_bindgen(jspi)]
pub fn take_guard_order() -> String {
    let _sqlite = SqliteGuard::lock();
    GUARD_ORDER.with_borrow_mut(|order| std::mem::take(order).join(","))
}

#[wasm_bindgen(jspi)]
pub fn finish_guard_connections() -> Result<(), JsValue> {
    let _sqlite = SqliteGuard::lock();
    let mut vfs = GUARD_VFS
        .with_borrow_mut(Option::take)
        .expect("guard VFS prepared");
    for id in ["first", "second"] {
        let name = format!("guard-{id}.db");
        vfs.remove(&name).map_err(js_error)?;
    }
    unsafe { vfs.uninstall().map_err(js_error)? };
    Ok(())
}

/// Models a resource finalizer running outside any JSPI boundary. The outer
/// cleanup enqueues another cleanup while the queue is being drained.
#[wasm_bindgen]
pub fn enqueue_cleanup_probe() {
    defer(|| {
        CLEANUP_ORDER.with_borrow_mut(|order| order.push("outer"));
        defer(|| CLEANUP_ORDER.with_borrow_mut(|order| order.push("inner")));
    });
}

#[wasm_bindgen(jspi)]
pub fn take_cleanup_order() -> String {
    let _sqlite = SqliteGuard::lock();
    CLEANUP_ORDER.with_borrow_mut(|order| std::mem::take(order).join(","))
}

#[wasm_bindgen(jspi)]
pub fn prepare_deferred_rollback() -> Result<(), JsValue> {
    let _sqlite = SqliteGuard::lock();
    let vfs = opfs_jspi::install::<ffi::WasmOsCallback>(
        "jspi-example",
        "sqlite-wasm-jspi-deferred",
        false,
    )
    .map_err(js_error)?;
    let db = Db::open("deferred.db")?;
    db.exec("DROP TABLE IF EXISTS data; CREATE TABLE data(value); INSERT INTO data VALUES ('committed'); BEGIN; UPDATE data SET value='uncommitted'")?;
    DEFERRED_DB.with_borrow_mut(|slot| *slot = Some(db));
    DEFERRED_VFS.with_borrow_mut(|slot| *slot = Some(vfs));
    Ok(())
}

/// Called directly from JavaScript, deliberately outside a promising export.
#[wasm_bindgen]
pub fn drop_deferred_rollback() {
    let db = DEFERRED_DB
        .with_borrow_mut(Option::take)
        .expect("database prepared");
    defer(move || drop(db));
}

#[wasm_bindgen(jspi)]
pub fn finish_deferred_rollback() -> Result<(), JsValue> {
    // Acquiring the guard first drains the rollback/close queued by the prior
    // plain Wasm export, even if its scheduled cleanup microtask has not run.
    let _sqlite = SqliteGuard::lock();
    let db = Db::open("deferred.db")?;
    assert_eq!(db.scalar("SELECT value FROM data")?, "committed");
    drop(db);
    let mut vfs = DEFERRED_VFS
        .with_borrow_mut(Option::take)
        .expect("VFS prepared");
    vfs.remove("deferred.db").map_err(js_error)?;
    unsafe { vfs.uninstall().map_err(js_error)? };
    Ok(())
}

// Exercise the actual SQLite callback table, including byte-level contracts
// which normal SQL queries do not conveniently expose.
fn file_contract_tests() {
    use rsqlite_vfs::ffi::*;
    use std::mem::MaybeUninit;
    unsafe {
        let vfs = sqlite3_vfs_find(c"jspi-example".as_ptr());
        let mut storage = Box::new(MaybeUninit::<rsqlite_vfs::SQLiteVfsFile>::zeroed());
        let file: *mut sqlite3_file = storage.as_mut_ptr().cast();
        let flags = SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE | SQLITE_OPEN_MAIN_JOURNAL;
        let mut actual = 0;
        let open = (*vfs).xOpen.unwrap();
        assert_eq!(
            open(vfs, c"bytes".as_ptr(), file, flags, &mut actual),
            SQLITE_OK
        );
        let methods = &*(*file).pMethods;
        let write = methods.xWrite.unwrap();
        let read = methods.xRead.unwrap();
        let truncate = methods.xTruncate.unwrap();
        let sync = methods.xSync.unwrap();
        assert_eq!(truncate(file, 0), SQLITE_OK);
        assert_eq!(write(file, b"abc".as_ptr().cast(), 3, 4), SQLITE_OK);
        let mut bytes = [255u8; 10];
        assert_eq!(
            read(file, bytes.as_mut_ptr().cast(), 10, 0),
            SQLITE_IOERR_SHORT_READ
        );
        assert_eq!(bytes, [0, 0, 0, 0, b'a', b'b', b'c', 0, 0, 0]);
        assert_eq!(truncate(file, 5), SQLITE_OK);
        assert_eq!(write(file, b"XYZ".as_ptr().cast(), 3, 2), SQLITE_OK);
        let mut overlapped = [0u8; 5];
        assert_eq!(read(file, overlapped.as_mut_ptr().cast(), 5, 0), SQLITE_OK);
        assert_eq!(&overlapped, b"\0\0XYZ");
        assert_eq!(truncate(file, 2), SQLITE_OK);
        assert_eq!(write(file, b"q".as_ptr().cast(), 1, 5), SQLITE_OK);
        let mut extended = [255u8; 6];
        assert_eq!(read(file, extended.as_mut_ptr().cast(), 6, 0), SQLITE_OK);
        assert_eq!(extended, [0, 0, 0, 0, 0, b'q']);
        let mut size = -1;
        assert_eq!(methods.xFileSize.unwrap()(file, &mut size), SQLITE_OK);
        assert_eq!(size, 6);
        assert_eq!(sync(file, SQLITE_SYNC_FULL), SQLITE_OK);
        let mut published = [0u8; 6];
        assert_eq!(read(file, published.as_mut_ptr().cast(), 6, 0), SQLITE_OK);
        assert_eq!(published, [0, 0, 0, 0, 0, b'q']);
        assert_eq!(write(file, b"z".as_ptr().cast(), 1, 5), SQLITE_OK);
        assert_eq!(sync(file, SQLITE_SYNC_FULL), SQLITE_OK);
        assert_eq!(read(file, published.as_mut_ptr().cast(), 6, 0), SQLITE_OK);
        assert_eq!(published, [0, 0, 0, 0, 0, b'z']);
        assert_eq!(
            write(file, b"x".as_ptr().cast(), 1, 1i64 << 53),
            SQLITE_IOERR_WRITE
        );
        fail_next_write();
        // Buffered writes report delayed stream-creation/storage failures at
        // the next submission/publication boundary.
        assert_eq!(write(file, b"x".as_ptr().cast(), 1, 0), SQLITE_OK);
        assert_eq!(methods.xSync.unwrap()(file, SQLITE_SYNC_FULL), SQLITE_FULL);
        assert_eq!(
            methods.xSync.unwrap()(file, SQLITE_SYNC_FULL),
            SQLITE_IOERR_FSYNC
        );
        assert_eq!(methods.xClose.unwrap()(file), SQLITE_IOERR_CLOSE);
        assert_eq!(
            open(
                vfs,
                c"bytes".as_ptr(),
                file,
                SQLITE_OPEN_READONLY | SQLITE_OPEN_MAIN_JOURNAL,
                &mut actual
            ),
            SQLITE_OK
        );
        let methods = &*(*file).pMethods;
        assert_eq!(
            methods.xWrite.unwrap()(file, b"x".as_ptr().cast(), 1, 0),
            SQLITE_READONLY
        );
        assert_eq!(methods.xClose.unwrap()(file), SQLITE_OK);
        assert_eq!(
            (*vfs).xDelete.unwrap()(vfs, c"bytes".as_ptr(), 1),
            SQLITE_OK
        );
        assert_eq!(
            (*vfs).xDelete.unwrap()(vfs, c"bytes".as_ptr(), 1),
            SQLITE_IOERR_DELETE_NOENT
        );
        assert_eq!(
            open(
                vfs,
                c"temporary".as_ptr(),
                file,
                flags | SQLITE_OPEN_DELETEONCLOSE,
                &mut actual
            ),
            SQLITE_OK
        );
        assert_eq!((*(*file).pMethods).xClose.unwrap()(file), SQLITE_OK);
        let mut found = -1;
        assert_eq!(
            (*vfs).xAccess.unwrap()(vfs, c"temporary".as_ptr(), SQLITE_ACCESS_EXISTS, &mut found),
            SQLITE_OK
        );
        assert_eq!(found, 0);

        assert_eq!(
            open(vfs, c"write-failure".as_ptr(), file, flags, &mut actual),
            SQLITE_OK
        );
        let methods = &*(*file).pMethods;
        assert_eq!(
            methods.xWrite.unwrap()(file, b"data".as_ptr().cast(), 4, 0),
            SQLITE_OK
        );
        fail_next_stream_write();
        assert_eq!(
            methods.xSync.unwrap()(file, SQLITE_SYNC_FULL),
            SQLITE_IOERR_FSYNC
        );
        assert_eq!(methods.xClose.unwrap()(file), SQLITE_IOERR_CLOSE);
        assert_eq!(
            (*vfs).xDelete.unwrap()(vfs, c"write-failure".as_ptr(), 1),
            SQLITE_OK
        );

        assert_eq!(
            open(vfs, c"truncate-failure".as_ptr(), file, flags, &mut actual),
            SQLITE_OK
        );
        let methods = &*(*file).pMethods;
        assert_eq!(methods.xTruncate.unwrap()(file, 32), SQLITE_OK);
        fail_next_stream_truncate();
        assert_eq!(
            methods.xSync.unwrap()(file, SQLITE_SYNC_FULL),
            SQLITE_IOERR_FSYNC
        );
        assert_eq!(methods.xClose.unwrap()(file), SQLITE_IOERR_CLOSE);
        assert_eq!(
            (*vfs).xDelete.unwrap()(vfs, c"truncate-failure".as_ptr(), 1),
            SQLITE_OK
        );

        assert_eq!(
            open(vfs, c"buffered".as_ptr(), file, flags, &mut actual),
            SQLITE_OK
        );
        let methods = &*(*file).pMethods;
        for byte in 0..100u8 {
            assert_eq!(
                methods.xWrite.unwrap()(file, (&byte as *const u8).cast(), 1, 0),
                SQLITE_OK
            );
        }
        let mut overwritten = [0u8; 1];
        assert_eq!(
            methods.xRead.unwrap()(file, overwritten.as_mut_ptr().cast(), 1, 0),
            SQLITE_OK
        );
        assert_eq!(overwritten, [99]);
        assert_eq!(methods.xClose.unwrap()(file), SQLITE_OK);
        assert_eq!(
            (*vfs).xDelete.unwrap()(vfs, c"buffered".as_ptr(), 1),
            SQLITE_OK
        );

        let mut a_storage = Box::new(MaybeUninit::<rsqlite_vfs::SQLiteVfsFile>::zeroed());
        let mut b_storage = Box::new(MaybeUninit::<rsqlite_vfs::SQLiteVfsFile>::zeroed());
        let mut journal_storage = Box::new(MaybeUninit::<rsqlite_vfs::SQLiteVfsFile>::zeroed());
        let a: *mut sqlite3_file = a_storage.as_mut_ptr().cast();
        let b: *mut sqlite3_file = b_storage.as_mut_ptr().cast();
        let journal: *mut sqlite3_file = journal_storage.as_mut_ptr().cast();
        let db_flags = SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE | SQLITE_OPEN_MAIN_DB;
        assert_eq!(
            open(vfs, c"scope-a.db".as_ptr(), a, db_flags, &mut actual),
            SQLITE_OK
        );
        assert_eq!(
            open(vfs, c"scope-b.db".as_ptr(), b, db_flags, &mut actual),
            SQLITE_OK
        );
        assert_eq!(
            open(
                vfs,
                c"scope-a.db-journal".as_ptr(),
                journal,
                flags,
                &mut actual
            ),
            SQLITE_OK
        );
        assert_eq!(
            (*(*b).pMethods).xWrite.unwrap()(b, b"b".as_ptr().cast(), 1, 0),
            SQLITE_OK
        );
        fail_file_creation("scope-b.db");
        assert_eq!(
            (*(*journal).pMethods).xWrite.unwrap()(journal, b"j".as_ptr().cast(), 1, 0),
            SQLITE_OK
        );
        assert_eq!(
            (*(*a).pMethods).xWrite.unwrap()(a, b"a".as_ptr().cast(), 1, 0),
            SQLITE_OK
        );
        assert_eq!(
            (*(*b).pMethods).xSync.unwrap()(b, SQLITE_SYNC_FULL),
            SQLITE_FULL
        );
        assert_eq!((*(*journal).pMethods).xClose.unwrap()(journal), SQLITE_OK);
        assert_eq!((*(*a).pMethods).xClose.unwrap()(a), SQLITE_OK);
        assert_eq!((*(*b).pMethods).xClose.unwrap()(b), SQLITE_IOERR_CLOSE);
        for name in [c"scope-a.db", c"scope-b.db", c"scope-a.db-journal"] {
            assert_eq!((*vfs).xDelete.unwrap()(vfs, name.as_ptr(), 1), SQLITE_OK);
        }
    }
}

#[wasm_bindgen(jspi)]
pub fn prepare_recovery_test() -> Result<(), JsValue> {
    let _sqlite = SqliteGuard::lock();
    let _vfs = opfs_jspi::install::<ffi::WasmOsCallback>(
        "jspi-example",
        "sqlite-wasm-jspi-example",
        false,
    )
    .map_err(js_error)?;
    let db = Db::open("recovery.db")?;
    db.exec("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL; DROP TABLE IF EXISTS data; CREATE TABLE data(value); INSERT INTO data VALUES ('before interruption');")?;
    crash_after_database_write();
    // Journal publication precedes database writes. The hook reloads the
    // page after the first database write and never resolves that write.
    db.exec("UPDATE data SET value='uncommitted';")?;
    Err(js_error("crash hook did not run"))
}

#[wasm_bindgen(jspi)]
pub fn finish_recovery_test() -> Result<String, JsValue> {
    let _sqlite = SqliteGuard::lock();
    let mut vfs = opfs_jspi::install::<ffi::WasmOsCallback>(
        "jspi-example",
        "sqlite-wasm-jspi-example",
        false,
    )
    .map_err(js_error)?;
    assert!(vfs.contains("recovery.db-journal").map_err(js_error)?);
    let db = Db::open("recovery.db")?;
    assert_eq!(db.scalar("SELECT value FROM data")?, "before interruption");
    assert_eq!(db.scalar("PRAGMA integrity_check")?, "ok");
    drop(db);
    vfs.remove("recovery.db").map_err(js_error)?;
    unsafe {
        vfs.uninstall().map_err(js_error)?;
    }
    Ok("PASS: hot-journal recovery after page termination during a database write".into())
}

/// Keep committed pages in the WAL, then interrupt a checkpoint after its first
/// database page is published. Recovery must retain every committed row.
#[wasm_bindgen(jspi)]
pub fn prepare_wal_recovery_test() -> Result<(), JsValue> {
    let _sqlite = SqliteGuard::lock();
    let _vfs = opfs_jspi::install::<ffi::WasmOsCallback>(
        "jspi-example",
        "sqlite-wasm-jspi-example",
        false,
    )
    .map_err(js_error)?;
    let db = Db::open("wal-recovery.db")?;
    assert_eq!(db.scalar("PRAGMA journal_mode=WAL")?, "wal");
    db.exec(
        "PRAGMA synchronous=FULL; PRAGMA wal_autocheckpoint=0;
        DROP TABLE IF EXISTS data; CREATE TABLE data(value);
        INSERT INTO data VALUES ('committed before interruption');",
    )?;
    crash_during_wal_checkpoint();
    db.exec("PRAGMA wal_checkpoint(TRUNCATE)")?;
    Err(js_error("WAL crash hook did not run"))
}

#[wasm_bindgen(jspi)]
pub fn finish_wal_recovery_test() -> Result<String, JsValue> {
    let _sqlite = SqliteGuard::lock();
    let mut vfs = opfs_jspi::install::<ffi::WasmOsCallback>(
        "jspi-example",
        "sqlite-wasm-jspi-example",
        false,
    )
    .map_err(js_error)?;
    assert!(vfs.contains("wal-recovery.db-wal").map_err(js_error)?);
    assert!(!vfs.contains("wal-recovery.db-shm").map_err(js_error)?);
    let db = Db::open("wal-recovery.db")?;
    assert_eq!(
        db.scalar("SELECT value FROM data")?,
        "committed before interruption"
    );
    assert_eq!(db.scalar("PRAGMA integrity_check")?, "ok");
    assert_eq!(db.scalar("PRAGMA wal_checkpoint(TRUNCATE)")?, "0");
    db.exec("INSERT INTO data VALUES ('after recovery')")?;
    drop(db);
    let db = Db::open("wal-recovery.db")?;
    assert_eq!(db.scalar("SELECT count(*) FROM data")?, "2");
    drop(db);
    vfs.remove("wal-recovery.db").map_err(js_error)?;
    unsafe {
        vfs.uninstall().map_err(js_error)?;
    }
    Ok("PASS: persistent WAL recovery after page termination during checkpoint".into())
}
