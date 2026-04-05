#![allow(dead_code)] // Gateway is WIP — unused code is for upcoming features

use std::sync::Arc;

use axum::routing::{any, get, post};
use axum::Router;
use clap::Parser;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

mod config;
mod enrollment;
mod proxy;
mod state;

use config::GatewayConfig;
use state::GatewayState;

/// hs-gateway — authenticated reverse proxy for home-still cloud access
#[derive(Parser)]
#[command(name = "hs-gateway", version = env!("HS_VERSION"))]
struct Args {
    /// Override listen address (default: from config or 127.0.0.1:7440)
    #[arg(long)]
    listen: Option<String>,

    /// Override gateway URL (for enrollment responses)
    #[arg(long)]
    gateway_url: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(fmt::layer().with_target(false))
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let args = Args::parse();
    let config = GatewayConfig::load()?;
    let secret = config.load_or_create_secret()?;

    let listen = args.listen.unwrap_or_else(|| config.listen.clone());
    let gateway_url = args
        .gateway_url
        .unwrap_or_else(|| format!("http://{listen}"));

    tracing::info!("Starting gateway on {listen}");
    tracing::info!("Routes: {:?}", config.routes.keys().collect::<Vec<_>>());

    let state = Arc::new(GatewayState {
        config,
        secret,
        http: reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()?,
        enrollments: enrollment::new_enrollment_store(),
        gateway_url,
        cf_access_client_id: None,     // TODO: load from config
        cf_access_client_secret: None, // TODO: load from config
    });

    let app = Router::new()
        // Unauthenticated endpoints
        .route("/health", get(handle_health))
        .route("/cloud/enroll", post(enrollment::handle_enroll))
        .route("/cloud/refresh", post(enrollment::handle_refresh))
        // Admin: register enrollment codes (only accessible from localhost)
        .route("/cloud/admin/invite", post(enrollment::handle_admin_invite))
        // Authenticated proxy — catch all remaining paths
        .fallback(any(proxy::proxy_handler))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&listen).await?;
    tracing::info!("Gateway listening on {listen}");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

async fn handle_health() -> &'static str {
    "ok"
}

async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("Failed to install CTRL+C handler");
    tracing::info!("Shutting down gateway");
}
