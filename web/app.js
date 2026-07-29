// The IssunDB playground.
//
// Vanilla ES modules, no build step and no network dependency: the page loads the
// wasm-bindgen glue, the module, this file, and one stylesheet. Everything else,
// including the Cypher highlighter and the force-directed layout, is here, which keeps
// the whole page auditable and lets it be served from any static host.
//
// There is exactly one `Playground` for the tab's lifetime, so data accumulates across
// queries the way it would in an embedded database. "Reset data" replaces it.

import init, { Playground } from "./pkg/issundb_wasm.js";
import { DEMO_CATEGORIES, SAMPLE_SOCIAL } from "./demos.js";

const $ = (id) => document.getElementById(id);

// ---------------------------------------------------------------------------
// Cypher highlighting
// ---------------------------------------------------------------------------

// Written here rather than vendored because a highlighter for one language is smaller
// than the library that would supply it, and the page deliberately loads nothing it does
// not contain.
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
    "(\\/\\/[^\\n]*)", // 1 line comment
    "(\\/\\*[\\s\\S]*?\\*\\/)", // 2 block comment
    "('(?:[^'\\\\]|\\\\.)*'|\"(?:[^\"\\\\]|\\\\.)*\")", // 3 string
    "(\\$[A-Za-z_]\\w*)", // 4 parameter
    "(:[A-Za-z_]\\w*)", // 5 label or relationship type
    "(\\b\\d+\\.?\\d*(?:[eE][-+]?\\d+)?\\b)", // 6 number
    "([A-Za-z_]\\w*)(?=\\s*\\()", // 7 function call
    "([A-Za-z_][\\w.]*)", // 8 word
    "([-=<>|*+\\/%!,.;{}\\[\\]()]+)", // 9 operator or punctuation
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
let lastResult = null; // {columns, rows} of the most recent successful query
let sim = null; // the running force layout, if any
let rankSizes = null; // node id -> PageRank score, when sizing by rank

const HISTORY_KEY = "issundb.history";
const THEME_KEY = "issundb.theme";

// ---------------------------------------------------------------------------
// Editor
// ---------------------------------------------------------------------------

const editor = $("editor");
const highlightEl = $("highlight");

function syncHighlight() {
  // The trailing newline keeps the backdrop's last line from collapsing, which would
  // otherwise let the two panes disagree by one line height at the bottom.
  highlightEl.innerHTML = highlight(editor.value) + "\n";
  highlightEl.parentElement.scrollTop = editor.scrollTop;
  highlightEl.parentElement.scrollLeft = editor.scrollLeft;
}

function setQuery(text, description = "") {
  editor.value = text;
  $("desc").textContent = description;
  syncHighlight();
  editor.focus();
}

editor.addEventListener("input", syncHighlight);
editor.addEventListener("scroll", syncHighlight);

editor.addEventListener("keydown", (e) => {
  if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
    e.preventDefault();
    run();
    return;
  }
  // A query is indented text, so Tab has to insert rather than leave the field.
  if (e.key === "Tab") {
    e.preventDefault();
    const { selectionStart: a, selectionEnd: b, value } = editor;
    editor.value = value.slice(0, a) + "  " + value.slice(b);
    editor.selectionStart = editor.selectionEnd = a + 2;
    syncHighlight();
  }
});

// ---------------------------------------------------------------------------
// Status and results
// ---------------------------------------------------------------------------

function setStatus(html) {
  $("status").innerHTML = html;
}

function showPane(name) {
  for (const tab of document.querySelectorAll(".tab")) {
    tab.setAttribute("aria-selected", String(tab.dataset.pane === name));
  }
  for (const pane of document.querySelectorAll(".pane")) {
    pane.classList.toggle("on", pane.id === `pane-${name}`);
  }
  if (name === "graph") {
    drawGraph();
  }
}

for (const tab of document.querySelectorAll(".tab")) {
  tab.addEventListener("click", () => showPane(tab.dataset.pane));
}

function cell(value) {
  if (value === null || value === undefined) return '<span class="null">null</span>';
  if (typeof value === "string") return `<span class="s">${esc(value)}</span>`;
  if (typeof value === "number") return `<span class="n">${value}</span>`;
  if (typeof value === "boolean") return `<span class="b">${value}</span>`;
  return `<span>${esc(JSON.stringify(value))}</span>`;
}

