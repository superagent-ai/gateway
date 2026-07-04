use clap::Parser;
use gateway::{http, schema};

#[derive(Parser)]
#[command(name = "gateway", version, about = "A tiny Rust gateway for running coding agents across model providers safely")]
struct Args {
    /// Path to the YAML config file
    #[arg(long, default_value = "./gateway.yaml")]
    config: std::path::PathBuf,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "gateway=info".into()),
        )
        .init();

    // Load ./.env.local and ./.env (if present) so provider keys work even
    // when the shell didn't source them. Existing env vars take precedence.
    for path in [".env.local", ".env"] {
        let Ok(content) = std::fs::read_to_string(path) else { continue };
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((k, v)) = line.split_once('=') {
                let (k, v) = (k.trim(), v.trim().trim_matches('"').trim_matches('\''));
                if std::env::var(k).is_err() {
                    std::env::set_var(k, v);
                }
            }
        }
    }

    let args = Args::parse();
    let mut cfg = schema::load(&std::fs::read_to_string(&args.config)?)
        .map_err(|e| format!("invalid config ({}): {e}", args.config.display()))?;
    // Deploy-friendly env overrides: containers/cloud set these instead of
    // editing the config file.
    if let Ok(bind) = std::env::var("GATEWAY_BIND") {
        cfg.server.bind = bind;
    }
    if let Ok(token) = std::env::var("GATEWAY_TOKEN") {
        cfg.auth.tokens.push(token);
    }

    let bind = cfg.server.bind.clone();
    let is_local = bind.starts_with("127.") || bind.starts_with("localhost") || bind.starts_with("[::1]");
    if !is_local && cfg.auth.tokens.is_empty() {
        return Err("refusing to bind to a non-localhost address without auth tokens configured".into());
    }

    let app = http::build_router(cfg);
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    // Node-based clients (Claude Code) resolve "localhost" to ::1 first, so a
    // 127.0.0.1-only bind looks like ConnectionRefused to them. Serve the IPv6
    // loopback too when bound to the IPv4 one.
    if let Some(port) = bind.strip_prefix("127.0.0.1:") {
        if let Ok(l6) = tokio::net::TcpListener::bind(format!("[::1]:{port}")).await {
            let app6 = app.clone();
            tokio::spawn(async move {
                let _ = axum::serve(l6, app6).await;
            });
        }
    }
    tracing::info!(bind, version = env!("CARGO_PKG_VERSION"), "gateway listening");
    axum::serve(listener, app).await?;
    Ok(())
}
