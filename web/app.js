// The IssunDB playground. Vanilla ES modules, no build step, and no library fetched from a network;
// the two web fonts are the page's only external request. One `Playground` for the tab's lifetime,
// so data accumulates across queries the
// way it would in an embedded database; "Reset data" replaces it.
//
// The engine itself runs in `worker.js`, so everything here that reaches it is asynchronous.

import {DEMO_CATEGORIES, FUNCTIONS, PROCEDURES, SAMPLE_GRAPHS} from "./demos.js";
import {formatCypher} from "./format.js";

const $ = (id) => document.getElementById(id);

// ---------------------------------------------------------------------------
// Engine
// ---------------------------------------------------------------------------

let worker = null;
let nextCall = 0;
const pending = new Map();

function spawnWorker() {
    worker = new Worker(new URL("./worker.js", import.meta.url), {type: "module"});
    worker.onmessage = ({data: {id, ok, value, error}}) => {
        const entry = pending.get(id);
        if (!entry) return;
        pending.delete(id);
        if (ok) entry.resolve(value);
        else entry.reject(new Error(error));
    };
    // A worker whose module fails to evaluate never reaches its message handler, so without this
    // every call would stay pending and the page would sit on the loading spinner rather than saying
    // what went wrong.
    worker.onerror = (e) => {
        const reason = new Error(e.message || "the engine worker failed to start");
        for (const entry of pending.values()) entry.reject(reason);
        pending.clear();
    };
}

function call(op, ...args) {
    return new Promise((resolve, reject) => {
        const id = ++nextCall;
        pending.set(id, {resolve, reject});
        worker.postMessage({id, op, args});
    });
}

const engine = {
    boot: () => call("boot"),
    reset: () => call("reset"),
    query: (cypher) => call("query", cypher),
    explain: (cypher) => call("explain", cypher),
    stats: () => call("stats"),
    graphSnapshot: () => call("graphSnapshot"),
    createTextIndex: (label, property) => call("createTextIndex", label, property),
    textSearch: (query, k) => call("textSearch", query, k),
    upsertVector: (id, vector) => call("upsertVector", id, vector),
    vectorSearch: (vector, k) => call("vectorSearch", vector, k),
    memory: () => call("memory"),
};

const CANCELLED = "The query was cancelled.";

// Terminating the worker is the only way to stop a running query, because a wasm call has no
// interruption point for a message to be handled at. The graph lives in the worker, so it dies with
// it, and the replay below is what makes canceling recoverable rather than destructive: the sample
// is re-seeded and every statement the page recorded as setup is applied again. A query that only
// read loses nothing; one that had already written is reapplied from `setupLog`, which is the same
// log a share link carries.
async function cancelRunningQuery() {
    worker.terminate();
    for (const entry of pending.values()) entry.reject(new Error(CANCELLED));
    pending.clear();
    spawnWorker();
    await engine.boot();
    await engine.query(currentSample().cypher);
    if (setupLog.length > 0) await engine.query(setupLog.join(";\n"));
}

// ---------------------------------------------------------------------------
// Cypher highlighting
// ---------------------------------------------------------------------------

const KEYWORDS = new Set(
    `match optional where return create merge set remove delete detach with unwind
   order by skip limit distinct as and or xor not in starts ends contains is null
   true false asc ascending desc descending union all call yield on constraint index
   explain profile case when then else end exists count collect sum avg min max
   foreach load csv from headers using periodic commit drop unique assert require
   for scalar single any none shortestpath allshortestpaths copy export import database`
        .split(/\s+/)
        .filter(Boolean),
);

const TOKEN = new RegExp(
    [
        "(\\/\\/[^\\n]*)",
        "(\\/\\*[\\s\\S]*?\\*\\/)",
        "('(?:[^'\\\\]|\\\\.)*'|\"(?:[^\"\\\\]|\\\\.)*\")",
        "(\\$[A-Za-z_]\\w*)",
        "(:[A-Za-z_]\\w*)",
        "(\\b\\d+\\.?\\d*(?:[eE][-+]?\\d+)?\\b)",
        "([A-Za-z_]\\w*)(?=\\s*\\()",
        "([A-Za-z_][\\w.]*)",
        "([-=<>|*+\\/%!,.;{}\\[\\]()]+)",
    ].join("|"),
    "g",
);

const esc = (s) =>
    s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");

function highlight(src) {
    let out = "";
    let last = 0;
    for (const m of src.matchAll(TOKEN)) {
        out += esc(src.slice(last, m.index));
        last = m.index + m[0].length;
        const cls = m[1] || m[2] ? "com"
            : m[3] ? "str"
                : m[4] ? "lbl"
                    : m[5] ? "lbl"
                        : m[6] ? "num"
                            : m[7] ? (KEYWORDS.has(m[7].toLowerCase()) ? "kw" : "fn")
                                : m[8] ? (KEYWORDS.has(m[8].toLowerCase()) ? "kw" : null)
                                    : m[9] ? "op"
                                        : null;
        out += cls ? `<span class="tok-${cls}">${esc(m[0])}</span>` : esc(m[0]);
    }
    return out + esc(src.slice(last));
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

let ready = false;
let lastResult = null;
let sim = null;

const HISTORY_KEY = "issundb.history";
const SCHEME_KEY = "issundb.scheme";
const EDITOR_KEY = "issundb.editor";

// A result is rendered as one `innerHTML` assignment, so an uncapped table is a query away from
// locking the tab: `MATCH (a)-[*1..3]->(b) RETURN *` on a graph of any size, or anything after a
// bulk load. The caps are on the view only. Both downloads read `lastResult`, so an export is
// still complete, and the row counter still reports the true total.
const MAX_TABLE_ROWS = 1000;
const MAX_CELL_CHARS = 200;
// Past this a value is clipped without a tooltip, rather than putting a megabyte in an attribute.
const MAX_TITLE_CHARS = 2000;

// Every vertex is drawn at this radius. Sizing by degree restated what the edges already show, and
// sizing by PageRank made the view depend on a whole extra pass over the graph for a difference the
// table reports precisely anyway.
const NODE_RADIUS = 10;

// Edges are paths rather than lines, so one element covers a straight hop, the curve that tells two
// edges between the same pair apart, and a self-loop. `EDGE_CURVE` is the gap between the curves of
// a parallel or reciprocal group, and `ARROW_GAP` clears the arrowhead off the target's rim.
const EDGE_CURVE = 22;
const ARROW_GAP = 4;
const LOOP_SIZE = 18;

// Every vertex is captioned below this many; above it only the best-connected are, and the rest
// appear on hover. A hundred captions at one size is a wall of text rather than a labeling.
const LABEL_ALL_MAX = 45;
const LABEL_TOP = 18;

// The force layout settles over an animation loop, which is motion a visitor can ask not to see.
const REDUCED_MOTION = matchMedia("(prefers-reduced-motion: reduce)");

// ---------------------------------------------------------------------------
// Editor
// ---------------------------------------------------------------------------

const editor = $("editor");
const highlightEl = $("highlight");

function syncScroll() {
    const backdrop = highlightEl.parentElement;
    backdrop.scrollTop = editor.scrollTop;
    backdrop.scrollLeft = editor.scrollLeft;
}

function syncHighlight() {
    // The trailing newline stops the backdrop's last line from collapsing, which would let
    // the two panes disagree by one line height at the bottom.
    highlightEl.innerHTML = highlight(editor.value) + "\n";
    syncScroll();
}

// This build keeps the graph in memory, so a reload starts from the seeded sample either way.
// What a reload should not also discard is the query being written, which is the one thing the
// visitor produced. Past this length it is not stored at all: the quota is per origin and shared
// with the history, and losing the history to a pasted bulk script would be the worse trade.
const MAX_STORED_EDITOR = 100000;

function storeEditor() {
    try {
        if (editor.value.length > MAX_STORED_EDITOR) localStorage.removeItem(EDITOR_KEY);
        else localStorage.setItem(EDITOR_KEY, editor.value);
    } catch {
        // Storage being unavailable only costs the restore.
    }
}

function readStoredEditor() {
    try {
        return localStorage.getItem(EDITOR_KEY) ?? "";
    } catch {
        return "";
    }
}

function setQuery(text) {
    editor.value = text;
    syncHighlight();
    storeEditor();
    editor.focus();
}

// Debounced rather than written per keystroke, since every write serializes the whole buffer.
let storeTimer = null;

editor.addEventListener("input", () => {
    syncHighlight();
    // The follow-up belongs to the example that was loaded, so editing the query retires it.
    pendingDemo = null;
    clearTimeout(storeTimer);
    storeTimer = setTimeout(storeEditor, 400);
    openCompletions();
});
// Scrolling only moves the backdrop. Re-running the highlighter per scroll event rebuilt
// the whole document's markup on every frame of a drag.
editor.addEventListener("scroll", syncScroll);

editor.addEventListener("keydown", (e) => {
    // The popup owns these keys while it is open, or Enter would run the query instead of accepting
    // the highlighted completion and Escape would do nothing.
    if (completionOpen()) {
        if (e.key === "ArrowDown" || e.key === "ArrowUp") {
            e.preventDefault();
            moveCompletion(e.key === "ArrowDown" ? 1 : -1);
            return;
        }
        if (e.key === "Enter" || e.key === "Tab") {
            e.preventDefault();
            acceptCompletion();
            return;
        }
        if (e.key === "Escape") {
            e.preventDefault();
            closeCompletions();
            return;
        }
    }

    if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
        e.preventDefault();
        run();
        return;
    }
    // Shift-Alt-F, the shortcut an editor is expected to answer to.
    if (e.altKey && e.shiftKey && (e.key === "F" || e.key === "f")) {
        e.preventDefault();
        formatEditor();
        return;
    }
    // Ctrl-Space asks for completions where the prefix rules below would not have offered any.
    if (e.key === " " && (e.ctrlKey || e.metaKey)) {
        e.preventDefault();
        openCompletions(true);
        return;
    }
    if (e.key === "Tab") {
        e.preventDefault();
        const {selectionStart: a, selectionEnd: b, value} = editor;
        editor.value = value.slice(0, a) + "  " + value.slice(b);
        editor.selectionStart = editor.selectionEnd = a + 2;
        syncHighlight();
    }
});

// ---------------------------------------------------------------------------
// Autocomplete
// ---------------------------------------------------------------------------

// Two characters before anything is offered unprompted, since one letter matches most of the
// catalog and a popup over every keystroke is noise. Ctrl-Space overrides the rule.
const COMPLETION_MIN_PREFIX = 2;
const COMPLETION_LIMIT = 12;

let completions = [];
let completionAt = 0;
let completionStart = 0;

const completionOpen = () => !$("ac").hidden;

// The word under the caret, taken as the characters a Cypher name can contain. A leading colon is
// part of it so `:Pers` completes to a label rather than being read as an empty prefix.
function prefixBeforeCaret() {
    const caret = editor.selectionStart;
    let start = caret;
    while (start > 0 && /[A-Za-z0-9_.]/.test(editor.value[start - 1])) start -= 1;
    if (start > 0 && editor.value[start - 1] === ":") start -= 1;
    return {start, text: editor.value.slice(start, caret)};
}

// The schema half is live: it comes from the same `stats` call the schema panel renders, so a label
// created by the query you just ran is offered by the next keystroke.
function completionPool() {
    const pool = [];
    for (const label of Object.keys(lastStats?.label_counts ?? {})) {
        pool.push({text: `:${label}`, kind: "label"});
    }
    for (const type of Object.keys(lastStats?.type_counts ?? {})) {
        pool.push({text: `:${type}`, kind: "type"});
    }
    for (const entry of REFERENCE) {
        pool.push({text: entry.name, kind: entry.kind === "function" ? "fn" : "proc"});
    }
    for (const keyword of KEYWORDS) pool.push({text: keyword.toUpperCase(), kind: "kw"});
    return pool;
}

function rankCompletions(prefix) {
    const needle = prefix.toLowerCase();
    const scored = [];
    for (const item of completionPool()) {
        const haystack = item.text.toLowerCase();
        const at = haystack.indexOf(needle);
        if (at < 0) continue;
        // A prefix match is what the typist meant; a match in the middle is a fallback, which is what
        // makes `jacc` reach `issundb.link.jaccard` without burying the keywords that start with it.
        scored.push({...item, rank: at === 0 ? 0 : 1, length: item.text.length});
    }
    scored.sort((a, b) => a.rank - b.rank || a.length - b.length || a.text.localeCompare(b.text));
    return scored.slice(0, COMPLETION_LIMIT);
}

// Measured against the highlight backdrop rather than a second hidden mirror. Its text is the
// editor's text exactly, and it already carries the same font, padding, and scroll offset, so a
// range inside it lands where the caret is drawn.
function caretPoint() {
    const index = editor.selectionStart;
    const walker = document.createTreeWalker(highlightEl, NodeFilter.SHOW_TEXT);
    let seen = 0;
    let node = walker.nextNode();
    while (node) {
        const length = node.nodeValue.length;
        if (seen + length >= index) {
            const range = document.createRange();
            range.setStart(node, index - seen);
            range.collapse(true);
            const rect = range.getBoundingClientRect();
            const box = $("editor-box").getBoundingClientRect();
            return {x: rect.left - box.left, y: rect.bottom - box.top};
        }
        seen += length;
        node = walker.nextNode();
    }
    return null;
}

