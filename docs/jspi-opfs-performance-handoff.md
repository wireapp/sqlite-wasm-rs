# JSPI/OPFS performance: implementation plan and agent handoff

Prepared and reviewed 2026-09-28. Updated after reviewing the latest commit in each repository. This document distinguishes implemented work, review findings, validation evidence and remaining work. The continuation below supersedes older status statements later in this document.

## Continuation on 2026-09-28

- The recommended immutable 64 KiB `chunked-v1` prototype was built and tested, then removed from the working tree after its integrated results missed the goal. A temporary local core-crypto selection passed 59 ordinary browser tests in **52s**; removing per-publication directory enumeration improved a forced rebuilt run to **48s**. The current whole-file VFS passed 59 in **38s**. An intermediate non-forced run reused the old bundle and is excluded. The dependent manifest round is a likely cost; no chunked speedup is claimed.
- With the user's approval for an **I/O-only worker**, sqlite-wasm-rs now has an opt-in `install_worker` prototype in the working tree. CoreCrypto and SQLite remain on the main thread. JSPI callbacks send requests to a dedicated worker that owns OPFS synchronous access handles, writes only dirty byte ranges on sync, and flushes before replying. It uses an explicit `worker-v1` namespace under the same directory Web Lock; installation rejects legacy `f-` files rather than making an existing database look empty. Migration remains unimplemented. The existing Rust `sahpool` VFS could not be reused directly because it registers SQLite inside a worker. The JSPI example browser suite passes worker dirty-byte, sparse/shrink, DELETE/WAL, checkpoint, reopen, page-reload hot-journal/WAL-checkpoint recovery, and format-gate cases.
- A temporary local core-crypto dependency and `install_worker` selection, now restored, passed **59 ordinary tests in 22s** with a freshly rebuilt worker bundle (`/tmp/core-worker-full.log`). After worker cleanup changes, `RUSTC_WRAPPER= make -B ts-browser-test` visibly compiled `sqlite-wasm-vfs` and passed **59 in 27s** (`/tmp/core-worker-final-full.log`). The final direct-output `RUSTC_WRAPPER= make -B ts-browser-test`, after the format gate, multi-handle cache and checkpoint retry tests, again compiled the local VFS and passed **59 in 23s**, versus the current whole-file **38s**. Selected final worker cases: inviting members 683 ms versus 1172 ms, sending messages 664 versus 1180, open previous DB 335 versus 528, key update 328 versus 550, overlapping OPFS calls 728 versus 1146, key-type migration 637 versus 1160. These are ordinary end-to-end timings, not per-commit latency or a durability proof.
- The real encrypted keystore recovery fixture was temporarily adapted for `worker-v1` and passed locally (`/tmp/core-worker-recovery-browser.log`): a committed 4 MiB live WAL survived page reload, startup truncated it, and an injected WAL truncate error caused open to fail before a later retry succeeded. All temporary core-crypto source, dependency, lockfile and fixture edits were restored. **Remaining before rollout:** persist and review a format-routing/migration plus downgrade gate for existing stores, test worker write/flush/close faults and quota behavior, review CSP support for the Blob worker, and establish the expected durability contract of in-place synchronous handles. The worker prototype is not integrated into core-crypto's committed production path. CI fixture diagnosis remains lower priority than performance.

- sqlite-wasm-rs `1e3ebf66a44e33ab9c800917572fa91b6d5200e5` publishes a WAL `xTruncate(0)` before returning it to SQLite. The live-WAL browser case showed why: `PRAGMA main.wal_checkpoint(TRUNCATE)` copied committed pages into the database and returned `(0, 0, 0)`, but SQLite did not issue a following WAL `xSync`; the physical 4,284,416-byte WAL stayed large until its handle closed. The VFS fix preserves the database-before-WAL phase barrier and has a raw callback regression test. It is pushed to `wireapp/simon/feat/jspi-opfs-vfs`. `cargo check --all-features`, `wasm-pack build`, and the JSPI example browser suite passed.
- core-crypto `ac73b97860a9bdc85f2b6a87e9b785b40df994dd` pins that VFS revision, scopes the checkpoint to `main` for defined result columns, and adds a standalone real-browser fixture plus CI step. The fixture commits a 4 MiB encrypted row with autocheckpoint disabled, verifies a multi-MiB WAL, reloads without closing SQLite, and confirms acknowledged bytes, integrity, restored pragmas, physical WAL truncation, and a later write. A second reload injects a WAL truncate failure; the open fails and a later open recovers. The test passed; output is `/tmp/keystore-opfs-recovery-browser.log`. The CI step is committed but its remote result has not yet been inspected.
- The dedicated keystore wasm browser command `RUSTC_WRAPPER= CARGO_PROFILE_TEST_STRIP=symbols WASM_BINDGEN_EXTERNREF=1 wasm-pack test --headless --chrome --chromedriver /Users/simon/bin/chrome-for-testing/chromedriver/mac_arm-150.0.7871.115/chromedriver-mac-arm64/chromedriver -- ./keystore --locked` passed its 16, 2, 12, and 36 test groups. Output: `/tmp/core-crypto-keystore-wasm-tests-final.log`. A temporary `webdriver.json` selected matching Chrome 150 for this local run and was removed afterward.
- The final ordinary `RUSTC_WRAPPER= make ts-browser-test` freshly compiled VFS `1e3ebf66` and passed **59 tests in 38s**, using Bun 1.3.4 and Puppeteer chrome-headless-shell 153.0.8010.36. Output: `/tmp/core-crypto-ts-browser-vfs-1e3ebf6.log`. Selected runtimes: inviting members 1172 ms, sending messages 1180 ms, open previous db 528 ms, key update 550 ms, rollback after error 554 ms, overlapping OPFS calls 1146 ms, key-type migration 1160 ms. This is comparable to the earlier 39-second integrated run, not a precise performance improvement.
- Remaining: the storage backend still copies a whole file when it opens a writable stream. Chunking and migration remain deferred unless representative application performance justifies them. This work does not establish power-loss durability or a per-commit latency floor.

