// The demo catalog.
//
// Nothing in the Rust suite can see these queries, so `make playground-check` runs the whole
// catalog through the compiled module and fails on an error. The first draft of this file had
// the wrong yield names and the wrong argument form for every procedure.

export const SAMPLE_SOCIAL = `CREATE (ada:Person {name: 'Ada', city: 'London', age: 36}),
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

// The Setup panel's sample graphs. Each is a self-contained `CREATE`, small enough to read in one
// screen and shaped so that one part of the engine has something to say about it. `make
// playground-check` runs every one of them, since a script living in a JavaScript file is invisible
// to every Rust test.
//
// The social graph is first because it is what the page seeds on load and what the Examples panel
// queries; the other four replace it when Reset Database is pressed with one of them selected.
// The corpora the retrieval and knowledge-graph examples build before querying. Each example runs
// its corpus first, so it works from whatever state the page is in rather than depending on a
// sample being loaded. Running one twice adds a second copy, which each description says.
const RAG_CORPUS = `CREATE (d1:Doc {title: 'Graph databases', body: 'A graph database stores nodes and relationships instead of tables and joins.'}),
       (d2:Doc {title: 'Vector search', body: 'Approximate nearest neighbour search finds similar embeddings quickly.'}),
       (d3:Doc {title: 'Hybrid retrieval', body: 'Fusing vector similarity with full-text relevance retrieves better context for a language model.'}),
       (d4:Doc {title: 'Query planning', body: 'A planner chooses a join order using cardinality statistics gathered from the graph.'}),
       (d3)-[:CITES]->(d2),
       (d3)-[:CITES]->(d1),
       (d1)-[:CITES]->(d4)`;

const KG_CORPUS = `CREATE (ada:Researcher {name: 'Ada Ito'}),
       (bo:Researcher {name: 'Bo Chen'}),
       (cai:Researcher {name: 'Cai Rossi'}),
       (lab1:Lab {name: 'Retrieval Group', city: 'Kyoto'}),
       (lab2:Lab {name: 'Systems Group', city: 'Zurich'}),
       (p1:Paper {title: 'Fusing text and vector relevance', year: 2023}),
       (p2:Paper {title: 'Adjacency layouts for traversal', year: 2022}),
       (p3:Paper {title: 'Grounding answers in a graph', year: 2024}),
       (c1:Concept {name: 'retrieval augmented generation'}),
       (c2:Concept {name: 'nearest neighbour search'}),
       (c3:Concept {name: 'query planning'}),
       (ada)-[:WORKS_IN]->(lab1),
       (bo)-[:WORKS_IN]->(lab2),
       (cai)-[:WORKS_IN]->(lab1),
       (ada)-[:AUTHORED]->(p1),
       (cai)-[:AUTHORED]->(p1),
       (bo)-[:AUTHORED]->(p2),
       (ada)-[:AUTHORED]->(p3),
       (p1)-[:MENTIONS]->(c1),
       (p1)-[:MENTIONS]->(c2),
       (p3)-[:MENTIONS]->(c1),
       (p2)-[:MENTIONS]->(c3)`;