function openCompletions(forced = false) {
    const {start, text} = prefixBeforeCaret();
    if (!forced && text.length < COMPLETION_MIN_PREFIX) return closeCompletions();
    const matches = rankCompletions(text);
    if (matches.length === 0) return closeCompletions();

    completions = matches;
    completionAt = 0;
    completionStart = start;
    renderCompletions();

    const point = caretPoint();
    const host = $("ac");
    host.hidden = false;
    if (point) {
        host.style.left = `${Math.max(4, point.x)}px`;
        host.style.top = `${point.y + 4}px`;
    }
}

function renderCompletions() {
    $("ac").innerHTML = completions
        .map(
            (item, i) =>
                `<div class="ac-row${i === completionAt ? " on" : ""}" data-i="${i}">` +
                `<span class="ac-kind ${item.kind}">${item.kind}</span>` +
                `<span class="ac-text">${esc(item.text)}</span></div>`,
        )
        .join("");
}

function moveCompletion(step) {
    completionAt = (completionAt + step + completions.length) % completions.length;
    renderCompletions();
    $("ac").querySelector(".ac-row.on")?.scrollIntoView({block: "nearest"});
}

function acceptCompletion() {
    const chosen = completions[completionAt];
    if (!chosen) return closeCompletions();
    const caret = editor.selectionStart;
    const before = editor.value.slice(0, completionStart);
    const after = editor.value.slice(caret);
    // A procedure or function is always followed by an argument list, so the parentheses come with
    // it and the caret lands between them.
    const call = chosen.kind === "proc" || chosen.kind === "fn";
    const inserted = call ? `${chosen.text}()` : chosen.text;
    editor.value = before + inserted + after;
    const caretAt = before.length + inserted.length - (call ? 1 : 0);
    editor.selectionStart = editor.selectionEnd = caretAt;
    closeCompletions();
    syncHighlight();
    storeEditor();
}

function closeCompletions() {
    $("ac").hidden = true;
    completions = [];
}

$("ac").addEventListener("mousedown", (e) => {
    // Ahead of blur, or the popup would close before the click registered.
    e.preventDefault();
    const row = e.target.closest(".ac-row");
    if (!row) return;
    completionAt = Number(row.dataset.i);
    acceptCompletion();
});

editor.addEventListener("blur", closeCompletions);
editor.addEventListener("click", closeCompletions);


// ---------------------------------------------------------------------------
// Status and results
// ---------------------------------------------------------------------------

// The banner carries one short sentence and a state. A detailed error stays in the table pane,
// where it can be several lines long and can carry the did-you-mean hint; the banner only says
// that the run failed. `kind` is "", "busy", "ok", or "err".
function setStatus(kind, text) {
    const banner = $("status");
    banner.className = kind ? `banner ${kind}` : "banner";
    banner.innerHTML =
        kind === "busy" ? `<span class="sp"></span><span>${esc(text)}</span>` : esc(text);
}

function setMeta(text) {
    $("result-meta").textContent = text;
}

const plural = (n, word) => `${n} ${word}${n === 1 ? "" : "s"}`;

function showPane(name) {
    for (const tab of document.querySelectorAll(".tab")) {
        tab.setAttribute("aria-selected", String(tab.dataset.pane === name));
    }
    for (const pane of document.querySelectorAll(".pane")) {
        pane.classList.toggle("on", pane.id === `pane-${name}`);
    }
    if (name === "graph") {
        if (snapshotStale) loadSnapshot().then(drawGraph);
        else drawGraph();
    }
}

for (const tab of document.querySelectorAll(".tab")) {
    tab.addEventListener("click", () => showPane(tab.dataset.pane));
}

// One long property would otherwise set the width of its whole column. The full value stays
// reachable through the tooltip, the JSON tab, and both downloads.
function clip(text) {
    if (text.length <= MAX_CELL_CHARS) return esc(text);
    const shown = `${esc(text.slice(0, MAX_CELL_CHARS))}…`;
    if (text.length > MAX_TITLE_CHARS) return shown;
    return `<span title="${esc(text)}">${shown}</span>`;
}

function cell(value) {
    if (value === null || value === undefined) return '<span class="null">null</span>';
    if (typeof value === "string") return `<span class="s">${clip(value)}</span>`;
    if (typeof value === "number") return `<span class="n">${value}</span>`;
    if (typeof value === "boolean") return `<span class="b">${value}</span>`;
    return `<span>${clip(JSON.stringify(value))}</span>`;
}

function renderTable(result) {
    const pane = $("pane-table");
    setMeta(`${plural(result.rows.length, "row")}, ${plural(result.columns.length, "column")}.`);
    if (result.columns.length === 0) {
        pane.innerHTML =
            '<div class="notice info">The statement returned no columns. Writes report nothing unless the statement ends in RETURN.</div>';
        return;
    }
    if (result.rows.length === 0) {
        pane.innerHTML = `<div class="notice info">No rows. The query is valid and matched nothing.</div>`;
        return;
    }
    const shown = result.rows.slice(0, MAX_TABLE_ROWS);
    const head = result.columns.map((c) => `<th>${esc(c)}</th>`).join("");
    const body = shown
        .map(
            (row, i) =>
                `<tr><td class="rownum">${i + 1}</td>${row.map((v) => `<td>${cell(v)}</td>`).join("")}</tr>`,
        )
        .join("");
    const capped =
        result.rows.length > shown.length
            ? `<div class="notice info">Showing the first ${MAX_TABLE_ROWS} of ${result.rows.length} rows. ` +
            `The CSV and JSON downloads include every row.</div>`
            : "";
    pane.innerHTML =
        `<table><thead><tr><th></th>${head}</tr></thead><tbody>${body}</tbody></table>` + capped;
}

// The JSON tab is one string too, so it takes the same cap. Values are not clipped here, since
// this is the pane a reader opens to see a value the table clipped.
function renderJson(result) {
    const shown = result.rows.slice(0, MAX_TABLE_ROWS);
    const note =
        result.rows.length > shown.length
            ? `// Showing the first ${MAX_TABLE_ROWS} of ${result.rows.length} rows. The JSON download includes every row.\n`
            : "";
    $("pane-json").innerHTML = `<pre class="json">${esc(note + JSON.stringify(shown, null, 2))}</pre>`;
}

// ---------------------------------------------------------------------------
// Query plan
// ---------------------------------------------------------------------------

// What each operator says about how the query will run. The engine's plan text names the operator
// first on every line, so the name alone is enough to classify it, and an operator missing from
// here still renders with no badge rather than breaking the tree.
//
// `kernel` is the interesting one: those operators mean the query stopped being a row pipeline and
// became a single pass over the adjacency arrays, which is usually the difference between
// milliseconds and seconds. Nothing else in the page ever showed that.
const PLAN_ROLES = {
    PathCount: "kernel",
    GroupedDegree: "kernel",
    TriangleCount: "kernel",
    ExpandIntersect: "kernel",
    VectorTopK: "kernel",
    NodeIndexScan: "index",
    NodeRangeScan: "index",
    NodeByIdSeek: "index",
    CorrelatedIndexSeek: "index",
    LabelScan: "scan",
    AllNodesScan: "scan",
    HashJoin: "join",
    MultiwayJoin: "join",
    Expand: "expand",
    ExpandInto: "expand",
};

const ROLE_TITLE = {
    kernel: "Runs as a counting kernel over the adjacency arrays, not as a row pipeline",
    index: "Seeks an index instead of scanning",
    scan: "Reads every node carrying the label",
    join: "Joins two branches",
    expand: "Walks the adjacency one hop",
    pruned: "Provably empty: the optimizer proved this hop returns nothing",
};

function parsePlan(text) {
    const rows = [];
    for (const line of text.split("\n")) {
        if (!line.trim()) continue;
        const depth = (line.match(/^ */)[0].length / 2) | 0;
        const [, op, detail = ""] = line.trim().match(/^(\S+)\s*(.*)$/);
        // A `Limit` with a zero count is the type-inference pass reporting that it proved the pattern
        // unsatisfiable, which is worth saying out loud rather than leaving as an odd-looking bound.
        const role = /^Limit\b/.test(op) && /\bcount=0\b/.test(detail) ? "pruned" : PLAN_ROLES[op];
        rows.push({depth, op, detail, role});
    }
    return rows;
}

// The rendered tree carries every line the engine emits, but it carries depth as CSS padding, so a
// selection copied out of it pastes as a flat list of operators. This is what the Copy button sends
// to the clipboard, and it is the reason the pane no longer shows the same plan a second time as
// preformatted text.
let planText = "";

function renderPlan(text) {
    planText = text;
    const rows = parsePlan(text);
    const host = $("pane-plan");
    if (rows.length === 0) {
        host.innerHTML = '<div class="notice info">No plan: this statement is not a query.</div>';
        return;
    }

    const body = rows
        .map(({depth, op, detail, role}) => {
            const badge = role ? `<i class="plan-badge ${role}" title="${esc(ROLE_TITLE[role])}"></i>` : "";
            return (
                `<li class="plan-node" style="--depth:${depth}">${badge}` +
                `<span class="plan-op ${role ?? ""}">${esc(op)}</span>` +
                (detail ? ` <span class="plan-detail">${esc(detail)}</span>` : "") +
                "</li>"
            );
        })
        .join("");

    const fast = [...new Set(rows.filter((r) => r.role === "kernel").map((r) => r.op))];
    const pruned = rows.some((r) => r.role === "pruned");
    let summary = `${plural(rows.length, "operator")}.`;
    if (fast.length > 0) summary += ` Lowered to ${fast.join(", ")}.`;
    if (pruned) summary += " One branch was proved empty and will not run.";

    host.innerHTML =
        `<div class="plan-summary"><span>${esc(summary)}</span>` +
        `<button class="btn sm" id="copy-plan" title="Copy the plan as indented text">Copy</button></div>` +
        `<ul class="plan-tree">${body}</ul>`;
}

// Delegated, because the button is rebuilt with the pane on every explain and binding it inside
// `renderPlan` would add a listener per query.
$("pane-plan").addEventListener("click", async (e) => {
    if (!e.target.closest("#copy-plan")) return;
    try {
        await navigator.clipboard.writeText(planText);
        setStatus("ok", "Plan copied.");
    } catch {
        setStatus("err", "The browser would not give the page access to the clipboard.");
    }
});

function showError(message) {
    $("pane-table").innerHTML = `<div class="notice err">${esc(message)}</div>`;
    setMeta("No results.");
    showPane("table");
}

// ---------------------------------------------------------------------------
// Running
// ---------------------------------------------------------------------------

let busy = false;

// Conservative on purpose: matching too often only costs a rescan, while matching too rarely
// would leave the schema panel and the graph stale.
const MAY_WRITE = /\b(CREATE|MERGE|SET|DELETE|DETACH|REMOVE|COPY|IMPORT|DROP)\b/i;

async function run(mode = "run") {
    // The disabled Run button does not cover the keyboard shortcut, the Explain button, or a
    // demo click, so two runs could interleave and the slower one's panes would win.
    if (!ready || busy) return;
    const cypher = editor.value.trim();
    if (!cypher) return;

    setBusy(true);
    setStatus("busy", "Running…");

    try {
        if (mode === "explain") {
            renderPlan(await engine.explain(cypher));
            setStatus("ok", "Plan generated.");
            setMeta("Physical plan. The query was not executed.");
            showPane("plan");
            remember(cypher);
            return;
        }

        const started = performance.now();
        const result = JSON.parse(await engine.query(cypher));
        const wall = performance.now() - started;
        lastResult = result;

        renderTable(result);
        renderJson(result);

        try {
            renderPlan(await engine.explain(cypher));
        } catch {
            $("pane-plan").innerHTML =
                '<div class="notice info">No plan: this statement is not a query.</div>';
        }

        const multi =
            result.statement_count > 1
                ? ` ${result.statement_count} statements ran; this is the last one's result.`
                : "";
        setStatus("ok", "Query finished.");
        setMeta(
            `${plural(result.rows.length, "row")}, ${plural(result.columns.length, "column")}.` +
            ` Query took ${result.elapsed_ms.toFixed(2)} ms` +
            ` (${wall.toFixed(1)} ms including the round trip).${multi}`,
        );
        showPane("table");
        remember(cypher);
        // `stats` is a full node scan and an adjacency walk. A read-only statement cannot change
        // what it reports, so only a statement that might have written is worth rescanning for.
        const mayWrite = MAY_WRITE.test(cypher);
        if (mayWrite) {
            await refreshSchema();
            rememberSetup(cypher);
        }
        await refreshGraph(mayWrite);
        await renderFooter();

        if (pendingDemo) {
            const demo = pendingDemo;
            pendingDemo = null;
            if (demo.embed) await embedLabel(demo.embed);
            if (demo.thenQuery) {
                if (demo.textIndex) await engine.createTextIndex(demo.textIndex[0], demo.textIndex[1]);
                await runThenQuery(demo.thenQuery);
            } else if (demo.textSearch) {
                await runTextDemo(demo);
            } else if (demo.vectors) {
                await runVectorDemo(demo.vectors);
            }
        }
    } catch (e) {
        // Cleared, or the export buttons would hand back the previous query's rows while the
        // table shows this one's error.
        lastResult = null;
        const message = String(e.message ?? e);
        if (message === CANCELLED) {
            showError(
                `${CANCELLED} The engine was restarted and the ${esc(currentSample().label)} sample` +
                " re-seeded, because a WebAssembly call cannot be interrupted any other way.",
            );
            setStatus("", "Cancelled.");
        } else {
            showError(message + procedureHint(cypher, message));
            setStatus("err", "Query failed.");
        }
    } finally {
        setBusy(false);
    }
}

