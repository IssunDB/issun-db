#![allow(clippy::unwrap_used, clippy::expect_used)]

use issundb_core::{EdgeId, Graph, NodeId};
use tempfile::TempDir;

/// Open a fresh Graph in a temporary directory. The TempDir must be kept alive
/// for the lifetime of the Graph (dropping it removes the LMDB files).
pub fn open_tmp() -> (TempDir, Graph) {
    let dir = TempDir::new().expect("tempdir");
    let g = Graph::open(dir.path(), 1).expect("open graph");
    (dir, g)
}

/// Open a fresh Graph in `dir` (caller controls the lifetime).
pub fn open_at(dir: &std::path::Path) -> Graph {
    Graph::open(dir, 1).expect("open graph")
}

/// Build a linear chain: n0 -> n1 -> n2 -> ... -> n(len-1).
/// Returns (nodes, edges) where nodes[i] -> nodes[i+1] via edges[i].
pub fn chain(g: &Graph, label: &str, etype: &str, len: usize) -> (Vec<NodeId>, Vec<EdgeId>) {
    assert!(len >= 1, "chain length must be at least 1");
    let props = serde_json::json!({});
    let nodes: Vec<NodeId> = (0..len)
        .map(|_| g.add_node(label, &props).expect("add_node"))
        .collect();
    let edges: Vec<EdgeId> = nodes
        .windows(2)
        .map(|w| g.add_edge(w[0], w[1], etype, &props).expect("add_edge"))
        .collect();
    (nodes, edges)
}

/// Build a complete directed clique of `n` nodes (every ordered pair has an edge).
/// Returns the node IDs.
pub fn clique(g: &Graph, label: &str, etype: &str, n: usize) -> Vec<NodeId> {
    assert!(n >= 1, "clique size must be at least 1");
    let props = serde_json::json!({});
    let nodes: Vec<NodeId> = (0..n)
        .map(|_| g.add_node(label, &props).expect("add_node"))
        .collect();
    for i in 0..n {
        for j in 0..n {
            if i != j {
                g.add_edge(nodes[i], nodes[j], etype, &props)
                    .expect("add_edge");
            }
        }
    }
    nodes
}

/// Build a diamond graph: source -> left, source -> right, left -> sink, right -> sink.
/// Returns (source, left, right, sink).
pub fn diamond(g: &Graph, label: &str, etype: &str) -> (NodeId, NodeId, NodeId, NodeId) {
    let props = serde_json::json!({});
    let source = g.add_node(label, &props).expect("add_node source");
    let left = g.add_node(label, &props).expect("add_node left");
    let right = g.add_node(label, &props).expect("add_node right");
    let sink = g.add_node(label, &props).expect("add_node sink");
    g.add_edge(source, left, etype, &props)
        .expect("add_edge source->left");
    g.add_edge(source, right, etype, &props)
        .expect("add_edge source->right");
    g.add_edge(left, sink, etype, &props)
        .expect("add_edge left->sink");
    g.add_edge(right, sink, etype, &props)
        .expect("add_edge right->sink");
    (source, left, right, sink)
}

/// Build two disconnected triangles. Returns (component_a_nodes, component_b_nodes).
pub fn two_triangles(g: &Graph, label: &str, etype: &str) -> (Vec<NodeId>, Vec<NodeId>) {
    let props = serde_json::json!({});
    let mk_triangle = |g: &Graph| -> Vec<NodeId> {
        let a = g.add_node(label, &props).expect("add_node");
        let b = g.add_node(label, &props).expect("add_node");
        let c = g.add_node(label, &props).expect("add_node");
        g.add_edge(a, b, etype, &props).expect("add_edge a->b");
        g.add_edge(b, c, etype, &props).expect("add_edge b->c");
        g.add_edge(c, a, etype, &props).expect("add_edge c->a");
        vec![a, b, c]
    };
    let comp_a = mk_triangle(g);
    let comp_b = mk_triangle(g);
    (comp_a, comp_b)
}

/// Assert two node ID slices contain the same elements (order-independent).
pub fn assert_nodes_eq(label: &str, mut got: Vec<NodeId>, mut want: Vec<NodeId>) {
    got.sort_unstable();
    want.sort_unstable();
    assert_eq!(got, want, "{label}");
}

/// Assert that `path` starts with `src`, ends with `dst`, and all consecutive
/// pairs are connected by an outgoing edge in `g`.
pub fn assert_valid_path(g: &Graph, path: &[NodeId], src: NodeId, dst: NodeId) {
    assert!(!path.is_empty(), "path must not be empty");
    assert_eq!(path[0], src, "path must start at src");
    assert_eq!(*path.last().unwrap(), dst, "path must end at dst");
    for window in path.windows(2) {
        let (a, b) = (window[0], window[1]);
        let neighbors = g.out_neighbors(a).expect("out_neighbors");
        assert!(
            neighbors.iter().any(|ne| ne.node == b),
            "no edge from {a} to {b} in path"
        );
    }
}