export const SAMPLE_GRAPHS = [
  {
    id: "social",
    label: "Social network",
    blurb:
      "People and weighted acquaintances. Backs the Examples panel, and the one the centrality and community procedures are worth running against.",
    cypher: SAMPLE_SOCIAL,
  },
  {
    id: "articles",
    label: "Article corpus",
    blurb:
      "Documents, their topics, and citations between them. The corpus for full-text search and for hybrid retrieval, where text hits are expanded over CITES.",
    cypher: `CREATE (a1:Article {title: 'Graph databases', year: 2019,
         body: 'A graph database stores nodes and relationships instead of tables and joins.'}),
       (a2:Article {title: 'Vector search', year: 2021,
         body: 'Approximate nearest neighbour search finds similar embeddings quickly.'}),
       (a3:Article {title: 'Query planning', year: 2020,
         body: 'A planner chooses a join order using cardinality statistics gathered from the graph.'}),
       (a4:Article {title: 'Transactions', year: 2018,
         body: 'ACID transactions keep the graph and its indexes consistent under concurrent writes.'}),
       (a5:Article {title: 'Hybrid retrieval', year: 2023,
         body: 'Fusing vector similarity with full-text relevance retrieves better context for a language model.'}),
       (t1:Topic {name: 'storage'}),
       (t2:Topic {name: 'search'}),
       (t3:Topic {name: 'optimizer'}),
       (a1)-[:ABOUT]->(t1),
       (a4)-[:ABOUT]->(t1),
       (a2)-[:ABOUT]->(t2),
       (a5)-[:ABOUT]->(t2),
       (a3)-[:ABOUT]->(t3),
       (a1)-[:CITES]->(a4),
       (a3)-[:CITES]->(a1),
       (a5)-[:CITES]->(a2),
       (a5)-[:CITES]->(a3)`,
  },
  {
    id: "org",
    label: "Org chart",
    blurb:
      "A reporting tree. Every path runs one way, so it is the sample for variable-length hops, shortest path, and a range scan over a numeric property.",
    cypher: `CREATE (rin:Employee {name: 'Rin', title: 'CEO', level: 1}),
       (sato:Employee {name: 'Sato', title: 'CTO', level: 2}),
       (mori:Employee {name: 'Mori', title: 'CFO', level: 2}),
       (kaito:Employee {name: 'Kaito', title: 'Engineering Manager', level: 3}),
       (yuki:Employee {name: 'Yuki', title: 'Staff Engineer', level: 4}),
       (hana:Employee {name: 'Hana', title: 'Engineer', level: 5}),
       (taro:Employee {name: 'Taro', title: 'Controller', level: 3}),
       (sato)-[:REPORTS_TO]->(rin),
       (mori)-[:REPORTS_TO]->(rin),
       (kaito)-[:REPORTS_TO]->(sato),
       (yuki)-[:REPORTS_TO]->(kaito),
       (hana)-[:REPORTS_TO]->(yuki),
       (taro)-[:REPORTS_TO]->(mori)`,
  },
  {
    id: "transport",
    label: "Transport network",
    blurb:
      "Cities joined by routes carrying a weight, a cost, and a capacity. Dijkstra reads the first of those it finds, so this is where a weighted path differs from a shortest one.",
    cypher: `CREATE (tokyo:City {name: 'Tokyo', country: 'Japan'}),
       (nagoya:City {name: 'Nagoya', country: 'Japan'}),
       (kyoto:City {name: 'Kyoto', country: 'Japan'}),
       (osaka:City {name: 'Osaka', country: 'Japan'}),
       (fukuoka:City {name: 'Fukuoka', country: 'Japan'}),
       (sapporo:City {name: 'Sapporo', country: 'Japan'}),
       (tokyo)-[:ROUTE {weight: 350, cost: 11300, capacity: 1300}]->(nagoya),
       (nagoya)-[:ROUTE {weight: 140, cost: 5600, capacity: 900}]->(kyoto),
       (kyoto)-[:ROUTE {weight: 40, cost: 1400, capacity: 700}]->(osaka),
       (tokyo)-[:ROUTE {weight: 500, cost: 14500, capacity: 1100}]->(osaka),
       (osaka)-[:ROUTE {weight: 480, cost: 15400, capacity: 600}]->(fukuoka),
       (tokyo)-[:ROUTE {weight: 830, cost: 25000, capacity: 400}]->(sapporo)`,
  },
  {
    id: "retail",
    label: "Retail co-purchase",
    blurb:
      "Customers, products, and what they bought. Two labels and two relationship types, so it is the sample for grouped counts, a price range scan, and a co-purchase join.",
    cypher: `CREATE (aiko:Customer {name: 'Aiko'}),
       (ben:Customer {name: 'Ben'}),
       (chie:Customer {name: 'Chie'}),
       (keyboard:Product {name: 'Mechanical keyboard', price: 129, category: 'peripherals'}),
       (monitor:Product {name: 'Ultrawide monitor', price: 749, category: 'displays'}),
       (dock:Product {name: 'USB-C dock', price: 199, category: 'peripherals'}),
       (lamp:Product {name: 'Desk lamp', price: 59, category: 'lighting'}),
       (aiko)-[:BOUGHT {rating: 5}]->(keyboard),
       (aiko)-[:BOUGHT {rating: 4}]->(dock),
       (ben)-[:BOUGHT {rating: 5}]->(keyboard),
       (ben)-[:BOUGHT {rating: 3}]->(monitor),
       (chie)-[:BOUGHT {rating: 4}]->(monitor),
       (chie)-[:BOUGHT {rating: 5}]->(lamp),
       (keyboard)-[:SIMILAR_TO]->(dock),
       (dock)-[:SIMILAR_TO]->(keyboard),
       (monitor)-[:SIMILAR_TO]->(lamp)`,
  },
];