function renderTable(result) {
  const pane = $("pane-table");
  $("tab-rows").textContent = result.rows.length;
  if (result.columns.length === 0) {
    pane.innerHTML =
      '<div class="notice info">The statement returned no columns. Writes report nothing unless the statement ends in RETURN.</div>';
    return;
  }
  if (result.rows.length === 0) {
    pane.innerHTML = `<div class="notice info">No rows. The query is valid and matched nothing.</div>`;
    return;
  }
  const head = result.columns.map((c) => `<th>${esc(c)}</th>`).join("");
  const body = result.rows
    .map(
      (row, i) =>
        `<tr><td class="rownum">${i + 1}</td>${row.map((v) => `<td>${cell(v)}</td>`).join("")}</tr>`,
    )
    .join("");
  pane.innerHTML = `<table><thead><tr><th></th>${head}</tr></thead><tbody>${body}</tbody></table>`;
}

function showError(message) {
  $("pane-table").innerHTML = `<div class="notice err">${esc(message)}</div>`;
  $("tab-rows").textContent = "0";
  showPane("table");
}

// ---------------------------------------------------------------------------
// Running
// ---------------------------------------------------------------------------

async function run(mode = "run") {
  if (!db) return;
  const cypher = editor.value.trim();
  if (!cypher) return;

  $("run").disabled = true;
  setStatus("running…");
  // Yield once so the browser paints the disabled button before the engine blocks the
  // thread. Execution is synchronous inside the module, so this is the only chance to.
  await new Promise((r) => setTimeout(r, 0));

  try {
    if (mode === "explain") {
      const plan = db.explain(cypher);
      $("pane-plan").innerHTML = `<pre class="plan">${esc(plan)}</pre>`;
      setStatus(`<span class="t">plan</span>`);
      showPane("plan");
      remember(cypher);
      return;
    }

    const started = performance.now();
    const result = JSON.parse(db.query(cypher));
    const wall = performance.now() - started;
    lastResult = result;

    renderTable(result);
    $("pane-json").innerHTML = `<pre class="json">${esc(JSON.stringify(result.rows, null, 2))}</pre>`;

    // The plan pane is kept in step with the query, so switching to it never shows the
    // plan of something else. A statement EXPLAIN cannot describe is simply left blank.
    try {
      $("pane-plan").innerHTML = `<pre class="plan">${esc(db.explain(cypher))}</pre>`;
    } catch {
      $("pane-plan").innerHTML =
        '<div class="notice info">No plan: this statement is not a query.</div>';
    }

    const multi =
      result.statement_count > 1
        ? ` <span>${result.statement_count} statements, showing the last</span>`
        : "";
    setStatus(
      `<span class="t">${result.rows.length} row${result.rows.length === 1 ? "" : "s"}</span>` +
        ` <span>${result.elapsed_ms.toFixed(2)} ms engine</span>` +
        ` <span>${wall.toFixed(1)} ms total</span>${multi}`,
    );
    showPane("table");
    remember(cypher);
    refreshSchema();
    await refreshGraph();
  } catch (e) {
    showError(String(e.message ?? e));
    setStatus(`<span style="color:var(--err)">error</span>`);
  } finally {
    $("run").disabled = false;
  }
}

$("run").addEventListener("click", () => run());
$("explain").addEventListener("click", () => run("explain"));
$("load-sample").addEventListener("click", () =>
  setQuery(SAMPLE_SOCIAL, "The sample social graph. Running it again adds a second copy."),
);

// ---------------------------------------------------------------------------
// History
// ---------------------------------------------------------------------------

function readHistory() {
  try {
    return JSON.parse(localStorage.getItem(HISTORY_KEY) ?? "[]");
  } catch {
    return [];
  }
}