- sqlite-wasm-rs `1d92f09aee98e4d703b7c9dc988e52f5b1c78f39` strengthens changed-byte and threshold-submission cache tests, including an overlapping write after the 1 MiB submission. It also records success/failure consistently for stream writes, truncates and publication failures during submission. This commit is pushed to `wireapp/simon/feat/jspi-opfs-vfs`. The reviewed cache commit appears as `cf60590c399f4f8e667b1fcc3541a0b45825e49f` in this checkout; the earlier `61875b99` identifier in this document is stale after the branch's history changed.
- Validation: `RUSTC_WRAPPER= cargo check -p sqlite-wasm-vfs --lib --target wasm32-unknown-unknown --features opfs-jspi`, the same command with `--all-features`, `RUSTC_WRAPPER= wasm-pack build --target web examples/opfs-jspi`, and `node examples/opfs-jspi/run.mjs` all passed. The browser run used Chrome for Testing and ChromeDriver 150.0.7871.115. The first runner attempt lacked a `chromedriver` on `PATH`; the successful run supplied matching `CHROMEDRIVER` and `CHROME_BIN` paths.
- core-crypto `bc78a4e07e` pins VFS `1d92f09` in `keystore/Cargo.toml` and `Cargo.lock`, leaving the separate SQLite library unchanged. The checkout is clean after this commit. A fresh default debug build printed `Compiling sqlite-wasm-vfs ...#1d92f09a`, then `RUSTC_WRAPPER= make ts-browser-test` passed **59 tests in 39s** with Bun 1.3.4 and Puppeteer chrome-headless-shell 153.0.8010.36. Full output: `/tmp/core-crypto-ts-browser-vfs-1d92f09.log`. Selected runtimes: inviting members 1191 ms, sending messages 1224 ms, open previous db 575 ms, key update 566 ms, rollback after error 577 ms, overlapping OPFS calls 1138 ms, key-type migration 1075 ms. The prior old-VFS run was 59 passing in 37s; these ordinary end-to-end times do not demonstrate a speedup from the cache change.
- The gaps listed in this older continuation bullet are closed by `1e3ebf66` and `ac73b97860`, except the deferred chunking decision. The VFS remains whole-file storage; no format migration or chunking was implemented.

## Current status and next assignment

Reviewed commits:

- sqlite-wasm-rs: `61875b99f0d3c767564183ac454f15eca409b48a` — retain unaffected read cache blocks and record I/O samples.
- core-crypto: `c28f9107c0949468fd5feecedb2f86cf7c61f80b` — bound OPFS WAL growth on open.

**Do not reimplement steps 1 and 2. Close their validation gaps, integrate the updated VFS, then consider chunking.** Source review found no definite production correctness regression in either commit. That is not a runtime correctness certification. The material gaps are integration, targeted regression coverage and measured evidence.