// Cancel is only reachable while a query is in flight, and the restart it performs is itself a
// series of engine calls, so the button stays disabled until they finish.
function setBusy(value) {
    busy = value;
    $("run").disabled = value;
    $("cancel").disabled = !value;
    $("cancel").hidden = !value;
}

$("cancel").addEventListener("click", async () => {
    if (!busy) return;
    $("cancel").disabled = true;
    setStatus("busy", "Canceling…");
    try {
        await cancelRunningQuery();
        await refreshSchema();
        await refreshGraph();
    } catch (e) {
        setStatus("err", `The engine could not be restarted: ${String(e.message ?? e)}`);
    }
});

$("run").addEventListener("click", () => run());
$("explain").addEventListener("click", () => run("explain"));
$("clear").addEventListener("click", () => {
    setQuery("");
    setStatus("", "Editor cleared.");
});

// Generous for a query and small enough that the highlighter, which rebuilds the whole document's
// markup on each change, does not stall the tab on a file that was never meant to be edited.
const MAX_LOADED_FILE = 512 * 1024;

$("load").addEventListener("click", () => $("load-file").click());

$("load-file").addEventListener("change", async (e) => {
    const file = e.target.files?.[0];
    // Cleared so choosing the same file twice fires the event again.
    e.target.value = "";
    if (!file) return;
    if (file.size > MAX_LOADED_FILE) {
        setStatus("err", `${file.name} is ${Math.round(file.size / 1024)} KB; the editor takes 512 KB.`);
        return;
    }
    try {
        setQuery(await file.text());
        pendingDemo = null;
        setStatus("", `Loaded ${file.name}. Press Execute Query to run it.`);
    } catch {
        setStatus("err", `${file.name} could not be read.`);
    }
});

$("download").addEventListener("click", () => {
    const cypher = editor.value;
    if (!cypher.trim()) {
        setStatus("", "Nothing to download: the editor is empty.");
        return;
    }
    download("issundb-query.cypher", "text/plain;charset=utf-8", cypher);
    setStatus("", "Saved issundb-query.cypher.");
});

function formatEditor() {
    const before = editor.value;
    if (!before.trim()) return;
    const after = formatCypher(before);
    if (after === before) {
        setStatus("", "Already formatted.");
        return;
    }
    setQuery(after);
    setStatus("", "Formatted.");
}

$("format").addEventListener("click", formatEditor);
// Loaded into the editor rather than executed, so the statement is read before it writes. Running
// it on a database that already holds the sample adds a second copy, which the caption says.
$("load-sample").addEventListener("click", () => {
    const sample = currentSample();
    setQuery(sample.cypher);
    pendingDemo = null;
    setStatus(
        "",
        `Loaded the ${sample.label} sample. Press Execute Query to create it.` +
        " Running it on a database that already has it adds a second copy.",
    );
});

// ---------------------------------------------------------------------------
// History
// ---------------------------------------------------------------------------

// Trimmed on read as well as on write, so a longer list stored by an earlier visit is cut to the
// limit straight away rather than only after the next query pushes an entry out.
const MAX_HISTORY_ITEMS = 10;

function readHistory() {
    try {
        const stored = JSON.parse(localStorage.getItem(HISTORY_KEY) ?? "[]");
        return Array.isArray(stored) ? stored.slice(0, MAX_HISTORY_ITEMS) : [];
    } catch {
        return [];
    }
}

function remember(cypher) {
    const history = readHistory().filter((q) => q !== cypher);
    history.unshift(cypher);
    try {
        localStorage.setItem(HISTORY_KEY, JSON.stringify(history.slice(0, MAX_HISTORY_ITEMS)));
    } catch {
        // A browser refusing storage must not fail the query.
    }
    renderHistory();
}

function renderHistory() {
    const history = readHistory();
    $("history").innerHTML = history.length
        ? ""
        : '<div class="empty">Queries you run appear here.</div>';
    for (const cypher of history) {
        const button = document.createElement("button");
        button.className = "hist";
        button.textContent = cypher.replace(/\s+/g, " ").slice(0, 70);
        button.title = cypher;
        button.addEventListener("click", () => setQuery(cypher));
        $("history").append(button);
    }
}

// ---------------------------------------------------------------------------
// Setup log
// ---------------------------------------------------------------------------

// The write statements run this session, in order, so a share link can carry the data a query
// needs rather than only the query. The seed is not in here: the recipient's own boot runs it,
// and replaying it as well would hand them two copies of the sample graph.
const setupLog = [];

function rememberSetup(cypher) {
    // The log is replayed as one semicolon-separated statement, so a statement that already ends
    // in one would contribute an empty statement between two real ones.
    setupLog.push(cypher.replace(/;\s*$/, ""));
}

// ---------------------------------------------------------------------------
// Procedure reference
// ---------------------------------------------------------------------------

const procedureNames = new Set(PROCEDURES.flatMap((p) => [p.name, p.aka].filter(Boolean)));
const functionNames = new Set(FUNCTIONS.map((f) => f.name));

// A procedure yields rows and a function returns a value, so the two are listed apart rather than
// merged behind one word that would be wrong for half of them.
const REFERENCE = [
    ...PROCEDURES.map((entry) => ({...entry, kind: "procedure"})),
    ...FUNCTIONS.map((entry) => ({...entry, kind: "function"})),
];

// One list, procedures before functions, rather than a filter control. The split is not
// cosmetic: a procedure is invoked with CALL and yields rows, a function is called inside an
// expression and returns one value, and only a function can see a variable a MATCH bound, because
// CALL evaluates its arguments against no bindings. Each row carries that in its "yields" or
// "returns" suffix.
function renderProcedures(filter = "") {
    const needle = filter.trim().toLowerCase();
    const host = $("proc-list");
    const matches = REFERENCE.filter(
        (entry) =>
            !needle ||
            `${entry.name} ${entry.aka ?? ""} ${entry.args} ${entry.yields} ${entry.summary}`
                .toLowerCase()
                .includes(needle),
    );
    host.replaceChildren();
    if (matches.length === 0) {
        host.innerHTML = '<div class="empty">Nothing matches.</div>';
        return;
    }

    // Procedures first, then functions, with no heading between them: the sidebar is too narrow
    // for one naming the calling convention. Each row already ends in "yields" for a procedure or
    // "returns" for a function, which carries the same distinction in the space available.
    for (const kind of ["procedure", "function"]) {
        for (const entry of matches.filter((e) => e.kind === kind)) host.append(referenceRow(entry));
    }
}

function referenceRow(entry) {
    const button = document.createElement("button");
    button.className = "proc";
    // The signature is in the tooltip rather than the row. In a sidebar this narrow a form like
    // `issundb.pageRank([{iterations, damping}])` wraps mid-identifier, which is harder to scan
    // than the name alone, and clicking inserts the call anyway.
    const signature = `${entry.name}(${entry.args})`;
    button.title = entry.aka
        ? `${signature}\n\n${entry.summary}\n\nAlso registered as ${entry.aka}.`
        : `${signature}\n\n${entry.summary}`;
    const name = document.createElement("span");
    name.className = "nm";
    name.textContent = entry.name;
    const yields = document.createElement("span");
    yields.className = "yd";
    yields.textContent =
        entry.kind === "function" ? `returns ${entry.yields}` : `yields ${entry.yields}`;
    button.append(name, yields);
    button.addEventListener("click", () => setQuery(entry.snippet));
    return button;
}

// Iterative over two rows, so the whole matrix is never held.
function editDistance(a, b) {
    let previous = Array.from({length: b.length + 1}, (_, j) => j);
    for (let i = 1; i <= a.length; i += 1) {
        const current = [i];
        for (let j = 1; j <= b.length; j += 1) {
            current[j] =
                a[i - 1] === b[j - 1]
                    ? previous[j - 1]
                    : 1 + Math.min(previous[j - 1], previous[j], current[j - 1]);
        }
        previous = current;
    }
    return previous[b.length];
}

// `ProcedureNotFound` says the name is wrong without saying what was meant, and the difference is
// usually one character of casing. The catalog is the only list of real names the page has, so a
// suggestion is exactly as complete as the catalog is; a procedure missing from it gets no hint
// rather than a wrong one.
function procedureHint(cypher, message) {
    if (!/ProcedureNotFound/.test(message)) return "";
    for (const token of new Set(cypher.match(/\bissundb\.[A-Za-z_][\w.]*/g) ?? [])) {
        if (procedureNames.has(token)) continue;
        let best = null;
        // Further than three edits apart the suggestion is noise rather than a correction.
        let bestDistance = 4;
        for (const name of procedureNames) {
            const distance = editDistance(token.toLowerCase(), name.toLowerCase());
            if (distance < bestDistance) {
                bestDistance = distance;
                best = name;
            }
        }
        if (best) return `\n\nThere is no ${token}. Did you mean ${best}?`;
    }
    return "";
}

// The count comes from the catalogs rather than the markup, which carried a stale number for as
// long as the catalog had more than that, then undercounted again when functions were added.
$("proc-search").placeholder = `Search ${REFERENCE.length} procedures and functions…`;
$("proc-search").addEventListener("input", (e) => renderProcedures(e.target.value));

// ---------------------------------------------------------------------------
// Demos
// ---------------------------------------------------------------------------

let activeCategory = 0;

// The example whose text is in the editor, if it has a follow-up step. Full-text search and vector
// search are Rust extension traits rather than Cypher, so those two examples need something to
// happen after their statement; holding the example here is what lets that still work now that
// selecting one no longer runs it.
let pendingDemo = null;

// Every example queries a graph from Pick a Graph rather than creating one, so it can only answer
// once that graph is loaded. The label a category names is how the page tells, out of the schema it
// already has, and says so instead of leaving an empty table to be read as a fault.
function graphIsLoaded(category) {
    if (!category?.requiresLabel) return true;
    return Boolean(lastStats?.label_counts?.[category.requiresLabel]);
}

const sampleLabel = (id) => SAMPLE_GRAPHS.find((sample) => sample.id === id)?.label ?? id;

function selectDemo(index) {
    const category = DEMO_CATEGORIES[activeCategory];
    const demo = category?.demos[index];
    if (!demo) return;
    for (const other of document.querySelectorAll(".demo")) {
        other.classList.toggle("active", Number(other.dataset.index) === index);
    }
    setQuery(demo.cypher);
    pendingDemo =
        demo.textSearch || demo.vectors || demo.thenQuery || demo.embed ? demo : null;

    if (!graphIsLoaded(category)) {
        setStatus(
            "",
            `Loaded "${demo.label}". It queries the ${sampleLabel(category.sample)} graph, which is not` +
            " in the database: press Reset Graph to load it, then Execute Query.",
        );
        return;
    }
    setStatus(
        "",
        demo.explain
            ? `Loaded "${demo.label}". Press Explain to see the plan.`
            : `Loaded "${demo.label}". Press Execute Query to run it.`,
    );
}

function renderCategory() {
    const category = DEMO_CATEGORIES[activeCategory];
    // The picker follows the category, so loading the graph these examples want is one press of Reset
    // Graph rather than a hunt through the list. Nothing runs here: this moves a dropdown.
    const wanted = SAMPLE_GRAPHS.findIndex((sample) => sample.id === category.sample);
    if (wanted >= 0) {
        activeSample = wanted;
        $("sample-graph").value = String(wanted);
    }
    const host = $("demo-buttons");
    host.replaceChildren();
    category.demos.forEach((demo, i) => {
        const button = document.createElement("button");
        button.className = "demo";
        button.textContent = demo.label;
        button.dataset.index = String(i);
        button.title = demo.desc;
        button.addEventListener("click", () => selectDemo(i));
        host.append(button);
    });
    const link = $("category-docs");
    if (category.docs) {
        link.href = category.docs;
        link.textContent = `Read more: ${category.label}`;
        link.hidden = false;
    } else {
        link.hidden = true;
    }
}

