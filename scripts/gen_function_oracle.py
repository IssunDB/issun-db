#!/usr/bin/env python3
"""Generate a ground-truth corpus for IssunDB's built-in functions and the three
procedures whose definitions are exact.

The other generators in this directory cover the path and centrality algorithms.
What had no reference at all was the `issundb.link.*`, `issundb.similarity.*`,
and `issundb.distance.*` function families, plus closeness, the clustering
coefficient, and the triangle count: those were checked only against IssunDB's
own row pipeline, which catches a fast-path defect and nothing else. A wrong
formula agreed with itself.

Each value here is computed twice and the two must match before anything is
written: once from the definition in plain Python, and once from NetworkX where
it has an equivalent. That double computation is the point. NetworkX alone would
not do, because its graph types are simple: it collapses the parallel edges and
drops the self-loops that IssunDB's conventions have to take a position on, so
for those cases the plain-Python side is the only reference, and this script
says so per algorithm rather than quietly comparing on graphs where the question
never arises.

Conventions pinned here, taken from the crate docs and not from NetworkX:

  - the `link.*` family reads DISTINCT neighbors over the undirected view, so a
    parallel edge counts once and a self-loop is excluded from a node's own
    neighborhood
  - `adamicAdar` skips a shared neighbor of degree one, since `ln(1)` is zero
  - `clusteringCoefficient` is likewise over distinct undirected neighbors
  - `closeness` is Wasserman-Faust over OUT distances, so the NetworkX call is
    made on the reversed graph, the same adjustment `gen_centrality_fixtures.py`
    documents for harmonic centrality
  - `triangleCount` counts ASSIGNMENTS of the directed pattern
    `(a)->(b)->(c)->(a)` with three pairwise-distinct relationships, so one
    directed 3-cycle contributes three and parallel edges multiply

Usage: gen_function_oracle.py <output.json>
"""
import json
import math
import random
import struct
import sys
from itertools import permutations

import networkx as nx

SEED = 20260802


def undirected_neighbors(n, edges):
    """Distinct neighbors of each node over the undirected view, self excluded."""
    nbrs = [set() for _ in range(n)]
    for s, d in edges:
        if s != d:
            nbrs[s].add(d)
            nbrs[d].add(s)
    return nbrs


def link_scores(n, edges):
    """The five link-prediction metrics for every ordered pair a < b."""
    nbrs = undirected_neighbors(n, edges)
    out = []
    for a in range(n):
        for b in range(a + 1, n):
            shared = nbrs[a] & nbrs[b]
            union = nbrs[a] | nbrs[b]
            common = float(len(shared))
            jaccard = len(shared) / len(union) if union else 0.0
            # `ln(1)` is zero, so a degree-one shared neighbor has no defined
            # weight and contributes nothing.
            adamic = sum(1.0 / math.log(len(nbrs[w])) for w in shared if len(nbrs[w]) > 1)
            resource = sum(1.0 / len(nbrs[w]) for w in shared if nbrs[w])
            preferential = float(len(nbrs[a]) * len(nbrs[b]))
            out.append(
                {
                    "a": a,
                    "b": b,
                    "commonNeighbors": common,
                    "jaccard": jaccard,
                    "adamicAdar": adamic,
                    "resourceAllocation": resource,
                    "preferentialAttachment": preferential,
                }
            )
    return out


def check_link_against_networkx(n, edges, rows):
    """Cross-check the plain-Python link scores against NetworkX.

    NetworkX's link prediction is defined for simple undirected graphs, which is
    exactly the undirected distinct-neighbor view IssunDB documents, so the two
    must agree on every pair. A mismatch means this generator is wrong, and the
    corpus is not written.
    """
    g = nx.Graph()
    g.add_nodes_from(range(n))
    for s, d in edges:
        if s != d:
            g.add_edge(s, d)
    pairs = [(r["a"], r["b"]) for r in rows]
    for name, fn in (
        ("jaccard", nx.jaccard_coefficient),
        ("adamicAdar", nx.adamic_adar_index),
        ("resourceAllocation", nx.resource_allocation_index),
        ("preferentialAttachment", nx.preferential_attachment),
    ):
        reference = {(a, b): v for a, b, v in fn(g, pairs)}
        for row in rows:
            mine = row[name]
            theirs = reference[(row["a"], row["b"])]
            if abs(mine - theirs) > 1e-9:
                raise SystemExit(
                    f"generator disagrees with NetworkX on {name} for "
                    f"({row['a']}, {row['b']}): {mine} vs {theirs}"
                )