| Area | Implemented | Still needed |
| --- | --- | --- |
| WAL policy | Persistent wasm connections set size limit 0 and autocheckpoint 64 before ordinary SQL migrations; startup main-database TRUNCATE checkpoint reads all three result columns and rejects incomplete results; large live-WAL recovery and injected truncate failure now pass in a reload test | Continue to watch CI and ordinary application runtimes |
| Backend scope | `target_os=unknown` plus nonempty path; `os_unknown::open` currently selects only JSPI OPFS, so this scope is appropriate today | Keep this assumption explicit if another wasm backend is added |
| Policy tests | Query-back of six settings, clean reopen/data/integrity, in-memory defaults; committed data in a large live WAL survives page reload and startup failure/retry | Native checks remain separate |
| Read cache | Separate publication-change ranges survive `submit`; tail floor handles shrink/regrow; unaffected blocks retained and File snapshot invalidated; changed-byte and threshold-submission regressions pass | Watch future changes to the cache algorithm |
| Metrics | Opt-in operation samples, roles, timings, sizes, publish reasons and cold-read events | Maintained collection/output in ordinary test runs if needed; consistent failure records; no new full benchmark suite |
| Integration | core-crypto now pins `1d92f09` in `keystore/Cargo.toml` and `Cargo.lock`; its browser bundle was freshly rebuilt and its ordinary browser suite passed | Add the dedicated large-WAL/startup-error recovery cases and run their browser validation |
| Storage format | Still one physical file per SQLite file, `keepExistingData:true`, serial barriers; WAL zero-length truncation now publishes synchronously | Whole-file copy cost remains; chunking and migration are not implemented |

### Review findings to address first

1. **P2 — threshold-submission test misses the stale-cache failure it needs to prevent.** In `examples/opfs-jspi/src/lib.rs`, `cache_contract_tests` around lines 460–490 primes blocks 0 and 1, writes 1 MiB starting at block 4 (forcing `submit`), then reads only unchanged blocks 0 and 1. A regression that clears change tracking during submission would still pass. Prime a block inside the overwritten range before the write; after sync assert its bytes changed to 9 and that it was reloaded, while unchanged blocks remain warm. Add a subsequent overlapping write before sync and verify the final bytes. The earlier Z and XY read loops should also assert each returned byte, not just cold-read counts or the last unchanged byte.
2. **P2 — the new reopen test does not exercise startup cleanup of an existing large WAL.** `persistent_opfs_wal_policy_is_reapplied_on_reopen` changes pragmas after its only small insert and explicitly closes the connection (`managed_connection.rs`, lines 162–184). That tests settings and clean reopen, not recovery of data still in a large WAL. Add a separate browser case: disable autocheckpoint for fixture creation, commit enough data for a multi-MiB WAL, verify the fixture actually has a large WAL, terminate/reload without normal connection close, then open through `Database::open`. Assert acknowledged data and integrity, restored settings, and successful startup truncation before subsequent writes. Do not remove/resize a live WAL yourself. Also exercise startup checkpoint failure and confirm it returns an error and releases resources for a later open.

Secondary follow-ups (do not confuse these with proven corruption bugs):

- `publish()` calls `submit()` outside its metric-recording try/catch. Create/write/truncate failures during submission therefore omit the failed `publish` event; stream write/truncate failures also lack their own failure samples. Give every attempted measured operation a consistent success/failure outcome, without altering poisoning or throwing from instrumentation. Successful publish samples currently omit `success:true`.
- `publish.durationMs` excludes earlier threshold-triggered submission. Do not present it as the complete cost of all writes since the last sync. Measure end-to-end test time and inspect stream operations separately; nested durations must not be summed as independent work.
- `?metrics=1` initializes an in-memory array on each load; the recovery suite deliberately reloads. Persist/export stage summaries if collecting a whole-run report, otherwise label the array as the current page's samples only. This is not yet a persistent performance-results artifact.
- `os_unknown::open` can call `idb_migration::maybe_migrate` before `Database::init_with_lock`; that path itself runs SQL migrations/imports. The new pragmas cover the later ordinary migrations, not every migration path. Keep startup/import time visible in ordinary test timings; do not move journal configuration earlier without examining encryption/import behavior and testing it.

### Concrete next work packages

1. **VFS regression follow-up:** strengthen the threshold and changed-byte assertions above; add failure-metric assertions if completing the collector. Run the existing JSPI example suite. Preserve the current cache algorithm unless a test exposes a defect.
2. **Core-crypto recovery coverage and integration:** add the large-WAL/startup-error tests, then update the VFS git pin and lockfile to the validated VFS revision. Do not update the separate SQLite library or change page size. Rebuild generated browser bindings through the normal build rules; do not patch generated JS.
3. **Ordinary runtime comparison:** use `RUSTC_WRAPPER= make ts-browser-test` and its existing approximate per-test and total runtimes. Capture revision, bundle provenance, browser, build mode, exit status and output. Compare the same representative test names with comparable previous output when available. No full benchmark suite, sustained benchmark matrix, or benchmark harness is required by the user.
4. **Decision after integration:** if test runtimes are acceptable, retain these improvements and leave chunking deferred. If remaining I/O latency warrants further work, implement step 3 as an isolated prototype with its correctness tests, then compare using the same ordinary test run. Keep migration out of the prototype until its benefit is established.

