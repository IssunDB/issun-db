// Run every playground demo through the compiled wasm module and fail on any error.
//
// Invoked by `make playground-check`. It needs the `nodejs` target of the module rather than
// the `web` target the page uses, because the latter fetches its own `.wasm` by URL.

import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { readdirSync, readFileSync } from "node:fs";

const here = dirname(fileURLToPath(import.meta.url));
const pkg = process.env.PLAYGROUND_PKG ?? join(here, "..", "target", "playground-pkg-node");

const require = createRequire(import.meta.url);
const { Playground } = require(join(pkg, "issundb_wasm.js"));
const { DEMO_CATEGORIES, PROCEDURES, SAMPLE_SOCIAL } = await import(
  join(here, "..", "web", "demos.js")
);

let failures = 0;
let checked = 0;

console.log(`IssunDB ${Playground.version()} (persistent: ${Playground.isPersistent()})\n`);

for (const category of DEMO_CATEGORIES) {
  console.log(category.label);
  for (const demo of category.demos) {
    checked += 1;
    // A fresh instance per demo, so one demo's writes cannot make another pass or fail.
    const p = new Playground();
    try {
      if (demo.cypher !== SAMPLE_SOCIAL) {
        p.query(SAMPLE_SOCIAL);
      }

      let detail;
      if (demo.explain) {
        const plan = p.explain(demo.cypher);
        if (!plan.trim()) {
          throw new Error("empty plan");
        }
        detail = `plan: ${plan.trim().split("\n")[0].slice(0, 48)}`;
      } else {
        const result = JSON.parse(p.query(demo.cypher));
        detail = `${result.rows.length} row(s), ${result.columns.length} col(s)`;
      }

      if (demo.textIndex) {
        p.createTextIndex(demo.textIndex[0], demo.textIndex[1]);
        const hits = JSON.parse(p.textSearch(demo.textSearch, 10)).hits;
        if (hits.length === 0) {
          throw new Error(`no full-text hits for ${JSON.stringify(demo.textSearch)}`);
        }
        detail += `, ${hits.length} text hit(s)`;
      }
      if (demo.vectors) {
        const ids = JSON.parse(p.query("MATCH (p:Person) RETURN id(p) ORDER BY id(p)"))
          .rows.map((r) => r[0]);
        // A node id is a u64, which wasm-bindgen exposes as a BigInt parameter.
        ids.forEach((id, i) =>
          p.upsertVector(BigInt(id), new Float32Array([Math.cos(i), Math.sin(i), 0.5])),
        );
        const hits = JSON.parse(p.vectorSearch(new Float32Array([1, 0, 0.5]), 3)).hits;
        if (hits.length === 0) {
          throw new Error("no vector hits");
        }
        detail += `, ${hits.length} vector hit(s)`;
      }

      console.log(`  ok    ${demo.label.padEnd(22)} ${detail}`);
    } catch (e) {
      failures += 1;
      console.log(`  FAIL  ${demo.label.padEnd(22)} ${String(e.message).split("\n")[0]}`);
    }
  }
}

// The sidebar's procedure reference. The engine cannot enumerate its procedures, so the catalog is
// written out by hand and this is what keeps it honest: every snippet has to reach a real procedure
// with the yield names it claims. A `requiresVectors` entry cannot run on the sample graph, which
// stores no embeddings, so for those the empty-index failure is accepted and anything else is not.
// `ProcedureNotFound` is never accepted, since that is exactly the drift this exists to catch.
console.log("\nProcedure reference");

let procChecked = 0;
let procFailures = 0;

for (const proc of PROCEDURES) {
  procChecked += 1;
  const p = new Playground();
  try {
    p.query(SAMPLE_SOCIAL);
    const result = JSON.parse(p.query(proc.snippet));
    console.log(`  ok    ${proc.name.padEnd(34)} ${result.rows.length} row(s)`);
  } catch (e) {
    const message = String(e.message ?? e);
    const tolerated = proc.requiresVectors && /vector index is empty/.test(message);
    if (tolerated && !/ProcedureNotFound/.test(message)) {
      console.log(`  ok    ${proc.name.padEnd(34)} resolves, needs embeddings`);
    } else {
      procFailures += 1;
      console.log(`  FAIL  ${proc.name.padEnd(34)} ${message.split("\n")[0]}`);
    }
  }
}

// Cypher blocks in `docs/` marked `<!-- playground -->`, which `docs/hooks/playground_links.py`
// turns into a "Run in the playground" link. The marker is a claim that the block runs against the
// seeded sample graph, and nothing else checks it, so an example edited into a parameter or a
// procedure rename would ship as a link that lands on an error. Each block runs with the earlier
// marked blocks on its own page replayed first, which is the order the generated link produces.
console.log("\nMarked documentation blocks");

const MARKED_BLOCK = /^<!--[ \t]*playground[ \t]*-->\n```cypher\n([\s\S]*?)^```$/gm;
const docsDir = join(here, "..", "docs");

let docChecked = 0;
let docFailures = 0;

for (const file of readdirSync(docsDir).filter((f) => f.endsWith(".md")).sort()) {
  const markdown = readFileSync(join(docsDir, file), "utf8");
  const blocks = [...markdown.matchAll(MARKED_BLOCK)].map((m) => m[1].trim());
  const earlier = [];
  for (const [i, block] of blocks.entries()) {
    docChecked += 1;
    const label = `${file}#${i + 1}`;
    const p = new Playground();
    try {
      p.query(SAMPLE_SOCIAL);
      if (earlier.length > 0) {
        p.query(earlier.join(";\n"));
      }
      const result = JSON.parse(p.query(block));
      // A block that runs but matches nothing is a link to an empty table, which reads as the
      // playground being broken rather than as the example being about something else.
      if (result.rows.length === 0) {
        throw new Error("no rows against the seeded sample graph");
      }
      console.log(`  ok    ${label.padEnd(28)} ${result.rows.length} row(s)`);
    } catch (e) {
      docFailures += 1;
      console.log(`  FAIL  ${label.padEnd(28)} ${String(e.message ?? e).split("\n")[0]}`);
    }
    earlier.push(block);
  }
}

if (docChecked === 0) {
  console.log("  none marked");
}

console.log(
  `\n${checked - failures}/${checked} demos ok` + (failures ? `, ${failures} failed` : ""),
);
console.log(
  `${procChecked - procFailures}/${procChecked} procedures ok` +
    (procFailures ? `, ${procFailures} failed` : ""),
);
console.log(
  `${docChecked - docFailures}/${docChecked} marked doc blocks ok` +
    (docFailures ? `, ${docFailures} failed` : ""),
);
process.exit(failures + procFailures + docFailures ? 1 : 0);
