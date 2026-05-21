use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use issundb_core::Graph;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn open_tmp() -> (TempDir, Graph) {
    let dir = TempDir::new().unwrap();
    let g = Graph::open(dir.path(), 1).unwrap();
    (dir, g)
}

// ---------------------------------------------------------------------------
// Node insertion
// ---------------------------------------------------------------------------

fn bench_node_insert(c: &mut Criterion) {
    let mut group = c.benchmark_group("node_insert");

    for count in [1_000u64, 10_000, 100_000] {
        group.throughput(Throughput::Elements(count));
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, &n| {
            b.iter_batched(
                open_tmp,
                |(_dir, g)| {
                    for i in 0..n {
                        g.add_node("Person", &i).unwrap();
                    }
                },
                BatchSize::PerIteration,
            );
        });
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Edge insertion (includes adjacency index writes)
// ---------------------------------------------------------------------------

fn bench_edge_insert(c: &mut Criterion) {
    let mut group = c.benchmark_group("edge_insert");

    for count in [1_000u64, 10_000, 100_000] {
        group.throughput(Throughput::Elements(count));
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, &n| {
            b.iter_batched(
                || {
                    let (dir, g) = open_tmp();
                    let src = g.add_node("N", &0u64).unwrap();
                    let dst = g.add_node("N", &1u64).unwrap();
                    (dir, g, src, dst)
                },
                |(_dir, g, src, dst)| {
                    for i in 0..n {
                        g.add_edge(src, dst, "E", &i).unwrap();
                    }
                },
                BatchSize::PerIteration,
            );
        });
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Adjacency read (out_neighbors)
// ---------------------------------------------------------------------------

fn bench_out_neighbors(c: &mut Criterion) {
    let mut group = c.benchmark_group("out_neighbors");

    for degree in [10u64, 100, 1_000] {
        group.throughput(Throughput::Elements(degree));
        group.bench_with_input(BenchmarkId::from_parameter(degree), &degree, |b, &d| {
            let (_dir, g) = open_tmp();
            let src = g.add_node("N", &0u64).unwrap();
            let dst = g.add_node("N", &1u64).unwrap();
            for i in 0..d {
                g.add_edge(src, dst, "E", &i).unwrap();
            }
            b.iter(|| g.out_neighbors(src).unwrap());
        });
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// BFS: CSR snapshot vs raw LMDB cursors
//
// Nodes inserted before rebuild_csr are in the snapshot (CSR path).
// Nodes inserted after open but before the threshold is crossed use LMDB.
// The setup keeps inserts below REBUILD_THRESHOLD so the snapshot stays empty
// until rebuild_csr is called explicitly.
// ---------------------------------------------------------------------------

fn bench_bfs(c: &mut Criterion) {
    const HOPS: u8 = 4;
    const EDGES_PER_NODE: u64 = 4;

    let mut group = c.benchmark_group("bfs");

    for &n in &[200u64, 500] {
        // Build a random-ish directed graph with n nodes and n*EDGES_PER_NODE edges.
        // n * (1 + EDGES_PER_NODE) writes stays well below REBUILD_THRESHOLD=1000,
        // so the background thread never fires and we control the snapshot state.
        let setup = || -> (TempDir, Graph, u64) {
            let (dir, g) = open_tmp();
            let ids: Vec<u64> = (0..n).map(|i| g.add_node("N", &i).unwrap()).collect();
            for i in 0..n * EDGES_PER_NODE {
                let src = ids[(i % n) as usize];
                let dst = ids[((i * 7 + 3) % n) as usize];
                g.add_edge(src, dst, "E", &i).unwrap();
            }
            let start = ids[0];
            (dir, g, start)
        };

        group.throughput(Throughput::Elements(n));

        group.bench_with_input(BenchmarkId::new("csr", n), &n, |b, _| {
            let (_dir, g, start) = setup();
            g.rebuild_csr().unwrap(); // populate snapshot
            b.iter(|| g.bfs(start, HOPS).unwrap());
        });

        group.bench_with_input(BenchmarkId::new("lmdb", n), &n, |b, _| {
            let (_dir, g, start) = setup();
            // No rebuild: snapshot is empty (open built it before any inserts).
            b.iter(|| g.bfs_lmdb(start, HOPS).unwrap());
        });
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Load test: 1 million nodes, 5 million edges
//
// Run with: cargo bench -p issundb-core --bench storage -- load_test
// Not included in the default benchmark groups to keep `cargo bench` fast.
// ---------------------------------------------------------------------------

fn load_test(c: &mut Criterion) {
    // Run with: ISSUNDB_LOAD_TEST=1 cargo bench -p issundb-core -- load_test
    // Skipped by default so `cargo test --all-targets` is not OOM-killed.
    if std::env::var("ISSUNDB_LOAD_TEST").is_err() {
        return;
    }

    let mut group = c.benchmark_group("load_test");
    group.sample_size(10);

    const NODES: u64 = 1_000_000;
    const EDGES: u64 = 5_000_000;

    group.throughput(Throughput::Elements(NODES + EDGES));
    group.bench_function("1M_nodes_5M_edges", |b| {
        b.iter_batched(
            open_tmp,
            |(_dir, g)| {
                let mut ids = Vec::with_capacity(NODES as usize);
                for i in 0..NODES {
                    ids.push(g.add_node("N", &i).unwrap());
                }
                let n = ids.len() as u64;
                for i in 0..EDGES {
                    let src = ids[(i % n) as usize];
                    let dst = ids[((i * 7 + 13) % n) as usize];
                    g.add_edge(src, dst, "E", &i).unwrap();
                }
            },
            BatchSize::PerIteration,
        );
    });

    group.finish();
}

criterion_group!(
    storage_benches,
    bench_node_insert,
    bench_edge_insert,
    bench_out_neighbors,
    bench_bfs
);
criterion_group!(load_benches, load_test);
criterion_main!(storage_benches, load_benches);