function remember(cypher) {
  const history = readHistory().filter((q) => q !== cypher);
  history.unshift(cypher);
  try {
    localStorage.setItem(HISTORY_KEY, JSON.stringify(history.slice(0, 25)));
  } catch {
    // A browser refusing storage is not a reason to fail a query.
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
// Demos
// ---------------------------------------------------------------------------

function renderDemos() {
  const host = $("demos");
  for (const category of DEMO_CATEGORIES) {
    const wrap = document.createElement("div");
    wrap.className = "cat";
    wrap.innerHTML = `<h4>${esc(category.label)}</h4><p>${esc(category.blurb)}</p>`;
    for (const demo of category.demos) {
      const button = document.createElement("button");
      button.className = "demo";
      button.textContent = demo.label;
      button.addEventListener("click", async () => {
        for (const other of document.querySelectorAll(".demo")) {
          other.classList.remove("active");
        }
        button.classList.add("active");
        setQuery(demo.cypher, demo.desc);
        await run(demo.explain ? "explain" : "run");
        // The two capabilities Cypher cannot express are driven here, after the demo's
        // own statement, so a single click still shows the whole feature.
        if (demo.textIndex) await runTextDemo(demo);
        if (demo.vectors) await runVectorDemo();
      });
      wrap.append(button);
    }
    host.append(wrap);
  }
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
    setStatus(
      `<span class="t">${rows.length} hit${rows.length === 1 ? "" : "s"}</span>` +
        ` <span>full-text index on ${label}.${property}</span>` +
        ` <span>query "${esc(demo.textSearch)}"</span>`,
    );
    showPane("table");
  } catch (e) {
    showError(String(e.message ?? e));
  }
}

async function runVectorDemo() {
  try {
    const people = JSON.parse(
      db.query("MATCH (p:Person) RETURN id(p) AS id, p.name AS name ORDER BY id"),
    ).rows;
    if (people.length === 0) {
      showError("No Person nodes to embed. Run the sample graph first.");
      return;
    }
    // A readable stand-in for a real embedding: each person is placed on a circle, so
    // "nearest" has an obvious meaning the table can be checked against by eye.
    people.forEach(([id], i) => {
      const angle = (i / people.length) * Math.PI * 2;
      db.upsertVector(id, new Float32Array([Math.cos(angle), Math.sin(angle), 0.25]));
    });
    const query = new Float32Array([1, 0, 0.25]);
    const { hits } = JSON.parse(db.vectorSearch(query, Math.min(5, people.length)));
    const names = new Map(people.map(([id, name]) => [id, name]));
    lastResult = {
      columns: ["rank", "node", "name", "distance"],
      rows: hits.map((h, i) => [i + 1, h.node, names.get(h.node) ?? null, Number(h.distance.toFixed(5))]),
    };
    renderTable(lastResult);
    setStatus(
      `<span class="t">${hits.length} neighbour${hits.length === 1 ? "" : "s"}</span>` +
        ` <span>${people.length} embeddings, exact search</span>` +
        ` <span>query [1, 0, 0.25]</span>`,
    );
    showPane("table");
  } catch (e) {
    showError(String(e.message ?? e));
  }
}

// ---------------------------------------------------------------------------
// Schema panel
// ---------------------------------------------------------------------------

// A stable color per label, so a vertex keeps its color across redraws and matches the
// legend and the schema list. Hashing the name is what makes it stable without a table.
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
      `<div class="schema-row"><span style="color:var(--ink-faint)">→</span>` +
        `<span>:${esc(type)}</span><span class="n">${n}</span></div>`,
    );
  }
  $("schema").innerHTML = rows.length
    ? rows.join("")
    : '<div class="empty">Empty database. Run a CREATE.</div>';
  $("footer-note").textContent = `${stats.nodes} nodes, ${stats.edges} relationships`;
}

// ---------------------------------------------------------------------------
// Graph view
// ---------------------------------------------------------------------------

let snapshot = { nodes: [], edges: [], truncated: false };

async function refreshGraph() {
  try {
    snapshot = JSON.parse(db.graphSnapshot());
  } catch {
    snapshot = { nodes: [], edges: [], truncated: false };
  }
  if (rankSizes) await computeRanks();
  if ($("pane-graph").classList.contains("on")) drawGraph();
}

async function computeRanks() {
  try {
    const result = JSON.parse(
      db.query("CALL issundb.pageRank() YIELD nodeId, score RETURN nodeId, score"),
    );
    rankSizes = new Map(result.rows.map(([id, score]) => [id, score]));
  } catch {
    rankSizes = null;
  }
}

