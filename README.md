# Yatch 🛥️

A minimal, production-ready OCI image registry written in Rust.

- **Storage**: S3 or Cloudflare R2 — blobs are served via direct URL redirect (no egress through Yatch)
- **Metadata**: SQLite (standalone) or Cloudflare D1 (worker)
- **Deploy anywhere**: Cloudflare Worker or standalone binary

## Architecture

```
Client → Yatch (routes + auth)
            │
            ├── manifests (small, served inline)
            │
            └── blobs → 307 redirect → S3 / R2 (direct download, zero egress cost)
```

Blobs are content-addressed (`sha256:<hash>`) and global — cross-repo mounts are free.

## Crates

| Crate | Description |
|-------|-------------|
| `yatch-core` | OCI types, path parsing, digest utils, S3 key naming |
| `yatch-server` | Standalone binary: Axum + AWS S3 + SQLite |
| `yatch-worker` | Cloudflare Worker: R2 + D1 |

## Quickstart: Standalone Server

```bash
# Required
export S3_BUCKET=my-registry-bucket
export S3_REGION=us-east-1

# Optional (Cloudflare R2)
export S3_ENDPOINT=https://<account>.r2.cloudflarestorage.com
export S3_PUBLIC_URL=https://registry-assets.example.com  # public R2 domain

# Optional auth
export AUTH_TOKEN=my-secret-token

# Optional
export PORT=5000
export DB_PATH=./yatch.db

cargo run -p yatch-server
```

### Use with Docker

```bash
# Push
docker tag myimage localhost:5000/myimage:latest
docker push localhost:5000/myimage:latest

# Pull
docker pull localhost:5000/myimage:latest
```

## Quickstart: Cloudflare Worker

```bash
# 1. Create D1 database
wrangler d1 create yatch-meta

# 2. Apply schema (update database_id in wrangler.toml first)
wrangler d1 execute yatch-meta --file=crates/worker/migrations/0001_init.sql

# 3. Create R2 bucket
wrangler r2 bucket create yatch-blobs

# 4. Set auth token (optional)
wrangler secret put AUTH_TOKEN

# 5. Deploy
cd crates/worker
wrangler deploy

# 6. Test
curl https://yatch.<account>.workers.dev/v2/
```

## OCI Distribution API

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/v2/` | API version check |
| GET | `/v2/_catalog` | List repositories |
| GET/HEAD | `/v2/<name>/manifests/<ref>` | Get/check manifest |
| PUT | `/v2/<name>/manifests/<ref>` | Push manifest |
| DELETE | `/v2/<name>/manifests/<ref>` | Delete manifest |
| GET/HEAD | `/v2/<name>/blobs/<digest>` | **307 → S3/R2** (free egress) |
| POST | `/v2/<name>/blobs/uploads/` | Initiate blob upload |
| PATCH | `/v2/<name>/blobs/uploads/<uuid>` | Upload chunk |
| PUT | `/v2/<name>/blobs/uploads/<uuid>` | Complete upload |
| DELETE | `/v2/<name>/blobs/uploads/<uuid>` | Cancel upload |
| GET | `/v2/<name>/tags/list` | List tags |

## S3 / R2 Key Layout

```
blobs/sha256:<hash>          # content-addressed, shared across repos
manifests/<repo>/<digest>    # per-repo manifest content
uploads/<uuid>               # temporary, deleted on completion
```

## Configuration (Standalone)

| Env Var | Default | Description |
|---------|---------|-------------|
| `S3_BUCKET` | **required** | S3/R2 bucket name |
| `S3_REGION` | `us-east-1` | AWS region |
| `S3_ENDPOINT` | — | Custom endpoint (R2, MinIO) |
| `S3_PUBLIC_URL` | — | Public base URL for blob redirects |
| `PRESIGN_TTL_SECS` | `3600` | Presigned URL expiry |
| `HOST` | `0.0.0.0` | Bind address |
| `PORT` | `5000` | Listen port |
| `DB_PATH` | `./yatch.db` | SQLite file path |
| `AUTH_TOKEN` | — | Static Bearer token |
| `LOG_LEVEL` | `info` | Tracing log level |

## Configuration (Worker — `wrangler.toml`)

| Binding / Var | Description |
|---------------|-------------|
| `BUCKET` (R2) | Blob storage bucket |
| `DB` (D1) | Metadata database |
| `R2_PUBLIC_URL` (var) | Optional public domain for blob redirects |
| `AUTH_TOKEN` (secret) | Optional static auth token |

## Building

```bash
# Standalone
cargo build -p yatch-server --release

# Worker (requires wrangler + worker-build)
cd crates/worker
wrangler deploy
```

## Cost Model

With `S3_PUBLIC_URL` / `R2_PUBLIC_URL` set:
- **Reads**: free via public R2 domain or direct S3 URL
- **Writes**: S3/R2 PUT cost only
- **Worker/Server**: only processes manifests and metadata (tiny payloads)

Without public URL:
- Standalone: presigned S3 URLs (307 redirect, no server egress)
- Worker: blob data streams through the worker (CF egress applies)
