//! S3 / Cloudflare R2 (S3-compatible) storage operations.

use anyhow::{anyhow, Result};
use aws_sdk_s3::{
    config::{Builder as S3ConfigBuilder, Region},
    presigning::PresigningConfig,
    Client,
};
use bytes::Bytes;
use std::time::Duration;

use crate::config::Config;

pub struct S3Store {
    client: Client,
    bucket: String,
    presign_ttl: Duration,
    public_url: Option<String>,
}

impl S3Store {
    pub async fn new(cfg: &Config) -> Result<Self> {
        let aws_cfg = aws_config::load_from_env().await;

        let mut builder = S3ConfigBuilder::from(&aws_cfg)
            .region(Region::new(cfg.s3_region.clone()))
            .force_path_style(cfg.s3_endpoint.is_some()); // R2/MinIO need path-style

        if let Some(endpoint) = &cfg.s3_endpoint {
            builder = builder.endpoint_url(endpoint.clone());
        }

        let client = Client::from_conf(builder.build());

        Ok(Self {
            client,
            bucket: cfg.s3_bucket.clone(),
            presign_ttl: Duration::from_secs(cfg.presign_ttl_secs),
            public_url: cfg.s3_public_url.clone(),
        })
    }

    // ── Object operations ────────────────────────────────────────────────────

    pub async fn put(&self, key: &str, data: Bytes, content_type: &str) -> Result<()> {
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .content_type(content_type)
            .body(data.into())
            .send()
            .await
            .map_err(|e| anyhow!("S3 put error: {e}"))?;
        Ok(())
    }

    pub async fn get(&self, key: &str) -> Result<Option<(Bytes, String)>> {
        let res = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await;

        match res {
            Ok(out) => {
                let content_type = out
                    .content_type()
                    .unwrap_or("application/octet-stream")
                    .to_string();
                let body = out.body.collect().await.map(|b| b.into_bytes())?;
                Ok(Some((body, content_type)))
            }
            Err(e) => {
                let svc_err = e.into_service_error();
                if svc_err.is_no_such_key() {
                    Ok(None)
                } else {
                    Err(anyhow!("S3 get error: {svc_err}"))
                }
            }
        }
    }

    pub async fn head(&self, key: &str) -> Result<Option<i64>> {
        let res = self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await;

        match res {
            Ok(out) => Ok(Some(out.content_length().unwrap_or(0))),
            Err(e) => {
                let svc_err = e.into_service_error();
                if svc_err.is_not_found() {
                    Ok(None)
                } else {
                    Err(anyhow!("S3 head error: {svc_err}"))
                }
            }
        }
    }

    pub async fn delete(&self, key: &str) -> Result<()> {
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| anyhow!("S3 delete error: {e}"))?;
        Ok(())
    }

    /// Returns a URL for the blob: presigned URL or public URL redirect.
    /// Clients download directly from S3/R2 — no egress through Yatch.
    pub async fn blob_url(&self, key: &str) -> Result<String> {
        if let Some(base) = &self.public_url {
            return Ok(format!("{}/{}", base.trim_end_matches('/'), key));
        }

        let presign_cfg = PresigningConfig::expires_in(self.presign_ttl)
            .map_err(|e| anyhow!("Presign config error: {e}"))?;

        let req = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .presigned(presign_cfg)
            .await
            .map_err(|e| anyhow!("Presign error: {e}"))?;

        Ok(req.uri().to_string())
    }

    /// Copy object within the same bucket (finalize upload: uploads/uuid → blobs/digest).
    pub async fn copy(&self, src_key: &str, dst_key: &str) -> Result<()> {
        let copy_src = format!("{}/{}", self.bucket, src_key);
        self.client
            .copy_object()
            .bucket(&self.bucket)
            .copy_source(&copy_src)
            .key(dst_key)
            .send()
            .await
            .map_err(|e| anyhow!("S3 copy error: {e}"))?;
        Ok(())
    }
}
