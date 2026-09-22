# Main-thread OPFS with JSPI

This example runs real SQLite on `Window`, with no worker. It also serves as the
browser integration suite for the experimental `sqlite-wasm-vfs/opfs-jspi` feature.

```sh
wasm-pack build --target web examples/opfs-jspi
python3 -m http.server 8000 --directory examples/opfs-jspi
```

Open <http://localhost:8000> in a browser supporting JSPI, OPFS and Web Locks.
The page automatically reloads twice to test hot-journal recovery after abandoning
a commit and WAL recovery after interrupting a checkpoint. Test databases live in the dedicated `sqlite-wasm-jspi-example` OPFS
directory on this origin. Successful tests remove their files.

For headless CI, with Chrome and its matching ChromeDriver installed:

```sh
node examples/opfs-jspi/run.mjs
```

`CHROME_BIN` and `CHROMEDRIVER` can specify executable paths. The runner starts its
own localhost server and uses a fresh browser profile. No npm dependencies are
needed. The release build disables wasm-opt because the JSPI transform requires
exception-handling instructions.

The test covers SQL commits/rollback in DELETE, TRUNCATE, PERSIST and WAL modes,
reopen/integrity checks, WAL checkpointing, duplicate opens, directory ownership,
uninstall/reinstall, pending and overlapping byte reads/writes/truncation, sparse
extension, pending file size, EOF zero filling, published-snapshot invalidation,
repeated overwrites, read-only writes, injected
stream creation/write/truncate/close failures, database-scoped barriers,
attached-database/super-journal commits, recovery after a close error following
publication, deletion, queued guard contention while suspended, and nested deferred cleanup.
Page termination is a recovery test, not a power-loss durability test.

Every exported SQLite operation acquires the crate's shared `SqliteGuard`.
The page starts two promising exports concurrently and verifies that the second
suspends until the first releases the guard. It also schedules cleanup outside a
JSPI boundary and verifies that nested cleanup drains before the next operation.
That exercise proves the dependency's crate-specific promising cleanup export
survives downstream wasm-bindgen compilation.

WAL uses a persistent OPFS `-wal` file and SQLite's heap-memory index. Each
connection sets `PRAGMA locking_mode=EXCLUSIVE` before database access, including
on reopen. No `-shm` file or shared-memory callbacks are needed. Directory
ownership and the single-connection rule remain required.