// The procedure reference the sidebar lists and searches. It lives here rather than in `app.js`
// because `make playground-check` runs every snippet, so a renamed procedure or a wrong yield
// name fails the build instead of reaching the page as a dead entry. The engine exposes no way
// to enumerate its procedures at runtime, which is why the list is written out at all.
//
// `requiresVectors` marks the two entries the sample graph cannot satisfy, since it stores no
// embeddings. Their snippets are still run, and still have to resolve to a real procedure; only
// the empty-index failure is tolerated.
export const PROCEDURES = [
  {
    name: "issundb.pageRank",
    args: "[{iterations, damping}]",
    yields: "nodeId, score",
    summary:
      "Ranks nodes by importance. A source spreads its rank across its edges, so parallel edges each carry mass, and dangling mass is not redistributed.",
    snippet: `CALL issundb.pageRank({iterations: 20, damping: 0.85})
YIELD nodeId, score
RETURN nodeId, score
ORDER BY score DESC`,
  },
  {
    name: "issundb.betweenness",
    args: "",
    yields: "nodeId, score",
    summary:
      "How often a node lies on a shortest path between two others, by Brandes' algorithm. Unnormalized, directed, and counted over distinct pairs.",
    snippet: `CALL issundb.betweenness()
YIELD nodeId, score
RETURN nodeId, score
ORDER BY score DESC`,
  },
  {
    name: "issundb.harmonic",
    args: "",
    yields: "nodeId, score",
    summary:
      "Sums the reciprocal of the shortest-path distance to every other node, so an unreachable node contributes nothing rather than infinity.",
    snippet: `CALL issundb.harmonic()
YIELD nodeId, score
RETURN nodeId, score
ORDER BY score DESC`,
  },
  {
    name: "issundb.degree",
    args: "[{direction}]",
    yields: "nodeId, score",
    summary:
      "Counts distinct neighbors in one direction, so parallel edges between the same pair count once. Direction is IN, OUT, or BOTH.",
    snippet: `CALL issundb.degree({direction: 'OUT'})
YIELD nodeId, score
RETURN nodeId, score
ORDER BY score DESC`,
  },
  {
    name: "issundb.wcc",
    aka: "issundb.connectedComponents",
    args: "",
    yields: "nodeId, componentId",
    summary:
      "Weakly connected components by union-find, treating every edge as undirected. The component id is the smallest node id in the component.",
    snippet: `CALL issundb.wcc()
YIELD nodeId, componentId
RETURN componentId, count(nodeId) AS size
ORDER BY componentId`,
  },
  {
    name: "issundb.scc",
    aka: "issundb.stronglyConnectedComponents",
    args: "",
    yields: "nodeId, componentId",
    summary:
      "Strongly connected components by Tarjan's algorithm, written iteratively so graph depth cannot reach the call stack. The browser stack is small, so that matters here.",
    snippet: `CALL issundb.scc()
YIELD nodeId, componentId
RETURN componentId, count(nodeId) AS size
ORDER BY size DESC, componentId`,
  },
  {
    name: "issundb.labelPropagation",
    args: "[{maxIterations}]",
    yields: "nodeId, communityId",
    summary:
      "Assigns each node the most common community among its neighbors, iterating to a fixed point. Ties break toward the smallest label, so the partition is stable run to run.",
    snippet: `CALL issundb.labelPropagation({maxIterations: 10})
YIELD nodeId, communityId
RETURN nodeId, communityId
ORDER BY communityId, nodeId`,
  },
  {
    name: "issundb.communities",
    args: "[{maxIterations, topPerCommunity}]",
    yields: "communityId, nodeId, rank",
    summary:
      "Label propagation, with each community's members ranked by PageRank. topPerCommunity keeps only the leading members of each.",
    snippet: `CALL issundb.communities({topPerCommunity: 3})
YIELD communityId, nodeId, rank
RETURN communityId, rank, nodeId
ORDER BY communityId, rank`,
  },
  {
    name: "issundb.shortestPath",
    args: "source, target",
    yields: "index, nodeId",
    summary:
      "Breadth-first shortest path by hop count, yielding one row per node along the path in order. An unreachable target yields no rows.",
    snippet: `CALL issundb.shortestPath(0, 5)
YIELD index, nodeId
RETURN index, nodeId
ORDER BY index`,
  },
  {
    name: "issundb.dijkstra",
    args: "source, target",
    yields: "index, nodeId, totalWeight",
    summary:
      "Least-weight path from a binary heap. The weight is the first present of the weight, cost, capacity, or cap property, defaulting to 1, and totalWeight repeats on every row.",
    snippet: `CALL issundb.dijkstra(0, 5)
YIELD index, nodeId, totalWeight
RETURN index, nodeId, totalWeight
ORDER BY index`,
  },
  {
    name: "issundb.triangleCount",
    args: "",
    yields: "count",
    summary:
      "Counts assignments of the directed pattern (a)->(b)->(c)->(a), so one cycle of three distinct nodes counts three times, once per rotation, as a Cypher MATCH would return it.",
    snippet: `CALL issundb.triangleCount()
YIELD count
RETURN count AS triangle_rows`,
  },
  {
    name: "issundb.retrieve.vector",
    args: "queryVector [, {k, hops, maxDistance, maxNodes}]",
    yields: "nodeId, distance",
    summary:
      "Vector search seeds expanded by breadth-first traversal. Lower distance is closer, and it is null for a node reached only by expansion.",
    snippet: `CALL issundb.retrieve.vector([1.0, 0.0, 0.25], {k: 3, hops: 1})
YIELD nodeId, distance
RETURN nodeId, distance
ORDER BY nodeId`,
    requiresVectors: true,
  },
  {
    name: "issundb.retrieve.hybrid",
    args: "queryVector, queryText [, config]",
    yields: "nodeId, score",
    summary:
      "Fuses vector and text relevance into one score before expanding. An empty query vector disables vector search, and an empty text query disables text search.",
    snippet: `CALL issundb.retrieve.hybrid([1.0, 0.0, 0.25], 'graph', {vectorK: 3, textK: 3, hops: 1})
YIELD nodeId, score
RETURN nodeId, score
ORDER BY nodeId`,
    requiresVectors: true,
  },
];

