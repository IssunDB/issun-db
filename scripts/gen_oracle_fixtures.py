#!/usr/bin/env python3
"""Generate a NetworkX oracle corpus for IssunDB graph algorithms.

This script produces a deterministic, seeded corpus of simple directed graphs
together with the reference output of NetworkX for a fixed set of algorithms.
The corpus is committed to the repository and replayed by a hermetic Rust test
(`crates/issundb/tests/oracle.rs`); the NetworkX dependency lives here, in the
generator, and never in the Rust test path.

Scope (first slice): the exact, spec-unambiguous algorithms only, where a
mismatch is unambiguously a bug rather than a convention difference:

  - weakly connected components   -> nx.weakly_connected_components
  - strongly connected components -> nx.strongly_connected_components
  - unweighted shortest path len  -> nx.shortest_path_length (directed)
  - maximum flow value            -> nx.maximum_flow_value (integer capacities)
  - acyclicity                    -> nx.is_directed_acyclic_graph
  - in/out/total degree           -> nx.DiGraph.in_degree / out_degree
  - weighted shortest path len    -> nx.dijkstra_path_length (capacity weights)
  - all shortest paths            -> nx.all_shortest_paths (directed)
  - bfs/dfs reachable set         -> nx.single_source_shortest_path_length
  - top-k shortest path weights   -> nx.shortest_simple_paths (capacity weights)
  - spanning forest weight/size   -> nx.minimum/maximum_spanning_edges

The spanning forest is compared by total weight and edge count rather than by
edge set, because the minimum/maximum spanning forest is not unique when edge
weights tie; the total weight and edge count are. IssunDB iterates the directed
CSR edges and unions them as undirected, so the oracle runs Kruskal over the
same directed edge multiset modeled as an undirected `MultiGraph` (anti-parallel
directed edges become parallel undirected candidates).

The graphs are simple (no self-loops and no parallel edges) so that no multigraph
or self-loop convention difference can creep into the comparison.

Regenerate with `make oracle-fixtures`.
"""

import json
import random
import sys

import networkx as nx

SEED = 0x1553_0DB5
NUM_GRAPHS = 80
MIN_NODES = 4
MAX_NODES = 12
# Edge densities sampled per graph; the spread covers sparse (disconnected),
# moderate, and dense graphs so component counts and reachability vary.
DENSITIES = [0.0, 0.1, 0.2, 0.35, 0.5, 0.7]
MIN_CAPACITY = 1
MAX_CAPACITY = 10


def build_graph(rng, n, density):
    """Build a simple directed graph with integer edge capacities."""
    g = nx.DiGraph()
    g.add_nodes_from(range(n))
    for u in range(n):
        for v in range(n):
            if u == v:
                continue
            if rng.random() < density:
                g.add_edge(u, v, capacity=rng.randint(MIN_CAPACITY, MAX_CAPACITY))
    return g


def partition(component_sets):
    """Canonicalize a NetworkX component iterator to sorted node lists."""
    parts = [sorted(int(x) for x in comp) for comp in component_sets]
    parts.sort()
    return parts


def shortest_path_lengths(g, n):
    """All ordered (src, dst) pairs as [src, dst, len]; -1 means unreachable."""
    out = []
    for s in range(n):
        for t in range(n):
            if s == t:
                continue
            if nx.has_path(g, s, t):
                out.append([s, t, nx.shortest_path_length(g, s, t)])
            else:
                out.append([s, t, -1])
    return out


def max_flows(g, n):
    """All ordered (src, sink) pairs as [src, sink, value]."""
    out = []
    for s in range(n):
        for t in range(n):
            if s == t:
                continue
            value = nx.maximum_flow_value(g, s, t, capacity="capacity")
            out.append([s, t, int(value)])
    return out


def degrees(g, n):
    """In, out, and total degree per node (no self-loops in this corpus)."""
    return {
        "in": [g.in_degree(i) for i in range(n)],
        "out": [g.out_degree(i) for i in range(n)],
        "both": [g.in_degree(i) + g.out_degree(i) for i in range(n)],
    }


def dijkstra_lengths(g, n):
    """All ordered (src, dst) pairs as [src, dst, weight] using the `capacity`
    edge attribute as the weight; -1.0 marks an unreachable pair. IssunDB's
    weighted shortest path reads the same `weight`-then-`capacity` source."""
    out = []
    for s in range(n):
        for t in range(n):
            if s == t:
                continue
            if nx.has_path(g, s, t):
                w = nx.dijkstra_path_length(g, s, t, weight="capacity")
                out.append([s, t, float(w)])
            else:
                out.append([s, t, -1.0])
    return out