def closeness_wf(n, edges):
    """Wasserman-Faust closeness over OUT distances, cross-checked with NetworkX."""
    g = nx.DiGraph()
    g.add_nodes_from(range(n))
    g.add_edges_from((s, d) for s, d in edges if s != d or True)
    # Reversed, so NetworkX's in-distance closeness computes the out-distance
    # convention IssunDB uses.
    reference = nx.closeness_centrality(g.reverse(), wf_improved=True)

    mine = []
    for src in range(n):
        lengths = nx.single_source_shortest_path_length(g, src)
        reachable = [d for t, d in lengths.items() if t != src]
        total = sum(reachable)
        if total == 0 or n <= 1:
            mine.append(0.0)
        else:
            mine.append((len(reachable) / total) * (len(reachable) / (n - 1)))
    for i in range(n):
        if abs(mine[i] - reference[i]) > 1e-9:
            raise SystemExit(
                f"generator disagrees with NetworkX on closeness for node {i}: "
                f"{mine[i]} vs {reference[i]}"
            )
    return mine


def clustering(n, edges):
    """Clustering coefficient over distinct undirected neighbors."""
    nbrs = undirected_neighbors(n, edges)
    g = nx.Graph()
    g.add_nodes_from(range(n))
    for s, d in edges:
        if s != d:
            g.add_edge(s, d)
    reference = nx.clustering(g)

    mine = []
    for v in range(n):
        deg = len(nbrs[v])
        if deg < 2:
            mine.append(0.0)
            continue
        links = sum(1 for x in nbrs[v] for y in nbrs[v] if x < y and y in nbrs[x])
        mine.append(2.0 * links / (deg * (deg - 1)))
    for i in range(n):
        if abs(mine[i] - reference[i]) > 1e-9:
            raise SystemExit(
                f"generator disagrees with NetworkX on clustering for node {i}: "
                f"{mine[i]} vs {reference[i]}"
            )
    return mine


def triangle_assignments(edges):
    """Assignments of `(a)->(b)->(c)->(a)` with three distinct relationships.

    Brute force over ordered triples of edge indices, which is the definition
    itself. NetworkX cannot serve here: it counts undirected triangles per node
    over a simple graph, which is a different quantity on a directed multigraph.
    """
    indexed = list(enumerate(edges))
    total = 0
    for (_, (s1, d1)), (_, (s2, d2)), (_, (s3, d3)) in permutations(indexed, 3):
        if d1 == s2 and d2 == s3 and d3 == s1:
            total += 1
    return total


def as_f32(x):
    """Round a float to single precision.

    The vector distance functions take `&[f32]`, so a literal written in a query
    is rounded to single precision before any arithmetic happens; the arithmetic
    itself then widens back to double. Modelling that here keeps the reference
    exact rather than approximately right: without it `0.9` differs from the
    engine's `0.9` in the eighth decimal, and the test would have to tolerate a
    band wide enough to hide a real defect.
    """
    return struct.unpack("f", struct.pack("f", x))[0]