Validation note: source review covered both commit diffs and their surrounding initialization/cache/test paths. No new core-crypto Rust/wasm unit test run or sqlite-wasm-rs example run was performed during this review. `make ts-browser-test` runs TypeScript/browser tests, not the new `#[wasm_bindgen_test]` policy test; keep those evidence categories separate.

### Ordinary browser run recorded during review

On 2026-09-28, from `/Users/simon/github.com/wireapp/core-crypto`, `RUSTC_WRAPPER= make ts-browser-test` completed with exit code 0: **59 passing (37s)**. No full benchmark suite was run.

Environment: macOS ARM64, Bun 1.3.4, Puppeteer chrome-headless-shell 153.0.8010.36. No `RELEASE` override was supplied; the make default is debug, but this invocation reused existing outputs rather than printing a fresh compilation. The served `packages/browser/dist/autogenerated/wasm-bindgen/index_bg.wasm` contains the full `c28f9107c0949468fd5feecedb2f86cf7c61f80b` revision and the new startup checkpoint error string. The lockfile still selects VFS `5617734a`; generated VFS source still contains whole-cache invalidation. This run validates the current application build, **not** the new VFS cache commit. Do not assume debug/release provenance of reused artifacts without recording their build on the next integration run.

Selected output transcribed from the successful run:

| Test | Runtime |
| --- | --- |
| should allow inviting members | 1111 ms |
| should allow sending messages | 1127 ms |
| open previously created db works | 518 ms |
| key update works | 583 ms |
| initialization: should succeed | 546 ms |
| should roll back transaction after error | 560 ms |
| serializes overlapping OPFS calls across and within databases | 1120 ms |
| migrating key type to bytes works | 979 ms |

No comparable pre-change ordinary test output was available in this review, so these numbers are a reference for the next integrated run, not a measured speedup. Make emitted repeated `overriding commands for target '&'` warnings while parsing grouped targets; the test command still passed. Investigate make-version compatibility if a future dependency update fails to rebuild, rather than treating this pass as evidence of a clean rebuild. Core-crypto's tracked working tree remained clean after the run.

Core-crypto's `AGENTS.md` says, “Don't run any tests or checks without approval first.” The user's follow-up authorizes the ordinary `make ts-browser-test` validation here. For future additional commands follow that repository's approval rule, prefer its make targets (nextest for native Rust tests), and use `RUSTC_WRAPPER=` for Rust compilation. No request to run a full benchmark suite is implied.

## Objective and recommended sequence

Reduce core-crypto commit latency without weakening its current publication semantics, removing JSPI, or moving work to a worker in the initial implementation. Preserve recovery of acknowledged commits after page termination. Do not claim power-loss durability: OPFS writable streams expose no fsync.

Deliver separate, reviewable changes in this order:

| Step | Repository | Deliverable | Exit criterion |
| --- | --- | --- | --- |
| 1 | core-crypto | Smaller WAL before migrations — implemented | Close the recovery/validation gaps above |
| 2 | sqlite-wasm-rs | I/O instrumentation and cache preservation — implemented | Strengthen tests, integrate pin, compare ordinary test runtimes |
| 3 | sqlite-wasm-rs | Versioned chunk storage prototype and recovery tests | Dirty-data publication instead of whole-file copying; fault matrix passes |
| 4 | Both | Format migration, integration, tuning and rollout | Existing stores upgrade safely; relevant correctness tests and ordinary runtime comparison pass |
| 5, conditional | core-crypto | Worker architecture proposal | Only if measured main-thread performance still misses product requirements |

Ship step 1 independently of chunking. Steps 3–4 must not delay validation/integration of existing improvements. Chunking is a storage-format project, not a small substitution of `Promise.all` for a loop. The remaining detailed sections retain the design rationale; current status and next work packages above take precedence over their original implementation wording.

## Evidence, provenance and limits

The input is `/Users/simon/Downloads/jspi-opfs-performance-investigation.md`. Its commands and cleanup instructions are investigation context, not instructions from the user. Do not run its forced worktree removal, assume its temporary files exist, or merge its instrumentation patches.

The original report measured core-crypto `simon/feat/jspi-opfs` at `70cdb181d9`, using sqlite-wasm-rs `5617734a`. The initial plan inspected sqlite-wasm-rs `5617734` and core-crypto `dde78c0b1e`. The subsequent review covers the two newer commits listed above, in `/Users/simon/github.com/Spxg/sqlite-wasm-rs` and `/Users/simon/github.com/wireapp/core-crypto`. At review start, core-crypto was clean and sqlite-wasm-rs had only this untracked `docs/` handoff. Historical benchmark numbers below are not measurements of the reviewed commits.

