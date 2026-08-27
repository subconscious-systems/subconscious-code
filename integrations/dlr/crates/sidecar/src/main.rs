use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;

use clap::Parser;
use dlr_compress::Compressor;
use dlr_core::ContentStore;
use dlr_receiver::Receiver;
use dlr_sidecar::{router, SidecarState};

#[derive(Debug, Parser)]
#[command(
    name = "dlr-sidecar",
    about = "Stateful DLR ingress and OpenAI-compatible streaming proxy"
)]
struct Args {
    /// Listener address. Non-loopback listeners require an ingress token.
    #[arg(long, env = "DLR_LISTEN", default_value = "127.0.0.1:32180")]
    listen: SocketAddr,

    /// Existing OpenAI-compatible gateway base URL.
    #[arg(long, env = "DLR_UPSTREAM_URL")]
    upstream_url: String,

    /// Durable receiver write-ahead log.
    #[arg(long, env = "DLR_WAL", default_value = "dlr-receiver.wal")]
    wal: PathBuf,

    /// Environment variable containing the sidecar ingress token. The token is
    /// never accepted directly as a CLI argument, keeping it out of process listings.
    #[arg(long, default_value = "DLR_INGRESS_TOKEN")]
    ingress_token_env: String,

    /// fsync every accepted append before returning its ACK. Disable only when
    /// the surrounding durable volume explicitly provides equivalent semantics.
    #[arg(long, env = "DLR_SYNC_WAL", default_value_t = true)]
    sync_wal: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let ingress_token = std::env::var(&args.ingress_token_env)
        .ok()
        .filter(|value| !value.is_empty());
    if !is_loopback(args.listen.ip()) && ingress_token.is_none() {
        return Err(format!(
            "refusing to bind {} without a token in {}",
            args.listen, args.ingress_token_env
        )
        .into());
    }
    if let Some(parent) = args
        .wal
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    let store = ContentStore::with_wal(&args.wal)?;
    let receiver = Receiver::new(store, Compressor::default());
    let state = SidecarState::new(receiver, args.upstream_url, ingress_token, args.sync_wal)?;
    let listener = tokio::net::TcpListener::bind(args.listen).await?;
    eprintln!(
        "dlr-sidecar listening on {} (protocol v{}, durable WAL {})",
        listener.local_addr()?,
        dlr_sidecar::PROTOCOL_VERSION,
        args.wal.display()
    );
    let server_result = axum::serve(listener, router(state.clone()))
        .with_graceful_shutdown(shutdown_signal())
        .await;
    // Even when per-request fsync is disabled for throughput, a normal
    // SIGTERM/ctrl-c shutdown must leave all accepted records durable.
    state.receiver().store().flush_wal(true)?;
    server_result?;
    Ok(())
}

fn is_loopback(ip: IpAddr) -> bool {
    ip.is_loopback()
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut signal) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            signal.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