function renderDemos() {
    const select = $("demo-category");
    DEMO_CATEGORIES.forEach((category, i) => {
        const option = document.createElement("option");
        option.value = String(i);
        option.textContent = category.label;
        select.append(option);
    });
    select.addEventListener("change", () => {
        activeCategory = Number(select.value);
        renderCategory();
    });
    renderCategory();
}

async function runTextDemo(demo) {
    const [label, property] = demo.textIndex;
    try {
        await engine.createTextIndex(label, property);
        const {hits} = JSON.parse(await engine.textSearch(demo.textSearch, 10));
        const rows = [];
        for (const hit of hits) {
            const title = JSON.parse(await engine.query(`MATCH (a) WHERE id(a) = ${hit.node} RETURN a.title`));
            rows.push([hit.node, title.rows[0]?.[0] ?? null, Number(hit.score.toFixed(4)), hit.property]);
        }
        lastResult = {columns: ["node", "title", "bm25", "field"], rows};
        renderTable(lastResult);
        setStatus("ok", "Full-text search finished.");
        setMeta(
            `${plural(rows.length, "hit")}, 4 columns.` +
            ` BM25 over the ${label}.${property} index for "${demo.textSearch}".`,
        );
        showPane("table");
    } catch (e) {
        showError(String(e.message ?? e));
    }
}

// Places each node of a label on a circle, so "nearest" has a meaning the table can be checked
// against by eye. A node id is a u64, which wasm-bindgen takes as a BigInt. Returns the ids with
// their captions, so the search that follows can name its hits.
async function embedLabel(spec) {
    const label = spec.label ?? "Person";
    const caption = spec.caption ?? "name";
    const rows = JSON.parse(
        await engine.query(`MATCH (n:${label}) RETURN id(n) AS id, n.${caption} AS caption ORDER BY id`),
    ).rows;
    for (const [i, [id]] of rows.entries()) {
        const angle = (i / Math.max(rows.length, 1)) * Math.PI * 2;
        await engine.upsertVector(BigInt(id), new Float32Array([Math.cos(angle), Math.sin(angle), 0.25]));
    }
    return {label, rows};
}

async function runVectorDemo(spec) {
    try {
        const {label, rows} = await embedLabel(spec);
        if (rows.length === 0) {
            showError(`No ${label} nodes to embed. Run the example's own CREATE first.`);
            return;
        }
        const {hits} = JSON.parse(
            await engine.vectorSearch(new Float32Array([1, 0, 0.25]), Math.min(5, rows.length)),
        );
        const captions = new Map(rows.map(([id, caption]) => [id, caption]));
        lastResult = {
            columns: ["rank", "node", "label", "distance"],
            rows: hits.map((h, i) => [
                i + 1,
                h.node,
                captions.get(h.node) ?? null,
                Number(h.distance.toFixed(5)),
            ]),
        };
        renderTable(lastResult);
        setStatus("ok", "Vector search finished.");
        setMeta(
            `${plural(hits.length, "neighbor")}, 4 columns.` +
            ` Exact search over ${plural(rows.length, `${label} embedding`)} for [1, 0, 0.25].`,
        );
        showPane("table");
    } catch (e) {
        showError(String(e.message ?? e));
    }
}

// A query that needs embeddings or a text index in place before it can run, so it cannot be part of
// the example's own statement. `issundb.retrieve.hybrid` is the case this exists for.
async function runThenQuery(cypher) {
    try {
        const result = JSON.parse(await engine.query(cypher));
        lastResult = result;
        renderTable(result);
        renderJson(result);
        setStatus("ok", "Query finished successfully.");
        setMeta(
            `${plural(result.rows.length, "row")}, ${plural(result.columns.length, "column")}.` +
            ` Query took ${result.elapsed_ms.toFixed(2)} ms, after the example put its index and`
            + " embeddings in place.",
        );
        showPane("table");
        await refreshGraph(false);
    } catch (e) {
        showError(String(e.message ?? e));
        setStatus("err", "The follow-up query failed.");
    }
}

// ---------------------------------------------------------------------------
// Schema panel
// ---------------------------------------------------------------------------

// The counts the footer reports, from the last `stats` call. Kept rather than re-read, because
// `stats` is a full node scan and the footer is refreshed after every run.
let lastStats = null;

// The module's exports, for the one figure only the browser knows: how much WebAssembly heap it has
// committed. That number never falls, so it is reported beside the live one rather than instead of
// it.

// Live bytes with an empty database, captured after the instance is built and before it is seeded.
// Subtracting it is what separates the graph from the engine that holds it.
let baselineBytes = 0;

// KB below a megabyte, because a small graph is a few hundred kilobytes and reporting it as 0.1 MB
// says less than 143 KB does.
function bytesLabel(bytes) {
    if (bytes < 1024) return `${bytes} B`;
    if (bytes < 1048576) return `${Math.round(bytes / 1024)} KB`;
    return `${(bytes / 1048576).toFixed(1)} MB`;
}

async function renderFooter() {
    const note = $("footer-note");
    const counts = lastStats
        ? `${plural(lastStats.nodes, "node")} and ${plural(lastStats.edges, "edge")}`
        : "";

    let live = 0;
    let heap = 0;
    try {
        ({live, heap, baseline: baselineBytes} = await engine.memory());
    } catch {
        // A build without the counter still reports the counts.
    }
    if (live === 0) {
        note.textContent = counts;
        note.title = "";
        return;
    }

    // Two figures rather than three. Splitting the live total into engine and graph was the first
    // shape this took, and it reported the same number twice: an empty database allocates a few
    // kilobytes, so the graph is very nearly all of it. The baseline is stated in the tooltip
    // instead, where it says that rather than implying a division that does not exist.
    note.textContent = `| ${counts} | ${bytesLabel(live)} in use | ${bytesLabel(heap)} heap |`;
    note.title =
        `IssunDB has ${bytesLabel(live)} allocated and not freed. An empty database accounts for` +
        ` ${bytesLabel(baselineBytes)} of that, so the rest is graph data and the structures derived` +
        ` from it. The WebAssembly heap the browser has committed is ${bytesLabel(heap)}, which only` +
        " ever grows, so it stays above the figure in use.";
}

// Hashing the name is what keeps a vertex's color stable across redraws without a table.
function hueOf(name) {
    let hash = 0;
    for (let i = 0; i < name.length; i += 1) {
        hash = (hash * 31 + name.charCodeAt(i)) | 0;
    }
    return Math.abs(hash) % 360;
}

const colorOf = (name) => `hsl(${hueOf(name)} 62% 52%)`;

async function refreshSchema() {
    let stats;
    try {
        stats = JSON.parse(await engine.stats());
    } catch {
        return;
    }
    const rows = [];
    for (const [label, n] of Object.entries(stats.label_counts ?? {})) {
        rows.push(
            `<div class="schema-row"><i class="swatch" style="background:${colorOf(label)}"></i>` +
            `<span>:${esc(label)}</span><span class="n">${n}</span></div>`,
        );
    }
    for (const [type, n] of Object.entries(stats.type_counts ?? {})) {
        rows.push(
            `<div class="schema-row"><span style="color:var(--md-default-fg-color--light)">→</span>` +
            `<span>:${esc(type)}</span><span class="n">${n}</span></div>`,
        );
    }
    $("schema").innerHTML = rows.length
        ? rows.join("")
        : '<div class="empty">Empty database. Run a CREATE.</div>';
    lastStats = stats;
    renderFooter();
}

// ---------------------------------------------------------------------------
// Graph view
// ---------------------------------------------------------------------------

let snapshot = {nodes: [], edges: [], truncated: false};
let snapshotStale = true;

async function loadSnapshot() {
    let next;
    try {
        next = JSON.parse(await engine.graphSnapshot());
    } catch {
        next = {nodes: [], edges: [], truncated: false};
    }
    // Each surviving node keeps its position, so running a query does not discard a layout the
    // user arranged by hand or watched settle. Without this the whole graph re-seeded onto the
    // starting circle after every statement.
    const previous = new Map(snapshot.nodes.map((node) => [node.id, node]));
    for (const node of next.nodes) {
        const old = previous.get(node.id);
        if (old && old.x !== undefined) {
            node.x = old.x;
            node.y = old.y;
            node.vx = old.vx ?? 0;
            node.vy = old.vy ?? 0;
        }
    }
    snapshot = next;
    snapshotStale = false;
}

// `mayWrite` false means only the highlighting can have changed, so the graph is redrawn
// without paying for a fresh scan or another PageRank pass.
async function refreshGraph(mayWrite = true) {
    // `graphSnapshot` is a full node scan, so paying for it while the graph tab is hidden is
    // waste. Switching to the tab loads it instead.
    if (mayWrite) snapshotStale = true;
    if (!$("pane-graph").classList.contains("on")) return;
    if (snapshotStale) await loadSnapshot();
    drawGraph();
}

// Velocity-Verlet, with all-pairs repulsion: at the 300-node cap that pass is cheap enough
// that no spatial index is worth the code it would take. The canvas size is passed in rather
// than read off the element, since the caller needs the same two numbers for the view box and
// the two must agree or a fit is computed against a different box than the layout used.
// How much larger than the canvas the layout's world is. The clamp in the tick used to hold every
// vertex inside the element, so a hundred nodes were packed into the same rectangle as six and any
// structure they had came out as a blob. The world is what grows with the graph; the canvas reaches
// it through Fit and the zoom, which is what those are for.
//
// A multiple of the canvas rather than an absolute size, so the world keeps the element's aspect
// ratio and a small graph gets exactly the rectangle it always had. Anything up to roughly sixty
// vertices returns 1 and is laid out as before.
function worldScale(nodes, width, height) {
    const room = Math.sqrt(Math.max(nodes.length, 1)) * 62;
    return Math.max(1, room / Math.min(width, height));
}

function layout(nodes, edges, width, height) {
    const index = new Map(nodes.map((n, i) => [n.id, i]));
    const links = edges
        .map((e) => [index.get(e.source), index.get(e.target)])
        .filter(([a, b]) => a !== undefined && b !== undefined);

    const scale = worldScale(nodes, width, height);
    const worldW = width * scale;
    const worldH = height * scale;
    const cx = width / 2;
    const cy = height / 2;

    for (const [i, node] of nodes.entries()) {
        if (node.x === undefined) {
            // Seeded on a circle rather than at random, so a re-layout of the same graph is
            // reproducible and the first frame is never a knot at the centre.
            const angle = (i / Math.max(nodes.length, 1)) * Math.PI * 2;
            const radius = Math.min(worldW, worldH) * 0.36;
            node.x = cx + Math.cos(angle) * radius;
            node.y = cy + Math.sin(angle) * radius;
            node.vx = 0;
            node.vy = 0;
        }
    }

    let alpha = 1;
    const repulsion = 2600;
    const springLength = 78;
    const springK = 0.045;

    return function tick() {
        alpha *= 0.985;
        for (let i = 0; i < nodes.length; i += 1) {
            const a = nodes[i];
            for (let j = i + 1; j < nodes.length; j += 1) {
                const b = nodes[j];
                let dx = b.x - a.x;
                let dy = b.y - a.y;
                let d2 = dx * dx + dy * dy;
                if (d2 < 0.01) {
                    // Coincident nodes have no direction to separate along, so nudge them by index
                    // rather than randomly, which would make a layout unreproducible.
                    dx = (i - j) * 0.1 + 0.1;
                    dy = 0.1;
                    d2 = dx * dx + dy * dy;
                }
                const force = repulsion / d2;
                const d = Math.sqrt(d2);
                const fx = (dx / d) * force;
                const fy = (dy / d) * force;
                a.vx -= fx;
                a.vy -= fy;
                b.vx += fx;
                b.vy += fy;
            }
        }
        for (const [ai, bi] of links) {
            const a = nodes[ai];
            const b = nodes[bi];
            const dx = b.x - a.x;
            const dy = b.y - a.y;
            const d = Math.hypot(dx, dy) || 0.01;
            const force = (d - springLength) * springK;
            const fx = (dx / d) * force;
            const fy = (dy / d) * force;
            a.vx += fx;
            a.vy += fy;
            b.vx -= fx;
            b.vy -= fy;
        }
        // Weakened as the world grows, or the pull to the centre overwhelms the repulsion at the
        // far edge of a large layout and undoes the room the world was widened to provide.
        const pull = 0.006 / scale;
        const halfW = worldW / 2;
        const halfH = worldH / 2;
        const margin = 26;
        for (const node of nodes) {
            node.vx += (cx - node.x) * pull;
            node.vy += (cy - node.y) * pull;
            if (node.pinned) {
                node.vx = 0;
                node.vy = 0;
                continue;
            }
            node.vx *= 0.82;
            node.vy *= 0.82;
            node.x += node.vx * alpha;
            node.y += node.vy * alpha;
            node.x = Math.max(cx - halfW + margin, Math.min(cx + halfW - margin, node.x));
            node.y = Math.max(cy - halfH + margin, Math.min(cy + halfH - margin, node.y));
        }
        return alpha > 0.02;
    };
}