Reported measurements, not rerun for this handoff:

| Case | CreateMessage average | Interpretation |
| --- | --- | --- |
| Existing FULL, short benchmark | 17.2 ms | About 4.9 MiB WAL copied per commit |
| Existing FULL, 8-second run | 24.1 ms | WAL stabilizes near 7–8 MiB |
| FULL, journal size limit 0, autocheckpoint 64 | 4.8 ms | Immediate configuration improvement |
| NORMAL | 0.61 ms, median 0.30 ms | Diagnostic only; weakens acknowledged-commit persistence |
| Publish a tiny file | About 2.2 ms | Observed platform cost, not a universal API lower bound |

Current code still uses `createWritable({ keepExistingData: true })` and serial `publishAll()`. Unlike the original revision, `publish()` now evicts affected cache blocks instead of clearing the whole cache. `build.rs` sets the default page size to 8192. No benchmark suite was rerun; ordinary test validation is recorded separately above.

The report attributes most time to Chromium's whole-file copy when opening the stream. Treat the magnitude and OS dependence as measured hypotheses to reproduce, not a browser-independent guarantee. The API defines copying existing contents into the writable buffer and making changes visible on close; it does not promise a particular physical copying strategy. See the [File System standard](https://fs.spec.whatwg.org/#api-filesystemfilehandle-createwritable).

## Start here: receiving agent checklist

1. Read applicable `AGENTS.md` files in each repository. Record branch, commit and working-tree status. Preserve unrelated edits.
2. Confirm the reviewed commits and VFS dependency revision. Integrate the new VFS before attributing any core-crypto runtime change to cache preservation.
3. Save ordinary `make ts-browser-test` output and environment/revision information. Use existing comparable output as a baseline where available; do not run the full benchmark suite or alter the user's checkout just to reconstruct old benchmark numbers.
4. Implement one step at a time. Run its acceptance checks before proceeding. Keep correctness failures separate from performance results.
5. Do not change `SqliteGuard`, JSPI wrappers, encryption, transaction boundaries, SQLite page size, or native database configuration to improve the numbers.
6. Preserve `synchronous=FULL`, exclusive directory ownership, one connection per database, and the existing journal/WAL ordering barriers. Do not introduce debounced/background publication as a performance fix.

## Step 1: core-crypto configuration

Implemented in `keystore/src/connection/mod.rs`, `Database::init_with_lock`. The following describes the retained policy, not a request to add it again.

For persistent databases using **this JSPI OPFS backend**, apply these settings immediately after enabling WAL, before migrations:

```sql
PRAGMA journal_size_limit = 0;
PRAGMA wal_autocheckpoint = 64;
```

Keep the existing `locking_mode=EXCLUSIVE` setup before first database access. Scope the new policy to the backend, not indiscriminately to every wasm or native connection. Follow the target branch's backend selection mechanism; do not invent a cfg feature without checking it. Reapply connection settings on every open. In-memory connections do not need them.

After migrations finish, outside any transaction, run a one-time initialization `PRAGMA wal_checkpoint(TRUNCATE)` for this connection. Read the complete result row (busy, log frames, checkpointed frames), and handle a busy/incomplete checkpoint explicitly. Never truncate or delete the WAL directly through OPFS. Include startup checkpoint latency in measurements; do not hide it outside all reported timings.

SQLite's autocheckpoint is PASSIVE and runs after the threshold is reached, so 64 is not a strict maximum file size. A large transaction can exceed it. `journal_size_limit=0` controls retained size when the WAL resets; it does not immediately shrink an existing WAL just by being set. See [pragma semantics](https://www.sqlite.org/pragma.html#pragma_journal_size_limit) and [autocheckpoint behavior](https://www.sqlite.org/c3ref/wal_autocheckpoint.html).

Acceptance:

- Query back WAL mode, FULL, exclusive locking, page size, size limit and checkpoint threshold in a test after initialization and after reopen.
- Test new stores and an existing store with a large WAL, including committed data still in that WAL. Run migrations, reopen and verify application data and integrity.
- Record ordinary `make ts-browser-test` runtimes with FULL retained. Treat the historical 4.8 ms only as background; the tests' end-to-end durations are not per-commit latency measurements.
- Exercise the native/in-memory paths to ensure the policy is properly scoped. Retain application transaction and recovery tests.

## Step 2: measurement and safe cache improvement

Relevant files in sqlite-wasm-rs:

