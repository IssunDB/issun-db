// Run every playground demo through the compiled wasm module and fail on any error.
//
// Invoked by `make playground-check`. It needs the `nodejs` target of the module rather than
// the `web` target the page uses, because the latter fetches its own `.wasm` by URL.

import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const here = dirname(fileURLToPath(import.meta.url));
const pkg = process.env.PLAYGROUND_PKG ?? join(here, "..", "target", "playground-pkg-node");

const require = createRequire(import.meta.url);
const { Playground } = require(join(pkg, "issundb_wasm.js"));
const { DEMO_CATEGORIES, SAMPLE_SOCIAL } = await import(
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
        ids.forEach((id, i) =>
          p.upsertVector(id, new Float32Array([Math.cos(i), Math.sin(i), 0.5])),
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

console.log(
  `\n${checked - failures}/${checked} demos ok` + (failures ? `, ${failures} failed` : ""),
);
process.exit(failures ? 1 : 0);