const SVG_NS = "http://www.w3.org/2000/svg";
const el = (name, attrs = {}) => {
    const node = document.createElementNS(SVG_NS, name);
    for (const [k, v] of Object.entries(attrs)) node.setAttribute(k, v);
    return node;
};

function labelOf(node) {
    return node.labels?.[0] ?? "(none)";
}

function captionOf(node) {
    const props = node.props ?? {};
    for (const key of ["name", "title", "id", "label", "key"]) {
        if (typeof props[key] === "string") return props[key];
    }
    return `#${node.id}`;
}

// Only these column names are read out of a result, in both cases because guessing from the
// values instead would light up unrelated integers. A node reference highlights its vertex; a
// group column beside one recolors the vertices by group, which is what makes the components and
// communities procedures show their answer in the graph rather than only in the table.
const NODE_COLUMNS = new Set(["id", "nodeid", "node"]);
const GROUP_COLUMNS = new Set(["communityid", "componentid"]);

// Fixed rather than hashed off the group id, so two adjacent communities get colors that can be
// told apart instead of whatever two hashes happen to land on.
const GROUP_PALETTE = [
    "#7e56c2",
    "#0b6bcb",
    "#1c7c54",
    "#c77700",
    "#b3261e",
    "#00868b",
    "#7a5195",
    "#556b2f",
    "#a0522d",
    "#3f51b5",
];

function resultOverlay() {
    if (!lastResult) return {lit: null, groupIds: null, groupLabel: ""};
    const lower = lastResult.columns.map((c) => c.toLowerCase());

    const lit = new Set();
    lower.forEach((name, i) => {
        if (!NODE_COLUMNS.has(name)) return;
        for (const row of lastResult.rows) {
            if (Number.isInteger(row[i])) lit.add(row[i]);
        }
    });

    const nodeAt = lower.findIndex((name) => NODE_COLUMNS.has(name));
    const groupAt = lower.findIndex((name) => GROUP_COLUMNS.has(name));
    let groupIds = null;
    if (nodeAt >= 0 && groupAt >= 0) {
        groupIds = new Map();
        for (const row of lastResult.rows) {
            if (Number.isInteger(row[nodeAt]) && Number.isInteger(row[groupAt])) {
                groupIds.set(row[nodeAt], row[groupAt]);
            }
        }
        if (groupIds.size === 0) groupIds = null;
    }

    return {
        lit: lit.size ? lit : null,
        groupIds,
        groupLabel: groupIds ? lastResult.columns[groupAt] : "",
    };
}

// Pointer capture keeps a drag alive once the pointer leaves the element, which improves the gesture
// rather than enabling it. It throws when the id is not an active pointer, and letting that escape
// abandoned the gesture entirely, since the move listener is attached after this call.
function capturePointer(element, pointerId) {
    try {
        element.setPointerCapture(pointerId);
    } catch {
        // Without capture the drag still works while the pointer stays over the element.
    }
}

let simGeneration = 0;

// What is actually on screen: the snapshot narrowed by the focus and result-only controls. Fit, the
// drag handlers, and the hover highlight all read this rather than the snapshot, or they would
// frame and move vertices that are not drawn.
let drawn = {nodes: [], edges: []};

// Set by any deliberate view change, so the automatic fit a layout wider than the canvas needs
// cannot undo a zoom the visitor just made.
let viewAdjusted = false;

// The vertex the inspector is showing. Hovering sets it; clicking pins it so that moving the pointer
// on, including to the inspector's own Focus button, leaves the panel where it was.
let selectedId = null;
let pinnedSelection = false;

// Hover selects only once the pointer has rested on a vertex. Switching on entry made the panel
// unusable on a dense graph: the straight line from a vertex to the inspector's Focus button crosses
// whatever lies between them, so by the time the pointer arrived the panel was showing the last
// vertex it passed over and Focus acted on that one instead. Measured at six vertices crossed on a
// ninety-vertex lattice. A glide spends well under this on each; parking on one still feels instant.
const HOVER_INTENT_MS = 160;
let hoverTimer = null;

// Nothing changes the selection while the pointer is over the inspector, so a panel being read
// cannot be replaced by whatever is underneath it. Cleared on leaving, so hovering keeps working.
let overPanel = false;

// Focus narrows the view to one vertex's neighborhood. The ball is taken over the drawn graph
// rather than the database, so on a capped snapshot it is a neighborhood within what was drawn;
// the count in the toolbar already says when the cap is in play.
let focusRoot = null;
const FOCUS_HOPS = 2;

// Draw only the vertices the last result mentioned. Off by default, since the dimming overlay
// already answers "which of these did my query touch" without discarding the context around them.
let resultOnly = false;

// Focus and Result only narrow the view by a rule, which is all there was: any vertex the rule
// excluded needed the rule relaxed for every other vertex too. These two are the per-vertex
// override, so a neighborhood can be grown one vertex at a time and anything uninteresting taken
// back out. Both hold ids rather than vertices, so a vertex a later query deletes drops out of the
// view by itself.
const revealed = new Set();
const dismissed = new Set();

// What the find box is matching. Held here rather than read off the input, so a redraw reapplies
// the highlight instead of losing it.
let searchTerm = "";

// Assigned by `drawGraph`, because the highlight has to reach the elements that drawing created.
// Repainting classes is what the find box does instead of redrawing: a redraw restarts the
// simulation, and searching a graph should not rearrange it.
let highlightSearch = () => [];

// A redraw clears the selection, which is right for a new query and wrong for an action taken from
// the inspector. Exploring is a run of expansions from one vertex, and closing the panel after each
// would mean hovering the same vertex back open every time.
let reselectId = null;

// Undirected adjacency over the drawn edges. Focus wants the context around a vertex, and following
// only the outgoing side would hide whatever points at it, which is usually the interesting half.
function neighborhood(edges, root, hops) {
    const near = new Map();
    const link = (from, to) => {
        const list = near.get(from);
        if (list) list.push(to);
        else near.set(from, [to]);
    };
    for (const e of edges) {
        link(e.source, e.target);
        link(e.target, e.source);
    }
    const ball = new Set([root]);
    let frontier = [root];
    for (let hop = 0; hop < hops; hop += 1) {
        const next = [];
        for (const id of frontier) {
            for (const other of near.get(id) ?? []) {
                if (!ball.has(other)) {
                    ball.add(other);
                    next.push(other);
                }
            }
        }
        frontier = next;
    }
    return ball;
}

function visibleGraph() {
    let {nodes, edges} = snapshot;
    if (resultOnly) {
        const {lit} = resultOverlay();
        if (lit) {
            nodes = nodes.filter((node) => lit.has(node.id));
            const kept = new Set(nodes.map((node) => node.id));
            edges = edges.filter((e) => kept.has(e.source) && kept.has(e.target));
        }
    }
    if (focusRoot !== null && nodes.some((node) => node.id === focusRoot)) {
        const ball = neighborhood(edges, focusRoot, FOCUS_HOPS);
        nodes = nodes.filter((node) => ball.has(node.id));
        edges = edges.filter((e) => ball.has(e.source) && ball.has(e.target));
    }

    // The overrides are applied last, so revealing wins over both rules above and dismissing wins
    // over revealing. Edges are re-derived from the whole snapshot rather than narrowed from the
    // set above, or a revealed vertex would arrive with no line joining it to what revealed it.
    const shown = new Set(nodes.map((node) => node.id));
    for (const id of revealed) shown.add(id);
    for (const id of dismissed) shown.delete(id);
    return {
        nodes: snapshot.nodes.filter((node) => shown.has(node.id)),
        edges: snapshot.edges.filter((e) => shown.has(e.source) && shown.has(e.target)),
    };
}

// Snapshot neighbors of `id` in either direction that the view is not currently drawing. The
// snapshot is the capped scan and not the database, so this can only ever surface what the cap
// already kept; the toolbar count is what says the cap is in play.
function hiddenNeighbors(id) {
    const shown = new Set(drawn.nodes.map((node) => node.id));
    const out = new Set();
    for (const e of snapshot.edges) {
        if (e.source === id && !shown.has(e.target)) out.add(e.target);
        if (e.target === id && !shown.has(e.source)) out.add(e.source);
    }
    out.delete(id);
    return out;
}

// A vertex arriving from an expansion has never been laid out, so the force pass would seed it on
// the circle it seeds a fresh graph on, which is nowhere near the vertex that revealed it. Placed
// in a ring around that vertex instead, so the expansion reads as growth from where it happened.
const REVEAL_RING = 70;

function expandFrom(id) {
    const fresh = hiddenNeighbors(id);
    if (fresh.size === 0) return 0;
    const root = snapshot.nodes.find((node) => node.id === id);
    const byId = new Map(snapshot.nodes.map((node) => [node.id, node]));
    let i = 0;
    for (const other of fresh) {
        revealed.add(other);
        // Revealing overrides an earlier dismissal, or expanding a vertex whose neighbor was
        // dismissed would silently do nothing and read as a broken button.
        dismissed.delete(other);
        const node = byId.get(other);
        if (node && node.x === undefined && root?.x !== undefined) {
            const angle = (i / fresh.size) * Math.PI * 2;
            node.x = root.x + Math.cos(angle) * REVEAL_RING;
            node.y = root.y + Math.sin(angle) * REVEAL_RING;
            node.vx = 0;
            node.vy = 0;
        }
        i += 1;
    }
    return fresh.size;
}

// Matched against what the vertex shows on screen, plus its `#id`, which is the other handle the
// page gives a vertex (the inspector titles by it, and so does every "selection stolen" report).
// Searching every property would find vertices whose match is nowhere visible.
function matchesSearch(node, needle) {
    if (captionOf(node).toLowerCase().includes(needle)) return true;
    if ((node.labels ?? []).some((l) => l.toLowerCase().includes(needle))) return true;
    return `#${node.id}`.includes(needle);
}

// Slot each edge within its unordered pair, so parallel and reciprocal edges are drawn on separate
// curves instead of one exactly on top of another. Two `SIMILAR_TO` edges between the same pair of
// products were one line before this, and a directed graph that draws `a->b` and `b->a` identically
// is not showing its direction at all.
function edgeSlots(edges) {
    const groups = new Map();
    for (const e of edges) {
        const key = e.source < e.target ? `${e.source}|${e.target}` : `${e.target}|${e.source}`;
        const list = groups.get(key);
        if (list) list.push(e);
        else groups.set(key, [e]);
    }
    const slots = new Map();
    for (const list of groups.values()) {
        list.forEach((e, index) => slots.set(e, {index, count: list.length}));
    }
    return slots;
}

// The path for one edge, with both ends pulled back off the vertex rims so the arrowhead sits on
// the target's edge rather than under it.
function edgePath(a, b, {index, count}) {
    if (a === b) {
        // A self-loop has no chord to bend, so it is a teardrop above the vertex, widened per slot
        // so two loops on one vertex read as two. Nothing drew at all for these before.
        const size = LOOP_SIZE * (1 + index * 0.6);
        return (
            `M ${a.x - NODE_RADIUS * 0.7} ${a.y - NODE_RADIUS * 0.7}` +
            ` C ${a.x - size * 1.7} ${a.y - size * 2.7},` +
            ` ${a.x + size * 1.7} ${a.y - size * 2.7},` +
            ` ${a.x + NODE_RADIUS * 0.75} ${a.y - NODE_RADIUS * 0.6}`
        );
    }
    const dx = b.x - a.x;
    const dy = b.y - a.y;
    const len = Math.hypot(dx, dy) || 0.01;
    // The perpendicular is measured from the lower node id, so the two halves of a reciprocal pair
    // offset the same way and therefore land on opposite sides of the chord.
    const flip = a.id < b.id ? 1 : -1;
    const offset = count > 1 ? (index - (count - 1) / 2) * EDGE_CURVE * flip : 0;
    const cx = (a.x + b.x) / 2 + (-dy / len) * offset;
    const cy = (a.y + b.y) / 2 + (dx / len) * offset;

    // Pulled back along the curve's own tangent at each end, which for a quadratic is the direction
    // to the control point. Using the chord instead leaves the arrow off the rim on a curved edge.
    const inX = b.x - cx;
    const inY = b.y - cy;
    const inLen = Math.hypot(inX, inY) || 0.01;
    const outX = cx - a.x;
    const outY = cy - a.y;
    const outLen = Math.hypot(outX, outY) || 0.01;
    const back = NODE_RADIUS + ARROW_GAP;
    return (
        `M ${a.x + (outX / outLen) * NODE_RADIUS} ${a.y + (outY / outLen) * NODE_RADIUS}` +
        ` Q ${cx} ${cy} ${b.x - (inX / inLen) * back} ${b.y - (inY / inLen) * back}`
    );
}