export const DEMO_CATEGORIES = [
  {
    label: "Cypher basics",
    docs: "../cypher/",
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
    docs: "../examples/#graph-data-science-in-cypher",
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
    docs: "../api-reference/#optimizer-statistics",
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
    docs: "../examples/",
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
    docs: "../api-reference/#vector-search-extensions",
    blurb: "An embedding per node, searched by exact distance in this build.",
    demos: [
      {
        label: "Nearest neighbours",
        desc: "Attaches a three-dimensional embedding to each person, then finds the closest to a query vector. This build uses the exact backend, so these are the true nearest neighbours rather than approximate ones.",
        cypher: `MATCH (p:Person) RETURN id(p) AS id, p.name AS name ORDER BY id`,
        vectors: { label: "Person", caption: "name" },
      },
    ],
  },
  {
    label: "GraphRAG",
    docs: "../hybrid-retrieval/",
    blurb:
      "Retrieval that starts with search and continues over relationships. Each example builds its own small corpus first, so running one twice adds a second copy.",
    demos: [
      {
        label: "Retrieve context",
        desc: "The shape a retrieval-augmented prompt is assembled from: rank documents by BM25, then read the winning passages back. The full-text index is written inside the same transaction as the node, so a hit is never stale.",
        cypher: RAG_CORPUS,
        textIndex: ["Doc", "body"],
        textSearch: "search graph relevance",
      },
      {
        label: "Semantic neighbours",
        desc: "The same corpus reached by embedding instead of by wording. This build searches by exact distance, so these are the true nearest neighbours rather than approximate ones.",
        cypher: RAG_CORPUS,
        vectors: { label: "Doc", caption: "title" },
      },
      {
        label: "Fuse text and vectors",
        desc: "Hybrid retrieval in one call: vector hits and text hits are scored, fused by reciprocal rank, and expanded over the graph before anything is returned. A node reached only by expansion has a null score, which is why the ordering puts nulls last.",
        cypher: RAG_CORPUS,
        embed: { label: "Doc" },
        textIndex: ["Doc", "body"],
        thenQuery: `CALL issundb.retrieve.hybrid([1.0, 0.0, 0.25], 'retrieval relevance',
  {vectorK: 2, textK: 2, hops: 1, textLabel: 'Doc', textProperty: 'body'})
YIELD nodeId, score
MATCH (d:Doc) WHERE id(d) = nodeId
RETURN d.title AS title, score
ORDER BY score IS NULL, score DESC, title`,
      },
      {
        label: "Ground an answer",
        desc: "What a language model would be handed: the retrieved documents plus the ones they sit next to, collected into a single list per seed. Assembling context is a traversal, which is the argument for keeping it in the database.",
        cypher: `${RAG_CORPUS};
MATCH (d:Doc)-[:CITES]->(other:Doc)
WHERE d.title IN ['Hybrid retrieval', 'Graph databases']
RETURN d.title AS seed, collect(other.title) AS also_read
ORDER BY seed`,
      },
    ],
  },
  {
    label: "Knowledge graph",
    docs: "../cypher/",
    blurb:
      "Entities, typed relations, and questions that need more than one hop. Each example builds its own graph first, so running one twice adds a second copy.",
    demos: [
      {
        label: "Entities and relations",
        desc: "A small research graph: people, the labs they work in, the papers they wrote, and the concepts those mention. Four labels and three relationship types, which is the shape most knowledge graphs reduce to.",
        cypher: `${KG_CORPUS};
MATCH (r:Researcher)-[:AUTHORED]->(p:Paper)-[:MENTIONS]->(c:Concept)
RETURN r.name AS researcher, p.title AS paper, c.name AS concept
ORDER BY researcher, paper, concept`,
      },
      {
        label: "Multi-hop question",
        desc: "\"Which cities work on retrieval augmented generation?\" is three hops and a group-by, not a join plan a reader has to write. The optimizer splits the conjunction so each filter pushes down to its own lowest binder.",
        cypher: `${KG_CORPUS};
MATCH (c:Concept {name: 'retrieval augmented generation'})<-[:MENTIONS]-(p:Paper)<-[:AUTHORED]-(r:Researcher)-[:WORKS_IN]->(lab:Lab)
RETURN lab.city AS city, count(DISTINCT p) AS papers, collect(DISTINCT r.name) AS researchers
ORDER BY city`,
      },
      {
        label: "Shared concepts",
        desc: "Two researchers connected by what they write about rather than by an edge between them. The closing hop of a cyclic pattern is fused rather than materialized as a wedge per intermediate node.",
        cypher: `${KG_CORPUS};
MATCH (a:Researcher)-[:AUTHORED]->(:Paper)-[:MENTIONS]->(c:Concept)<-[:MENTIONS]-(:Paper)<-[:AUTHORED]-(b:Researcher)
WHERE a.name < b.name
RETURN a.name AS one, b.name AS other, collect(DISTINCT c.name) AS shared
ORDER BY one, other`,
      },
      {
        label: "Reach by hops",
        desc: "How far each concept sits from one researcher, over any of the three relationship types. The relationship variable binds to the whole list traversed, so size(path) is the hop count.",
        cypher: `${KG_CORPUS};
MATCH (r:Researcher {name: 'Ada Ito'})-[path*1..3]->(c:Concept)
RETURN c.name AS concept, min(size(path)) AS hops
ORDER BY hops, concept`,
      },
    ],
  },
];