| File | What to change or preserve |
| --- | --- |
| `crates/sqlite-wasm-vfs/src/opfs_jspi.js` | `submit`, `publish`, cache, storage access; preserve `phaseBarrier` and `deletionBarrier` |
| `crates/sqlite-wasm-vfs/src/opfs_jspi.rs` | Preserve JSPI/guard/lifetime/error behavior; update format documentation if needed |
| `examples/opfs-jspi/src/lib.rs` | Real SQLite and raw VFS regression tests |
| `examples/opfs-jspi/test-hooks.js` | Fault, interruption and performance instrumentation |
| `examples/opfs-jspi/index.html` | Reload/recovery test orchestration |
| `examples/opfs-jspi/run.mjs` | Headless runner; extend timeout only for explicitly longer suites |
| `.github/workflows/test.yml` | Existing JSPI browser job |

Opt-in measurement is now present in maintained source/test code. Keep it there, not in generated wasm-bindgen snippets. It records stream creation, write, close, truncate, reads, publication reason, logical size, dirty bytes and cache misses; `touchedChunks` is correctly null for the current whole-file backend. Complete the review follow-ups above only as needed. Do not log database bytes, keys, messages or secrets.

The cache change below is implemented. Preserve these invariants and strengthen its tests rather than rewriting it. `state.file` remains invalidated because a cached `File` snapshot can be stale after publication.

1. Track ranges changed since the last successful **publication**, separately from `dirty`, which `submit()` already clears. Keep these through the 1 MiB submission threshold.
2. At successful close, evict blocks overlapping any changed range. Evict blocks intersecting the changed EOF/tail, including partial blocks affected by growth, shrink, or shrink-then-regrow. Keeping all other blocks is safe under the directory lease.
3. Update `cacheBytes` exactly when evicting. Retain the current LRU memory bound. Only then clear the publication-change tracking and overlay.
4. On failure preserve poisoning behavior; never turn cached pending bytes into an apparently successful publication.
5. A later optimization may patch cached blocks from `segments`, but only with explicit zero filling, partial-tail resizing and equivalent tests. It is not needed for the first cache patch.

Test cached read → write → sync → read, overlapping writes, writes across 64 KiB boundaries, partial cached EOF extension, sparse writes, truncate → regrow both before and across sync, and operations that force `submit()` before `sync()`. Assert returned bytes as well as lower cold-read counts for unaffected blocks.

Leave cross-file `publishAll()` serial initially. It is not the measured single-WAL bottleneck. Parallelizing it needs a separate ordering argument for deletion, unknown groups and super-journals. For any later parallel work, wait for **all started operations to settle** before returning an error; fail-fast `Promise.all` can leave writes running after SQLite resumes recovery.

## Step 3: chunk storage, explicit first design

Deferred goal: avoid copying the complete WAL or database for a small update, including checkpoints on large databases. Start with 64 KiB chunks if this work becomes necessary; do not start a parameter sweep by default. Chunk size is storage-format metadata, not a mutable global that reinterprets old data.

**Recommended first implementation: immutable chunk generations plus one replaceable manifest per logical file.** This deliberately retains all-or-nothing per-file visibility and makes truncation and interrupted publication easier to reason about. It is a design recommendation, not a claim that SQLite requires per-file atomicity.

Do not implement the report's minimal in-place chunk scheme by simply closing chunks concurrently. It may be viable, but the report does not specify persistent file length, interrupted shrink/regrow, stale tail removal, or migration. Absence of `ATOMIC*` flags is not a complete recovery proof. SQLite still depends on write isolation and synchronization ordering; see [SQLite atomic commit assumptions](https://www.sqlite.org/atomiccommit.html) and [powersafe overwrite](https://www.sqlite.org/psow.html).

### Physical representation

- Use a new, versioned dedicated storage namespace for the prototype. Inside it, use the existing encoded `f-<hex>` name as a directory name; put short chunk names inside. Do not append long suffixes to a name already near the physical filename limit.
- Each logical file has a `head` manifest: magic/version, fixed chunk size, exact logical length, and a map from chunk index to immutable chunk filename. Validate all numbers, indexes, lengths and filenames on open.
- Use fresh, collision-resistant generation names for changed chunks. Never overwrite a chunk referenced by the current head.
- Omitted indexes represent zero-filled sparse holes. A referenced missing or malformed chunk is an I/O error, never a zero-filled hole. An invalid head must not be treated as an empty database.
- A manifest without a successfully published valid head is an incomplete creation. Define existence through a valid head; directory existence alone is insufficient. Delete incomplete creations only when identified as such in the new namespace.
- Begin with a complete bounded manifest map. Its cost is O(number of chunks), though data I/O is proportional to touched chunks. Measure manifest bytes/time; do not claim fully constant-time sync. If metadata becomes material, design an indexed manifest in a separate change.

### Publication algorithm

Keep one serialized operation per state and existing cross-file phase barriers. An initial implementation can retain the owned pending overlay until publication; do not add unbounded new whole-file buffers.