function drawGraph() {
    // A drag whose pointer is released after a redraw calls the previous drawing's `start`,
    // which would put a loop over replaced nodes back into the shared handle.
    const generation = ++simGeneration;
    const svg = $("svg");
    if (sim) {
        cancelAnimationFrame(sim);
        sim = null;
    }
    svg.replaceChildren();
    svg.classList.remove("hovering");
    $("inspect").hidden = true;
    clearTimeout(hoverTimer);
    selectedId = null;
    pinnedSelection = false;

    // A redraw returns the view to the whole canvas, so a fit is discarded by the next query
    // rather than silently framing a graph it was not computed for.
    const width = svg.clientWidth || 800;
    const height = svg.clientHeight || 500;
    setViewBox(svg, {x: 0, y: 0, w: width, h: height});
    viewAdjusted = false;

    const {nodes, edges} = visibleGraph();
    drawn = {nodes, edges};
    const hidden = snapshot.nodes.length - nodes.length;
    $("graph-count").textContent =
        `${plural(nodes.length, "node")} and ${plural(edges.length, "edge")}` +
        (snapshot.truncated ? " (capped at 300)" : "") +
        (hidden > 0 ? `, ${hidden} hidden` : "");
    $("reset-view").hidden = focusRoot === null && revealed.size === 0 && dismissed.size === 0;

    const {lit, groupIds, groupLabel} = resultOverlay();

    // A node the result did not mention has no group, so it keeps a neutral fill rather than
    // borrowing the color of a group it is not in.
    const groupOrder = groupIds ? [...new Set(groupIds.values())].sort((a, b) => a - b) : [];
    const groupColor = new Map(
        groupOrder.map((value, i) => [value, GROUP_PALETTE[i % GROUP_PALETTE.length]]),
    );
    const fillOf = (node) =>
        groupIds
            ? (groupColor.get(groupIds.get(node.id)) ?? "var(--md-default-fg-color--lighter)")
            : colorOf(labelOf(node));

    // One type is the common case and colouring it says nothing, so a single-type graph keeps the
    // neutral stroke it always had. Past one, the colour is the only thing telling a `BOUGHT` from
    // a `SIMILAR_TO`, both of which used to draw as the same grey line.
    const edgeTypes = [...new Set(edges.map((e) => e.type ?? ""))].sort();
    const manyTypes = edgeTypes.length > 1;
    const edgeColor = new Map(
        edgeTypes.map((type) => [
            type,
            manyTypes ? `hsl(${hueOf(type)} 52% 44%)` : "var(--md-default-fg-color--lighter)",
        ]),
    );
    const markerId = new Map(edgeTypes.map((type, i) => [type, `arrow${i}`]));

    $("legend").innerHTML = (groupIds
            ? groupOrder.map(
                (value) =>
                    `<span><i style="background:${groupColor.get(value)}"></i>${esc(groupLabel)} ${value}</span>`,
            )
            : [...new Set(nodes.map(labelOf))]
                .sort()
                .map((l) => `<span><i style="background:${colorOf(l)}"></i>${esc(l)}</span>`)
    )
        .concat(
            manyTypes
                ? edgeTypes.map(
                    (t) =>
                        `<span><i class="rel" style="background:${edgeColor.get(t)}"></i>${esc(t)}</span>`,
                )
                : [],
        )
        .join("");

    if (nodes.length === 0) {
        highlightSearch = () => [];
        reportSearch([]);
        svg.append(
            el("text", {
                x: 16,
                y: 28,
                fill: "var(--md-default-fg-color--light)",
                "font-size": "13"
            }),
        );
        svg.lastChild.textContent = snapshot.nodes.length
            ? "Nothing to draw under the current filter. Press Show all, or turn off Result only."
            : "Nothing to draw. Run a CREATE, or click Reset data.";
        return;
    }

    // One marker per edge colour. A marker cannot inherit the stroke of the path that references
    // it, so a shared arrowhead would be one colour while its edges were several.
    const defs = el("defs");
    for (const [type, id] of markerId) {
        const marker = el("marker", {
            id,
            viewBox: "0 0 10 10",
            refX: "10",
            refY: "5",
            markerWidth: "5",
            markerHeight: "5",
            orient: "auto",
        });
        marker.append(el("path", {d: "M 0 0 L 10 5 L 0 10 z", fill: edgeColor.get(type)}));
        defs.append(marker);
    }
    svg.append(defs);

    const linkLayer = el("g");
    const nodeLayer = el("g");
    svg.append(linkLayer, nodeLayer);

    const byId = new Map(nodes.map((n) => [n.id, n]));
    const slots = edgeSlots(edges);
    const degree = new Map();
    for (const e of edges) {
        degree.set(e.source, (degree.get(e.source) ?? 0) + 1);
        degree.set(e.target, (degree.get(e.target) ?? 0) + 1);
    }

    const paths = edges.map((e) => {
        const type = e.type ?? "";
        const path = el("path", {
            class: "edge",
            fill: "none",
            stroke: edgeColor.get(type),
            "marker-end": `url(#${markerId.get(type)})`,
        });
        path.dataset.source = e.source;
        path.dataset.target = e.target;
        if (e.type) {
            const title = el("title");
            title.textContent = e.type;
            path.append(title);
        }
        linkLayer.append(path);
        return path;
    });

    // Above the density where captions stop being a labeling and become a wall of text, only the
    // best-connected keep one; the rest appear when a vertex is hovered.
    const quietCaptions = nodes.length > LABEL_ALL_MAX;
    const loud = new Set(
        quietCaptions
            ? [...nodes]
                .sort((a, b) => (degree.get(b.id) ?? 0) - (degree.get(a.id) ?? 0))
                .slice(0, LABEL_TOP)
                .map((n) => n.id)
            : nodes.map((n) => n.id),
    );

    const groups = nodes.map((node) => {
        const group = el("g", {class: "node"});
        group.dataset.id = node.id;
        const circle = el("circle", {
            r: NODE_RADIUS,
            fill: fillOf(node),
        });
        const text = el("text", {
            class: loud.has(node.id) ? "cap" : "cap quiet",
            "text-anchor": "middle",
            dy: NODE_RADIUS + 12,
        });
        text.textContent = captionOf(node);
        group.append(circle, text);
        if (lit && !lit.has(node.id)) group.classList.add("dim");

        group.addEventListener("pointerenter", () => {
            clearTimeout(hoverTimer);
            hoverTimer = setTimeout(() => {
                setHover(node.id);
                select(node, false);
            }, HOVER_INTENT_MS);
        });
        group.addEventListener("pointerleave", () => {
            clearTimeout(hoverTimer);
            setHover(null);
        });

        group.addEventListener("pointerdown", (e) => {
            e.stopPropagation();
            select(node, true);
            node.pinned = true;
            capturePointer(group, e.pointerId);
            const move = (ev) => {
                const point = toSvg(svg, ev);
                node.x = point.x;
                node.y = point.y;
                paint();
            };
            const up = () => {
                node.pinned = false;
                group.removeEventListener("pointermove", move);
                group.removeEventListener("pointerup", up);
                group.removeEventListener("pointercancel", up);
                group.removeEventListener("lostpointercapture", up);
                start();
            };
            group.addEventListener("pointermove", move);
            group.addEventListener("pointerup", up);
            group.addEventListener("pointercancel", up);
            group.addEventListener("lostpointercapture", up);
        });
        nodeLayer.append(group);
        return group;
    });

    const groupById = new Map(nodes.map((node, i) => [node.id, groups[i]]));

    // Hovering selects: the inspector opens on the vertex under the pointer rather than waiting for
    // a click. A click pins that selection, which is what makes the inspector reachable at all,
    // since crossing another vertex on the way to the panel would otherwise replace what the panel
    // is showing before the pointer arrived. Clicking the background unpins and clears.
    function select(node, pin) {
        if ((pinnedSelection || overPanel) && !pin) return;
        pinnedSelection = pin;
        if (selectedId !== node.id) {
            groupById.get(selectedId)?.classList.remove("selected");
            selectedId = node.id;
            groupById.get(node.id)?.classList.add("selected");
            inspect(node);
        } else if (pin) {
            groupById.get(node.id)?.classList.add("selected");
        }
    }

    // Hovering one vertex fades everything that is not it or next to it. This is a separate class
    // from the result overlay's `dim` so the two compose: a hover inside a highlighted result still
    // shows which vertices the result lit.
    function setHover(id) {
        for (const g of groups) g.classList.remove("near");
        for (const p of paths) p.classList.remove("near");
        if (id === null) {
            svg.classList.remove("hovering");
            return;
        }
        groupById.get(id)?.classList.add("near");
        for (const [i, e] of edges.entries()) {
            if (e.source !== id && e.target !== id) continue;
            paths[i].classList.add("near");
            groupById.get(e.source)?.classList.add("near");
            groupById.get(e.target)?.classList.add("near");
        }
        svg.classList.add("hovering");
    }

    // A separate class from `dim` and from `near` for the same reason those are separate from each
    // other: a find inside a highlighted result, or with a vertex hovered, has to compose with both
    // rather than replace either.
    highlightSearch = () => {
        const needle = searchTerm.trim().toLowerCase();
        const hits = [];
        for (const [i, group] of groups.entries()) {
            const hit = needle !== "" && matchesSearch(nodes[i], needle);
            group.classList.toggle("match", hit);
            if (hit) hits.push(nodes[i]);
        }
        svg.classList.toggle("searching", needle !== "");
        return hits;
    };

    function paint() {
        for (const [i, path] of paths.entries()) {
            const a = byId.get(Number(path.dataset.source));
            const b = byId.get(Number(path.dataset.target));
            if (!a || !b) continue;
            path.setAttribute("d", edgePath(a, b, slots.get(edges[i])));
        }
        for (const [i, group] of groups.entries()) {
            group.setAttribute("transform", `translate(${nodes[i].x} ${nodes[i].y})`);
        }
    }

    const tick = layout(nodes, edges, width, height);
    // A world wider than the canvas is drawn mostly off-screen until something frames it, and the
    // visitor has no way to know there is more out there. Only when it settles, and only if the
    // view has not been touched since the redraw.
    const needsFit = worldScale(nodes, width, height) > 1;
    const settle = () => {
        if (needsFit && !viewAdjusted) fitToGraph();
    };

    function start() {
        if (generation !== simGeneration) return;
        if (sim) cancelAnimationFrame(sim);
        if (REDUCED_MOTION.matches) {
            // Settled in one pass and painted once. The bound is where the animated form stops anyway,
            // since alpha starts at 1, decays by 0.985 a step, and the loop ends below 0.02.
            for (let i = 0; i < 260 && tick(); i += 1) ;
            paint();
            settle();
            return;
        }
        const step = () => {
            const running = tick();
            paint();
            if (running) {
                sim = requestAnimationFrame(step);
            } else {
                sim = null;
                settle();
            }
        };
        sim = requestAnimationFrame(step);
    }

    if (reselectId !== null) {
        const again = nodes.find((n) => n.id === reselectId);
        reselectId = null;
        // Pinned, so the panel the action came from cannot be replaced by whatever the pointer
        // happens to be resting on when the redraw lands.
        if (again) select(again, true);
    }

    paint();
    reportSearch(highlightSearch());
    start();
}

// The view box the graph is currently drawn through. Layout coordinates are canvas pixels, so
// before Fit existed this was always one to one with the element and a pointer position needed no
// conversion. It is tracked rather than read back off the attribute so a drag cannot parse a
// string per pointer move.
let viewBox = null;

function setViewBox(svg, box) {
    viewBox = box;
    svg.setAttribute("viewBox", `${box.x} ${box.y} ${box.w} ${box.h}`);
    // Published so captions and strokes can divide it back out and hold their size on screen. A
    // fitted graph otherwise renders its labels at whatever fraction of a pixel the fit implies,
    // which is the state the fit was supposed to rescue it from. One write per view change rather
    // than one per element per frame.
    svg.style.setProperty("--gscale", String(box.w / (svg.clientWidth || 800)));
}

function toSvg(svg, event) {
    const rect = svg.getBoundingClientRect();
    const box = viewBox ?? {x: 0, y: 0, w: rect.width, h: rect.height};
    return {
        x: box.x + ((event.clientX - rect.left) * box.w) / rect.width,
        y: box.y + ((event.clientY - rect.top) * box.h) / rect.height,
    };
}

function inspect(node) {
    const props = Object.entries(node.props ?? {});
    const rows = props
        .map(([k, v]) => `<dt>${esc(k)}</dt><dd>${esc(JSON.stringify(v))}</dd>`)
        .join("");
    const more = hiddenNeighbors(node.id).size;
    $("inspect").innerHTML =
        `<h5><i class="swatch" style="width:9px;height:9px;border-radius:3px;background:${colorOf(
            labelOf(node),
        )}"></i>${esc(node.labels?.join(":") || "(no label)")} <span style="color:var(--md-default-fg-color--light);font-family:var(--md-code-font)">#${node.id}</span></h5>` +
        `<div class="inspect-actions">` +
        `<button class="btn sm" id="expand-node"${more === 0 ? " disabled" : ""} title="${
            more === 0
                ? "Every neighbor of this vertex is already drawn"
                : `Add this vertex's ${plural(more, "undrawn neighbor")} to the view`
        }">Expand${more === 0 ? "" : ` ${more}`}</button>` +
        `<button class="btn sm" id="focus-node" title="Show only this vertex and what is within ${FOCUS_HOPS} hops of it">Focus</button>` +
        `<button class="btn sm" id="dismiss-node" title="Take this vertex out of the view">Dismiss</button>` +
        `</div>` +
        (rows ? `<dl>${rows}</dl>` : '<div class="empty">No properties.</div>');
    $("inspect").hidden = false;
    $("focus-node").addEventListener("click", () => {
        focusRoot = node.id;
        drawGraph();
    });
    $("expand-node").addEventListener("click", () => {
        const grew = expandFrom(node.id);
        reselectId = node.id;
        drawGraph();
        setStatus("ok", `Revealed ${plural(grew, "neighbor")} of #${node.id}.`);
    });
    $("dismiss-node").addEventListener("click", () => {
        dismissed.add(node.id);
        revealed.delete(node.id);
        drawGraph();
    });
}

