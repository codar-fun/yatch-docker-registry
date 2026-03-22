//! Yatch — lightweight OCI image registry
//! Standalone mode: Axum + AWS S3 (or Cloudflare R2) + SQLite

mod config;
mod db;
mod routes;
mod state;
mod storage;

use anyhow::Result;
use state::AppState;
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<()> {
    // Load config from environment
    let cfg: config::Config = envy::from_env().map_err(|e| anyhow::anyhow!("Config error: {e}"))?;

    // Init tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| cfg.log_level.clone().into()),
        )
        .init();

    tracing::info!("Starting Yatch registry on {}:{}", cfg.host, cfg.port);

    // Init storage and database
    let s3 = storage::S3Store::new(&cfg).await?;
    let db = db::open(&cfg.db_path).await?;

    let state = AppState {
        s3: Arc::new(s3),
        db,
        config: Arc::new(cfg.clone()),
    };

    let app = routes::router(state);

    let addr = format!("{}:{}", cfg.host, cfg.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("Listening on {}", addr);

    axum::serve(listener, app).await?;
    Ok(())
}