// A small velocity-Verlet layout: repulsion between every pair, springs along the
// relationships, and a pull toward the centre. At the 300-node cap the all-pairs pass is
// cheap enough that no spatial index is worth the code it would take.
function layout(nodes, edges, svg) {
  const width = svg.clientWidth || 800;
  const height = svg.clientHeight || 500;
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
          // Two nodes exactly on top of each other have no direction to separate along,
          // so nudge them deterministically by index instead of randomly.
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

// Node ids the current result names, so a query's answer can be seen in the picture. Only
// these three column names are treated as node references; guessing from the values would
// light up unrelated integers.
function highlighted() {
  if (!lastResult) return null;
  const columns = lastResult.columns
    .map((c, i) => [c.toLowerCase(), i])
    .filter(([c]) => c === "id" || c === "nodeid" || c === "node");
  if (columns.length === 0) return null;
  const ids = new Set();
  for (const row of lastResult.rows) {
    for (const [, i] of columns) {
      if (Number.isInteger(row[i])) ids.add(row[i]);
    }
  }
  return ids.size ? ids : null;
}

function drawGraph() {
  const svg = $("svg");
  if (sim) {
    cancelAnimationFrame(sim);
    sim = null;
  }
  svg.replaceChildren();
  $("inspect").hidden = true;

  const { nodes, edges, truncated } = snapshot;
  $("graph-count").textContent = `${nodes.length} nodes, ${edges.length} relationships${
    truncated ? " (capped at 300)" : ""
  }`;

  const labels = [...new Set(nodes.map(labelOf))].sort();
  $("legend").innerHTML = labels
    .map((l) => `<span><i style="background:${colorOf(l)}"></i>${esc(l)}</span>`)
    .join("");

  if (nodes.length === 0) {
    svg.append(
      el("text", { x: 16, y: 28, fill: "var(--ink-faint)", "font-size": "13" }),
    );
    svg.lastChild.textContent = "Nothing to draw. Run a CREATE, or click Reset data.";
    return;
  }

  const lit = highlighted();
  const maxDegree = Math.max(1, ...nodes.map((n) => n.degree ?? 0));
  const maxRank = rankSizes ? Math.max(1e-9, ...rankSizes.values()) : 1;
  const radiusOf = (node) => {
    if (rankSizes) return 6 + 16 * Math.sqrt((rankSizes.get(node.id) ?? 0) / maxRank);
    return 6 + 10 * Math.sqrt((node.degree ?? 0) / maxDegree);
  };

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
      r: radiusOf(node),
      fill: colorOf(labelOf(node)),
    });
    const text = el("text", { "text-anchor": "middle", dy: radiusOf(node) + 12 });
    text.textContent = captionOf(node);
    group.append(circle, text);
    if (lit && !lit.has(node.id)) group.classList.add("dim");

    group.addEventListener("pointerdown", (e) => {
      e.stopPropagation();
      node.pinned = true;
      group.setPointerCapture(e.pointerId);
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
        start();
      };
      group.addEventListener("pointermove", move);
      group.addEventListener("pointerup", up);
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

  const tick = layout(nodes, edges, svg);
  function start() {
    if (sim) cancelAnimationFrame(sim);
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

function toSvg(svg, event) {
  const rect = svg.getBoundingClientRect();
  return { x: event.clientX - rect.left, y: event.clientY - rect.top };
}

function inspect(node) {
  const props = Object.entries(node.props ?? {});
  const rows = props
    .map(([k, v]) => `<dt>${esc(k)}</dt><dd>${esc(JSON.stringify(v))}</dd>`)
    .join("");
  $("inspect").innerHTML =
    `<h5><i class="swatch" style="width:9px;height:9px;border-radius:3px;background:${colorOf(
      labelOf(node),
    )}"></i>${esc(node.labels?.join(":") || "(no label)")} <span style="color:var(--ink-faint);font-family:var(--mono)">#${node.id}</span></h5>` +
    (rows ? `<dl>${rows}</dl>` : '<div class="empty">No properties.</div>');
  $("inspect").hidden = false;
}

$("svg").addEventListener("pointerdown", () => {
  $("inspect").hidden = true;
});
$("relayout").addEventListener("click", () => {
  for (const node of snapshot.nodes) node.x = undefined;
  drawGraph();
});
$("size-by-rank").addEventListener("change", async (e) => {
  if (e.target.checked) {
    rankSizes = new Map();
    await computeRanks();
  } else {
    rankSizes = null;
  }
  drawGraph();
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
  URL.revokeObjectURL(url);
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

// The query travels in the fragment, so a shared link never reaches a server even if the
// page is hosted on one.
const b64url = {
  encode: (s) =>
    btoa(String.fromCharCode(...new TextEncoder().encode(s)))
      .replace(/\+/g, "-")
      .replace(/\//g, "_")
      .replace(/=+$/, ""),
  decode: (s) =>
    new TextDecoder().decode(
      Uint8Array.from(atob(s.replace(/-/g, "+").replace(/_/g, "/")), (c) => c.charCodeAt(0)),
    ),
};

$("share").addEventListener("click", async () => {
  const url = `${location.origin}${location.pathname}#q=${b64url.encode(editor.value)}`;
  try {
    await navigator.clipboard.writeText(url);
    setStatus('<span class="t">link copied</span>');
  } catch {
    location.hash = `q=${b64url.encode(editor.value)}`;
    setStatus("<span>link is in the address bar</span>");
  }
});

function applyTheme(theme) {
  if (theme) document.documentElement.dataset.theme = theme;
  else delete document.documentElement.dataset.theme;
}

$("theme").addEventListener("click", () => {
  const dark =
    document.documentElement.dataset.theme === "dark" ||
    (!document.documentElement.dataset.theme &&
      matchMedia("(prefers-color-scheme: dark)").matches);
  const next = dark ? "light" : "dark";
  applyTheme(next);
  try {
    localStorage.setItem(THEME_KEY, next);
  } catch {
    // Storage being unavailable only costs the choice its persistence.
  }
  if ($("pane-graph").classList.contains("on")) drawGraph();
});

$("toggle-side").addEventListener("click", () => $("side").classList.toggle("hidden"));

// ---------------------------------------------------------------------------
// Boot
// ---------------------------------------------------------------------------

function seed() {
  db.query(SAMPLE_SOCIAL);
  refreshSchema();
}

$("reset").addEventListener("click", async () => {
  db = new Playground();
  lastResult = null;
  rankSizes = null;
  $("size-by-rank").checked = false;
  seed();
  await refreshGraph();
  setStatus('<span class="t">reset</span> <span>sample graph re-seeded</span>');
  showPane("table");
  $("pane-table").innerHTML =
    '<div class="notice info">Fresh database, seeded with the sample social graph. Pick a demo on the left, or write a query.</div>';
});

async function boot() {
  try {
    applyTheme(localStorage.getItem(THEME_KEY));
  } catch {
    // No stored preference; the system one applies.
  }

  await init();
  db = new Playground();

  $("version").textContent = `v${Playground.version()}`;
  $("build-badge").textContent = Playground.isPersistent()
    ? "persistent build"
    : "in-memory build";
  $("build-badge").title = Playground.isPersistent()
    ? "This build stores data on disk."
    : "This build keeps everything in memory, so a reload starts over. Use the Share button to keep a query.";

  renderDemos();
  renderHistory();
  seed();

  const shared = /#q=(.+)$/.exec(location.hash);
  if (shared) {
    try {
      setQuery(b64url.decode(shared[1]), "Shared query.");
    } catch {
      setQuery("MATCH (p:Person) RETURN p.name AS name, p.city AS city ORDER BY name");
    }
  } else {
    setQuery(
      "MATCH (a:Person)-[:KNOWS]->(b:Person)\nRETURN a.name AS from, b.name AS to\nORDER BY from, to",
      "A starting query over the seeded sample graph. Press ⌘↵ (or Ctrl↵) to run it.",
    );
  }

  await refreshGraph();
  $("boot").remove();
  await run();
}

boot().catch((e) => {
  $("boot").innerHTML =
    `<div style="max-width:34rem;text-align:left;font-family:var(--mono);font-size:12.5px">` +
    `<strong>The engine did not load.</strong><br><br>${esc(String(e))}<br><br>` +
    `The module is served as <code>web/pkg/</code>; build it with <code>make playground-build</code> ` +
    `and serve the directory over HTTP, since a module cannot be loaded from a file:// path.</div>`;
});