def value_function_cases():
    """Closed forms for the value functions, which read no graph at all."""
    def jaccard(a, b):
        sa, sb = set(a), set(b)
        return len(sa & sb) / len(sa | sb) if (sa | sb) else 0.0

    def overlap(a, b):
        sa, sb = set(a), set(b)
        smaller = min(len(sa), len(sb))
        return len(sa & sb) / smaller if smaller else 0.0

    def cosine_distance(a, b):
        a = [as_f32(x) for x in a]
        b = [as_f32(y) for y in b]
        dot = sum(x * y for x, y in zip(a, b))
        na = math.sqrt(sum(x * x for x in a))
        nb = math.sqrt(sum(y * y for y in b))
        return 1.0 - dot / (na * nb) if na and nb else 0.0

    def euclidean(a, b):
        a = [as_f32(x) for x in a]
        b = [as_f32(y) for y in b]
        return math.sqrt(sum((x - y) ** 2 for x, y in zip(a, b)))

    int_pairs = [
        ([1, 2, 3], [2, 3, 4]),
        ([1, 2], [3, 4]),
        ([1, 2], [1, 2, 3, 4]),
        ([5], [5]),
        ([1, 2, 3, 4], [4, 3, 2, 1]),
    ]
    vec_pairs = [
        ([1.0, 0.0], [0.0, 1.0]),
        ([1.0, 2.0], [1.0, 2.0]),
        ([1.0, 0.0], [-1.0, 0.0]),
        ([3.0, 4.0], [0.0, 0.0]),
        ([1.0, 0.0, 0.25], [0.0, 1.0, 0.25]),
        ([0.5, 0.5, 0.5], [0.1, 0.9, 0.2]),
    ]
    def nonzero(v):
        return any(x != 0.0 for x in v)

    return {
        "similarity": [
            {"a": a, "b": b, "jaccard": jaccard(a, b), "overlap": overlap(a, b)}
            for a, b in int_pairs
        ],
        # `cosineDefined` marks the pairs where cosine has an answer at all. With a
        # zero vector the similarity divides by a zero norm, and the engine's two
        # cosine entry points take different positions on that: `issundb.distance.
        # cosine` yields null while `vector_dist`, which goes through the vector
        # index's configured metric rather than a fixed one, yields 1.0. Neither is
        # ground truth, so the corpus records the euclidean answer for such a pair
        # and declines to assert a cosine one.
        "distance": [
            {
                "a": a,
                "b": b,
                "cosineDefined": nonzero(a) and nonzero(b),
                "cosine": cosine_distance(a, b) if nonzero(a) and nonzero(b) else None,
                "euclidean": euclidean(a, b),
            }
            for a, b in vec_pairs
        ],
    }


def graph_cases():
    rng = random.Random(SEED)
    cases = []

    # Hand-built shapes first, so the conventions that random graphs reach only
    # by luck are always covered: a self-loop, a parallel edge, an isolated node,
    # and a triangle whose reverse edges are all present.
    handmade = [
        ("empty", 3, []),
        ("self_loop", 3, [(0, 0), (0, 1), (1, 2)]),
        ("parallel", 4, [(0, 1), (0, 1), (1, 2), (2, 0)]),
        ("triangle", 3, [(0, 1), (1, 2), (2, 0)]),
        ("triangle_both_ways", 3, [(0, 1), (1, 2), (2, 0), (1, 0), (2, 1), (0, 2)]),
        ("isolated", 5, [(0, 1), (1, 0)]),
        ("star", 5, [(0, 1), (0, 2), (0, 3), (0, 4)]),
        ("chain", 5, [(0, 1), (1, 2), (2, 3), (3, 4)]),
        ("k4", 4, [(a, b) for a in range(4) for b in range(4) if a != b]),
    ]
    for name, n, edges in handmade:
        cases.append((name, n, list(edges)))

    for i in range(40):
        n = rng.randint(3, 9)
        m = rng.randint(0, 18)
        edges = [(rng.randrange(n), rng.randrange(n)) for _ in range(m)]
        cases.append((f"random_{i}", n, edges))

    out = []
    for name, n, edges in cases:
        rows = link_scores(n, edges)
        check_link_against_networkx(n, edges, rows)
        out.append(
            {
                "id": name,
                "n": n,
                "edges": [[s, d] for s, d in edges],
                "link": rows,
                "closeness": closeness_wf(n, edges),
                "clustering": clustering(n, edges),
                "triangleAssignments": triangle_assignments(edges),
            }
        )
    return out


def main():
    if len(sys.argv) != 2:
        raise SystemExit("usage: gen_function_oracle.py <output.json>")
    corpus = {
        "seed": SEED,
        "graphs": graph_cases(),
        "values": value_function_cases(),
    }
    with open(sys.argv[1], "w") as fh:
        json.dump(corpus, fh, indent=1)
        fh.write("\n")
    print(
        f"wrote {len(corpus['graphs'])} graph cases and "
        f"{len(corpus['values']['similarity']) + len(corpus['values']['distance'])} "
        f"value cases to {sys.argv[1]}"
    )


if __name__ == "__main__":
    main()
