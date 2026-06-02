#!/usr/bin/env python3
"""Generate a NetworkX path-enumeration oracle corpus for IssunDB.

`all_paths` and `longest_path` enumerate simple paths, whose count grows
combinatorially with graph size and density. This corpus therefore uses small,
sparse directed graphs so the enumeration stays bounded, unlike the general
`networkx_oracle.json` corpus.

  - all simple paths   -> nx.all_simple_paths (directed)
  - longest simple path -> max length over nx.all_simple_paths

Regenerate with `make oracle-fixtures`.
"""

import json
import random
import sys

import networkx as nx

SEED = 0x9A7F_5A11
NUM_GRAPHS = 60
MIN_NODES = 3
MAX_NODES = 7
DENSITIES = [0.2, 0.3, 0.4]


def build_graph(rng, n, density):
    """Small simple directed graph; sparse enough to bound path enumeration."""
    g = nx.DiGraph()
    g.add_nodes_from(range(n))
    for u in range(n):
        for v in range(n):
            if u != v and rng.random() < density:
                g.add_edge(u, v)
    return g


def all_simple(g, n):
    """Reachable ordered (src, dst) pairs as [src, dst, [path, ...]]; each path is
    a node-index list. The path set is sorted for a canonical comparison."""
    out = []
    for s in range(n):
        for t in range(n):
            if s == t:
                continue
            paths = sorted([int(x) for x in p] for p in nx.all_simple_paths(g, s, t))
            if paths:
                out.append([s, t, paths])
    return out


def longest(g, n):
    """Reachable ordered (src, dst) pairs as [src, dst, node_count] where
    node_count is the number of nodes on the longest simple path."""
    out = []
    for s in range(n):
        for t in range(n):
            if s == t:
                continue
            lengths = [len(p) for p in nx.all_simple_paths(g, s, t)]
            if lengths:
                out.append([s, t, max(lengths)])
    return out


def main():
    rng = random.Random(SEED)
    cases = []
    for i in range(NUM_GRAPHS):
        n = rng.randint(MIN_NODES, MAX_NODES)
        density = rng.choice(DENSITIES)
        g = build_graph(rng, n, density)
        edges = sorted((int(u), int(v)) for u, v in g.edges())
        cases.append(
            {
                "id": f"p{i:04d}",
                "n": n,
                "edges": [[u, v] for u, v in edges],
                "all_paths": all_simple(g, n),
                "longest": longest(g, n),
            }
        )

    corpus = {
        "meta": {
            "generator": "scripts/gen_paths_fixtures.py",
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
