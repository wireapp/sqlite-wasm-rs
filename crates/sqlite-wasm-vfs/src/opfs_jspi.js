// Each VFS owns a dedicated directory. All clients of that directory must use
// this Web Lock protocol; it does not exclude unrelated OPFS users.
export async function acquire(directory) {
    if (!globalThis.navigator?.storage?.getDirectory || !navigator.locks ||
        !WebAssembly.Suspending || !WebAssembly.promising) {
        throw new Error('OPFS, Web Locks and JSPI are required in a secure context');
    }
    let release;
    const held = new Promise(resolve => { release = resolve; });
    let acquired;
    let rejected;
    const ready = new Promise((resolve, reject) => { acquired = resolve; rejected = reject; });
    const finished = navigator.locks.request(`sqlite-wasm-vfs:opfs-jspi:${directory}`,
        { mode: 'exclusive', ifAvailable: true }, async lock => {
            if (!lock) throw new DOMException('OPFS directory is in use', 'NoModificationAllowedError');
            acquired();
            await held;
        });
    finished.catch(rejected);
    await ready;
    try {
        let root = await navigator.storage.getDirectory();
        for (const part of directory.split('/')) {
            root = await root.getDirectoryHandle(part, { create: true });
        }
        return { root, release, finished, files: new Set(), metadata: new Map() };
    } catch (error) {
        release();
        await finished;
        throw error;
    }
}

export function release(lease) { lease.release(); return lease.finished; }

// Opt-in, in-memory I/O samples. Callers set this to an array and read it once
// after timing; samples contain sizes and roles, never file contents or names.
function metricStart() {
    return Array.isArray(globalThis.__sqliteWasmVfsMetrics) ? performance.now() : undefined;
}

function recordMetric(state, operation, started, details = {}) {
    if (started === undefined) return;
    const samples = globalThis.__sqliteWasmVfsMetrics;
    if (Array.isArray(samples)) {
        // Measurement must never change the outcome of a SQLite I/O call.
        try {
            samples.push({ operation, role: state.role, durationMs: performance.now() - started, ...details });
        } catch (_) { /* ignore a broken optional collector */ }
    }
}

async function measureStreamOperation(state, operation, details, action) {
    const started = metricStart();
    try {
        await action();
        recordMetric(state, operation, started, { ...details, success: true });
    } catch (error) {
        recordMetric(state, operation, started, { ...details, success: false });
        throw error;
    }
}

export async function open(lease, name, create, exclusive, role, group) {
    if (exclusive && await exists(lease, name)) {
        throw new DOMException('File already exists', 'InvalidModificationError');
    }
    const fileHandle = await lease.root.getFileHandle(name, { create });
    const file = await fileHandle.getFile();
    group ||= undefined;
    const state = {
        lease, fileHandle, name, role, group, file,
        stream: undefined, failed: undefined,
        baseLimit: file.size, logicalSize: file.size,
        stagedSize: file.size, segments: [], dirty: [],
        sizeDirty: false, shrinkFloor: undefined,
        publicationChanges: [], publicationTailFloor: undefined,
        cache: new Map(), cacheBytes: 0,
    };
    lease.metadata.set(name, { role, group });
    lease.files.add(state);
    return state;
}

export async function exists(lease, name) {
    try {
        await lease.root.getFileHandle(name);
        return true;
    } catch (error) {
        if (error.name === 'NotFoundError') return false;
        throw error;
    }
}

function sameGroup(left, right) {
    return left.group !== undefined && left.group === right.group;
}

function isSidecar(state) {
    return state.role === 'journal' || state.role === 'wal';
}

async function deletionBarrier(lease, metadata) {
    if (!metadata || metadata.role === 'super-journal' || !metadata.group) {
        await publishAll(lease, () => true, 'deletion');
        return;
    }
    await publishAll(lease, state => state.group === metadata.group, 'deletion');
}

export async function remove(lease, name) {
    // Metadata recorded at xOpen scopes ordinary sidecars without guessing from
    // an encoded filename. Unknown and super-journal deletes stay conservative.
    await deletionBarrier(lease, lease.metadata.get(name));
    await lease.root.removeEntry(name);
    lease.metadata.delete(name);
}

function check(state) {
    if (state.failed) throw state.failed;
}

async function ensureStream(state) {
    check(state);
    if (!state.stream) {
        const started = metricStart();
        try {
            state.stream = await state.fileHandle.createWritable({ keepExistingData: true });
            recordMetric(state, 'streamCreate', started, { logicalSize: state.logicalSize, success: true });
        } catch (error) {
            recordMetric(state, 'streamCreate', started, { logicalSize: state.logicalSize, success: false });
            throw error;
        }
    }
    return state.stream;
}

