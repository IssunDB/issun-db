#!/usr/bin/env python3
"""Generate a NetworkX PageRank oracle corpus for IssunDB.

PageRank is spec-sensitive: IssunDB and NetworkX agree only where their
conventions coincide. IssunDB runs a fixed number of power iterations with
teleportation and does not redistribute the rank mass of dangling nodes
(out-degree 0); NetworkX redistributes dangling mass uniformly, iterates to a
tolerance, and normalizes the result to sum to 1.

On graphs where every node has out-degree at least 1 there are no dangling
nodes, so no mass leaks: the IssunDB transition matrix is fully column-stochastic,
the rank total stays at 1 every iteration, and with a matched damping factor and
enough iterations IssunDB converges to the same unique stationary distribution
NetworkX computes. This generator therefore emits only no-dangling directed
graphs, which is the subclass where a mismatch is a genuine bug rather than a
convention difference.

Regenerate with `make oracle-fixtures`.
"""

import json
import random
import sys

import networkx as nx

SEED = 0x9A6E_5A11
NUM_GRAPHS = 60
MIN_NODES = 3
MAX_NODES = 12
DENSITIES = [0.1, 0.2, 0.35, 0.5, 0.7]
ALPHA = 0.85
# NetworkX is run to a tight tolerance so the reference is effectively exact; the
# Rust side runs a fixed, comfortably converged iteration count and compares with
# a slack tolerance that absorbs f32 rounding but still catches any formula bug.
NX_TOL = 1e-12
NX_MAX_ITER = 2000


def build_no_dangling_graph(rng, n, density):
    """Simple directed graph in which every node has out-degree at least 1."""
    g = nx.DiGraph()
    g.add_nodes_from(range(n))
    for u in range(n):
        for v in range(n):
            if u != v and rng.random() < density:
                g.add_edge(u, v)
    # Guarantee no dangling node: give every out-degree-0 node one out-edge to a
    # distinct random target.
    for u in range(n):
        if g.out_degree(u) == 0:
            v = rng.randrange(n)
            while v == u:
                v = rng.randrange(n)
            g.add_edge(u, v)
    return g


def main():
    rng = random.Random(SEED)
    cases = []
    for i in range(NUM_GRAPHS):
        n = rng.randint(MIN_NODES, MAX_NODES)
        density = rng.choice(DENSITIES)
        g = build_no_dangling_graph(rng, n, density)
        edges = sorted((int(u), int(v)) for u, v in g.edges())
        ranks = nx.pagerank(g, alpha=ALPHA, tol=NX_TOL, max_iter=NX_MAX_ITER)
        cases.append(
            {
                "id": f"pr{i:04d}",
                "n": n,
                "edges": [[u, v] for u, v in edges],
                "pagerank": [ranks[k] for k in range(n)],
            }
        )

    corpus = {
        "meta": {
            "generator": "scripts/gen_pagerank_fixtures.py",
            "networkx_version": nx.__version__,
            "seed": SEED,
            "num_graphs": NUM_GRAPHS,
            "alpha": ALPHA,
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
