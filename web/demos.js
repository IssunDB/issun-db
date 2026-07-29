// The demo catalog.
//
// Nothing in the Rust suite can see these queries, so `make playground-check` runs the whole
// catalog through the compiled module and fails on an error. The first draft of this file had
// the wrong yield names and the wrong argument form for every procedure.

export const SAMPLE_SOCIAL = `// A small social graph. Several demos below query it.
CREATE (ada:Person {name: 'Ada', city: 'London', age: 36}),
       (grace:Person {name: 'Grace', city: 'New York', age: 45}),
       (alan:Person {name: 'Alan', city: 'London', age: 41}),
       (edsger:Person {name: 'Edsger', city: 'Amsterdam', age: 52}),
       (barbara:Person {name: 'Barbara', city: 'New York', age: 38}),
       (donald:Person {name: 'Donald', city: 'Chicago', age: 60}),
       (ada)-[:KNOWS {since: 1936, weight: 4}]->(alan),
       (alan)-[:KNOWS {since: 1938, weight: 1}]->(grace),
       (grace)-[:KNOWS {since: 1952, weight: 2}]->(barbara),
       (barbara)-[:KNOWS {since: 1960, weight: 3}]->(donald),
       (donald)-[:KNOWS {since: 1968, weight: 1}]->(edsger),
       (edsger)-[:KNOWS {since: 1972, weight: 5}]->(ada),
       (ada)-[:KNOWS {since: 1940, weight: 9}]->(grace),
       (alan)-[:KNOWS {since: 1945, weight: 2}]->(barbara),
       (grace)-[:KNOWS {since: 1955, weight: 3}]->(ada)`;

const PATH_NOTE = `// Procedure arguments are resolved before planning, so they are literal ids
// rather than expressions. 0 is Ada and 5 is Donald in the seeded sample.`;

