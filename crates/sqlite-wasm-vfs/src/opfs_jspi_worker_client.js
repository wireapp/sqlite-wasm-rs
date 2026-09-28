// JSPI bridge to an I/O-only worker. SQLite and its guard remain on Window.
import { workerMain } from './opfs_jspi_io_worker.js';
export function clientProtocolVersion() { return 1; }

function rpc(lease, operation, args = {}) {
    if (lease.workerFailed) return Promise.reject(lease.workerFailed);
    const id = ++lease.nextRequest;
    return new Promise((resolve, reject) => {
        lease.pending.set(id, { resolve, reject });
        lease.ioWorker.postMessage({ id, operation, args });
    });
}

export async function start(lease, directory) {
    // Inline the self-contained worker function so single-file browser bundles
    // do not need to serve a separate JS asset at a bundle-specific URL.
    const url = URL.createObjectURL(new Blob([`(${workerMain.toString()})();`], { type: 'text/javascript' }));
    try { lease.ioWorker = new Worker(url); }
    finally { URL.revokeObjectURL(url); }
    lease.nextRequest = 0;
    lease.pending = new Map();
    lease.workerFailed = undefined;
    lease.ioWorker.onmessage = event => {
        const { id, value, error } = event.data;
        const pending = lease.pending.get(id);
        if (!pending) return;
        lease.pending.delete(id);
        if (error) pending.reject(new DOMException(error.message, error.name));
        else pending.resolve(value);
    };
    lease.ioWorker.onerror = event => {
        lease.workerFailed = new Error(event.message || 'OPFS worker failed');
        for (const pending of lease.pending.values()) pending.reject(lease.workerFailed);
        lease.pending.clear();
    };
    lease.ioWorker.onmessageerror = () => {
        lease.workerFailed = new Error('OPFS worker message could not be decoded');
        for (const pending of lease.pending.values()) pending.reject(lease.workerFailed);
        lease.pending.clear();
    };
    await rpc(lease, 'init', { directory: `${directory}/worker-v1` });
}

export async function release(lease) {
    try { if (!lease.workerFailed) await rpc(lease, 'release'); }
    finally { lease.ioWorker.terminate(); }
}

export function exists(lease, name) { return rpc(lease, 'exists', { name }); }

export async function open(lease, name, create, exclusive, role, group) {
    const { id, size } = await rpc(lease, 'open', { name, create, exclusive });
    const state = {
        worker: true, lease, id, name, role, group: group || undefined,
        baseLimit: size, logicalSize: size, segments: [], dirty: [],
        shrinkFloor: undefined, sizeDirty: false, failed: undefined,
        cache: new Map(), cacheBytes: 0,
    };
    lease.metadata.set(name, { role, group: state.group });
    lease.files.add(state);
    return state;
}

export function remove(lease, name) { return rpc(lease, 'remove', { name }); }
export function hasPending(state) { return state.sizeDirty || state.dirty.length !== 0; }
function check(state) { if (state.failed) throw state.failed; }

function addRange(ranges, start, end) {
    if (start >= end) return ranges;
    const result = [];
    let inserted = false;
    for (const range of ranges) {
        if (range.end < start) result.push(range);
        else if (end < range.start) {
            if (!inserted) { result.push({ start, end }); inserted = true; }
            result.push(range);
        } else { start = Math.min(start, range.start); end = Math.max(end, range.end); }
    }
    if (!inserted) result.push({ start, end });
    return result;
}

function replaceSegment(state, start, bytes) {
    const end = start + bytes.byteLength;
    const result = [];
    let inserted = false;
    for (const segment of state.segments) {
        const tail = segment.start + segment.bytes.byteLength;
        if (tail <= start) result.push(segment);
        else if (segment.start >= end) {
            if (!inserted) { result.push({ start, bytes }); inserted = true; }
            result.push(segment);
        } else {
            if (segment.start < start) result.push({ start: segment.start, bytes: segment.bytes.slice(0, start - segment.start) });
            if (tail > end) {
                if (!inserted) { result.push({ start, bytes }); inserted = true; }
                result.push({ start: end, bytes: segment.bytes.slice(end - segment.start) });
            }
        }
    }
    if (!inserted) result.push({ start, bytes });
    state.segments = result;
}

export function write(state, offset, bytes) {
    check(state);
    try {
        replaceSegment(state, offset, bytes);
        state.dirty = addRange(state.dirty, offset, offset + bytes.byteLength);
        if (offset + bytes.byteLength > state.logicalSize) state.sizeDirty = true;
        state.logicalSize = Math.max(state.logicalSize, offset + bytes.byteLength);
    } catch (error) { state.failed = error; throw error; }
}

export function truncate(state, length) {
    check(state);
    if (length === state.logicalSize) return;
    if (length < state.logicalSize) {
        state.shrinkFloor = Math.min(state.shrinkFloor ?? length, length);
        state.segments = state.segments.flatMap(segment => {
            if (segment.start >= length) return [];
            const count = Math.min(segment.bytes.byteLength, length - segment.start);
            return [{ start: segment.start, bytes: segment.bytes.subarray(0, count) }];
        });
        state.dirty = state.dirty.flatMap(range => range.start >= length ? [] :
            [{ start: range.start, end: Math.min(range.end, length) }]);
    }
    state.logicalSize = length;
    state.sizeDirty = true;
}