async function poison(state, error) {
    if (state.stream) {
        try { await state.stream.abort(); } catch (_) { /* preserve the original error */ }
    }
    state.stream = undefined;
    state.failed = error;
    throw error;
}

function intervalUnion(intervals, start, end) {
    if (start >= end) return intervals;
    const result = [];
    let inserted = false;
    for (const range of intervals) {
        if (range.end < start) result.push(range);
        else if (end < range.start) {
            if (!inserted) { result.push({ start, end }); inserted = true; }
            result.push(range);
        } else {
            start = Math.min(start, range.start);
            end = Math.max(end, range.end);
        }
    }
    if (!inserted) result.push({ start, end });
    return result;
}

function clipIntervals(intervals, end) {
    return intervals.flatMap(range => range.start >= end ? [] : [{
        start: range.start, end: Math.min(range.end, end),
    }]);
}

function replaceSegment(state, offset, bytes) {
    const end = offset + bytes.byteLength;
    const replacement = [];
    let inserted = false;
    for (const segment of state.segments) {
        const segmentEnd = segment.start + segment.bytes.byteLength;
        if (segmentEnd <= offset) {
            replacement.push(segment);
        } else if (segment.start >= end) {
            if (!inserted) { replacement.push({ start: offset, bytes }); inserted = true; }
            replacement.push(segment);
        } else {
            if (segment.start < offset) {
                replacement.push({ start: segment.start, bytes: segment.bytes.slice(0, offset - segment.start) });
            }
            if (segmentEnd > end) {
                if (!inserted) { replacement.push({ start: offset, bytes }); inserted = true; }
                replacement.push({ start: end, bytes: segment.bytes.slice(end - segment.start) });
            }
        }
    }
    if (!inserted) replacement.push({ start: offset, bytes });
    state.segments = replacement;
}

function truncateSegments(state, length) {
    const replacement = [];
    for (const segment of state.segments) {
        if (segment.start >= length) break;
        const count = Math.min(segment.bytes.byteLength, length - segment.start);
        replacement.push(count === segment.bytes.byteLength ? segment : {
            start: segment.start, bytes: segment.bytes.slice(0, count),
        });
    }
    state.segments = replacement;
}

function materialize(state, start, end) {
    const bytes = new Uint8Array(end - start);
    let covered = start;
    for (const segment of state.segments) {
        const segmentEnd = segment.start + segment.bytes.byteLength;
        if (segmentEnd <= start) continue;
        if (segment.start >= end) break;
        const from = Math.max(start, segment.start);
        const to = Math.min(end, segmentEnd);
        if (from > covered) throw new Error('dirty range is not covered by pending data');
        bytes.set(segment.bytes.subarray(from - segment.start, to - segment.start), from - start);
        covered = Math.max(covered, to);
    }
    if (covered < end) throw new Error('dirty range is not covered by pending data');
    return bytes;
}

function dirtyBytes(state) {
    return state.dirty.reduce((sum, range) => sum + range.end - range.start, 0);
}

async function submit(state) {
    check(state);
    if (!state.sizeDirty && !state.dirty.length) return;
    try {
        const stream = await ensureStream(state);
        let stagedSize = state.stagedSize;
        if (state.shrinkFloor !== undefined) {
            await measureStreamOperation(state, 'streamTruncate', { length: state.shrinkFloor },
                () => stream.truncate(state.shrinkFloor));
            stagedSize = state.shrinkFloor;
        }
        if (state.sizeDirty && stagedSize !== state.logicalSize) {
            await measureStreamOperation(state, 'streamTruncate', { length: state.logicalSize },
                () => stream.truncate(state.logicalSize));
            stagedSize = state.logicalSize;
        }
        for (const range of state.dirty) {
            const bytes = materialize(state, range.start, range.end);
            await measureStreamOperation(state, 'streamWrite', { bytes: bytes.byteLength },
                () => stream.write({ type: 'write', position: range.start, data: bytes }));
            stagedSize = Math.max(stagedSize, range.end);
        }
        state.stagedSize = state.logicalSize;
        state.dirty = [];
        state.sizeDirty = false;
        state.shrinkFloor = undefined;
    } catch (error) {
        await poison(state, error);
    }
}

function hasPending(state) {
    return state.stream || state.sizeDirty || state.dirty.length;
}

