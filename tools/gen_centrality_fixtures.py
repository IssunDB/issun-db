#!/usr/bin/env python3
"""Generate a NetworkX centrality oracle corpus for IssunDB.

Betweenness and harmonic centrality are spec-sensitive; this generator pins each
to the convention IssunDB actually implements, verified against the source:

  - Betweenness: IssunDB runs directed Brandes over the out-edge adjacency,
    unnormalized, without endpoints. The matching NetworkX call is therefore
    `betweenness_centrality(G, normalized=False, endpoints=False)` on a DiGraph.

  - Harmonic: IssunDB sums 1 / d(u, v) over nodes v reachable FROM u (the
    out-distance convention). NetworkX's `harmonic_centrality` uses the standard
    in-distance convention (1 / d(v, u), summed over all v), so the two disagree
    on directed graphs. Reversing the graph before calling NetworkX makes it
    compute the out-distance quantity IssunDB reports; this equivalence is
    checked numerically on a small example during development.

The graphs are simple directed graphs (no self-loops, no parallel edges).
Dangling nodes are allowed: both algorithms are well defined on disconnected
graphs (harmonic sums only reachable nodes; Brandes contributes nothing through
unreached nodes).

Regenerate with `make oracle-fixtures`.
"""

import json
import random
import sys

import networkx as nx

SEED = 0xCE07_5A11
NUM_GRAPHS = 80
MIN_NODES = 3
MAX_NODES = 12
DENSITIES = [0.05, 0.15, 0.3, 0.5, 0.7]


def build_graph(rng, n, density):
    """Simple directed graph; dangling nodes are allowed."""
    g = nx.DiGraph()
    g.add_nodes_from(range(n))
    for u in range(n):
        for v in range(n):
            if u != v and rng.random() < density:
                g.add_edge(u, v)
    return g


def main():
    rng = random.Random(SEED)
    cases = []
    for i in range(NUM_GRAPHS):
        n = rng.randint(MIN_NODES, MAX_NODES)
        density = rng.choice(DENSITIES)
        g = build_graph(rng, n, density)
        edges = sorted((int(u), int(v)) for u, v in g.edges())
        betweenness = nx.betweenness_centrality(g, normalized=False, endpoints=False)
        # Reverse so NetworkX's in-distance harmonic computes IssunDB's
        # out-distance quantity (sum of 1/d(u, v) over v reachable from u).
        harmonic = nx.harmonic_centrality(g.reverse())
        cases.append(
            {
                "id": f"c{i:04d}",
                "n": n,
                "edges": [[u, v] for u, v in edges],
                "betweenness": [betweenness[k] for k in range(n)],
                "harmonic": [harmonic[k] for k in range(n)],
            }
        )

    corpus = {
        "meta": {
            "generator": "tools/gen_centrality_fixtures.py",
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
