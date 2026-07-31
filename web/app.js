// The IssunDB playground. Vanilla ES modules, no build step, and no library fetched from a network;
// the two web fonts are the page's only external request. One `Playground` for the tab's lifetime,
// so data accumulates across queries the
// way it would in an embedded database; "Reset data" replaces it.

import init, { Playground } from "./pkg/issundb_wasm.js";
import { DEMO_CATEGORIES, PROCEDURES, SAMPLE_GRAPHS } from "./demos.js";

const $ = (id) => document.getElementById(id);

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

let db = null;
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
});
// Scrolling only moves the backdrop. Re-running the highlighter per scroll event rebuilt
// the whole document's markup on every frame of a drag.
editor.addEventListener("scroll", syncScroll);

editor.addEventListener("keydown", (e) => {
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
  if (e.key === "Tab") {
    e.preventDefault();
    const { selectionStart: a, selectionEnd: b, value } = editor;
    editor.value = value.slice(0, a) + "  " + value.slice(b);
    editor.selectionStart = editor.selectionEnd = a + 2;
    syncHighlight();
  }
});

// ---------------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------------

// Clause phrases that begin a line, longest first so `ON CREATE SET` is recognized before the `SET`
// inside it.
const CLAUSE_PHRASES = [
  ["ON", "CREATE", "SET"],
  ["ON", "MATCH", "SET"],
  ["OPTIONAL", "MATCH"],
  ["DETACH", "DELETE"],
  ["ORDER", "BY"],
  ["UNION", "ALL"],
  ["MATCH"],
  ["WHERE"],
  ["WITH"],
  ["RETURN"],
  ["SKIP"],
  ["LIMIT"],
  ["CREATE"],
  ["MERGE"],
  ["SET"],
  ["REMOVE"],
  ["DELETE"],
  ["UNWIND"],
  ["CALL"],
  ["YIELD"],
  ["UNION"],
  ["FOREACH"],
];

// The clauses whose comma-separated items are patterns rather than expressions. Breaking after each
// comma there turns a long line into a readable list of paths; doing it in RETURN would scatter a
// projection over as many lines as it has columns.
const PATTERN_CLAUSES = new Set(["CREATE", "MERGE"]);

// Deliberately much narrower than the highlighter's keyword set. Uppercasing everything that set
// contains rewrote `issundb.shortestPath` to `issundb.SHORTESTPATH`, and the yield fields `index`
// and `count` to `INDEX` and `COUNT`, all three of which are case-sensitive names rather than
// syntax. So only operators are listed here, and a clause word is uppercased because the phrase
// scan recognized it as one, not because it appears in a list. Function names are left alone: an
// aggregate is conventionally lowercase, and `all(` is not the `ALL` of `UNION ALL`.
const FORMAT_UPPERCASE = new Set([
  "and",
  "or",
  "xor",
  "not",
  "in",
  "is",
  "null",
  "true",
  "false",
  "distinct",
  "as",
  "asc",
  "desc",
  "ascending",
  "descending",
  "starts",
  "ends",
  "contains",
]);

const FORMAT_TOKEN = new RegExp(
  [
    "(\\/\\/[^\\n]*|\\/\\*[\\s\\S]*?\\*\\/)",
    "('(?:[^'\\\\]|\\\\.)*'|\"(?:[^\"\\\\]|\\\\.)*\")",
    "([A-Za-z_]\\w*)",
    "(\\s+)",
    "([^\\s])",
  ].join("|"),
  "g",
);

