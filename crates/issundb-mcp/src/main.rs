mod server;

use std::{collections::HashSet, path::PathBuf, sync::Arc};

use axum::{
    extract::{Request, State},
    http::{StatusCode, header::HOST},
    middleware::{Next, from_fn_with_state},
    response::{IntoResponse, Response},
};
use clap::{Parser, ValueEnum};
use issundb::Graph;
use rmcp::{
    ServiceExt,
    transport::{
        stdio,
        streamable_http_server::{StreamableHttpService, session::local::LocalSessionManager},
    },
};
use tracing::{info, warn};
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
    /// Path to the LMDB database directory. Falls back to the ISSUNDB_DB_PATH
    /// environment variable when the flag is omitted (the container image sets
    /// it to /data).
    #[arg(long, env = "ISSUNDB_DB_PATH")]
    db_path: PathBuf,

    /// LMDB map size in gibibytes.
    #[arg(long, default_value_t = 4)]
    map_size_gb: usize,

    /// Transport to serve MCP over. Falls back to the ISSUNDB_MCP_TRANSPORT
    /// environment variable when the flag is omitted (the container image sets
    /// it to http).
    #[arg(long, value_enum, env = "ISSUNDB_MCP_TRANSPORT", default_value_t = Transport::Stdio)]
    transport: Transport,

    /// Address to bind when `--transport http` is used. Falls back to the
    /// ISSUNDB_MCP_BIND environment variable when the flag is omitted (the
    /// container image sets it to 0.0.0.0:8000).
    #[arg(long, env = "ISSUNDB_MCP_BIND", default_value = "127.0.0.1:8000")]
    bind: String,

    /// Path the Streamable HTTP endpoint is mounted at when `--transport http` is used.
    #[arg(long, default_value = "/mcp")]
    http_path: String,

    /// Additional Host header values accepted by the Streamable HTTP transport.
    /// The loopback names (`localhost`, `127.0.0.1`, `::1`) and the `--bind`
    /// host are always allowed. Repeat to add the public hostnames a reverse
    /// proxy forwards under.
    #[arg(long = "allowed-host")]
    allowed_hosts: Vec<String>,
}

/// Returns the host portion of an `addr:port` or `[ipv6]:port` value, stripped
/// of any port and surrounding brackets, lowercased for case-insensitive
/// comparison. Returns `None` when no host is present.
fn extract_host(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let host = if let Some(rest) = raw.strip_prefix('[') {
        // Bracketed IPv6 literal: take everything up to the closing bracket.
        rest.split(']').next()?
    } else if let Some((host, _port)) = raw.rsplit_once(':') {
        // `host:port`. A bare IPv6 literal has multiple colons and no brackets,
        // which is not valid in a Host header, so a single rsplit is correct
        // for the `addr:port` shape we accept.
        if host.contains(':') { raw } else { host }
    } else {
        raw
    };
    let host = host.trim();
    if host.is_empty() {
        None
    } else {
        Some(host.to_ascii_lowercase())
    }
}

/// Decides whether a Host header value is on the allowlist. The allowlist is
/// already lowercased. A request with no Host header is rejected.
fn host_allowed(header: Option<&str>, allowed: &HashSet<String>) -> bool {
    match header.and_then(extract_host) {
        Some(host) => allowed.contains(&host),
        None => false,
    }
}

/// Rejects requests whose Host header is not on the allowlist. The rmcp 0.11
/// Streamable HTTP transport does not validate the Host header, so a malicious
/// web page could drive a victim's browser to invoke tools on a loopback or
/// private-network MCP server (DNS rebinding, GHSA-89vp-x53w-74fx). This
/// middleware enforces the loopback-default allowlist that rmcp 1.4.0
/// introduced upstream, which we cannot adopt directly because it raises the
/// MSRV above the workspace pin.
async fn validate_host(
    State(allowed): State<Arc<HashSet<String>>>,
    req: Request,
    next: Next,
) -> Response {
    let header = req.headers().get(HOST).and_then(|v| v.to_str().ok());
    if host_allowed(header, &allowed) {
        next.run(req).await
    } else {
        warn!(host = ?header, "rejecting request with disallowed Host header");
        StatusCode::FORBIDDEN.into_response()
    }
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
            // Host header allowlist for DNS-rebinding defense: loopback names,
            // the bind host, and any operator-supplied proxy hostnames.
            let mut allowed: HashSet<String> = ["localhost", "127.0.0.1", "::1"]
                .iter()
                .map(|h| h.to_string())
                .collect();
            if let Some(host) = extract_host(&args.bind) {
                allowed.insert(host);
            }
            allowed.extend(args.allowed_hosts.iter().filter_map(|h| extract_host(h)));
            let allowed = Arc::new(allowed);

            let router = axum::Router::new()
                .nest_service(&args.http_path, service)
                .layer(from_fn_with_state(allowed.clone(), validate_host));
            let listener = tokio::net::TcpListener::bind(&args.bind).await?;
            info!(
                bind = %args.bind,
                path = %args.http_path,
                allowed_hosts = ?allowed,
                "serving MCP over streamable HTTP"
            );
            axum::serve(listener, router).await?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allowlist(hosts: &[&str]) -> HashSet<String> {
        hosts.iter().map(|h| h.to_string()).collect()
    }

    #[test]
    fn extract_host_strips_port() {
        assert_eq!(extract_host("127.0.0.1:8000").as_deref(), Some("127.0.0.1"));
        assert_eq!(extract_host("localhost:8000").as_deref(), Some("localhost"));
        assert_eq!(extract_host("example.com").as_deref(), Some("example.com"));
    }

    #[test]
    fn extract_host_handles_bracketed_ipv6() {
        assert_eq!(extract_host("[::1]:8000").as_deref(), Some("::1"));
        assert_eq!(extract_host("[::1]").as_deref(), Some("::1"));
    }

    #[test]
    fn extract_host_lowercases() {
        assert_eq!(extract_host("LOCALHOST:8000").as_deref(), Some("localhost"));
    }

    #[test]
    fn extract_host_rejects_empty() {
        assert_eq!(extract_host(""), None);
        assert_eq!(extract_host("   "), None);
    }

    #[test]
    fn host_allowed_accepts_loopback() {
        let allowed = allowlist(&["localhost", "127.0.0.1", "::1"]);
        assert!(host_allowed(Some("127.0.0.1:8000"), &allowed));
        assert!(host_allowed(Some("localhost"), &allowed));
        assert!(host_allowed(Some("[::1]:8000"), &allowed));
    }

    #[test]
    fn host_allowed_rejects_rebinding_host() {
        // A DNS-rebinding attack arrives with an attacker-controlled Host that
        // resolves to a loopback address but is not on the allowlist.
        let allowed = allowlist(&["localhost", "127.0.0.1", "::1"]);
        assert!(!host_allowed(Some("attacker.example.com"), &allowed));
        assert!(!host_allowed(Some("evil.test:8000"), &allowed));
    }

    #[test]
    fn host_allowed_rejects_missing_header() {
        let allowed = allowlist(&["localhost"]);
        assert!(!host_allowed(None, &allowed));
    }

    #[test]
    fn host_allowed_honors_operator_hostnames() {
        // A reverse proxy forwarding under a public name is accepted only when
        // that name is explicitly added to the allowlist.
        let allowed = allowlist(&["127.0.0.1", "mcp.internal"]);
        assert!(host_allowed(Some("mcp.internal"), &allowed));
        assert!(!host_allowed(Some("other.internal"), &allowed));
    }
}
