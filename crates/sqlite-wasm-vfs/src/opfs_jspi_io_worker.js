// OPFS operations only. SQLite and CoreCrypto stay in the calling thread.
export function protocolVersion() { return 1; }

export function workerMain() {
    let root;
    const handles = new Map();
    const byName = new Map();
    let nextHandle = 1;

    async function existing(name) {
        try { return await root.getFileHandle(name); }
        catch (error) { if (error.name === 'NotFoundError') return undefined; throw error; }
    }

    async function dispatch(request) {
        const { operation, args } = request;
        if (operation === 'init') {
            root = await navigator.storage.getDirectory();
            for (const component of args.directory.split('/')) {
                root = await root.getDirectoryHandle(component, { create: true });
            }
            return true;
        }
        if (!root) throw new Error('OPFS worker is not initialized');
        if (operation === 'exists') return !!(await existing(args.name));
        if (operation === 'open') {
            let file = await existing(args.name);
            if (file && args.exclusive) throw new DOMException('File already exists', 'InvalidModificationError');
            if (!file) file = await root.getFileHandle(args.name, { create: args.create });
            let entry = byName.get(args.name);
            if (!entry) {
                entry = { handle: await file.createSyncAccessHandle(), count: 0, name: args.name };
                byName.set(args.name, entry);
            }
            entry.count++;
            const id = nextHandle++;
            handles.set(id, entry);
            return { id, size: entry.handle.getSize() };
        }
        if (operation === 'remove') {
            await root.removeEntry(args.name);
            return true;
        }
        if (operation === 'release') {
            let failure;
            for (const entry of byName.values()) {
                try { entry.handle.close(); }
                catch (error) { failure ??= error; }
            }
            handles.clear();
            byName.clear();
            if (failure) throw failure;
            return true;
        }
        const entry = handles.get(args.id);
        if (!entry) throw new Error('invalid OPFS worker file handle');
        const handle = entry.handle;
        if (operation === 'read') {
            const result = new Uint8Array(args.length);
            const count = handle.read(result, { at: args.offset });
            return result.subarray(0, count);
        }
        if (operation === 'publish') {
            if (args.shrinkFloor !== undefined) handle.truncate(args.shrinkFloor);
            if (handle.getSize() !== args.length) handle.truncate(args.length);
            for (const range of args.ranges) {
                let written = 0;
                while (written < range.bytes.byteLength) {
                    const count = handle.write(range.bytes.subarray(written), { at: range.start + written });
                    if (!count) throw new Error('zero-byte OPFS write');
                    written += count;
                }
            }
            handle.flush();
            return true;
        }
        if (operation === 'close') {
            let failure;
            try { handle.flush(); }
            catch (error) { failure = error; }
            handles.delete(args.id);
            if (--entry.count === 0) {
                try { handle.close(); }
                catch (error) { failure ??= error; }
                byName.delete(entry.name);
            }
            if (failure) throw failure;
            return true;
        }
        throw new Error(`unknown OPFS worker operation: ${operation}`);
    }

    self.onmessage = async event => {
        const { id } = event.data;
        try {
            const value = await dispatch(event.data);
            const transfer = value instanceof Uint8Array ? [value.buffer] : [];
            self.postMessage({ id, value }, transfer);
        } catch (error) {
            self.postMessage({ id, error: { name: error.name, message: error.message } });
        }
    };
}
