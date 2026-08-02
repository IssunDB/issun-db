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
// queries; the other five replace it when Reset Database is pressed with one of them selected.
export const SAMPLE_GRAPHS = [
    {
        id: "social",
        label: "Social network",
        cypher: SAMPLE_SOCIAL,
    },
    {
        id: "articles",
        label: "Article corpus",
        cypher: `CREATE (a1:Article {title: 'Graph databases', year: 2019,
         body: 'A graph database stores nodes and relationships instead of tables and joins.'}),
       (a2:Article {title: 'Vector search', year: 2021,
         body: 'Approximate nearest neighbor search finds similar embeddings quickly.'}),
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
    {
        id: "knowledge",
        label: "Knowledge graph",
        cypher: `CREATE (ada:Researcher {name: 'Ada Ito'}),
       (bo:Researcher {name: 'Bo Chen'}),
       (cai:Researcher {name: 'Cai Rossi'}),
       (lab1:Lab {name: 'Retrieval Group', city: 'Kyoto'}),
       (lab2:Lab {name: 'Systems Group', city: 'Zurich'}),
       (p1:Paper {title: 'Fusing text and vector relevance', year: 2023}),
       (p2:Paper {title: 'Adjacency layouts for traversal', year: 2022}),
       (p3:Paper {title: 'Grounding answers in a graph', year: 2024}),
       (c1:Concept {name: 'retrieval augmented generation'}),
       (c2:Concept {name: 'nearest neighbor search'}),
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
       (p2)-[:MENTIONS]->(c3)`,
    },
    {
        id: "corpus",
        label: "Retrieval corpus",
        // The one sample that arrives ready for retrieval. Everything else here is pure Cypher, but
        // an embedding and a full-text index are Rust extension traits rather than statements, so
        // this entry names what the page must add after the `CREATE`: `textIndex` is created and
        // every `vectorProperty` list is upserted into the vector index. Keeping the vectors as an
        // ordinary property is what lets a reader see, edit, and re-run the numbers the search is
        // actually over.
        //
        // The ten documents sit in three deliberate clusters, so a nearest-neighbor query returns a
        // topic rather than an arbitrary row: storage near [1, 0], search near [0, 1], and
        // correctness near [-1, 0], with two deliberately between them.
        textIndex: ["Doc", "body"],
        vectorProperty: "embedding",
        cypher: `CREATE (d1:Doc {title: 'Adjacency storage', topic: 'storage', embedding: [0.98, 0.05, 0.25],
         body: 'A graph database stores adjacency as sorted rows so a traversal reads one range instead of joining tables.'}),
       (d2:Doc {title: 'Compressed rows', topic: 'storage', embedding: [0.95, 0.16, 0.25],
         body: 'Compressed sparse row layout keeps every neighbour of a node contiguous, which makes a graph traversal sequential.'}),
       (d3:Doc {title: 'Durable writes', topic: 'storage', embedding: [0.92, -0.12, 0.25],
         body: 'A write transaction appends to storage and commits atomically, so a reader never observes a partial graph.'}),
       (d4:Doc {title: 'Vector search', topic: 'search', embedding: [0.10, 0.97, 0.25],
         body: 'Nearest neighbour search over embeddings finds documents whose meaning is close even when the wording differs.'}),
       (d5:Doc {title: 'Full-text ranking', topic: 'search', embedding: [-0.05, 0.99, 0.25],
         body: 'BM25 ranks a document by term frequency against the corpus, so a rare word carries more relevance than a common one.'}),
       (d6:Doc {title: 'Hybrid retrieval', topic: 'search', embedding: [0.20, 0.94, 0.25],
         body: 'Fusing vector similarity with full-text relevance retrieves better context than either signal alone.'}),
       (d7:Doc {title: 'Join ordering', topic: 'correctness', embedding: [-0.95, 0.10, 0.25],
         body: 'A planner picks a join order from cardinality statistics, because the wrong order builds an intermediate nobody needs.'}),
       (d8:Doc {title: 'Isolation levels', topic: 'correctness', embedding: [-0.90, -0.20, 0.25],
         body: 'Snapshot isolation lets a long read run beside a writer without blocking it or observing half of its work.'}),
       (d9:Doc {title: 'Grounded answers', topic: 'bridge', embedding: [0.70, 0.70, 0.25],
         body: 'Grounding a language model in a graph traversal keeps an answer attached to the documents it came from.'}),
       (d10:Doc {title: 'Traversal cost', topic: 'bridge', embedding: [0.62, -0.75, 0.25],
         body: 'Expanding a relationship costs one adjacency read per source, so a planner prefers the smaller side of a join.'}),
       (d1)-[:CITES]->(d2),
       (d2)-[:CITES]->(d3),
       (d4)-[:CITES]->(d5),
       (d6)-[:CITES]->(d4),
       (d6)-[:CITES]->(d5),
       (d7)-[:CITES]->(d10),
       (d9)-[:CITES]->(d6),
       (d9)-[:CITES]->(d1),
       (d10)-[:CITES]->(d2),
       (d8)-[:CITES]->(d3)`,
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
// The `issundb.*` scalar functions. Kept apart from PROCEDURES because they are called in an
// expression rather than through CALL, which is not a presentation detail: a CALL evaluates its
// arguments against no bindings and runs once per statement, so a pairwise score could never see
// the two nodes a MATCH bound. That is why these are functions at all.
//
// Ordinary Cypher functions (toUpper, substring, the temporal family) are deliberately absent. They
// are documented by every Cypher reference there is, while these are documented nowhere else.
export const FUNCTIONS = [
    {
        name: "issundb.link.commonNeighbors",
        args: "a, b",
        yields: "number",
        summary:
            "How many neighbors two nodes share. The neighborhood is undirected and distinct, so a pair joined by several edges counts once.",
        snippet: `MATCH (a:Person {name: 'Ada'}), (b:Person {name: 'Barbara'})
RETURN issundb.link.commonNeighbors(a, b) AS shared`,
    },
    {
        name: "issundb.link.jaccard",
        args: "a, b",
        yields: "number",
        summary:
            "Shared neighbors over the size of the combined neighborhood, so a pair of quiet nodes is not out-scored by a pair of hubs.",
        snippet: `MATCH (a:Person), (b:Person) WHERE id(a) < id(b)
RETURN a.name, b.name, issundb.link.jaccard(a, b) AS score
ORDER BY score DESC, a.name, b.name LIMIT 5`,
    },
    {
        name: "issundb.link.adamicAdar",
        args: "a, b",
        yields: "number",
        summary:
            "Shared neighbors weighted by 1/ln(degree), so a neighbor everybody knows counts for little. A shared neighbor of degree one contributes nothing.",
        snippet: `MATCH (a:Person), (b:Person) WHERE id(a) < id(b)
RETURN a.name, b.name, issundb.link.adamicAdar(a, b) AS score
ORDER BY score DESC, a.name, b.name LIMIT 5`,
    },
    {
        name: "issundb.link.resourceAllocation",
        args: "a, b",
        yields: "number",
        summary:
            "Shared neighbors weighted by 1/degree, which penalizes a popular neighbor harder than Adamic-Adar does.",
        snippet: `MATCH (a:Person), (b:Person) WHERE id(a) < id(b)
RETURN a.name, b.name, issundb.link.resourceAllocation(a, b) AS score
ORDER BY score DESC, a.name, b.name LIMIT 5`,
    },
    {
        name: "issundb.link.preferentialAttachment",
        args: "a, b",
        yields: "number",
        summary:
            "The product of the two degrees. It ignores shared neighbors entirely, so it scores pairs that have nothing in common.",
        snippet: `MATCH (a:Person), (b:Person) WHERE id(a) < id(b)
RETURN a.name, b.name, issundb.link.preferentialAttachment(a, b) AS score
ORDER BY score DESC, a.name, b.name LIMIT 5`,
    },
    {
        name: "issundb.similarity.jaccard",
        args: "listA, listB",
        yields: "number",
        summary:
            "Set similarity over two lists of values, not over the graph. Intersection divided by union.",
        snippet: `RETURN issundb.similarity.jaccard([1, 2, 3], [2, 3, 4]) AS score`,
    },
    {
        name: "issundb.similarity.overlap",
        args: "listA, listB",
        yields: "number",
        summary:
            "Intersection divided by the size of the smaller list, so a subset scores 1 however lopsided the pair is.",
        snippet: `RETURN issundb.similarity.overlap([1, 2], [1, 2, 3, 4]) AS score`,
    },
    {
        name: "issundb.distance.cosine",
        args: "vectorA, vectorB",
        yields: "number",
        summary:
            "Cosine distance between two embeddings. Either argument may be a node, which resolves to its stored embedding, or a literal vector. Subtract from 1 for cosine similarity; a length mismatch is null.",
        snippet: `RETURN issundb.distance.cosine([1.0, 0.0], [1.0, 0.0]) AS d,
       1 - issundb.distance.cosine([1.0, 0.0], [0.0, 1.0]) AS similarity`,
    },
    {
        name: "issundb.distance.euclidean",
        args: "vectorA, vectorB",
        yields: "number",
        summary:
      "Straight-line distance between two embeddings, each either a node or a literal vector.",
        snippet: `RETURN issundb.distance.euclidean([0.0, 0.0], [3.0, 4.0]) AS d`,
    },
    {
        name: "vector_dist",
        args: "a, b",
        yields: "number",
        summary:
            "Distance between two embeddings under the graph's configured metric. Either argument may be a node, which resolves to its stored embedding, or a literal vector.",
        snippet: `RETURN vector_dist([1.0, 0.0, 0.25], [0.0, 1.0, 0.25]) AS d`,
    },
];

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
        name: "issundb.closeness",
        args: "",
        yields: "nodeId, score",
        summary:
            "Reciprocal mean distance to every reachable node, scaled by the fraction of the graph reached, so a node in a small component does not outscore a well-connected one.",
        snippet: `CALL issundb.closeness()
YIELD nodeId, score
RETURN nodeId, score
ORDER BY score DESC`,
    },
    {
        name: "issundb.eigenvector",
        args: "[{iterations, tolerance}]",
        yields: "nodeId, score",
        summary:
            "Ranks a node by how important the nodes pointing at it are, by power iteration. Scores are magnitudes scaled to sum to the node count.",
        snippet: `CALL issundb.eigenvector({iterations: 100})
YIELD nodeId, score
RETURN nodeId, score
ORDER BY score DESC`,
    },
    {
        name: "issundb.katz",
        args: "[{alpha, beta, iterations, tolerance}]",
        yields: "nodeId, score",
        summary:
            "Sums the walks reaching a node, attenuating a walk of length k by alpha^k, plus a beta baseline every node receives. Unlike eigenvector centrality it scores a node with no incoming edges.",
        snippet: `CALL issundb.katz({alpha: 0.1, beta: 1.0})
YIELD nodeId, score
RETURN nodeId, score
ORDER BY score DESC`,
    },
    {
        name: "issundb.clusteringCoefficient",
        args: "",
        yields: "nodeId, score",
        summary:
            "The fraction of a node's neighbor pairs that are themselves connected, read as undirected over distinct neighbors so the score stays within 0 and 1.",
        snippet: `CALL issundb.clusteringCoefficient()
YIELD nodeId, score
RETURN nodeId, score
ORDER BY score DESC`,
    },
    {
        name: "issundb.louvain",
        args: "",
        yields: "nodeId, communityId",
        summary:
            "Community detection by modularity optimization with coarsening. Separates communities joined by a few edges, which label propagation tends to merge. The community id is the smallest node id it contains.",
        snippet: `CALL issundb.louvain()
YIELD nodeId, communityId
RETURN nodeId, communityId
ORDER BY communityId, nodeId`,
    },
    {
        name: "issundb.communities",
        args: "[{maxIterations, topPerCommunity, algorithm}]",
        yields: "communityId, nodeId, rank",
        summary:
            "A partition with each community's members ranked by PageRank. algorithm selects labelPropagation (the default) or louvain, and topPerCommunity keeps only the leading members of each.",
        snippet: `CALL issundb.communities({topPerCommunity: 3, algorithm: 'louvain'})
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
        sample: "social",
        requiresLabel: "Person",
        docs: "../cypher/",
        demos: [
            {
                label: "Create nodes",
                desc: "Writes nodes and a relationship in one statement, and returns what it made. Every clause of a write statement shares one transaction, so an error anywhere rolls back all of it. To load a whole dataset instead of writing one, use Pick a Graph.",
                cypher: `CREATE (grete:Person {name: 'Grete', city: 'Berlin', age: 34}),
       (kurt:Person {name: 'Kurt', city: 'Vienna', age: 47}),
       (grete)-[:KNOWS {since: 1931, weight: 6}]->(kurt)
RETURN grete.name AS created, kurt.name AS and_also`,
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
                label: "Update",
                desc: "SET assigns a property and a label; a statement's own projection sees its uncommitted writes through a pending-writes overlay.",
                cypher: `MATCH (p:Person {name: 'Donald'})
SET p.city = 'Palo Alto', p:Retired
RETURN p.name AS name, p.city AS city, labels(p) AS labels`,
            },
        ],
    },
    {
        label: "Graph algorithms",
        sample: "social",
        requiresLabel: "Person",
        docs: "../examples/#graph-data-science-in-cypher",
        demos: [
            {
                label: "PageRank",
                desc: "Ranks nodes by importance. A source spreads its rank across its edges, so parallel edges each carry mass.",
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
                label: "Degree",
                desc: "Counts distinct neighbors in the chosen direction, so parallel edges between the same pair count once. Harmonic centrality, which sums the reciprocal of each shortest-path distance, is issundb.harmonic in the Reference panel.",
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
                desc: "Louvain communities, then each community's members ranked by PageRank. Louvain optimizes modularity and converges, so the partition is the same every run; label propagation, the other algorithm this procedure accepts, can oscillate between two partitions on a graph this small and hand back whichever one its last iteration landed on.",
                cypher: `CALL issundb.communities({algorithm: 'louvain', topPerCommunity: 3})
YIELD communityId, nodeId, rank
MATCH (p) WHERE id(p) = nodeId
RETURN communityId, rank, p.name AS name
ORDER BY communityId, rank`,
            },
        ],
    },
    {
        label: "Link prediction",
        sample: "social",
        requiresLabel: "Person",
        docs: "../api-reference/#link-prediction",
        demos: [
            {
                label: "Mutual connections",
                desc: "How many neighbors two people share, and the same count as a ratio of their combined neighborhood. The neighborhood is undirected and distinct, so who pointed at whom does not matter and a repeated edge counts once.",
                cypher: `MATCH (a:Person), (b:Person)
WHERE id(a) < id(b)
RETURN a.name AS a, b.name AS b,
       issundb.link.commonNeighbors(a, b) AS mutual,
       issundb.link.jaccard(a, b) AS jaccard
ORDER BY mutual DESC, a, b`,
            },
            {
                label: "Who might know whom",
                desc: "The same score, but only for pairs not already connected, which is the question link prediction actually answers. Scoring an existing edge highly proves nothing, so the known pairs are collected first and excluded.",
                cypher: `MATCH (x:Person)-[:KNOWS]->(y:Person)
WITH collect(toString(id(x)) + '>' + toString(id(y))) AS links
MATCH (a:Person), (b:Person)
WHERE id(a) < id(b)
  AND NOT toString(id(a)) + '>' + toString(id(b)) IN links
  AND NOT toString(id(b)) + '>' + toString(id(a)) IN links
RETURN a.name AS a, b.name AS b,
       issundb.link.commonNeighbors(a, b) AS mutual,
       issundb.link.adamicAdar(a, b) AS score
ORDER BY score DESC, a, b`,
            },
            {
                label: "The metrics disagree",
                desc: "All five on the same pairs. Adamic-Adar and resource allocation discount a neighbor everybody shares, jaccard normalizes by neighborhood size, and preferential attachment ignores shared neighbors entirely: it multiplies the two degrees, so it ranks busy pairs highly even with nothing in common.",
                cypher: `MATCH (a:Person), (b:Person)
WHERE id(a) < id(b)
RETURN a.name AS a, b.name AS b,
       issundb.link.commonNeighbors(a, b) AS common,
       issundb.link.jaccard(a, b) AS jaccard,
       issundb.link.adamicAdar(a, b) AS adamicAdar,
       issundb.link.resourceAllocation(a, b) AS resourceAlloc,
       issundb.link.preferentialAttachment(a, b) AS prefAttach
ORDER BY adamicAdar DESC, prefAttach DESC, a, b`,
            },
        ],
    },
    {
        label: "Query planning",
        sample: "social",
        requiresLabel: "Person",
        docs: "../api-reference/#optimizer-statistics",
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
        sample: "articles",
        requiresLabel: "Article",
        docs: "../examples/",
        demos: [
            {
                label: "Index and search",
                desc: "Provisions a full-text index over Article.body, then ranks the corpus for a query. The postings are written inside the same transaction as the node, so the index is transactional rather than eventually consistent and a hit is never stale.",
                cypher: `MATCH (a:Article)
RETURN a.title AS title, a.year AS year
ORDER BY year`,
                textIndex: ["Article", "body"],
                textSearch: "graph relationships",
            },
        ],
    },
    {
        label: "Vector search",
        sample: "social",
        requiresLabel: "Person",
        docs: "../api-reference/#vector-search-extensions",
        demos: [
            {
                label: "Nearest neighbors",
                desc: "Attaches a three-dimensional embedding to each person, then finds the closest to a query vector. This build uses the exact backend, so these are the true nearest neighbors rather than approximate ones.",
                cypher: `MATCH (p:Person) RETURN id(p) AS id, p.name AS name ORDER BY id`,
                vectors: {label: "Person", caption: "name"},
            },
        ],
    },
    {
        label: "GraphRAG",
        sample: "articles",
        requiresLabel: "Article",
        docs: "../hybrid-retrieval/",
        demos: [
            {
                label: "Retrieve context",
                desc: "The shape a retrieval-augmented prompt is assembled from: provision a full-text index over the corpus, then rank it by BM25. The index is written inside the same transaction as the node, so a hit is never stale.",
                cypher: `MATCH (a:Article)
RETURN a.title AS title, a.year AS year
ORDER BY year`,
                textIndex: ["Article", "body"],
                textSearch: "graph search relevance",
            },
            {
                label: "Semantic neighbors",
                desc: "The same corpus reached by embedding rather than by wording. This build searches by exact distance, so these are the true nearest neighbors rather than approximate ones.",
                cypher: `MATCH (a:Article)
RETURN a.title AS title, a.year AS year
ORDER BY year`,
                vectors: {label: "Article", caption: "title"},
            },
            {
                label: "Fuse text and vectors",
                desc: "Hybrid retrieval in one call: vector hits and text hits are scored, fused by reciprocal rank, and expanded over the graph before anything is returned. A node reached only by expansion has a null score, which is why the ordering puts nulls last.",
                cypher: `MATCH (a:Article)
RETURN a.title AS title, a.year AS year
ORDER BY year`,
                embed: {label: "Article"},
                textIndex: ["Article", "body"],
                thenQuery: `CALL issundb.retrieve.hybrid([1.0, 0.0, 0.25], 'graph relevance',
  {vectorK: 2, textK: 2, hops: 1, textLabel: 'Article', textProperty: 'body'})
YIELD nodeId, score
MATCH (a:Article) WHERE id(a) = nodeId
RETURN a.title AS title, score
ORDER BY score IS NULL, score DESC, title`,
            },
            {
                label: "Ground an answer",
                desc: "What a language model would be handed: the retrieved documents plus the ones they cite, collected into one list per seed. Assembling context is a traversal, which is the argument for keeping it in the database.",
                cypher: `MATCH (a:Article)-[:CITES]->(cited:Article)
WHERE a.title IN ['Hybrid retrieval', 'Graph databases']
RETURN a.title AS seed, collect(cited.title) AS also_read
ORDER BY seed`,
            },
        ],
    },
    {
        label: "Retrieval procedures",
        sample: "corpus",
        requiresLabel: "Doc",
        docs: "../hybrid-retrieval/",
        demos: [
            {
                label: "Nearest documents",
                desc: "Vector retrieval, then one hop of expansion. The query vector points at the storage cluster, so the three storage documents come back ahead of everything else. This is the sample that ships with embeddings; on any other one the procedure reports an empty index.",
                cypher: `CALL issundb.retrieve.vector([1.0, 0.0, 0.25], {k: 3, hops: 1})
YIELD nodeId, distance
MATCH (d:Doc) WHERE id(d) = nodeId
RETURN d.title AS title, d.topic AS topic, distance
ORDER BY distance, title`,
            },
            {
                label: "Fuse text and vectors",
                desc: "Hybrid retrieval over the same corpus. The vector half points at storage and the text half asks for ranking and relevance, so the fused result carries both topics; a document reached only by expansion has no score of its own and sorts last.",
                cypher: `CALL issundb.retrieve.hybrid([1.0, 0.0, 0.25], 'ranking relevance',
  {vectorK: 3, textK: 3, hops: 1})
YIELD nodeId, score
MATCH (d:Doc) WHERE id(d) = nodeId
RETURN d.title AS title, d.topic AS topic, score
ORDER BY score IS NULL, score DESC, title`,
            },
            {
                label: "Compare the two signals",
                desc: "The same corpus asked twice over. Vector distance answers what a document means, and the stored embedding is an ordinary property, so editing it in Pick a Graph and re-seeding changes what comes back.",
                cypher: `MATCH (d:Doc)
RETURN d.topic AS topic, count(d) AS documents,
       collect(d.title)[0] AS first_title
ORDER BY documents DESC, topic`,
            },
        ],
    },
    {
        label: "Knowledge graph",
        sample: "knowledge",
        requiresLabel: "Researcher",
        docs: "../cypher/",
        demos: [
            {
                label: "Entities and relations",
                desc: "A small research graph: people, the labs they work in, the papers they wrote, and the concepts those mention. Four labels and three relationship types, which is the shape most knowledge graphs reduce to.",
                cypher: `MATCH (r:Researcher)-[:AUTHORED]->(p:Paper)-[:MENTIONS]->(c:Concept)
RETURN r.name AS researcher, p.title AS paper, c.name AS concept
ORDER BY researcher, paper, concept`,
            },
            {
                label: "Multi-hop question",
                desc: "\"Which cities work on retrieval augmented generation?\" is three hops and a group-by, not a join plan a reader has to write. The optimizer splits the conjunction so each filter pushes down to its own lowest binder.",
                cypher: `MATCH (c:Concept {name: 'retrieval augmented generation'})<-[:MENTIONS]-(p:Paper)<-[:AUTHORED]-(r:Researcher)-[:WORKS_IN]->(lab:Lab)
RETURN lab.city AS city, count(DISTINCT p) AS papers, collect(DISTINCT r.name) AS researchers
ORDER BY city`,
            },
            {
                label: "Shared concepts",
                desc: "Two researchers connected by what they write about rather than by an edge between them. The closing hop of a cyclic pattern is fused rather than materialized as a wedge per intermediate node.",
                cypher: `MATCH (a:Researcher)-[:AUTHORED]->(:Paper)-[:MENTIONS]->(c:Concept)<-[:MENTIONS]-(:Paper)<-[:AUTHORED]-(b:Researcher)
WHERE a.name < b.name
RETURN a.name AS one, b.name AS other, collect(DISTINCT c.name) AS shared
ORDER BY one, other`,
            },
            {
                label: "Reach by hops",
                desc: "How far each concept sits from one researcher, over any of the three relationship types. The relationship variable binds to the whole list traversed, so size(path) is the hop count.",
                cypher: `MATCH (r:Researcher {name: 'Ada Ito'})-[path*1..3]->(c:Concept)
RETURN c.name AS concept, min(size(path)) AS hops
ORDER BY hops, concept`,
            },
        ],
    },
];
