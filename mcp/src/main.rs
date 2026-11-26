//! orvyn-mcp: Orvyn's trading MCP servers on PORT (default 3400).

use std::net::SocketAddr;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let port: u16 = std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(3400);
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
    let endpoints = orvyn_mcp::venues::VENUES.iter().map(|v| format!("/{v}/mcp")).collect::<Vec<_>>().join(", ");
    println!("[mcp] Orvyn trading MCP servers on :{port} ({endpoints})");
    axum::serve(listener, orvyn_mcp::router().into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(async {
            #[cfg(unix)]
            {
                let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM");
                tokio::select! { _ = tokio::signal::ctrl_c() => {} _ = term.recv() => {} }
            }
            #[cfg(not(unix))]
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