function clearCache(state) {
    state.cache.clear();
    state.cacheBytes = 0;
}

function invalidatePublishedCache(state) {
    const blockSize = globalThis.__sqliteWasmVfsReadBlockSize ?? 64 * 1024;
    let evicted = 0;
    for (const [start, block] of state.cache) {
        const end = start + blockSize;
        if ((state.publicationTailFloor !== undefined && end > state.publicationTailFloor) ||
            state.publicationChanges.some(range => start < range.end && end > range.start)) {
            state.cache.delete(start);
            state.cacheBytes -= block.byteLength;
            evicted++;
        }
    }
    return evicted;
}

async function publish(state, reason) {
    check(state);
    if (!hasPending(state)) return;
    const started = metricStart();
    try {
        await submit(state);
    } catch (error) {
        recordMetric(state, 'publish', started, { reason, logicalSize: state.logicalSize, success: false });
        throw error;
    }
    const stream = state.stream;
    if (!stream) return;
    const closeStarted = metricStart();
    let closed = false;
    try {
        await stream.close();
        closed = true;
        recordMetric(state, 'streamClose', closeStarted, { success: true });
        state.stream = undefined;
        state.baseLimit = state.logicalSize;
        state.stagedSize = state.logicalSize;
        state.file = undefined;
        const evictedBlocks = invalidatePublishedCache(state);
        recordMetric(state, 'publish', started, {
            reason, logicalSize: state.logicalSize,
            dirtyBytes: state.publicationChanges.reduce((sum, range) => sum + range.end - range.start, 0),
            touchedChunks: null, evictedBlocks, cacheBytes: state.cacheBytes, success: true,
        });
        state.publicationChanges = [];
        state.publicationTailFloor = undefined;
        state.segments = [];
    } catch (error) {
        if (!closed) recordMetric(state, 'streamClose', closeStarted, { success: false });
        recordMetric(state, 'publish', started, { reason, logicalSize: state.logicalSize, success: false });
        await poison(state, error);
    }
}

async function publishAll(lease, predicate = () => true, reason = 'barrier') {
    for (const state of lease.files) {
        if (hasPending(state) && predicate(state)) await publish(state, reason);
    }
}

async function phaseBarrier(state) {
    if (state.role === 'super-journal') {
        await publishAll(state.lease, () => true, 'phase');
    } else if ((state.role === 'database' || isSidecar(state)) && !state.group) {
        await publishAll(state.lease, other => state.role === 'database' ? isSidecar(other) : other.role === 'database', 'phase');
    } else if (state.role === 'database') {
        await publishAll(state.lease, other => sameGroup(state, other) && isSidecar(other), 'phase');
    } else if (isSidecar(state)) {
        await publishAll(state.lease, other => sameGroup(state, other) && other.role === 'database', 'phase');
    }
}

function missingBaseRanges(state, start, end) {
    const result = [];
    let cursor = start;
    const baseEnd = Math.min(end, state.baseLimit);
    for (const segment of state.segments) {
        const segmentEnd = segment.start + segment.bytes.byteLength;
        if (segmentEnd <= cursor) continue;
        if (segment.start >= baseEnd) break;
        if (segment.start > cursor) result.push({ start: cursor, end: Math.min(segment.start, baseEnd) });
        cursor = Math.max(cursor, segmentEnd);
        if (cursor >= baseEnd) break;
    }
    if (cursor < baseEnd) result.push({ start: cursor, end: baseEnd });
    return result;
}

async function readPublished(state, target, start, end, targetOffset) {
    if (!state.file) {
        const started = metricStart();
        state.file = await state.fileHandle.getFile();
        recordMetric(state, 'readSnapshot', started, { logicalSize: state.baseLimit });
    }
    const blockSize = globalThis.__sqliteWasmVfsReadBlockSize ?? 64 * 1024;
    const cacheLimit = globalThis.__sqliteWasmVfsReadCacheBytes ?? 8 * 1024 * 1024;
    for (let blockStart = Math.floor(start / blockSize) * blockSize;
        blockStart < end; blockStart += blockSize) {
        let block = state.cache.get(blockStart);
        if (block) {
            state.cache.delete(blockStart);
            state.cache.set(blockStart, block);
        } else {
            const blockEnd = Math.min(state.baseLimit, blockStart + blockSize);
            const started = metricStart();
            block = new Uint8Array(await state.file.slice(blockStart, blockEnd).arrayBuffer());
            recordMetric(state, 'cacheMiss', started, { bytes: block.byteLength, cacheBytes: state.cacheBytes });
            while (state.cacheBytes + block.byteLength > cacheLimit && state.cache.size) {
                const oldest = state.cache.keys().next().value;
                const removed = state.cache.get(oldest).byteLength;
                state.cache.delete(oldest);
                state.cacheBytes -= removed;
            }
            if (block.byteLength <= cacheLimit) {
                state.cache.set(blockStart, block);
                state.cacheBytes += block.byteLength;
            }
        }
        const from = Math.max(start, blockStart);
        const to = Math.min(end, blockStart + block.byteLength);
        target.set(block.subarray(from - blockStart, to - blockStart), targetOffset + from - start);
    }
}