// Line breaking and keyword casing, and nothing else. Spacing within a line is left as written apart
// from collapsing runs of whitespace, because re-spacing would have to know that the `-` in
// `-[:KNOWS]->` and the `*` in `[r*1..3]` are not binary operators. That restraint is what makes the
// pass safe to run on any query: it cannot change what the query means.
function formatCypher(src) {
  const tokens = [...src.matchAll(FORMAT_TOKEN)].map((m) => ({
    comment: m[1],
    string: m[2],
    word: m[3],
    space: m[4],
    other: m[5],
    text: m[0],
  }));

  // A bracket depth per token, so a clause word inside a pattern or a map is not mistaken for the
  // start of a line, and the index of every word, so a phrase can be matched by lookahead.
  let depth = 0;
  const words = [];
  tokens.forEach((token, i) => {
    token.depth = depth;
    if (token.other && "([{".includes(token.other)) depth += 1;
    if (token.other && ")]}".includes(token.other)) depth -= 1;
    if (token.word) words.push(i);
  });

  const previousWordOf = (index) => {
    for (let j = index - 1; j >= 0; j -= 1) {
      if (tokens[j].space || tokens[j].comment) continue;
      return tokens[j];
    }
    return null;
  };

  const nextNonSpaceOf = (index) => {
    for (let j = index + 1; j < tokens.length; j += 1) {
      if (tokens[j].space) continue;
      return tokens[j];
    }
    return null;
  };

  // `n.set` and `:Match` are names. Guarding the phrase scan and not only the casing is what stops
  // `RETURN n.set` from being broken across two lines at the property.
  const isQualifiedName = (index) => {
    const previous = previousWordOf(index);
    if (previous && (previous.other === "." || previous.other === ":")) return true;
    return Boolean(previous && previous.word && previous.word.toLowerCase() === "as");
  };

  const upperOf = (index) => (index === undefined ? "" : tokens[index].word.toUpperCase());
  const breakAt = new Set();
  const consumed = new Set();
  const phraseWords = new Set();
  words.forEach((i, w) => {
    if (consumed.has(i) || tokens[i].depth !== 0 || isQualifiedName(i)) return;
    const phrase = CLAUSE_PHRASES.find((candidate) =>
      candidate.every((word, k) => upperOf(words[w + k]) === word),
    );
    if (!phrase) return;
    breakAt.add(i);
    tokens[i].clause = phrase.join(" ");
    phrase.forEach((_, k) => phraseWords.add(words[w + k]));
    for (let k = 1; k < phrase.length; k += 1) consumed.add(words[w + k]);
  });

  function shouldUppercase(index) {
    if (phraseWords.has(index)) return true;
    if (!FORMAT_UPPERCASE.has(tokens[index].word.toLowerCase())) return false;
    if (isQualifiedName(index)) return false;
    // A word the phrase scan did not claim, followed by an open parenthesis, is a function name
    // rather than an operator. A clause keyword is exempt, since `MATCH (` is still a clause.
    const next = nextNonSpaceOf(index);
    return !(next && next.other === "(");
  }

  let out = "";
  let atLineStart = true;
  let pendingSpace = false;
  let clause = "";

  const newline = () => {
    if (!atLineStart) out += "\n";
    atLineStart = true;
    pendingSpace = false;
  };

  tokens.forEach((token, i) => {
    if (token.space) {
      pendingSpace = out.length > 0;
      return;
    }

    // A comment runs to the end of its line, so it has to keep one to itself or it would swallow
    // whatever the formatter put after it.
    if (token.comment) {
      newline();
      out += token.text;
      out += "\n";
      atLineStart = true;
      return;
    }

    if (breakAt.has(i)) {
      newline();
      clause = token.clause;
    }

    if (pendingSpace && !atLineStart) out += " ";
    pendingSpace = false;

    if (token.word) {
      out += shouldUppercase(i) ? token.word.toUpperCase() : token.word;
      atLineStart = false;
      return;
    }

    if (token.other === ";" && token.depth === 0) {
      out += ";\n";
      atLineStart = true;
      clause = "";
      return;
    }

    if (token.other === "," && token.depth === 0 && PATTERN_CLAUSES.has(clause)) {
      out += ",\n" + " ".repeat(clause.length + 1);
      atLineStart = true;
      return;
    }

    out += token.text;
    atLineStart = false;
  });

  return out.replace(/[ \t]+$/gm, "").trim();
}

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
    if (snapshotStale) loadSnapshot();
    drawGraph();
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
  if (!db || busy) return;
  const cypher = editor.value.trim();
  if (!cypher) return;

  busy = true;
  $("run").disabled = true;
  setStatus("busy", "Running…");
  // Execution is synchronous inside the module, so this is the only chance the browser gets
  // to paint the disabled button before the thread blocks.
  await new Promise((r) => setTimeout(r, 0));

  try {
    if (mode === "explain") {
      const plan = db.explain(cypher);
      $("pane-plan").innerHTML = `<pre class="plan">${esc(plan)}</pre>`;
      setStatus("ok", "Plan generated.");
      setMeta("Physical plan. The query was not executed.");
      showPane("plan");
      remember(cypher);
      return;
    }

    const started = performance.now();
    const result = JSON.parse(db.query(cypher));
    const wall = performance.now() - started;
    lastResult = result;

    renderTable(result);
    renderJson(result);

    try {
      $("pane-plan").innerHTML = `<pre class="plan">${esc(db.explain(cypher))}</pre>`;
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
      refreshSchema();
      rememberSetup(cypher);
    }
    await refreshGraph(mayWrite);
    renderFooter();

    if (pendingDemo) {
      const demo = pendingDemo;
      pendingDemo = null;
      if (demo.embed) embedLabel(demo.embed);
      if (demo.thenQuery) {
        if (demo.textIndex) db.createTextIndex(demo.textIndex[0], demo.textIndex[1]);
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
    showError(message + procedureHint(cypher, message));
    setStatus("err", "Query failed.");
  } finally {
    busy = false;
    $("run").disabled = false;
  }
}

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

function renderProcedures(filter = "") {
  const needle = filter.trim().toLowerCase();
  const host = $("proc-list");
  const matches = PROCEDURES.filter(
    (proc) =>
      !needle ||
      `${proc.name} ${proc.aka ?? ""} ${proc.args} ${proc.yields} ${proc.summary}`
        .toLowerCase()
        .includes(needle),
  );
  host.replaceChildren();
  if (matches.length === 0) {
    host.innerHTML = '<div class="empty">No procedure matches.</div>';
    return;
  }
  for (const proc of matches) {
    const button = document.createElement("button");
    button.className = "proc";
    // The signature is in the tooltip rather than the row. In a sidebar this narrow a form like
    // `issundb.pageRank([{iterations, damping}])` wraps mid-identifier, which is harder to scan
    // than the name alone, and clicking inserts the call anyway.
    const signature = `${proc.name}(${proc.args})`;
    button.title = proc.aka
      ? `${signature}\n\n${proc.summary}\n\nAlso registered as ${proc.aka}.`
      : `${signature}\n\n${proc.summary}`;
    const name = document.createElement("span");
    name.className = "nm";
    name.textContent = proc.name;
    const yields = document.createElement("span");
    yields.className = "yd";
    yields.textContent = `yields ${proc.yields}`;
    button.append(name, yields);
    button.addEventListener("click", () => setQuery(proc.snippet));
    host.append(button);
  }
}

// Iterative over two rows, so the whole matrix is never held.
function editDistance(a, b) {
  let previous = Array.from({ length: b.length + 1 }, (_, j) => j);
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

// The count comes from the catalog rather than the markup, which carried a stale 13 for as long as
// the catalog had more than that.
$("proc-search").placeholder = `Search ${PROCEDURES.length} procedures…`;
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
    db.createTextIndex(label, property);
    const { hits } = JSON.parse(db.textSearch(demo.textSearch, 10));
    const rows = [];
    for (const hit of hits) {
      const title = JSON.parse(db.query(`MATCH (a) WHERE id(a) = ${hit.node} RETURN a.title`));
      rows.push([hit.node, title.rows[0]?.[0] ?? null, Number(hit.score.toFixed(4)), hit.property]);
    }
    lastResult = { columns: ["node", "title", "bm25", "field"], rows };
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
function embedLabel(spec) {
  const label = spec.label ?? "Person";
  const caption = spec.caption ?? "name";
  const rows = JSON.parse(
    db.query(`MATCH (n:${label}) RETURN id(n) AS id, n.${caption} AS caption ORDER BY id`),
  ).rows;
  rows.forEach(([id], i) => {
    const angle = (i / Math.max(rows.length, 1)) * Math.PI * 2;
    db.upsertVector(BigInt(id), new Float32Array([Math.cos(angle), Math.sin(angle), 0.25]));
  });
  return { label, rows };
}

async function runVectorDemo(spec) {
  try {
    const { label, rows } = embedLabel(spec);
    if (rows.length === 0) {
      showError(`No ${label} nodes to embed. Run the example's own CREATE first.`);
      return;
    }
    const { hits } = JSON.parse(
      db.vectorSearch(new Float32Array([1, 0, 0.25]), Math.min(5, rows.length)),
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
      `${plural(hits.length, "neighbour")}, 4 columns.` +
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
    const result = JSON.parse(db.query(cypher));
    lastResult = result;
    renderTable(result);
    renderJson(result);
    setStatus("ok", "Query finished.");
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
let wasmExports = null;

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

function renderFooter() {
  const note = $("footer-note");
  const counts = lastStats
    ? `${plural(lastStats.nodes, "node")} and ${plural(lastStats.edges, "edge")}`
    : "";

  let live = 0;
  try {
    live = Playground.memoryBytes();
  } catch {
    // A build without the counter still reports the counts.
  }
  const heap = wasmExports?.memory?.buffer?.byteLength ?? 0;
  if (live === 0) {
    note.textContent = counts;
    note.title = "";
    return;
  }

  // Two figures rather than three. Splitting the live total into engine and graph was the first
  // shape this took, and it reported the same number twice: an empty database allocates a few
  // kilobytes, so the graph is very nearly all of it. The baseline is stated in the tooltip
  // instead, where it says that rather than implying a division that does not exist.
  note.textContent = `${counts} · ${bytesLabel(live)} in use, ${bytesLabel(heap)} heap`;
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

function refreshSchema() {
  let stats;
  try {
    stats = JSON.parse(db.stats());
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

let snapshot = { nodes: [], edges: [], truncated: false };
let snapshotStale = true;

function loadSnapshot() {
  let next;
  try {
    next = JSON.parse(db.graphSnapshot());
  } catch {
    next = { nodes: [], edges: [], truncated: false };
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
  if (snapshotStale) loadSnapshot();
  drawGraph();
}

// Velocity-Verlet, with all-pairs repulsion: at the 300-node cap that pass is cheap enough
// that no spatial index is worth the code it would take. The canvas size is passed in rather
// than read off the element, since the caller needs the same two numbers for the view box and
// the two must agree or a fit is computed against a different box than the layout used.
function layout(nodes, edges, width, height) {
  const index = new Map(nodes.map((n, i) => [n.id, i]));
  const links = edges
    .map((e) => [index.get(e.source), index.get(e.target)])
    .filter(([a, b]) => a !== undefined && b !== undefined);

  for (const [i, node] of nodes.entries()) {
    if (node.x === undefined) {
      // Seeded on a circle rather than at random, so a re-layout of the same graph is
      // reproducible and the first frame is never a knot at the centre.
      const angle = (i / Math.max(nodes.length, 1)) * Math.PI * 2;
      const radius = Math.min(width, height) * 0.32;
      node.x = width / 2 + Math.cos(angle) * radius;
      node.y = height / 2 + Math.sin(angle) * radius;
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
    for (const node of nodes) {
      node.vx += (width / 2 - node.x) * 0.006;
      node.vy += (height / 2 - node.y) * 0.006;
      if (node.pinned) {
        node.vx = 0;
        node.vy = 0;
        continue;
      }
      node.vx *= 0.82;
      node.vy *= 0.82;
      node.x += node.vx * alpha;
      node.y += node.vy * alpha;
      const margin = 26;
      node.x = Math.max(margin, Math.min(width - margin, node.x));
      node.y = Math.max(margin, Math.min(height - margin, node.y));
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
  if (!lastResult) return { lit: null, groupIds: null, groupLabel: "" };
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
  $("inspect").hidden = true;

  // A redraw returns the view to the whole canvas, so a fit is discarded by the next query
  // rather than silently framing a graph it was not computed for.
  const width = svg.clientWidth || 800;
  const height = svg.clientHeight || 500;
  setViewBox(svg, { x: 0, y: 0, w: width, h: height });

  const { nodes, edges, truncated } = snapshot;
  $("graph-count").textContent =
    `${plural(nodes.length, "node")} and ${plural(edges.length, "edge")}` +
    (truncated ? " (capped at 300)" : "");

  const { lit, groupIds, groupLabel } = resultOverlay();

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

  $("legend").innerHTML = groupIds
    ? groupOrder
        .map(
          (value) =>
            `<span><i style="background:${groupColor.get(value)}"></i>${esc(groupLabel)} ${value}</span>`,
        )
        .join("")
    : [...new Set(nodes.map(labelOf))]
        .sort()
        .map((l) => `<span><i style="background:${colorOf(l)}"></i>${esc(l)}</span>`)
        .join("");

  if (nodes.length === 0) {
    svg.append(
      el("text", { x: 16, y: 28, fill: "var(--md-default-fg-color--light)", "font-size": "13" }),
    );
    svg.lastChild.textContent = "Nothing to draw. Run a CREATE, or click Reset data.";
    return;
  }

  const linkLayer = el("g");
  const nodeLayer = el("g");
  svg.append(linkLayer, nodeLayer);

  const byId = new Map(nodes.map((n) => [n.id, n]));
  const lines = edges.map((e) => {
    const line = el("line", { "stroke-width": 1.2 });
    line.dataset.source = e.source;
    line.dataset.target = e.target;
    linkLayer.append(line);
    return line;
  });

  const groups = nodes.map((node) => {
    const group = el("g", { class: "node" });
    const circle = el("circle", {
      r: NODE_RADIUS,
      fill: fillOf(node),
    });
    const text = el("text", { "text-anchor": "middle", dy: NODE_RADIUS + 12 });
    text.textContent = captionOf(node);
    group.append(circle, text);
    if (lit && !lit.has(node.id)) group.classList.add("dim");

    group.addEventListener("pointerdown", (e) => {
      e.stopPropagation();
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
      inspect(node);
    });
    nodeLayer.append(group);
    return group;
  });

  function paint() {
    for (const line of lines) {
      const a = byId.get(Number(line.dataset.source));
      const b = byId.get(Number(line.dataset.target));
      if (!a || !b) continue;
      line.setAttribute("x1", a.x);
      line.setAttribute("y1", a.y);
      line.setAttribute("x2", b.x);
      line.setAttribute("y2", b.y);
    }
    for (const [i, group] of groups.entries()) {
      group.setAttribute("transform", `translate(${nodes[i].x} ${nodes[i].y})`);
    }
  }

  const tick = layout(nodes, edges, width, height);
  function start() {
    if (generation !== simGeneration) return;
    if (sim) cancelAnimationFrame(sim);
    if (REDUCED_MOTION.matches) {
      // Settled in one pass and painted once. The bound is where the animated form stops anyway,
      // since alpha starts at 1, decays by 0.985 a step, and the loop ends below 0.02.
      for (let i = 0; i < 260 && tick(); i += 1);
      paint();
      return;
    }
    const step = () => {
      const running = tick();
      paint();
      sim = running ? requestAnimationFrame(step) : null;
    };
    sim = requestAnimationFrame(step);
  }
  paint();
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
}

function toSvg(svg, event) {
  const rect = svg.getBoundingClientRect();
  const box = viewBox ?? { x: 0, y: 0, w: rect.width, h: rect.height };
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
  $("inspect").innerHTML =
    `<h5><i class="swatch" style="width:9px;height:9px;border-radius:3px;background:${colorOf(
      labelOf(node),
    )}"></i>${esc(node.labels?.join(":") || "(no label)")} <span style="color:var(--md-default-fg-color--light);font-family:var(--md-code-font)">#${node.id}</span></h5>` +
    (rows ? `<dl>${rows}</dl>` : '<div class="empty">No properties.</div>');
  $("inspect").hidden = false;
}

// Zoom and pan both move the view box, which is also what Fit sets and what `toSvg` maps a pointer
// through, so those three cannot disagree about where the graph is.
const ZOOM_STEP = 1.25;

const canvasSize = (svg) => ({ w: svg.clientWidth || 800, h: svg.clientHeight || 500 });

// `focal` is the world point to hold still, so a wheel zoom keeps whatever is under the pointer
// under the pointer. Without it, zooming in on a corner walks the graph off the canvas.
function zoomBy(factor, focal) {
  const svg = $("svg");
  if (!viewBox) return;
  const { w: canvasW } = canvasSize(svg);
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
  { passive: false },
);

$("svg").addEventListener("pointerdown", (e) => {
  $("inspect").hidden = true;
  // A vertex has its own drag handler and stops propagation; this is the guard for anything that
  // does not, so a pan cannot start on top of a node.
  if (e.target.closest(".node")) return;

  const svg = $("svg");
  const from = viewBox ? { ...viewBox } : null;
  if (!from) return;
  capturePointer(svg, e.pointerId);

  const move = (ev) => {
    // Measured against the box the drag started from rather than the current one, or the pan chases
    // itself: each move would be applied to a box the previous move had already shifted.
    const rect = svg.getBoundingClientRect();
    const dx = ((ev.clientX - e.clientX) * from.w) / rect.width;
    const dy = ((ev.clientY - e.clientY) * from.h) / rect.height;
    setViewBox(svg, { x: from.x - dx, y: from.y - dy, w: from.w, h: from.h });
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
$("fit").addEventListener("click", () => {
  const svg = $("svg");
  const placed = snapshot.nodes.filter((node) => node.x !== undefined);
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
  const url = URL.createObjectURL(new Blob([text], { type: mime }));
  const anchor = document.createElement("a");
  anchor.href = url;
  anchor.download = name;
  anchor.click();
  // Firefox and Safari fetch the object URL asynchronously after the click, so revoking on
  // this tick produces an empty download.
  setTimeout(() => URL.revokeObjectURL(url), 0);
}

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
        ? { d: SUN, stroke: "currentColor", "stroke-width": "2", fill: "none" }
        : { d: MOON },
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

// The build stamp comes out of the module rather than a sidecar file the page would have to fetch,
// so it is empty for a build made outside a git checkout and the footer then names the version
// alone.
function renderPoweredBy() {
  const build = Playground.buildRef();
  const engine = build
    ? `IssunDB (${Playground.version()}; ${build})`
    : `IssunDB (${Playground.version()})`;
  $("powered").textContent =
    `This playground app is powered by ${engine}; everything` +
    " (including the queries) runs safely in your browser.";
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

function seed() {
  db.query(currentSample().cypher);
  refreshSchema();
}

$("reset").addEventListener("click", async () => {
  // Freed rather than abandoned. wasm-bindgen registers a finalizer, so an abandoned instance
  // is reclaimed eventually, but until then its whole graph is still resident and wasm memory
  // never shrinks. The new instance is built first, so a failure leaves the old one usable.
  const previous = db;
  db = new Playground();
  previous?.free();
  // After the old instance is freed, so the baseline is one empty database rather than two.
  baselineBytes = Playground.memoryBytes();
  lastResult = null;
  // The discarded writes must not keep travelling in a share link, where replaying them against
  // the fresh seed would rebuild the state Reset was clicked to get rid of.
  setupLog.length = 0;
  // Node ids restart from zero, so a carried-over position would belong to a different node.
  snapshot = { nodes: [], edges: [], truncated: false };
  seed();
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

  wasmExports = await init();
  db = new Playground();
  baselineBytes = Playground.memoryBytes();

  renderPoweredBy();
  renderSamples();
  renderDemos();
  renderProcedures();
  renderHistory();
  seed();

  // Three link forms. `q` is the Share button's base64 query and `s` its optional setup script,
  // and `cypher` is percent-encoded plain text so a link can be written by hand or generated by a
  // docs build. A generator has to encode a plus as %2B, since a fragment read as a query string
  // turns a literal one into a space.
  const params = new URLSearchParams(location.hash.slice(1));

  const setup = params.get("s");
  if (setup) {
    try {
      db.query(b64url.decode(setup));
      refreshSchema();
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
    // No caption: the ribbon is for what an example is demonstrating, and the banner below the
    // editor already says the query was restored and not run. Three notices for one fact was two
    // too many.
    setQuery(stored);
  } else {
    setQuery(
      "MATCH (a:Person)-[:KNOWS]->(b:Person)\nRETURN a.name AS from, b.name AS to\nORDER BY from, to",
      "A starting query over the seeded sample graph. Press ⌘↵ (or Ctrl↵) to run it.",
    );
  }

  await refreshGraph();
  $("boot").remove();

  // A restored query is deliberately not run. It could be a CREATE, and running it on every
  // reload would quietly add another copy of its data.
  if (stored) {
    showPane("table");
    setStatus("", "Your last query was restored. It has not been run.");
    setMeta("Run a query to view results.");
    $("pane-table").innerHTML =
      '<div class="notice info">Your last query is in the editor. Press ⌘↵ (or Ctrl↵) to run it.</div>';
  } else {
    await run();
  }
}

boot().catch((e) => {
  $("boot").innerHTML =
    `<div style="max-width:34rem;text-align:left;font-family:var(--md-code-font);font-size:12.5px">` +
    `<strong>The engine did not load.</strong><br><br>${esc(String(e))}<br><br>` +
    `The module is served as <code>web/pkg/</code>; build it with <code>make playground-build</code> ` +
    `and serve the directory over HTTP, since a module cannot be loaded from a file:// path.</div>`;
});
