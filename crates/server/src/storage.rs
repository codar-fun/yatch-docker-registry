//! S3 / Cloudflare R2 (S3-compatible) storage operations.

use anyhow::{anyhow, Result};
use aws_sdk_s3::{
    config::{Builder as S3ConfigBuilder, Region},
    presigning::PresigningConfig,
    types::{CompletedMultipartUpload, CompletedPart},
    Client,
};
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use sha2::{Digest, Sha256};
use std::time::Duration;

use crate::config::Config;

const MULTIPART_PART_SIZE: usize = 8 * 1024 * 1024; // 8 MB

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
                // is_not_found() works with real AWS S3 which returns x-amz-error-code header.
                // S3-compatible stores (rustfs/MinIO) return HTTP 404 with no error header on
                // HEAD requests, so the SDK classifies it as "unhandled". Check raw status too.
                match e {
                    aws_sdk_s3::error::SdkError::ServiceError(se) => {
                        let is_404 = se.raw().status().as_u16() == 404;
                        if is_404 || se.err().is_not_found() {
                            Ok(None)
                        } else {
                            Err(anyhow!("S3 head error: {}", se.err()))
                        }
                    }
                    other => Err(anyhow!("S3 head error: {other}")),
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

    /// Stream-upload large blobs using S3 multipart upload.
    /// Computes SHA-256 incrementally while streaming.
    /// Returns (total_bytes, "sha256:<hex>").
    pub async fn put_multipart<S, E>(&self, key: &str, stream: S) -> Result<(i64, String)>
    where
        S: Stream<Item = Result<Bytes, E>> + Unpin + Send,
        E: std::fmt::Display,
    {
        let mpu = self
            .client
            .create_multipart_upload()
            .bucket(&self.bucket)
            .key(key)
            .content_type("application/octet-stream")
            .send()
            .await
            .map_err(|e| anyhow!("create_multipart_upload: {e}"))?;

        let upload_id = mpu
            .upload_id()
            .ok_or_else(|| anyhow!("missing upload_id in multipart response"))?
            .to_string();

        match self.stream_parts(key, &upload_id, stream).await {
            Ok(result) => Ok(result),
            Err(e) => {
                let _ = self
                    .client
                    .abort_multipart_upload()
                    .bucket(&self.bucket)
                    .key(key)
                    .upload_id(&upload_id)
                    .send()
                    .await;
                Err(e)
            }
        }
    }

    async fn stream_parts<S, E>(
        &self,
        key: &str,
        upload_id: &str,
        mut stream: S,
    ) -> Result<(i64, String)>
    where
        S: Stream<Item = Result<Bytes, E>> + Unpin + Send,
        E: std::fmt::Display,
    {
        let mut hasher = Sha256::new();
        let mut completed_parts: Vec<CompletedPart> = Vec::new();
        let mut buf: Vec<u8> = Vec::with_capacity(MULTIPART_PART_SIZE);
        let mut total_bytes: i64 = 0;
        let mut part_number = 1i32;

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| anyhow!("body stream error: {e}"))?;
            hasher.update(&chunk);
            total_bytes += chunk.len() as i64;
            buf.extend_from_slice(&chunk);

            if buf.len() >= MULTIPART_PART_SIZE {
                let data = Bytes::from(std::mem::replace(
                    &mut buf,
                    Vec::with_capacity(MULTIPART_PART_SIZE),
                ));
                let etag = self.upload_part(key, upload_id, part_number, data).await?;
                completed_parts.push(
                    CompletedPart::builder()
                        .part_number(part_number)
                        .e_tag(etag)
                        .build(),
                );
                part_number += 1;
            }
        }

        // Upload final (or only) part — allowed to be any size
        let etag = self
            .upload_part(key, upload_id, part_number, Bytes::from(buf))
            .await?;
        completed_parts.push(
            CompletedPart::builder()
                .part_number(part_number)
                .e_tag(etag)
                .build(),
        );

        let completed = CompletedMultipartUpload::builder()
            .set_parts(Some(completed_parts))
            .build();

        self.client
            .complete_multipart_upload()
            .bucket(&self.bucket)
            .key(key)
            .upload_id(upload_id)
            .multipart_upload(completed)
            .send()
            .await
            .map_err(|e| anyhow!("complete_multipart_upload: {e}"))?;

        let digest = format!("sha256:{}", hex::encode(hasher.finalize()));
        Ok((total_bytes, digest))
    }

    async fn upload_part(
        &self,
        key: &str,
        upload_id: &str,
        part_number: i32,
        data: Bytes,
    ) -> Result<String> {
        let len = data.len() as i64;
        let out = self
            .client
            .upload_part()
            .bucket(&self.bucket)
            .key(key)
            .upload_id(upload_id)
            .part_number(part_number)
            .content_length(len)
            .body(data.into())
            .send()
            .await
            .map_err(|e| anyhow!("upload_part {part_number}: {e}"))?;
        Ok(out.e_tag().unwrap_or_default().to_string())
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