export async function read(state, offset, length) {
    check(state);
    const started = metricStart();
    const count = Math.max(0, Math.min(length, state.logicalSize - offset));
    if (!count) {
        recordMetric(state, 'read', started, { bytes: 0 });
        return new Uint8Array();
    }
    const end = offset + count;
    const bytes = new Uint8Array(count);
    const missing = missingBaseRanges(state, offset, end);
    for (const range of missing) {
        await readPublished(state, bytes, range.start, range.end, range.start - offset);
    }
    for (const segment of state.segments) {
        const segmentEnd = segment.start + segment.bytes.byteLength;
        if (segmentEnd <= offset) continue;
        if (segment.start >= end) break;
        const from = Math.max(offset, segment.start);
        const to = Math.min(end, segmentEnd);
        bytes.set(segment.bytes.subarray(from - segment.start, to - segment.start), from - offset);
    }
    recordMetric(state, 'read', started, { bytes: count });
    return bytes;
}

export function size(state) {
    check(state);
    return Promise.resolve(state.logicalSize);
}

export async function write(state, offset, bytes) {
    check(state);
    await phaseBarrier(state);
    // Rust passes Uint8Array::from(&[u8]), which is already an owned JS copy.
    // It remains valid across this and later JSPI suspensions.
    try {
        replaceSegment(state, offset, bytes);
        state.dirty = intervalUnion(state.dirty, offset, offset + bytes.byteLength);
        state.publicationChanges = intervalUnion(state.publicationChanges, offset, offset + bytes.byteLength);
        if (offset + bytes.byteLength > state.logicalSize) {
            state.publicationTailFloor = Math.min(state.publicationTailFloor ?? state.logicalSize, state.logicalSize);
        }
        state.logicalSize = Math.max(state.logicalSize, offset + bytes.byteLength);
        if (dirtyBytes(state) >= 1024 * 1024) await submit(state);
    } catch (error) {
        if (state.stream) await poison(state, error);
        state.failed = error;
        throw error;
    }
}

export async function truncate(state, length) {
    check(state);
    await phaseBarrier(state);
    try {
        const previousSize = state.logicalSize;
        if (length < state.logicalSize) {
            state.publicationTailFloor = Math.min(state.publicationTailFloor ?? length, length);
            state.baseLimit = Math.min(state.baseLimit, length);
            state.shrinkFloor = Math.min(state.shrinkFloor ?? length, length);
            truncateSegments(state, length);
            state.dirty = clipIntervals(state.dirty, length);
        }
        if (length > state.logicalSize) {
            state.publicationTailFloor = Math.min(state.publicationTailFloor ?? state.logicalSize, state.logicalSize);
        }
        state.logicalSize = length;
        state.sizeDirty = true;
        // SQLite's TRUNCATE checkpoint does not necessarily call xSync after
        // xTruncate on the WAL. Publish its zero length before reporting that
        // the checkpoint has completed, after the database phase barrier.
        if (state.role === 'wal' && length === 0 && previousSize !== 0) {
            await publish(state, 'truncate');
        }
    } catch (error) {
        if (state.stream) await poison(state, error);
        state.failed = error;
        throw error;
    }
}

export function sync(state) { return publish(state, 'sync'); }

export async function close(state) {
    try {
        await phaseBarrier(state);
        await publish(state, 'close');
    } finally {
        state.segments = [];
        clearCache(state);
        state.lease.files.delete(state);
    }
}

export function scheduleSqliteCleanup(exports) {
    Promise.resolve()
        .then(() => WebAssembly.promising(exports.sqlite_wasm_vfs_sqlite_cleanup)())
        .catch(error => console.error('sqlite-wasm-vfs cleanup failed', error));
}

export async function sleep(milliseconds) {
    const until = performance.now() + milliseconds;
    while (performance.now() < until) {
        await new Promise(resolve => setTimeout(resolve, Math.min(until - performance.now(), 2147483647)));
    }
}
