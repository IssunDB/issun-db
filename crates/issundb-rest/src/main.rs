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

    /// Skip the schema-statistics warm-up at startup. The warm-up is one pass over
    /// the label index and the adjacency, and it is what makes the query optimizer's
    /// expand-ratio estimates and type-inference pruning available: nothing builds
    /// them as a side effect of a query. Skip it when readiness matters more than
    /// plan quality, which on a very large graph it may.
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

/// Build the schema statistics before the listener is bound.
///
/// A server outlives any one query, so the one pass this costs is amortized over
/// everything it will serve, and without it the process spends its whole life on
/// default plan weights: nothing builds the table as a side effect of a query, and a
/// query that would benefit cannot ask for it. Warming here rather than on the first
/// query also means the process is never serving while its planner is uninformed.
///
/// Synchronous on purpose. Nothing else is scheduled on the runtime yet (`Graph::open`
/// just above blocks the same way), and readiness that lags the warm-up is the point:
/// a caller that would rather serve immediately passes `--no-warm-statistics`.
///
/// A failure is logged and ignored, because every reader of the table works without
/// it: the fan-out estimates fall back to the global average, and the schema question
/// falls back to a bounded probe.
fn warm_statistics(graph: &Graph) {
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
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    fmt().with_env_filter(EnvFilter::from_default_env()).init();

    info!(db_path = %args.db_path.display(), "opening graph");
    let graph = Graph::open(&args.db_path, args.map_size_gb)?;
    if !args.no_warm_statistics {
        warm_statistics(&graph);
    }
    let graph = Arc::new(graph);

    let router = routes::build_router(graph);

    // Bind via `(host, port)` so a hostname (for example `localhost`) is
    // resolved through DNS; parsing into a `SocketAddr` first would reject
    // anything that is not a literal IP address.
    let listener = tokio::net::TcpListener::bind((args.host.as_str(), args.port)).await?;
    info!(addr = %listener.local_addr()?, "listening");
    axum::serve(listener, router).await?;

    Ok(())
}
