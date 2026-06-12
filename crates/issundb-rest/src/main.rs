mod routes;

use std::{path::PathBuf, sync::Arc};

use clap::Parser;
use issundb::Graph;
use tracing::info;
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
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    fmt().with_env_filter(EnvFilter::from_default_env()).init();

    info!(db_path = %args.db_path.display(), "opening graph");
    let graph = Graph::open(&args.db_path, args.map_size_gb)?;
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
