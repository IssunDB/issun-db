// The engine, off the main thread.
//
// A wasm call cannot yield, so running the module on the page's own thread froze the tab for the
// whole of a query: no repaint, no scrolling, no way to give up. That was tolerable while the
// sample graphs were the only data, and stopped being so once the analytics procedures landed,
// since an all-pairs pass over a graph loaded from a share link runs for as long as it runs.
//
// The engine therefore owns this worker and the page owns nothing but a promise per call. The
// graph lives here too, which is what makes termination the only possible cancel: see the note on
// the page side.

import init, {Playground} from "./pkg/issundb_wasm.js";

let db = null;
let wasmMemory = null;
let baseline = 0;

/// A build without the allocation counter still answers everything else, so a missing figure is
/// reported as zero rather than failing the call that asked.
function liveBytes() {
    try {
        return Playground.memoryBytes();
    } catch {
        return 0;
    }
}

function memory() {
    return {live: liveBytes(), heap: wasmMemory?.buffer?.byteLength ?? 0, baseline};
}

const OPS = {
    async boot() {
        const exports = await init();
        // The heap figure is the browser's, and only this side holds the module's memory object.
        wasmMemory = exports.memory;
        db = new Playground();
        baseline = liveBytes();
        let build = "";
        try {
            build = Playground.buildRef();
        } catch {
            // A module built outside a git checkout carries no stamp.
        }
        return {version: Playground.version(), build, ...memory()};
    },

    reset() {
        // Freed rather than abandoned. wasm-bindgen registers a finalizer, so an abandoned instance is
        // reclaimed eventually, but until then its whole graph is still resident and wasm memory never
        // shrinks. The new instance is built first, so a failure leaves the old one usable.
        const previous = db;
        db = new Playground();
        previous?.free();
        // After the old instance is freed, so the baseline is one empty database rather than two.
        baseline = liveBytes();
        return memory();
    },

    query: (cypher) => db.query(cypher),
    explain: (cypher) => db.explain(cypher),
    stats: () => db.stats(),
    graphSnapshot: () => db.graphSnapshot(),
    createTextIndex: (label, property) => void db.createTextIndex(label, property),
    textSearch: (query, k) => db.textSearch(query, k),
    upsertVector: (id, vector) => void db.upsertVector(id, vector),
    vectorSearch: (vector, k) => db.vectorSearch(vector, k),
    memory: () => memory(),
};

self.onmessage = async ({data: {id, op, args}}) => {
    try {
        const handler = OPS[op];
        if (!handler) throw new Error(`unknown engine operation: ${op}`);
        self.postMessage({id, ok: true, value: await handler(...(args ?? []))});
    } catch (e) {
        // Only the message survives structured cloning, and it is the whole of what the page shows.
        self.postMessage({id, ok: false, error: String(e?.message ?? e)});
    }
};