function materialize(state, start, end) {
    const bytes = new Uint8Array(end - start);
    let covered = start;
    for (const segment of state.segments) {
        const tail = segment.start + segment.bytes.byteLength;
        if (tail <= start) continue;
        if (segment.start >= end) break;
        const from = Math.max(start, segment.start);
        const to = Math.min(end, tail);
        if (from > covered) throw new Error('dirty range is not covered by pending data');
        bytes.set(segment.bytes.subarray(from - segment.start, to - segment.start), from - start);
        covered = Math.max(covered, to);
    }
    if (covered < end) throw new Error('dirty range is not covered by pending data');
    return bytes;
}

function metric(state, operation, started, details) {
    const samples = globalThis.__sqliteWasmVfsMetrics;
    if (Array.isArray(samples)) {
        try { samples.push({ operation, role: state.role, durationMs: performance.now() - started, ...details }); }
        catch (_) { /* instrumentation is optional */ }
    }
}

export async function publish(state, reason) {
    check(state);
    if (!hasPending(state)) return;
    const started = performance.now();
    try {
        const ranges = state.dirty.map(range => ({ start: range.start,
            bytes: materialize(state, range.start, range.end) }));
        await rpc(state.lease, 'publish', { id: state.id, length: state.logicalSize,
            shrinkFloor: state.shrinkFloor, ranges });
        const crashStage = globalThis.__sqliteWasmVfsCrashWorkerDatabasePublish;
        if (crashStage && state.role === 'database') {
            delete globalThis.__sqliteWasmVfsCrashWorkerDatabasePublish;
            sessionStorage.setItem('jspi-recovery', crashStage);
            location.reload();
            return await new Promise(() => {});
        }
        state.baseLimit = state.logicalSize;
        state.segments = [];
        state.dirty = [];
        state.shrinkFloor = undefined;
        state.sizeDirty = false;
        state.cache.clear();
        state.cacheBytes = 0;
        for (const other of state.lease.files) {
            if (other === state || other.name !== state.name) continue;
            other.cache.clear();
            other.cacheBytes = 0;
            if (!hasPending(other)) {
                other.baseLimit = state.logicalSize;
                other.logicalSize = state.logicalSize;
            }
        }
        metric(state, 'publish', started, { reason, logicalSize: state.logicalSize,
            dirtyBytes: ranges.reduce((sum, range) => sum + range.bytes.byteLength, 0), success: true });
    } catch (error) {
        state.failed = error;
        metric(state, 'publish', started, { reason, logicalSize: state.logicalSize, success: false });
        throw error;
    }
}

async function loadBlock(state, start) {
    let bytes = state.cache.get(start);
    if (bytes) {
        state.cache.delete(start);
        state.cache.set(start, bytes);
        return bytes;
    }
    const length = Math.min(64 * 1024, state.baseLimit - start);
    const started = performance.now();
    bytes = await rpc(state.lease, 'read', { id: state.id, offset: start, length });
    if (bytes.byteLength !== length) throw new Error('short OPFS worker read');
    metric(state, 'cacheMiss', started, { bytes: length });
    const limit = globalThis.__sqliteWasmVfsReadCacheBytes ?? 8 * 1024 * 1024;
    while (state.cacheBytes + length > limit && state.cache.size) {
        const oldest = state.cache.keys().next().value;
        state.cacheBytes -= state.cache.get(oldest).byteLength;
        state.cache.delete(oldest);
    }
    if (length <= limit) { state.cache.set(start, bytes); state.cacheBytes += length; }
    return bytes;
}

export async function read(state, offset, length) {
    check(state);
    const started = performance.now();
    const count = Math.max(0, Math.min(length, state.logicalSize - offset));
    const result = new Uint8Array(count);
    const baseEnd = Math.min(offset + count, state.baseLimit, state.shrinkFloor ?? state.baseLimit);
    for (let start = Math.floor(offset / (64 * 1024)) * (64 * 1024); start < baseEnd; start += 64 * 1024) {
        const bytes = await loadBlock(state, start);
        const from = Math.max(offset, start);
        const to = Math.min(baseEnd, start + bytes.byteLength);
        if (from < to) result.set(bytes.subarray(from - start, to - start), from - offset);
    }
    for (const segment of state.segments) {
        const from = Math.max(offset, segment.start);
        const to = Math.min(offset + count, segment.start + segment.bytes.byteLength);
        if (from < to) result.set(segment.bytes.subarray(from - segment.start, to - segment.start), from - offset);
    }
    metric(state, 'read', started, { bytes: count });
    return result;
}

export async function close(state) {
    try { await rpc(state.lease, 'close', { id: state.id }); }
    finally { state.cache.clear(); state.lease.files.delete(state); }
}
