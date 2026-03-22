//! Configuration loaded from environment variables.

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// Host to listen on (default: 0.0.0.0)
    #[serde(default = "default_host")]
    pub host: String,

    /// Port to listen on (default: 5000)
    #[serde(default = "default_port")]
    pub port: u16,

    /// S3 bucket name (required)
    pub s3_bucket: String,

    /// S3 region (default: us-east-1)
    #[serde(default = "default_region")]
    pub s3_region: String,

    /// Custom S3 endpoint URL — use this for Cloudflare R2 or MinIO.
    /// E.g.: https://<account>.r2.cloudflarestorage.com
    pub s3_endpoint: Option<String>,

    /// Optional public base URL for R2/S3 bucket (enables direct URL redirects).
    /// E.g.: https://registry-assets.example.com
    /// When set, blob GET requests redirect here instead of using presigned URLs.
    pub s3_public_url: Option<String>,

    /// Presigned URL expiry seconds (default: 3600)
    #[serde(default = "default_presign_ttl")]
    pub presign_ttl_secs: u64,

    /// SQLite database path (default: ./yatch.db)
    #[serde(default = "default_db_path")]
    pub db_path: String,

    /// Optional static Bearer token for authentication.
    /// If set, all requests require `Authorization: Bearer <token>`.
    pub auth_token: Option<String>,

    /// Log level (default: info)
    #[serde(default = "default_log_level")]
    pub log_level: String,
}

fn default_host() -> String { "0.0.0.0".into() }
fn default_port() -> u16 { 5000 }
fn default_region() -> String { "us-east-1".into() }
fn default_presign_ttl() -> u64 { 3600 }
fn default_db_path() -> String { "./yatch.db".into() }
fn default_log_level() -> String { "info".into() }
