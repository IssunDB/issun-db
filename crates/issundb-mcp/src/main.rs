mod server;

use std::{path::PathBuf, sync::Arc};

use clap::{Parser, ValueEnum};
use issundb::Graph;
use rmcp::{
    ServiceExt,
    transport::{
        stdio,
        streamable_http_server::{StreamableHttpService, session::local::LocalSessionManager},
    },
};
use tracing::info;
use tracing_subscriber::{EnvFilter, fmt};

use crate::server::IssunMcp;

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum Transport {
    /// Serve over stdin/stdout. Standard for local clients that launch the
    /// server as a subprocess.
    Stdio,
    /// Serve MCP's Streamable HTTP transport so remote clients can connect.
    Http,
}

#[derive(Parser, Debug)]
#[command(
    name = "issundb-mcp",
    about = "Model Context Protocol server for IssunDB"
)]
struct Args {
    /// Path to the LMDB database directory.
    #[arg(long)]
    db_path: PathBuf,

    /// LMDB map size in gibibytes.
    #[arg(long, default_value_t = 4)]
    map_size_gb: usize,

    /// Transport to serve MCP over.
    #[arg(long, value_enum, default_value_t = Transport::Stdio)]
    transport: Transport,

    /// Address to bind when `--transport http` is used.
    #[arg(long, default_value = "127.0.0.1:8000")]
    bind: String,

    /// Path the Streamable HTTP endpoint is mounted at when `--transport http` is used.
    #[arg(long, default_value = "/mcp")]
    http_path: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    // The stdio transport owns stdout, so all diagnostics go to stderr in
    // either mode for consistency.
    fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    info!(db_path = %args.db_path.display(), "opening graph");
    let graph = Arc::new(Graph::open(&args.db_path, args.map_size_gb)?);

    match args.transport {
        Transport::Stdio => {
            info!("serving MCP over stdio");
            let service = IssunMcp::new(graph).serve(stdio()).await?;
            service.waiting().await?;
        }
        Transport::Http => {
            // Each session gets its own handler cloned over the shared graph.
            let service = StreamableHttpService::new(
                move || Ok(IssunMcp::new(graph.clone())),
                LocalSessionManager::default().into(),
                Default::default(),
            );
            let router = axum::Router::new().nest_service(&args.http_path, service);
            let listener = tokio::net::TcpListener::bind(&args.bind).await?;
            info!(bind = %args.bind, path = %args.http_path, "serving MCP over streamable HTTP");
            axum::serve(listener, router).await?;
        }
    }

    Ok(())
}