def all_shortest(g, n):
    """Reachable ordered (src, dst) pairs as [src, dst, [path, ...]], where each
    path is a node-index list. Unreachable and trivial (s == t) pairs are
    omitted."""
    out = []
    for s in range(n):
        for t in range(n):
            if s == t or not nx.has_path(g, s, t):
                continue
            paths = sorted([int(x) for x in p] for p in nx.all_shortest_paths(g, s, t))
            out.append([s, t, paths])
    return out


def bfs_reach(g, n):
    """[start, hops, nodes] giving the set of nodes within `hops` out-edges of
    `start` (including `start`), for hops 1..3. BFS and DFS both return this
    set."""
    out = []
    for start in range(n):
        lengths = nx.single_source_shortest_path_length(g, start)
        for hops in (1, 2, 3):
            nodes = sorted(int(v) for v, d in lengths.items() if d <= hops)
            out.append([start, hops, nodes])
    return out


def top_k_weights(g, n, k=3):
    """[s, t, weights] for reachable pairs: the ascending total weights of up to
    `k` loopless shortest paths, weighted by the `capacity` attribute."""
    out = []
    for s in range(n):
        for t in range(n):
            if s == t or not nx.has_path(g, s, t):
                continue
            weights = []
            for i, path in enumerate(nx.shortest_simple_paths(g, s, t, weight="capacity")):
                if i >= k:
                    break
                weights.append(float(sum(g[u][v]["capacity"] for u, v in zip(path, path[1:]))))
            out.append([s, t, sorted(weights)])
    return out


def spanning_forest(g):
    """Minimum and maximum spanning forest total weight and edge count.

    IssunDB unions the directed CSR edges as undirected, so the oracle models
    the directed edge multiset as an undirected `MultiGraph` (anti-parallel
    directed edges become parallel undirected candidates) and runs Kruskal over
    it. Only the total weight and edge count are reported, since the chosen edge
    set is not unique under weight ties."""
    mg = nx.MultiGraph()
    for u, v, data in g.edges(data=True):
        mg.add_edge(u, v, capacity=data["capacity"])

    def forest(maximum):
        if maximum:
            gen = nx.maximum_spanning_edges(mg, weight="capacity", keys=False, data=True)
        else:
            gen = nx.minimum_spanning_edges(mg, weight="capacity", keys=False, data=True)
        edges = list(gen)
        total = sum(d["capacity"] for *_, d in edges)
        return {"weight": float(total), "edges": len(edges)}

    return {"min": forest(False), "max": forest(True)}


def main():
    rng = random.Random(SEED)
    cases = []
    for i in range(NUM_GRAPHS):
        n = rng.randint(MIN_NODES, MAX_NODES)
        density = rng.choice(DENSITIES)
        g = build_graph(rng, n, density)
        edges = sorted((int(u), int(v)) for u, v in g.edges())
        capacities = [int(g[u][v]["capacity"]) for u, v in edges]
        cases.append(
            {
                "id": f"g{i:04d}",
                "n": n,
                "edges": [[u, v] for u, v in edges],
                "capacities": capacities,
                "wcc": partition(nx.weakly_connected_components(g)),
                "scc": partition(nx.strongly_connected_components(g)),
                "sp_len": shortest_path_lengths(g, n),
                "maxflow": max_flows(g, n),
                "is_dag": nx.is_directed_acyclic_graph(g),
                "degree": degrees(g, n),
                "dijkstra": dijkstra_lengths(g, n),
                "all_sp": all_shortest(g, n),
                "bfs": bfs_reach(g, n),
                "top_k": top_k_weights(g, n),
                "spanning": spanning_forest(g),
            }
        )

    corpus = {
        "meta": {
            "generator": "scripts/gen_oracle_fixtures.py",
            "networkx_version": nx.__version__,
            "seed": SEED,
            "num_graphs": NUM_GRAPHS,
        },
        "cases": cases,
    }

    out_path = sys.argv[1] if len(sys.argv) > 1 else "-"
    text = json.dumps(corpus, indent=2, sort_keys=True) + "\n"
    if out_path == "-":
        sys.stdout.write(text)
    else:
        with open(out_path, "w", encoding="utf-8") as f:
            f.write(text)
        print(f"Wrote {len(cases)} graphs to {out_path}", file=sys.stderr)


if __name__ == "__main__":
    main()
