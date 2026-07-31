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
const { DEMO_CATEGORIES, FUNCTIONS, PROCEDURES, SAMPLE_GRAPHS, SAMPLE_SOCIAL } = await import(
  join(here, "..", "web", "demos.js")
);

// The graph each category's examples query, since none of them builds its own data any more.
const sampleById = new Map(SAMPLE_GRAPHS.map((sample) => [sample.id, sample.cypher]));

let failures = 0;
let checked = 0;

console.log(`IssunDB ${Playground.version()} (persistent: ${Playground.isPersistent()})\n`);

// The Setup panel's sample graphs. Each is a `CREATE` in a JavaScript file, so nothing else can see
// it; a typo would surface as an error the first time a visitor pressed Reset Database. Each is run
// on its own instance and has to produce nodes, which is what catches a script that parses but
// builds nothing.
console.log("Sample graphs");

let sampleFailures = 0;

for (const sample of SAMPLE_GRAPHS) {
  const p = new Playground();
  try {
    p.query(sample.cypher);
    const stats = JSON.parse(p.stats());
    if (stats.nodes === 0) {
      throw new Error("the script ran but created no nodes");
    }
    const labels = Object.keys(stats.label_counts ?? {}).sort().join(", ");
    console.log(
      `  ok    ${sample.id.padEnd(12)} ${stats.nodes} nodes, ${stats.edges} relationships (${labels})`,
    );
  } catch (e) {
    sampleFailures += 1;
    console.log(`  FAIL  ${sample.id.padEnd(12)} ${String(e.message ?? e).split("\n")[0]}`);
  }
}
console.log();

for (const category of DEMO_CATEGORIES) {
  console.log(category.label);
  for (const demo of category.demos) {
    checked += 1;
    // A fresh instance per demo, so one demo's writes cannot make another pass or fail.
    const p = new Playground();
    try {
      p.query(sampleById.get(category.sample) ?? SAMPLE_SOCIAL);

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

      // The same order the page uses: embeddings and the index go in first, then whichever
      // display step the example declared. A node id is a u64, which wasm-bindgen takes as a
      // BigInt.
      const embedSpec = demo.embed ?? (demo.vectors && demo.vectors !== true ? demo.vectors : null);
      if (embedSpec || demo.vectors) {
        const label = embedSpec?.label ?? "Person";
        const ids = JSON.parse(p.query(`MATCH (n:${label}) RETURN id(n) ORDER BY id(n)`))
          .rows.map((r) => r[0]);
        if (ids.length === 0) {
          throw new Error(`no ${label} nodes to embed`);
        }
        ids.forEach((id, i) =>
          p.upsertVector(BigInt(id), new Float32Array([Math.cos(i), Math.sin(i), 0.5])),
        );
        detail += `, embedded ${ids.length} ${label}`;
      }
      if (demo.textIndex) {
        p.createTextIndex(demo.textIndex[0], demo.textIndex[1]);
      }
      if (demo.textSearch) {
        const hits = JSON.parse(p.textSearch(demo.textSearch, 10)).hits;
        if (hits.length === 0) {
          throw new Error(`no full-text hits for ${JSON.stringify(demo.textSearch)}`);
        }
        detail += `, ${hits.length} text hit(s)`;
      }
      if (demo.vectors) {
        const hits = JSON.parse(p.vectorSearch(new Float32Array([1, 0, 0.5]), 3)).hits;
        if (hits.length === 0) {
          throw new Error("no vector hits");
        }
        detail += `, ${hits.length} vector hit(s)`;
      }
      if (demo.thenQuery) {
        const after = JSON.parse(p.query(demo.thenQuery));
        if (after.rows.length === 0) {
          throw new Error("the follow-up query returned no rows");
        }
        detail += `, follow-up ${after.rows.length} row(s)`;
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

// The function catalog, checked exactly as the procedures are. A function is called in an
// expression rather than through CALL, so its snippet is an ordinary query, and the same loop
// works: what matters is that the name resolves and the snippet runs. An `UnknownFunction` is the
// drift this catches, the way `ProcedureNotFound` is above.
console.log("\nFunction reference");

let fnChecked = 0;
let fnFailures = 0;

for (const fn of FUNCTIONS) {
  fnChecked += 1;
  const p = new Playground();
  try {
    p.query(SAMPLE_SOCIAL);
    const result = JSON.parse(p.query(fn.snippet));
    if (result.rows.length === 0) {
      fnFailures += 1;
      console.log(`  FAIL  ${fn.name.padEnd(38)} returned no rows`);
    } else {
      console.log(`  ok    ${fn.name.padEnd(38)} ${result.rows.length} row(s)`);
    }
  } catch (e) {
    fnFailures += 1;
    console.log(`  FAIL  ${fn.name.padEnd(38)} ${String(e.message ?? e).split("\n")[0]}`);
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
  `${fnChecked - fnFailures}/${fnChecked} functions ok` + (fnFailures ? `, ${fnFailures} failed` : ""),
);
console.log(
  `${docChecked - docFailures}/${docChecked} marked doc blocks ok` +
    (docFailures ? `, ${docFailures} failed` : ""),
);
console.log(
  `${SAMPLE_GRAPHS.length - sampleFailures}/${SAMPLE_GRAPHS.length} sample graphs ok` +
    (sampleFailures ? `, ${sampleFailures} failed` : ""),
);
process.exit(failures + procFailures + fnFailures + docFailures + sampleFailures ? 1 : 0);