export const DEMO_CATEGORIES = [
  {
    label: "Cypher basics",
    blurb: "Create, match, filter, aggregate, and mutate. The query layer is openCypher.",
    demos: [
      {
        label: "Create a graph",
        desc: "Writes nodes and relationships in one statement. Every clause of a write statement shares one transaction, so an error anywhere rolls back all of it.",
        cypher: SAMPLE_SOCIAL,
      },
      {
        label: "Match and filter",
        desc: "Pattern matching with a WHERE predicate. The optimizer splits a top-level AND so each conjunct pushes down to its own lowest binder.",
        cypher: `MATCH (p:Person)
WHERE p.city = 'London' AND p.age > 30
RETURN p.name AS name, p.age AS age
ORDER BY age DESC`,
      },
      {
        label: "Traverse",
        desc: "Follows a relationship. A typed hop is resolved as a bulk read of the in-memory CSR adjacency rather than a lookup per row.",
        cypher: `MATCH (a:Person)-[:KNOWS]->(b:Person)
RETURN a.name AS from, b.name AS to
ORDER BY from, to`,
      },
      {
        label: "Variable length",
        desc: "A path of one to three hops. The relationship variable binds to the whole list of relationships traversed, so size(r) is the hop count.",
        cypher: `MATCH (a:Person {name: 'Ada'})-[r:KNOWS*1..3]->(b:Person)
RETURN b.name AS reached, size(r) AS hops
ORDER BY hops, reached`,
      },
      {
        label: "Aggregate",
        desc: "Groups and counts. A count grouped by one endpoint of a single hop lowers to a kernel that emits one entry per group instead of a row per edge.",
        cypher: `MATCH (p:Person)-[:KNOWS]->(other)
RETURN p.city AS city, count(other) AS outgoing
ORDER BY outgoing DESC, city`,
      },
      {
        label: "Update and delete",
        desc: "SET assigns a property and a label; a statement's own projection sees its uncommitted writes through a pending-writes overlay.",
        cypher: `MATCH (p:Person {name: 'Donald'})
SET p.city = 'Palo Alto', p:Retired
RETURN p.name AS name, p.city AS city, labels(p) AS labels`,
      },
    ],
  },
  {
    label: "Graph algorithms",
    blurb: "Called as procedures, computed over the CSR snapshot in pure Rust.",
    demos: [
      {
        label: "PageRank",
        desc: "Ranks nodes by importance, and sizes the vertices in the graph view. A source spreads its rank across its edges, so parallel edges each carry mass.",
        cypher: `CALL issundb.pageRank({iterations: 20, damping: 0.85})
YIELD nodeId, score
MATCH (p) WHERE id(p) = nodeId
RETURN p.name AS name, round(score * 10000) / 10000 AS pagerank
ORDER BY pagerank DESC, name`,
      },
      {
        label: "Betweenness",
        desc: "How often each node lies on a shortest path between two others, by Brandes' algorithm. It counts distinct pairs, so two parallel edges are one path.",
        cypher: `CALL issundb.betweenness()
YIELD nodeId, score
MATCH (p) WHERE id(p) = nodeId
RETURN p.name AS name, round(score * 100) / 100 AS betweenness
ORDER BY betweenness DESC, name`,
      },
      {
        label: "Degree and harmonic",
        desc: "Two more centralities. Degree counts distinct neighbors in the chosen direction; harmonic sums the reciprocal of each shortest-path distance.",
        cypher: `CALL issundb.degree({direction: 'OUT'})
YIELD nodeId, score
MATCH (p) WHERE id(p) = nodeId
RETURN p.name AS name, score AS out_degree
ORDER BY out_degree DESC, name`,
      },
      {
        label: "Components",
        desc: "Weakly connected components by union-find, treating every edge as undirected.",
        cypher: `CALL issundb.wcc()
YIELD nodeId, componentId
MATCH (p) WHERE id(p) = nodeId
RETURN componentId, count(p) AS size, collect(p.name) AS members
ORDER BY componentId`,
      },
      {
        label: "Strongly connected",
        desc: "Tarjan's algorithm, written iteratively rather than recursively so graph depth cannot reach the call stack. That matters here: the browser stack is small.",
        cypher: `CALL issundb.scc()
YIELD nodeId, componentId
MATCH (p) WHERE id(p) = nodeId
RETURN componentId, count(p) AS size, collect(p.name) AS members
ORDER BY size DESC, componentId`,
      },
      {
        label: "Shortest path",
        desc: "Breadth-first shortest path by hop count, traced back through the incoming adjacency. Yields one row per node along the path, in order.",
        cypher: `${PATH_NOTE}
CALL issundb.shortestPath(0, 5)
YIELD index, nodeId
MATCH (p) WHERE id(p) = nodeId
RETURN index, p.name AS name
ORDER BY index`,
      },
      {
        label: "Dijkstra",
        desc: "Least-weight path from a binary heap. The weight is the first present of the weight, cost, capacity, or cap property, defaulting to 1, and totalWeight repeats on every row.",
        cypher: `${PATH_NOTE}
CALL issundb.dijkstra(0, 5)
YIELD index, nodeId, totalWeight
MATCH (p) WHERE id(p) = nodeId
RETURN index, p.name AS name, totalWeight
ORDER BY index`,
      },
      {
        label: "Triangles",
        desc: "Counts assignments of the directed pattern (a)->(b)->(c)->(a), so one cycle of three distinct nodes counts three times, once per rotation, as a Cypher MATCH would return it. This lowers to a counting kernel that walks the adjacency arrays and tallies integers rather than materializing a row per match.",
        cypher: `CALL issundb.triangleCount()
YIELD count
RETURN count AS triangle_rows`,
      },
      {
        label: "Communities",
        desc: "Label propagation, then each community's members ranked by PageRank. Ties break toward the smallest label, so the partition is stable run to run.",
        cypher: `CALL issundb.communities({topPerCommunity: 3})
YIELD communityId, nodeId, rank
MATCH (p) WHERE id(p) = nodeId
RETURN communityId, rank, p.name AS name
ORDER BY communityId, rank`,
      },
    ],
  },
  {
    label: "Query planning",
    blurb: "What the optimizer did with the query, and why. These show the plan, not rows.",
    demos: [
      {
        label: "An index seek",
        desc: "A property equality over a labeled scan becomes an index seek, because every scalar node property is auto-indexed. Compare the plan with the range form below.",
        cypher: `MATCH (p:Person) WHERE p.name = 'Ada' RETURN p.name, p.city`,
        explain: true,
      },
      {
        label: "A range scan",
        desc: "An inequality lowers to a range scan over the same index, bounded rather than seeked.",
        cypher: `MATCH (p:Person) WHERE p.age > 40 RETURN p.name, p.age`,
        explain: true,
      },
      {
        label: "A join linearized",
        desc: "Two patterns sharing a variable. A join whose one side merely re-scans a variable the other already binds is rewritten into a linear expand-into chain.",
        cypher: `MATCH (a:Person)-[:KNOWS]->(b:Person)
MATCH (a)-[:KNOWS]->(c:Person)
RETURN a.name, b.name, c.name`,
        explain: true,
      },
      {
        label: "An aggregate lowered",
        desc: "A grouped count over one hop lowers to the GroupedDegree kernel, which emits one entry per group rather than expanding every edge into a row.",
        cypher: `MATCH (p:Person)-[:KNOWS]->(o) RETURN p.name, count(o)`,
        explain: true,
      },
    ],
  },
  {
    label: "Full-text search",
    blurb: "BM25 over an inverted index, maintained inside the same transaction as the node.",
    demos: [
      {
        label: "Index and search",
        desc: "Creates four documents, provisions a full-text index over Article.body, then ranks them for a query. The index is transactional rather than eventually consistent, so a hit is never stale.",
        cypher: `CREATE (:Article {title: 'Graph databases', body: 'A graph database stores nodes and relationships instead of tables and joins.'}),
       (:Article {title: 'Vector search', body: 'Approximate nearest neighbour search finds similar embeddings quickly.'}),
       (:Article {title: 'Query planning', body: 'A planner chooses a join order using cardinality statistics gathered from the graph.'}),
       (:Article {title: 'Transactions', body: 'ACID transactions keep the graph and its indexes consistent under concurrent writes.'})`,
        textIndex: ["Article", "body"],
        textSearch: "graph relationships",
      },
    ],
  },
  {
    label: "Vector search",
    blurb: "An embedding per node, searched by exact distance in this build.",
    demos: [
      {
        label: "Nearest neighbours",
        desc: "Attaches a three-dimensional embedding to each person, then finds the closest to a query vector. This build uses the exact backend, so these are the true nearest neighbours rather than approximate ones.",
        cypher: `MATCH (p:Person) RETURN id(p) AS id, p.name AS name ORDER BY id`,
        vectors: true,
      },
    ],
  },
];