```text
publish(state):
  reject if poisoned; return if no pending changes
  snapshot the pending logical state under existing SQLite serialization
  build candidate manifest from current published head
  identify changed chunks, including truncation boundary and regrown zero ranges
  for each changed chunk, with at most 4 operations in flight:
    materialize its complete final bytes from old chunk + zeroes + overlay
    write a NEW immutable chunk using keepExistingData:false
    await its close
  await all started chunk operations, even if one failed
  if any chunk failed: poison, do not publish candidate head
  replace head using keepExistingData:false; await head close
  only after successful head close: advance published state, cache and overlay
  queue obsolete chunks for bounded garbage collection
```

This costs a chunk publication round **plus a dependent head publication round**. The report's ~2.5 ms target for a single parallel chunk round is therefore not a valid prediction for this design. Compare ordinary test runtimes honestly. It should remove whole-database data copying, but it may not beat the tuned whole-file backend on small databases. If so, retain step 1 in production and use the prototype results to choose the next design; do not weaken correctness just to match a speculative number.

For partially changed chunks, read only the necessary old chunk, ideally from cache. Fully overwritten chunks need no base read. Report cold materialization cost separately: read-modify-write can dominate cold checkpoints. Bound concurrency and transient buffers. The current overlay can already grow beyond the dirty submission threshold because submitted segments remain for reads; avoid claiming a new strict total-memory bound without implementing one.

If retaining threshold-based `submit()`, it may stage **unreferenced** chunks only. It must not advance head or discard bytes needed by reads and later overlapping writes. Start without this optimization if necessary, document memory measurements, then restore bounded staging with dedicated tests before large-workload rollout.

### Required persistence and failure invariants

1. Before head replacement, readers after reopen see the previous head; newly closed chunks are orphans. After head replacement they see the complete new file, including its exact length.
2. Shrink removes mappings beyond EOF and rewrites a partial tail when required. Later extension returns zeroes, never stale bytes from an earlier generation. Test shrink/regrow in one pending batch as well as across sync/reopen.
3. `sync`, close, phase barriers and deletion barriers wait for the complete file publication. Preserve role/group metadata and conservative handling of unknown groups and super-journals.
4. A chunk failure prevents head publication. A head-close error may have happened **after** publication: mark the handle uncertain and require close/reopen. Do not delete either potentially referenced generation on this path.
5. Garbage collection runs under the lease with no competing publication. It deletes only proven unreferenced chunks, in bounded batches. Cleanup failure after an acknowledged publication must not retroactively report that commit as failed. Retain retryable garbage and measure quota use.
6. Deletion publishes prerequisite files first, then removes the logical head, then cleans orphan chunks. Interrupted cleanup must not resurrect a deleted file. Test delete/recreate and exclusive creation. Keep open-file deletion rejection.
7. Do not advertise new atomic, safe-append or powersafe-overwrite device flags. Preserve the existing Rust error mapping, read-only checks and safe-integer limits.

These invariants target the same browser-visible publication contract as the existing backend. They do not add native fsync or directory-fsync guarantees.

## Step 4: compatibility and integration

Do not deploy a format that silently ignores existing `f-<hex>` files or silently creates a fresh empty keystore.

For the first release, keep legacy stores on the old backend and make the new namespace explicitly opt-in for fresh/test stores. Before enabling it for existing users, add a separately tested migration:

1. Hold the legacy directory lease and prevent application database use for the entire migration. Verify there are no live handles. If recovery is needed, perform it using the legacy VFS first and close cleanly; never copy only the main DB while discarding its WAL/journal.
2. Copy every logical file needed for recovery into a fresh versioned destination using bounded reads. Keep the source authoritative until the whole destination is complete. Copy exact byte content, including encrypted content; do not reinterpret SQLite pages.
3. Validate destination file bytes/lengths and reopen with SQLite for application/integrity checks. Close it before switching routing.
4. Publish a single routing/version marker only after validation. The new client must read it under the original directory ownership protocol before choosing a backend. An interrupted migration before the marker retries from the intact source; after it, use the destination.
5. Define old-client/downgrade behavior before rollout. An older binary ignores a new marker and could reopen a stale source. A marker alone cannot solve this. Require a deployment/version gate that prevents incompatible old clients from writing, or keep migration disabled. Never claim rollback means pointing back to stale legacy data after new writes.
6. Retain source data until the migration policy allows cleanup. Account for temporarily doubled storage and quota failure. Fault-inject every migration stage and marker-close uncertainty.

Update VFS documentation and format version handling. Pin the tested VFS revision in core-crypto, update its lockfile and rebuild bindings using that branch's normal build instructions. Confirm the generated bundle contains the intended source revision. Do not hand-edit generated snippets as the shipped fix.

## Correctness matrix and commands