// Zoom and pan both move the view box, which is also what Fit sets and what `toSvg` maps a pointer
// through, so those three cannot disagree about where the graph is.
const ZOOM_STEP = 1.25;

const canvasSize = (svg) => ({w: svg.clientWidth || 800, h: svg.clientHeight || 500});

// `focal` is the world point to hold still, so a wheel zoom keeps whatever is under the pointer
// under the pointer. Without it, zooming in on a corner walks the graph off the canvas.
function zoomBy(factor, focal) {
    const svg = $("svg");
    if (!viewBox) return;
    viewAdjusted = true;
    const {w: canvasW} = canvasSize(svg);
    // Bounded, or a few scrolls leave an empty canvas with no way to tell which direction the graph
    // went. The clamp is on the width and the same scale is applied to the height, so the box keeps
    // the element's aspect ratio and nothing is letterboxed.
    const clamped = Math.min(Math.max(viewBox.w * factor, canvasW / 8), canvasW * 4);
    const scale = clamped / viewBox.w;
    const point = focal ?? {
        x: viewBox.x + viewBox.w / 2,
        y: viewBox.y + viewBox.h / 2,
    };
    setViewBox(svg, {
        x: point.x - (point.x - viewBox.x) * scale,
        y: point.y - (point.y - viewBox.y) * scale,
        w: viewBox.w * scale,
        h: viewBox.h * scale,
    });
}

$("zoom-in").addEventListener("click", () => zoomBy(1 / ZOOM_STEP));
$("zoom-out").addEventListener("click", () => zoomBy(ZOOM_STEP));

$("svg").addEventListener(
    "wheel",
    (e) => {
        // Claimed rather than shared: the page scrolls as a document, and a wheel over the canvas that
        // both zoomed and scrolled the page would be unusable. Hence a non-passive listener.
        e.preventDefault();
        zoomBy(e.deltaY > 0 ? ZOOM_STEP : 1 / ZOOM_STEP, toSvg($("svg"), e));
    },
    {passive: false},
);

$("svg").addEventListener("pointerdown", (e) => {
    $("inspect").hidden = true;
    if (selectedId !== null) {
        $("svg").querySelector(".node.selected")?.classList.remove("selected");
        selectedId = null;
    }
    pinnedSelection = false;
    // A vertex has its own drag handler and stops propagation; this is the guard for anything that
    // does not, so a pan cannot start on top of a node.
    if (e.target.closest(".node")) return;

    const svg = $("svg");
    const from = viewBox ? {...viewBox} : null;
    if (!from) return;
    viewAdjusted = true;
    capturePointer(svg, e.pointerId);

    const move = (ev) => {
        // Measured against the box the drag started from rather than the current one, or the pan chases
        // itself: each move would be applied to a box the previous move had already shifted.
        const rect = svg.getBoundingClientRect();
        const dx = ((ev.clientX - e.clientX) * from.w) / rect.width;
        const dy = ((ev.clientY - e.clientY) * from.h) / rect.height;
        setViewBox(svg, {x: from.x - dx, y: from.y - dy, w: from.w, h: from.h});
    };
    const up = () => {
        svg.removeEventListener("pointermove", move);
        svg.removeEventListener("pointerup", up);
        svg.removeEventListener("pointercancel", up);
        svg.removeEventListener("lostpointercapture", up);
    };
    svg.addEventListener("pointermove", move);
    svg.addEventListener("pointerup", up);
    svg.addEventListener("pointercancel", up);
    svg.addEventListener("lostpointercapture", up);
});
// The layout keeps every vertex inside the canvas, so this only ever zooms in. That is the
// direction worth having: a handful of nodes otherwise sit in the middle of a mostly empty
// canvas. A redraw resets the view, so there is no "unfit" to provide.
// Framing the drawn graph, shared by the Fit button and the automatic fit a layout wider than the
// canvas needs. Before the world grew with the graph this only ever zoomed in, since every vertex
// was clamped inside the element; now a large graph is laid out beyond the canvas and this is the
// only thing that brings it into view.
function fitToGraph(subset = drawn.nodes) {
    const svg = $("svg");
    const placed = subset.filter((node) => node.x !== undefined);
    if (placed.length === 0) return;

    let minX = Infinity;
    let maxX = -Infinity;
    let minY = Infinity;
    let maxY = -Infinity;
    for (const node of placed) {
        if (node.x < minX) minX = node.x;
        if (node.x > maxX) maxX = node.x;
        if (node.y < minY) minY = node.y;
        if (node.y > maxY) maxY = node.y;
    }
    // Clears the largest radius and the caption below it, or fitting would crop the labels it is
    // meant to bring into view.
    const pad = 42;
    minX -= pad;
    maxX += pad;
    minY -= pad;
    maxY += pad;

    // Matched to the element's own aspect ratio. A view box with a different one is letterboxed by
    // the default `preserveAspectRatio`, so the fit would come out loose on one axis.
    const rect = svg.getBoundingClientRect();
    const aspect = (rect.width || 800) / (rect.height || 500);
    let w = maxX - minX;
    let h = maxY - minY;
    if (w / h > aspect) h = w / aspect;
    else w = h * aspect;

    setViewBox(svg, {
        x: (minX + maxX) / 2 - w / 2,
        y: (minY + maxY) / 2 - h / 2,
        w,
        h,
    });
}

$("fit").addEventListener("click", () => {
    viewAdjusted = true;
    fitToGraph();
});

$("inspect").addEventListener("pointerenter", () => {
    overPanel = true;
    clearTimeout(hoverTimer);
});
$("inspect").addEventListener("pointerleave", () => {
    overPanel = false;
});

$("reset-view").addEventListener("click", () => {
    focusRoot = null;
    revealed.clear();
    dismissed.clear();
    drawGraph();
});

function reportSearch(hits) {
    const empty = searchTerm.trim() === "";
    $("search-count").textContent = empty ? "" : `${hits.length}/${drawn.nodes.length}`;
    $("graph-search").closest(".graph-find").classList.toggle("miss", !empty && hits.length === 0);
}

$("graph-search").addEventListener("input", (e) => {
    searchTerm = e.target.value;
    reportSearch(highlightSearch());
});

// Enter frames the matches, which is the half of finding that highlighting alone does not do: on a
// graph large enough to need a find box, a match can be highlighted well outside the view.
$("graph-search").addEventListener("keydown", (e) => {
    if (e.key !== "Enter") return;
    e.preventDefault();
    const hits = highlightSearch();
    if (hits.length === 0) return;
    viewAdjusted = true;
    fitToGraph(hits);
});

$("result-only").addEventListener("change", (e) => {
    resultOnly = e.target.checked;
    drawGraph();
});

$("relayout").addEventListener("click", () => {
    for (const node of snapshot.nodes) node.x = undefined;
    drawGraph();
});
// The layout reads the viewport size when it starts, so a resize needs a fresh one. Positions
// survive, since only an undefined coordinate is re-seeded.
let resizeTimer = null;
addEventListener("resize", () => {
    if (!$("pane-graph").classList.contains("on")) return;
    clearTimeout(resizeTimer);
    resizeTimer = setTimeout(drawGraph, 150);
});

// ---------------------------------------------------------------------------
// Export, share, theme
// ---------------------------------------------------------------------------

function download(name, mime, text) {
    const url = URL.createObjectURL(new Blob([text], {type: mime}));
    const anchor = document.createElement("a");
    anchor.href = url;
    anchor.download = name;
    anchor.click();
    // Firefox and Safari fetch the object URL asynchronously after the click, so revoking on
    // this tick produces an empty download.
    setTimeout(() => URL.revokeObjectURL(url), 0);
}

// Every property the graph is drawn with, and nothing else. A downloaded picture is opened where
// neither the stylesheet nor the custom properties it reads exist, so each of these is resolved off
// the live element and written onto the copy as an attribute. `--gscale` disappears with them,
// since what it feeds is a `calc` that is already resolved by the time it is read.
//
// These inherit, so a value equal to the parent's is dropped rather than repeated. Writing all of
// them on every element made a twenty-six vertex graph a 68 KB file, most of it the page's font
// stack restated on each circle.
const EXPORT_INHERITED = [
    "fill",
    "fill-opacity",
    "stroke",
    "stroke-width",
    "stroke-linecap",
    "stroke-linejoin",
    "font-family",
    "font-size",
    "font-weight",
    "text-anchor",
    "paint-order",
];

// `opacity` does not inherit, so it is compared against its own initial value instead. This is what
// carries the result overlay's dimming and the suppressed captions into the file.
const EXPORT_OPACITY_DEFAULT = "1";

const RGBA = /^rgba\(\s*([\d.]+)[\s,]+([\d.]+)[\s,]+([\d.]+)[\s,/]+([\d.]+)\s*\)$/;

// A translucent colour computes to `rgba(r, g, b, a)`, which is a CSS colour rather than a value an
// SVG presentation attribute is required to accept. A browser takes it; Inkscape drops the whole
// declaration and paints black, which turned the exported captions from light text on a dark canvas
// into unreadable dark text on one. The opaque colour plus the matching `-opacity` attribute says
// the same thing in the form every consumer understands.
function splitTranslucentPaint(root) {
    for (const element of [root, ...root.querySelectorAll("*")]) {
        for (const prop of ["fill", "stroke"]) {
            const parts = element.getAttribute(prop)?.match(RGBA);
            if (!parts) continue;
            const [, r, g, b, a] = parts;
            element.setAttribute(prop, `rgb(${r}, ${g}, ${b})`);
            element.setAttribute(`${prop}-opacity`, a);
        }
    }
}

function standaloneSvg() {
    const svg = $("svg");
    // A hover or a find in progress is a state of the page rather than of the picture, and reading
    // computed styles under either bakes its dimming into the file. Dropped for the read and put
    // back, because the classes carry no information the export should keep.
    const overlays = ["hovering", "searching"].filter((c) => svg.classList.contains(c));
    for (const c of overlays) svg.classList.remove(c);

    const clone = svg.cloneNode(true);
    const from = [svg, ...svg.querySelectorAll("*")];
    const to = [clone, ...clone.querySelectorAll("*")];
    for (const [i, source] of from.entries()) {
        const computed = getComputedStyle(source);
        // The root writes the whole set unconditionally, establishing the baseline the rest are
        // diffed against. Diffing it too would compare against the enclosing HTML, which already
        // supplies the page's font, so the file would inherit that font from a stylesheet it no
        // longer has and fall back to the renderer's default instead.
        const inherited = i === 0 ? null : getComputedStyle(source.parentElement);
        for (const prop of EXPORT_INHERITED) {
            const value = computed.getPropertyValue(prop);
            if (value && value !== inherited?.getPropertyValue(prop)) {
                to[i].setAttribute(prop, value);
            }
        }
        const opacity = computed.getPropertyValue("opacity");
        if (opacity && opacity !== EXPORT_OPACITY_DEFAULT) to[i].setAttribute("opacity", opacity);
    }
    const background = getComputedStyle(svg).backgroundColor;
    for (const c of overlays) svg.classList.add(c);

    const box = viewBox ?? {x: 0, y: 0, w: svg.clientWidth || 800, h: svg.clientHeight || 500};
    clone.removeAttribute("style");
    clone.removeAttribute("class");
    clone.setAttribute("xmlns", SVG_NS);
    clone.setAttribute("width", String(Math.round(box.w)));
    clone.setAttribute("height", String(Math.round(box.h)));
    clone.setAttribute("viewBox", `${box.x} ${box.y} ${box.w} ${box.h}`);
    // The canvas colour is a CSS background, which a rasterizer composites onto nothing. An explicit
    // rectangle is what keeps a dark-theme export from being dark text on transparency.
    clone.insertBefore(
        el("rect", {x: box.x, y: box.y, width: box.w, height: box.h, fill: background}),
        clone.firstChild,
    );
    splitTranslucentPaint(clone);
    return {text: new XMLSerializer().serializeToString(clone), box};
}

$("svg-dl").addEventListener("click", () => {
    if (drawn.nodes.length === 0) return;
    download("issundb-graph.svg", "image/svg+xml", standaloneSvg().text);
});

