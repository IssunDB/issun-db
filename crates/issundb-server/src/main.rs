mod routes;

use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use clap::Parser;
use issundb::Graph;
use tracing::info;
use tracing_subscriber::{EnvFilter, fmt};

#[derive(Parser, Debug)]
#[command(name = "issundb-server", about = "HTTP server for IssunDB")]
struct Args {
    /// Path to the LMDB database directory.
    #[arg(long)]
    db_path: PathBuf,

    /// Host address to listen on.
    #[arg(long, default_value = "127.0.0.1")]
    host: String,

    /// TCP port to listen on.
    #[arg(long, default_value_t = 7474)]
    port: u16,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    fmt().with_env_filter(EnvFilter::from_default_env()).init();

    info!(db_path = %args.db_path.display(), "opening graph");
    let graph = Graph::open(&args.db_path, 4)?;
    let graph = Arc::new(graph);

    let router = routes::build_router(graph);

    let addr: SocketAddr = format!("{}:{}", args.host, args.port).parse()?;
    info!(%addr, "listening");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, router).await?;

    Ok(())
}