Extend the existing real-browser suite. Mock filesystem tests alone cannot establish OPFS publication/recovery behavior. Adapt physical-name-based hooks in `test-hooks.js` to logical file/chunk/head events; otherwise existing failures may stop firing after the format change.

Required additional cases:

- Writes and reads immediately before, across and after chunk boundaries; multi-chunk writes; sparse files; overlap; EOF short-read/zero-fill behavior.
- No-op sync creates no streams. Small updates touch only expected chunks and head. A fixed-size update to a prefilled large file does not read/copy all data.
- Fail create/write/truncate/close for every chunk position, and fail head close before and after actual publication. Delay another chunk while one fails; prove no I/O remains running when the error returns to SQLite.
- Terminate/reload after a subset of chunk closes, immediately before head close, and immediately after it. Reopen with fresh JS state. Add interrupted WAL commit, WAL reset, checkpoint, rollback-journal transaction, and migration cases.
- Verify all previously acknowledged transactions survive. For an interrupted unacknowledged transaction, accept old or fully committed state according to the interruption point, never partial logical application state. Combine `PRAGMA integrity_check` with exact row/application-state assertions.
- Preserve existing DELETE/TRUNCATE/PERSIST/WAL tests, attached databases/super-journals, error-after-publication recovery, directory lease, duplicate open, deferred cleanup and guard contention tests. Do not newly promise multi-database atomicity in WAL mode; SQLite documents its limitation in [WAL mode](https://www.sqlite.org/wal.html).
- Garbage collection, quota exhaustion, unknown format, corrupt/missing referenced chunks, delete/recreate, and migration restart/downgrade gating.

From sqlite-wasm-rs root, use the existing supported checks:

```sh
cargo check -p sqlite-wasm-vfs --lib --target wasm32-unknown-unknown --features opfs-jspi
cargo check -p sqlite-wasm-vfs --lib --target wasm32-unknown-unknown --all-features
wasm-pack build --target web examples/opfs-jspi
node examples/opfs-jspi/run.mjs
```

The browser runner requires Node 20+, Chrome with JSPI, and matching ChromeDriver; `CHROME_BIN` and `CHROMEDRIVER` select binaries. The example already disables wasm-opt in its release profile. Run `cargo test -p rsqlite-vfs` if changing shared Rust VFS behavior. Use core-crypto's checked-in build/test instructions for its target branch; the attachment's scratch commands are not a portable test harness.

## Ordinary test timings and completion criteria

The user explicitly prefers regular test runtimes over the full benchmark suite because the latter takes too long. **Use `RUSTC_WRAPPER= make ts-browser-test` as the default performance signal. Do not run the full benchmarks, create a benchmark harness, or require sustained sweeps as a completion gate.** Existing per-test and total timings are sufficient for this task.

Record the command, both source revisions, actual bundled VFS revision, build mode, browser/Bun versions, pass/fail count, total runtime and selected comparable test durations. Useful existing cases include `should allow sending messages`, `should allow inviting members`, `open previously created db works`, `key update works`, and initialization. Retain FULL and the same environment when comparing outputs. If no comparable pre-change output exists, state that; do not manufacture a speedup from the historical CreateMessage benchmark.

These are approximate end-to-end test timings, including test setup and unrelated work. They cannot establish a precise per-commit floor, p99, or removal of a database-size slope. Make only the claims they support. A single successful run is the normal validation pass; repeat only for a failure, material code change, or anomalous result.

Correctness tests remain required for the behavior being changed. `make ts-browser-test` does not execute the Rust `#[wasm_bindgen_test]` policy test or sqlite-wasm-rs's standalone example suite. Obtain/retain their own CI evidence or run the specific relevant suite with repository-required approval. The keystore wasm test command is documented in `.github/workflows/rust.yml` (including `CARGO_PROFILE_TEST_STRIP=symbols` and `WASM_BINDGEN_EXTERNREF=1`); do not substitute a native test run for it.

For a later chunk prototype, assert I/O structure in targeted correctness tests: a fixed dirty set touches only expected chunks plus manifest, and a no-op sync publishes nothing. No large benchmark matrix is needed. Do not claim the O(file size) problem is fixed by the current cache change; it still opens a whole-file writable stream.

The receiving agent's final handoff must include commits, exact commands/results, saved ordinary test output or CI links, comparable timing summaries where available, dependency-pin and format/migration status, and unresolved limitations. Mark implemented, integrated and validated as separate states.

If the measured two-round chunk design still misses requirements, the next decision is either a reviewed in-place chunk protocol with explicit crash/length semantics, or moving the keystore/core-crypto execution boundary into a worker using the existing SAH backend. The latter requires an API/ownership/message-overhead design and equivalent application tests; it is a separate architectural change, not an automatic follow-up to this plan.
