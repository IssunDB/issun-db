mod routes;

use std::{path::PathBuf, sync::Arc};

use clap::Parser;
use issundb::Graph;
use tracing::{info, warn};
use tracing_subscriber::{EnvFilter, fmt};

#[derive(Parser, Debug)]
#[command(name = "issundb-rest", about = "HTTP REST API server for IssunDB")]
struct Args {
    /// Path to the LMDB database directory. Falls back to the ISSUNDB_DB_PATH
    /// environment variable when the flag is omitted (the container image sets
    /// it to /data).
    #[arg(long, env = "ISSUNDB_DB_PATH")]
    db_path: PathBuf,

    /// LMDB map size in gigabytes (defaults to 4).
    #[arg(long, default_value_t = 4)]
    map_size_gb: usize,

    /// Host address to listen on. Falls back to the ISSUNDB_REST_HOST
    /// environment variable when the flag is omitted (the container image sets
    /// it to 0.0.0.0).
    #[arg(long, env = "ISSUNDB_REST_HOST", default_value = "127.0.0.1")]
    host: String,

    /// TCP port to listen on. Falls back to the ISSUNDB_REST_PORT environment
    /// variable when the flag is omitted.
    #[arg(long, env = "ISSUNDB_REST_PORT", default_value_t = 7474)]
    port: u16,

    /// Skip the schema-statistics warm-up. The warm-up is one background pass over
    /// the label index and the adjacency, and it is what makes the query optimizer's
    /// expand-ratio estimates and exact type-inference pruning available: nothing
    /// builds them as a side effect of a query. It does not delay readiness, so skip
    /// it only to avoid the scan itself, which on a large graph is seconds of I/O
    /// this process may never benefit from.
    // `FalseyValueParser` because a bare `#[arg(long, env)]` on a bool parses the
    // environment value with clap's strict bool parser, so the `=1`/`=yes`/`=on` that
    // container env blocks routinely set is a hard startup failure rather than a
    // toggle. Only `0`, `false`, `no`, `off`, and empty read as false here.
    #[arg(
        long,
        env = "ISSUNDB_NO_WARM_STATISTICS",
        value_parser = clap::builder::FalseyValueParser::new(),
    )]
    no_warm_statistics: bool,
}

/// Build the schema statistics in the background, without delaying readiness.
///
/// A server outlives any one query, so the pass is amortized over everything it will
/// serve, and without it the process spends its whole life on default plan weights:
/// nothing builds the table as a side effect of a query, and an HTTP caller has no way
/// to ask for it.
///
/// It runs in the background rather than before the listener binds because the scan
/// costs seconds on a large graph (measured at 3.4 s on a 1 M-node, 13.9 M-edge graph)
/// while the plans it sharpens were worth a few percent on a workload of ordinary
/// aggregations. Delaying readiness by seconds to buy that is the wrong trade, and
/// serving immediately costs only the queries that arrive before the scan lands.
///
/// This is safe because `materialize_edge_statistics` performs its scan *without*
/// holding the statistics lock, installing the finished table at the end. Concurrent
/// requests therefore keep planning throughout, on the bounded probe and the global
/// average fan-out, rather than blocking on a half-built table. Backgrounding this
/// while the build held that lock would have been worse than doing it synchronously.
///
/// It runs on a detached thread rather than the runtime's blocking pool, because
/// dropping the runtime waits for a started blocking task and would put the scan's
/// remaining time back on shutdown. The read transaction it holds pins a snapshot for
/// its duration, so on a write-heavy graph it delays page reuse while it runs.
///
/// A failure is logged and ignored, because every reader of the table works without
/// it: the fan-out estimates fall back to the global average, and the schema question
/// falls back to a bounded probe.
fn spawn_statistics_warm_up(graph: Arc<Graph>) {
    // A detached OS thread rather than `tokio::task::spawn_blocking`. Dropping the
    // runtime waits for any blocking task that has already started, so a session that
    // ends before the scan does would have blocked at exit for the rest of it (measured
    // at 1.5 s for a 1.5 s task, and this scan takes seconds on a large graph), which is
    // exactly the delay backgrounding it was supposed to remove. A detached thread is
    // not joined by the runtime and is abandoned at process exit; the scan installs its
    // table only at the end, so abandoning it leaves the graph as it was, which every
    // reader already tolerates.
    //
    // `issundb-mcp` carries the same function for the same reason. Keep the two bodies
    // identical: the shutdown behavior above is the kind of fix that gets applied to one
    // copy and not the other.
    // `Builder::spawn` rather than `thread::spawn`, which panics when the OS refuses a
    // thread (a low pids cgroup, an exhausted host). Panicking would unwind out of
    // `main` before the server starts serving, over an optimization this very comment
    // says is logged and ignored.
    let spawned = std::thread::Builder::new()
        .name("statistics-warm-up".to_string())
        .spawn(move || {
            let started = std::time::Instant::now();
            match graph.materialize_edge_statistics() {
                Ok(()) => info!(
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "warmed schema statistics"
                ),
                Err(error) => warn!(
                    %error,
                    "could not warm schema statistics; continuing on default plan weights"
                ),
            }
        });
    if let Err(error) = spawned {
        warn!(
            %error,
            "could not spawn the schema statistics warm-up; continuing on default plan weights"
        );
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    fmt().with_env_filter(EnvFilter::from_default_env()).init();

    info!(db_path = %args.db_path.display(), "opening graph");
    let graph = Arc::new(Graph::open(&args.db_path, args.map_size_gb)?);
    if !args.no_warm_statistics {
        spawn_statistics_warm_up(graph.clone());
    }

    let router = routes::build_router(graph);

    // Bind via `(host, port)` so a hostname (for example `localhost`) is
    // resolved through DNS; parsing into a `SocketAddr` first would reject
    // anything that is not a literal IP address.
    let listener = tokio::net::TcpListener::bind((args.host.as_str(), args.port)).await?;
    info!(addr = %listener.local_addr()?, "listening");
    axum::serve(listener, router).await?;

    Ok(())
}