// Twice the drawn size, so the file is legible when it is dropped into a document at its natural
// width rather than blocky.
const PNG_SCALE = 2;

$("png").addEventListener("click", async () => {
    if (drawn.nodes.length === 0) return;
    const {text, box} = standaloneSvg();
    const image = new Image();
    try {
        await new Promise((resolve, reject) => {
            image.onload = resolve;
            image.onerror = () => reject(new Error("the picture could not be rasterized"));
            image.src = `data:image/svg+xml;charset=utf-8,${encodeURIComponent(text)}`;
        });
    } catch {
        setStatus("err", "The browser could not turn the view into a PNG. The SVG download works.");
        return;
    }
    const canvas = document.createElement("canvas");
    canvas.width = Math.round(box.w * PNG_SCALE);
    canvas.height = Math.round(box.h * PNG_SCALE);
    canvas.getContext("2d").drawImage(image, 0, 0, canvas.width, canvas.height);
    canvas.toBlob((blob) => {
        if (!blob) {
            setStatus("err", "The browser could not turn the view into a PNG. The SVG download works.");
            return;
        }
        const url = URL.createObjectURL(blob);
        const anchor = document.createElement("a");
        anchor.href = url;
        anchor.download = "issundb-graph.png";
        anchor.click();
        setTimeout(() => URL.revokeObjectURL(url), 0);
    }, "image/png");
});

$("csv").addEventListener("click", () => {
    if (!lastResult?.columns.length) return;
    const quote = (v) => {
        if (v === null || v === undefined) return "";
        const s = typeof v === "object" ? JSON.stringify(v) : String(v);
        return /[",\n]/.test(s) ? `"${s.replace(/"/g, '""')}"` : s;
    };
    const csv = [
        lastResult.columns.map(quote).join(","),
        ...lastResult.rows.map((r) => r.map(quote).join(",")),
    ].join("\n");
    download("issundb-result.csv", "text/csv", csv);
});

$("json-dl").addEventListener("click", () => {
    if (!lastResult) return;
    const objects = lastResult.rows.map((row) =>
        Object.fromEntries(lastResult.columns.map((c, i) => [c, row[i]])),
    );
    download("issundb-result.json", "application/json", JSON.stringify(objects, null, 2));
});

// The query travels in the fragment, so a shared link never reaches a server even when the
// page is hosted on one.
const b64url = {
    encode: (s) => {
        const bytes = new TextEncoder().encode(s);
        // Chunked rather than one spread: `String.fromCharCode(...bytes)` raises a RangeError
        // past about 130 000 arguments, which a pasted bulk-insert script reaches.
        let binary = "";
        for (let i = 0; i < bytes.length; i += 0x8000) {
            binary += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
        }
        return btoa(binary).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
    },
    decode: (s) =>
        new TextDecoder().decode(
            Uint8Array.from(atob(s.replace(/-/g, "+").replace(/_/g, "/")), (c) => c.charCodeAt(0)),
        ),
};

// A fragment reaches no server, but it does have to survive being pasted, and enough clients
// truncate a long URL that a link past this is worth declining rather than sending out broken.
const MAX_SHARED_SETUP = 24000;

// Set when the page writes its own fragment, so the `hashchange` handler below can tell an incoming
// link from the clipboard fallback's own write.
let ownHashWrite = false;

$("share").addEventListener("click", async () => {
    const parts = [];
    try {
        parts.push(`q=${b64url.encode(editor.value)}`);
    } catch {
        // Encoding was outside the try before, so a query too large to encode rejected silently.
        setStatus("err", "The query is too large to put in a link.");
        return;
    }

    // Without this the link carried the query alone, so a query over data the sender had created
    // returned nothing for whoever opened it. The recipient's boot seeds the sample graph and then
    // replays these, which reproduces the state exactly rather than approximating it from a
    // snapshot: a snapshot is capped at 300 nodes and carries no relationship properties.
    let dropped = 0;
    if (setupLog.length > 0) {
        let setup = "";
        try {
            setup = b64url.encode(setupLog.join(";\n"));
        } catch {
            setup = "";
        }
        if (setup && setup.length <= MAX_SHARED_SETUP) parts.push(`s=${setup}`);
        else dropped = setupLog.length;
    }

    const note = dropped
        ? ` ${plural(dropped, "setup statement")} were too large to include.`
        : setupLog.length > 0
            ? ` It carries ${plural(setupLog.length, "setup statement")}.`
            : "";

    const fragment = parts.join("&");
    try {
        await navigator.clipboard.writeText(
            `${location.origin}${location.pathname}#${fragment}`,
        );
        setStatus("ok", `Link copied.${note}`);
    } catch {
        ownHashWrite = true;
        location.hash = fragment;
        setStatus("", `The link is in the address bar.${note}`);
    }
});

// Changing only the fragment is a same-document navigation, so the module is not re-evaluated and
// `boot` never sees the new link. That is the case for a shared link opened while the playground is
// already in front of the reader, which is how a documentation page's "run this" link behaves on a
// second click.
addEventListener("hashchange", () => {
    // The clipboard fallback above writes the hash itself, and treating that as an incoming link
    // would re-run the query as a side effect of copying it.
    if (ownHashWrite) {
        ownHashWrite = false;
        return;
    }
    const params = new URLSearchParams(location.hash.slice(1));
    // A setup script has to be replayed against a freshly seeded database, or it lands on top of
    // whatever is already here and adds a second copy of its data. Reloading is what gives `boot`
    // the chance to do it in the right order.
    if (params.get("s")) {
        location.reload();
        return;
    }
    let incoming = null;
    try {
        const encoded = params.get("q");
        incoming = encoded ? b64url.decode(encoded) : params.get("cypher");
    } catch {
        incoming = null;
    }
    if (!incoming) return;
    setQuery(incoming);
    run();
});

// `default` and `slate` are Material for MkDocs' scheme names, so the playground and the
// documentation around it are switched by the same vocabulary. The inline script in the head
// applies the stored choice before first paint; this only handles the toggle.
const SUN = "M12 7a5 5 0 100 10 5 5 0 000-10zM12 2v3m0 14v3M2 12h3m14 0h3M4.9 4.9l2.1 2.1m10 10l2.1 2.1m0-14.2l-2.1 2.1m-10 10l-2.1 2.1";
const MOON = "M12 3a9 9 0 109 9c0-.5 0-1-.1-1.4A7 7 0 0112 3z";

function currentScheme() {
    return document.documentElement.getAttribute("data-md-color-scheme") === "slate"
        ? "slate"
        : "default";
}

function applyScheme(scheme) {
    document.documentElement.setAttribute("data-md-color-scheme", scheme);
    // Built through `createElementNS` rather than `innerHTML`, since markup assigned to an SVG
    // element is not reliably parsed into the SVG namespace. The icon offers the scheme you
    // would switch to, which is the convention Material uses.
    const dark = scheme === "slate";
    $("scheme-icon").replaceChildren(
        el(
            "path",
            dark
                ? {d: SUN, stroke: "currentColor", "stroke-width": "2", fill: "none"}
                : {d: MOON},
        ),
    );
}

$("scheme").addEventListener("click", () => {
    const next = currentScheme() === "slate" ? "default" : "slate";
    applyScheme(next);
    try {
        localStorage.setItem(SCHEME_KEY, next);
    } catch {
        // Storage being unavailable only costs the choice its persistence.
    }
    // The graph is drawn with resolved colors rather than custom properties, so it has to be
    // repainted for a scheme change to reach it.
    if ($("pane-graph").classList.contains("on")) drawGraph();
});

$("toggle-side").addEventListener("click", () => $("side").classList.toggle("hidden"));

// ---------------------------------------------------------------------------
// Boot
// ---------------------------------------------------------------------------

// The build stamp comes out of the module rather than a separate file the page would have to fetch,
// so it is empty for a build made outside a git checkout and the footer then names the version
// alone.
function renderPoweredBy({version, build}) {
    const named = build ? `IssunDB (${version}; ${build})` : `IssunDB (${version})`;
    $("powered").textContent =
        `This playground app is powered by ${named}; everything runs safely in your browser.`;
}

let activeSample = 0;

const currentSample = () => SAMPLE_GRAPHS[activeSample] ?? SAMPLE_GRAPHS[0];

function renderSamples() {
    const select = $("sample-graph");
    SAMPLE_GRAPHS.forEach((sample, i) => {
        const option = document.createElement("option");
        option.value = String(i);
        option.textContent = sample.label;
        select.append(option);
    });
    select.addEventListener("change", () => {
        activeSample = Number(select.value);
    });
}

async function seed() {
    await engine.query(currentSample().cypher);
    await refreshSchema();
}

$("reset").addEventListener("click", async () => {
    ({baseline: baselineBytes} = await engine.reset());
    lastResult = null;
    // The discarded writes must not keep travelling in a share link, where replaying them against
    // the fresh seed would rebuild the state Reset was clicked to get rid of.
    setupLog.length = 0;
    // Node ids restart from zero, so a carried-over position would belong to a different node.
    snapshot = {nodes: [], edges: [], truncated: false};
    await seed();
    await refreshGraph();
    setStatus("ok", `Reset. The ${currentSample().label} sample was re-seeded.`);
    setMeta("Run a query to view results.");
    showPane("table");
    $("pane-table").innerHTML =
        `<div class="notice info">Fresh database, seeded with the ${esc(currentSample().label)} sample.` +
        " Pick an example on the left, or write a query.</div>";
});

async function boot() {
    applyScheme(currentScheme());

    spawnWorker();
    const info = await engine.boot();
    baselineBytes = info.baseline;
    ready = true;

    renderPoweredBy(info);
    renderSamples();
    renderDemos();
    renderProcedures();
    renderHistory();
    await seed();

    // Three link forms. `q` is the Share button's base64 query and `s` its optional setup script,
    // and `cypher` is percent-encoded plain text so a link can be written by hand or generated by a
    // docs build. A generator has to encode a plus as %2B, since a fragment read as a query string
    // turns a literal one into a space.
    const params = new URLSearchParams(location.hash.slice(1));

    const setup = params.get("s");
    if (setup) {
        try {
            await engine.query(b64url.decode(setup));
            await refreshSchema();
        } catch {
            // A setup script that no longer applies leaves the seeded graph in place rather than
            // stopping the page from loading. The query it came with still runs, and reports its own
            // error if it depended on what failed.
        }
    }

    let shared = null;
    try {
        const encoded = params.get("q");
        shared = encoded ? b64url.decode(encoded) : params.get("cypher");
    } catch {
        shared = null;
    }

    const stored = shared ? "" : readStoredEditor().trim();
    if (shared) {
        setQuery(shared);
    } else if (stored) {
        setQuery(stored);
    } else {
        setQuery(
            "MATCH (a:Person)-[:KNOWS]->(b:Person)\nRETURN a.name AS from, b.name AS to\nORDER BY from, to",
        );
    }

    await refreshGraph();
    $("boot").remove();

    // A restored query is deliberately not run. It could be a CREATE, and running it on every
    // reload would quietly add another copy of its data. The banner is the whole announcement: the
    // results pane stays empty because the editor header already carries the shortcut that runs it,
    // and the query is visibly sitting in the editor.
    if (stored) {
        showPane("table");
        setStatus("", "Your last query was restored. It has not been run.");
        setMeta("Run a query to view results.");
    } else {
        await run();
    }
}

// A boot failure has one likely cause that the browser's own message does not name. The generated
// glue and the wasm binary are written together and reference a snippet directory by a hash of the
// build; hold a cached copy of one against a fresh copy of the other and the pair disagrees about
// that name, which surfaces as an import object field that "is not an Object". Nothing about it
// suggests the real fix, which is to discard the cached half.
function bootAdvice(message) {
    if (/snippets\/|is not an Object|WebAssembly\.instantiate/.test(message)) {
        return (
            `The cached module does not match the one being served: the JavaScript glue and the ` +
            `<code>.wasm</code> file are generated together and disagree about a build hash. ` +
            `Reload bypassing the cache (<kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>R</kbd>, or ` +
            `<kbd>Cmd</kbd>+<kbd>Shift</kbd>+<kbd>R</kbd>). If it persists, rerun ` +
            `<code>make playground-build</code>, which now clears <code>web/pkg</code> first.`
        );
    }
    if (/Worker|module worker/i.test(message)) {
        return (
            `The engine runs in a module worker, which this browser appears not to support. ` +
            `Firefox 114, Safari 15, and Chrome 80 or newer all do.`
        );
    }
    return (
        `The module is served as <code>web/pkg/</code>; build it with <code>make playground-build</code> ` +
        `and serve the directory over HTTP, since a module cannot be loaded from a file:// path.`
    );
}

boot().catch((e) => {
    const message = String(e?.message ?? e);
    $("boot").innerHTML =
        `<div style="max-width:34rem;text-align:left;font-family:var(--md-code-font);font-size:12.5px">` +
        `<strong>The engine did not load.</strong><br><br>${esc(message)}<br><br>` +
        `${bootAdvice(message)}</div>`;
});
