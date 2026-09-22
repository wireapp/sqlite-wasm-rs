# sqlite-wasm-vfs

SQLite VFS implementations for `wasm32-unknown-unknown`.

`sahpool` stores databases in OPFS using sync access handles.

```toml
[dependencies]
sqlite-wasm-rs = { version = "0.6", features = ["wasm-bindgen"] }
sqlite-wasm-vfs = { version = "0.3", features = ["sahpool"] }
```

Install it as SQLite's default VFS:

```rust
use sqlite_wasm_rs::WasmOsCallback;
use sqlite_wasm_vfs::sahpool::{install, OpfsSAHPoolCfg};

async fn install_vfs() {
    install::<WasmOsCallback>(&OpfsSAHPoolCfg::default(), true)
        .await
        .unwrap();
}
```

Requires a secure context and a dedicated worker. Concurrent connections to the same database and WAL are not supported.

[API documentation](https://docs.rs/sqlite-wasm-vfs)

## Experimental main-thread OPFS with JSPI

Enable `opfs-jspi` to use OPFS on `Window` without a worker:

```toml
sqlite-wasm-vfs = { version = "0.3", features = ["opfs-jspi"] }
```

From a `#[wasm_bindgen(jspi)]` export, install with
`opfs_jspi::install::<WasmOsCallback>("opfs-jspi", "my-app-jspi", false)`.
Keep the returned `OpfsJspi` for explicit uninstallation. Open databases using
that VFS name. JavaScript must await the exported operation.

**Guard every complete SQLite operation in the wasm instance** with
`opfs_jspi::SqliteGuard`, including operations using other VFSes, connection
open/close, statement preparation/stepping/finalization, and VFS lifecycle and
management. Acquire it before synchronous connection mutexes and release those
mutexes first. Hold it for one operation, not across an application transaction
which awaits unrelated work. Contention suspends through JSPI; it does not block
the browser thread or return `SQLITE_BUSY`.

The guard is non-reentrant. Every ordinary acquisition still needs an outer
`#[wasm_bindgen(jspi)]`/`WebAssembly.promising` boundary, even when the lock is
currently uncontended. `with_sqlite` remains as a closure convenience, but now
uses this same guard and serialization domain rather than an independent busy
flag. Do not return a Future or spawn work from its closure.

Use `opfs_jspi::defer` when `Drop` or a JavaScript finalizer may run outside a
JSPI boundary. The closure retains resource ownership until a fresh crate-owned
promising export acquires the guard. Deferred work is drained before the next
operation is admitted; it already owns SQLite entry and must not lock again.

```rust
use sqlite_wasm_vfs::opfs_jspi::{defer, SqliteGuard};

let sqlite = SqliteGuard::lock();
let connection = connection_mutex.lock().unwrap();
// Complete prepare/step/finalize or open/close operation; VFS I/O may suspend.
drop(connection);
drop(sqlite);

// From Drop/finalizer code which cannot suspend:
defer(move || {
    // Roll back or close directly. Do not reacquire SqliteGuard here.
    drop(sqlite_resource);
});
```

The backend supports one connection per database and rollback journaling. It
holds an exclusive cooperative Web Lock on its directory for the installation's
lifetime. Do not use that directory through sahpool or other OPFS clients; those
clients do not participate in this lock protocol. Use flat database names of at
most 100 UTF-8 bytes; the on-disk filenames are hex-encoded. Anonymous temporary
files use the shared memory-file implementation. Named files, including journals,
use OPFS. `contains` and `remove` provide basic management; pool capacity controls
and database transfer APIs are not implemented for this backend.

Writes first enter an owned, non-overlapping interval overlay. The Rust binding
passes an owned `Uint8Array`, so the JavaScript side does not make a redundant
second copy and never retains a borrowed Wasm-memory view. Overwrites replace
covered intervals instead of growing an operation log. Explicit logical size and
a published-data limit model holes, shrink-then-extension, and short reads;
truncated published bytes can never reappear after a later extension.

Reads are assembled from overlay intervals, implicit zero ranges, and only the
missing published ranges. Fully covered reads do no OPFS I/O. A `File` snapshot
is cached until publication, and published bytes use 64 KiB LRU blocks with an
8 MiB per-file limit. Publication discards both snapshot and blocks before any
subsequent read. The browser integration test reads across two publications and
verifies that the second read cannot return the old snapshot.

Dirty byte ranges are coalesced and submitted when they reach 1 MiB or a
publication boundary. Submission may create a writable stream and call
`write()`/`truncate()`, but deliberately does **not** close it, so a memory-
pressure submission cannot make an unsafe transaction phase visible. A shrink
floor plus final logical size preserve truncate-then-extend semantics. Submitted
bytes stay in the overlay for read-your-writes until publication.

Publication rules are:

- `xSync` publishes that file and reports a close/publication failure.
- `xClose` publishes any remaining batch; SQLite need not sync immediately
  before close.
- Before a main-database mutation, pending rollback-journal or WAL data for that
  database is published. Before journal/WAL mutation or finalization, pending
  data for its main database is published. The association comes from SQLite's
  `FileKind` plus a sidecar name validated against an open main database, not an
  unvalidated suffix guess. Unknown associations remain directory-wide.
  Super-journal mutation/deletion is deliberately directory-wide, preserving
  attached-database ordering. These phase barriers preserve SQLite's ordering during
  rollback-journal commits, WAL commits and checkpoints, including
  `synchronous=NORMAL` paths which omit some sync calls.
- Deletion publishes preceding open work before removing the entry.
- A write, truncate or publication failure poisons that open state. It cannot be
  reused as success and must be closed/reopened.

Physical storage errors can be delayed because an `xWrite` below the submission
threshold only updates owned memory. They are reported by the next threshold
submission, cross-file barrier, `xSync`, or `xClose`. Stream creation, write,
truncate, and close failures abort the stream where possible and poison the
state; an uncertain state is never acknowledged or reused.

Closing an OPFS writable stream makes its snapshot visible but OPFS exposes no
native fsync or directory-fsync. Even `synchronous=FULL` cannot promise native-
filesystem power-loss durability.

Requires a secure context, JSPI, OPFS and Web Locks. JSPI support in wasm-bindgen
is experimental and cannot be combined with wasm threads/atomics. Use matching
wasm-bindgen library/CLI versions and disable wasm-opt or enable its exception
handling support. JSPI yields during I/O; CPU-heavy SQL still runs on the main
thread. SQLite sleep callbacks use a suspending timer rather than busy waiting.

The [main-thread example and browser tests](../../examples/opfs-jspi/README.md)
include interrupted-commit recovery and an event-loop/reentrancy check.
