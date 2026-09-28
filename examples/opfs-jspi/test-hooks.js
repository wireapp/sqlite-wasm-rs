// Test-only fault injection. Never included in the VFS package.
let originalArrayBuffer;
let coldReads = 0;
let metricStartIndex = 0;
let ownsMetricCollector = false;

export function startVfsMetrics() {
    ownsMetricCollector = !Array.isArray(globalThis.__sqliteWasmVfsMetrics);
    if (ownsMetricCollector) globalThis.__sqliteWasmVfsMetrics = [];
    metricStartIndex = globalThis.__sqliteWasmVfsMetrics.length;
}

export function takeVfsMetricOperations() {
    const samples = globalThis.__sqliteWasmVfsMetrics.slice(metricStartIndex);
    if (ownsMetricCollector) delete globalThis.__sqliteWasmVfsMetrics;
    return samples.map(sample => sample.operation + (sample.reason ? ':' + sample.reason : '')).join(',');
}

export function takeVfsMetricOutcomes() {
    const samples = globalThis.__sqliteWasmVfsMetrics.slice(metricStartIndex);
    if (ownsMetricCollector) delete globalThis.__sqliteWasmVfsMetrics;
    return samples.map(sample => sample.operation +
        (sample.reason ? ':' + sample.reason : '') + ':' + sample.success).join(',');
}

export function startColdReadCount() {
    if (originalArrayBuffer) throw new Error('cold read counter already started');
    coldReads = 0;
    originalArrayBuffer = Blob.prototype.arrayBuffer;
    Blob.prototype.arrayBuffer = function (...args) {
        coldReads++;
        return originalArrayBuffer.apply(this, args);
    };
}

export function takeColdReadCount() {
    const count = coldReads;
    coldReads = 0;
    return count;
}

export function stopColdReadCount() {
    Blob.prototype.arrayBuffer = originalArrayBuffer;
    originalArrayBuffer = undefined;
}

export function failNextWrite() {
    const original = FileSystemFileHandle.prototype.createWritable;
    FileSystemFileHandle.prototype.createWritable = function (...args) {
        FileSystemFileHandle.prototype.createWritable = original;
        return Promise.reject(new DOMException('injected quota failure', 'QuotaExceededError'));
    };
}

export function failFileCreation(name) {
    const original = FileSystemFileHandle.prototype.createWritable;
    const target = 'f-' + Array.from(new TextEncoder().encode(name), b =>
        b.toString(16).padStart(2, '0')).join('');
    FileSystemFileHandle.prototype.createWritable = function (...args) {
        if (this.name === target) {
            FileSystemFileHandle.prototype.createWritable = original;
            return Promise.reject(new DOMException('injected quota failure', 'QuotaExceededError'));
        }
        return original.apply(this, args);
    };
}

function failNextStreamMethod(method, message) {
    const original = FileSystemFileHandle.prototype.createWritable;
    FileSystemFileHandle.prototype.createWritable = async function (...args) {
        FileSystemFileHandle.prototype.createWritable = original;
        const stream = await original.apply(this, args);
        const operation = stream[method].bind(stream);
        let failed = false;
        stream[method] = async (...operationArgs) => {
            if (!failed) {
                failed = true;
                throw new DOMException(message, 'UnknownError');
            }
            return operation(...operationArgs);
        };
        return stream;
    };
}

export function failNextStreamWrite() {
    failNextStreamMethod('write', 'injected stream write failure');
}

export function failNextStreamTruncate() {
    failNextStreamMethod('truncate', 'injected stream truncate failure');
}

export function crashAfterDatabaseWrite() {
    crashAfterWrite('recovery.db', 'pending');
}

export function crashDuringWalCheckpoint() {
    crashAfterWrite('wal-recovery.db', 'wal-pending');
}

function crashAfterWrite(name, stage) {
    const original = FileSystemFileHandle.prototype.createWritable;
    const database = 'f-' + Array.from(new TextEncoder().encode(name), b => b.toString(16).padStart(2, '0')).join('');
    FileSystemFileHandle.prototype.createWritable = async function (...args) {
        const stream = await original.apply(this, args);
        if (this.name === database) {
            const close = stream.close.bind(stream);
            const write = stream.write.bind(stream);
            let dataPage = false;
            stream.write = async chunk => {
                // Coalescing may combine page zero and later pages into one
                // position-zero write, so inspect the covered range.
                dataPage ||= chunk.position > 0 || chunk.position + chunk.data.byteLength > 4096;
                return await write(chunk);
            };
            stream.close = async () => {
                await close();
                if (!dataPage) return;
                // Abandon the active SQLite stack after a database page reached
                // OPFS, before SQLite can finish its transaction or roll it back.
                sessionStorage.setItem('jspi-recovery', stage);
                location.reload();
                return new Promise(() => {});
            };
        }
        return stream;
    };
}

export function failDatabaseClose() {
    const original = FileSystemFileHandle.prototype.createWritable;
    const database = 'f-' + Array.from(new TextEncoder().encode('failure.db'), b => b.toString(16).padStart(2, '0')).join('');
    FileSystemFileHandle.prototype.createWritable = async function (...args) {
        const stream = await original.apply(this, args);
        if (this.name === database) {
            FileSystemFileHandle.prototype.createWritable = original;
            const close = stream.close.bind(stream);
            stream.close = async () => {
                await close();
                throw new DOMException('injected error after publication', 'UnknownError');
            };
        }
        return stream;
    };
}

export function delay(milliseconds) {
    return new Promise(resolve => setTimeout(resolve, milliseconds));
}
