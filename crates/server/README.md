# yatch-server

Standalone OCI registry binary. Axum HTTP server backed by AWS S3 (or any S3-compatible store) and SQLite.

## Run

```bash
export S3_BUCKET=my-bucket
export S3_REGION=us-east-1

# Cloudflare R2
export S3_ENDPOINT=https://<account>.r2.cloudflarestorage.com
export S3_PUBLIC_URL=https://registry-assets.example.com  # optional: free blob egress

cargo run -p yatch-server
```

## Configuration

| Variable | Default | Description |
|----------|---------|-------------|
| `S3_BUCKET` | **required** | Bucket name |
| `S3_REGION` | `us-east-1` | AWS region |
| `S3_ENDPOINT` | — | Custom endpoint (R2, MinIO) |
| `S3_PUBLIC_URL` | — | Public base URL for blob redirects |
| `PRESIGN_TTL_SECS` | `3600` | Presigned URL TTL |
| `HOST` | `0.0.0.0` | Bind address |
| `PORT` | `5000` | Bind port |
| `DB_PATH` | `./yatch.db` | SQLite file |
| `AUTH_TOKEN` | — | Static bearer token (unset = open) |
| `LOG_LEVEL` | `info` | Tracing level |

AWS credentials are resolved from the standard chain (`AWS_ACCESS_KEY_ID`, `~/.aws/credentials`, instance role, etc.).
