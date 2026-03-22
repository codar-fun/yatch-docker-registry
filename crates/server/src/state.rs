//! Shared application state injected into every Axum handler.

use sqlx::SqlitePool;
use crate::{config::Config, storage::S3Store};

#[derive(Clone)]
pub struct AppState {
    pub s3: std::sync::Arc<S3Store>,
    pub db: SqlitePool,
    pub config: std::sync::Arc<Config>,
}
