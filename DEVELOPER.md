# Yatch Developer Documentation

## Table of Contents

1. [Project Overview](#project-overview)
2. [Repository Structure](#repository-structure)
3. [Architecture](#architecture)
4. [Crate Reference](#crate-reference)
5. [OCI Distribution API](#oci-distribution-api)
6. [Storage Layer](#storage-layer)
7. [Database Schema](#database-schema)
8. [Upload Flow](#upload-flow)
9. [Authentication](#authentication)
10. [Configuration Reference](#configuration-reference)
11. [Deployment](#deployment)
12. [Local Development](#local-development)
13. [Testing](#testing)
14. [Cost Model](#cost-model)
15. [Extending Yatch](#extending-yatch)

---

## Project Overview

Yatch is a minimal, production-ready OCI image registry written in Rust. It implements the [OCI Distribution Specification](https://github.com/opencontainers/distribution-spec) and is designed to be deployed either as:

- A **standalone binary** backed by AWS S3 (or any S3-compatible store such as Cloudflare R2, MinIO) and a local SQLite database.
- A **Cloudflare Worker** backed by Cloudflare R2 (object storage) and Cloudflare D1 (serverless SQLite).

**Core design goal**: blob data is never routed through the registry process itself. `GET /blobs/<digest>` always returns a `307 Temporary Redirect` to a presigned S3 URL or a public R2/CDN URL. This eliminates egress cost through the registry server.

---

## Repository Structure

```
yatch/
├── Cargo.toml                   # Workspace root
├── README.md                    # Quick-start guide
├── DEVELOPER.md                 # This file
└── crates/
    ├── core/                    # yatch-core: shared types and utilities
    │   └── src/lib.rs
    ├── server/                  # yatch-server: standalone Axum binary
    │   └── src/
    │       ├── main.rs          # Entry point, server setup
    │       ├── config.rs        # Environment-based configuration
    │       ├── state.rs         # Shared AppState (S3 + SQLite + Config)
    │       ├── routes.rs        # All OCI route handlers
    │       ├── storage.rs       # S3/R2 storage abstraction
    │       └── db.rs            # SQLite metadata operations
    └── worker/                  # yatch-worker: Cloudflare Worker
        ├── Cargo.toml
        ├── wrangler.toml        # CF Worker deployment config
        ├── migrations/
        │   └── 0001_init.sql    # D1 schema
        └── src/lib.rs           # All worker logic (routing + R2 + D1)
```

---

## Architecture

### High-Level Overview

```
┌─────────────────────────────────────────────────────────┐
│                     Docker Client                        │
└──────────┬──────────────────────────┬───────────────────┘
           │ push/pull API             │ blob download
           ▼                          │ (307 redirect)
┌──────────────────────┐              │
│   Yatch Registry     │              ▼
│  (standalone/worker) │    ┌──────────────────────┐
│                      │    │    S3 / R2 Bucket     │
│  • auth check        │    │                       │
│  • manifest I/O      │    │  blobs/sha256:<hash>  │
│  • tag lookup        │    │  manifests/<repo>/... │
│  • upload sessions   │    │  uploads/<uuid>       │
└──────────┬───────────┘    └──────────────────────┘
           │ metadata reads/writes
           ▼
┌──────────────────────┐
│  SQLite / D1         │
│                      │
│  tags                │
│  manifests           │
│  uploads             │
└──────────────────────┘
```

### Request Flow

```
Client                      Yatch                         S3 / R2
  │                           │                              │
  │  GET /v2/name/blobs/sha   │                              │
  │──────────────────────────▶│                              │
  │                           │  HEAD blobs/sha256:...       │
  │                           │─────────────────────────────▶│
  │                           │◀─────────────────────────────│
  │                           │  presign URL / public URL    │
  │  307 Location: https://   │                              │
  │◀──────────────────────────│                              │
  │                           │                              │
  │  GET https://r2.../blobs/sha256:...                      │
  │──────────────────────────────────────────────────────────▶
  │◀──────────────────────────────────────────────────────────
  │  blob bytes                                              │
```

### Two Deployment Modes

| Aspect | Standalone (`yatch-server`) | Cloudflare Worker (`yatch-worker`) |
|--------|-----------------------------|------------------------------------|
| Runtime | Tokio async, native binary | V8 isolate, WASM |
| HTTP framework | Axum 0.8 | workers-rs 0.7 |
| Blob storage | AWS S3 (via `aws-sdk-s3`) | Cloudflare R2 (via `worker::Bucket`) |
| Metadata store | SQLite (via `sqlx`) | Cloudflare D1 (via `worker::D1Database`) |
| Cold start | None | ~5ms (WASM) |
| Presigned URLs | `aws-sdk-s3` presigning | Public R2 domain redirect |

---

## Crate Reference

### `yatch-core`

Pure synchronous crate with no async dependencies. Safe to use in both native and WASM targets.

#### Exports

```rust
// OCI path parsing
pub enum OciPath { Manifests, Blob, BlobUploadStart, BlobUpload, TagsList }
pub fn parse_oci_path(path: &str) -> Option<OciPath>

// S3 key naming — stable contract shared by server and worker
pub fn blob_key(digest: &str) -> String        // "blobs/sha256:<hex>"
pub fn manifest_key(repo: &str, digest: &str) -> String  // "manifests/<repo>/sha256:<hex>"
pub fn upload_key(uuid: &str) -> String        // "uploads/<uuid>"

// Digest utilities
pub fn compute_digest(data: &[u8]) -> String   // "sha256:<hex>"
pub fn verify_digest(data: &[u8], expected: &str) -> bool

// OCI response types
pub struct ManifestInfo { repo, digest, content_type, size }
pub struct TagList      { name, tags }
pub struct Catalog      { repositories }
pub struct OciErrors    { errors }
impl OciErrors {
    pub fn new(code, message) -> Self
    pub fn not_found(detail)  -> Self
    pub fn blob_unknown()     -> Self
    pub fn manifest_unknown() -> Self
    pub fn digest_invalid()   -> Self
    pub fn to_json()          -> String
}
```

#### Path Parsing Rules

`parse_oci_path` strips the leading `/v2/` before being called; the caller passes everything after that prefix.

```
"library/ubuntu/manifests/latest"
    → Manifests { name: "library/ubuntu", reference: "latest" }

"myrepo/blobs/sha256:deadbeef"
    → Blob { name: "myrepo", digest: "sha256:deadbeef" }

"myrepo/blobs/uploads/"
    → BlobUploadStart { name: "myrepo" }

"myrepo/blobs/uploads/abc-123-def"
    → BlobUpload { name: "myrepo", uuid: "abc-123-def" }

"library/ubuntu/tags/list"
    → TagsList { name: "library/ubuntu" }
```

Multi-component names (`a/b/c/manifests/tag`) are supported because the parser searches for the separator string (`/manifests/`, `/blobs/`, etc.) and treats everything before the first match as the name.

---

### `yatch-server`

Standalone binary. All configuration is via environment variables (see [Configuration Reference](#configuration-reference)).

#### `AppState`

```rust
#[derive(Clone)]
pub struct AppState {
    pub s3: Arc<S3Store>,
    pub db: SqlitePool,
    pub config: Arc<Config>,
}
```

Cloned cheaply into each Axum handler via `State<AppState>`.

#### `S3Store` (`storage.rs`)

Wraps `aws_sdk_s3::Client`. Configured once at startup from `Config`.

```rust
impl S3Store {
    pub async fn new(cfg: &Config) -> Result<Self>

    pub async fn put(&self, key: &str, data: Bytes, content_type: &str) -> Result<()>
    pub async fn get(&self, key: &str) -> Result<Option<(Bytes, String)>>
    pub async fn head(&self, key: &str) -> Result<Option<i64>>   // size in bytes
    pub async fn delete(&self, key: &str) -> Result<()>

    /// Returns presigned URL or public base URL + key path.
    /// Clients use this URL to download blobs directly from S3/R2.
    pub async fn blob_url(&self, key: &str) -> Result<String>

    pub async fn copy(&self, src_key: &str, dst_key: &str) -> Result<()>
}
```

**Cloudflare R2 compatibility**: Set `S3_ENDPOINT` to your R2 endpoint and `S3_PUBLIC_URL` to your custom domain. The SDK uses path-style addressing automatically when a custom endpoint is set.

#### SQLite DB (`db.rs`)

All functions are async and take `&SqlitePool`. The pool is initialized at startup via `db::open(path)` which also runs schema migrations.

```rust
// Schema init
pub async fn open(path: &str) -> Result<SqlitePool>

// Manifests
pub async fn put_manifest(pool, repo, digest, content_type, size) -> Result<()>
pub async fn get_manifest(pool, repo, reference) -> Result<Option<ManifestRow>>
pub async fn delete_manifest(pool, repo, reference) -> Result<()>

// Tags
pub async fn put_tag(pool, repo, tag, digest) -> Result<()>
pub async fn list_tags(pool, repo) -> Result<Vec<String>>

// Catalog
pub async fn list_repos(pool) -> Result<Vec<String>>

// Uploads
pub async fn create_upload(pool, uuid, repo) -> Result<()>
pub async fn get_upload(pool, uuid) -> Result<Option<UploadRow>>
pub async fn update_upload_offset(pool, uuid, offset) -> Result<()>
pub async fn delete_upload(pool, uuid) -> Result<()>
```

#### Routing (`routes.rs`)

Uses a single Axum catch-all route `/v2/*path` dispatched by `parse_oci_path`. The dispatch function:

1. Checks bearer token auth (if `AUTH_TOKEN` is set)
2. Parses the OCI path
3. Branches on HTTP method
4. Calls the specific async handler

All handlers return `Response` (Axum's type-erased response). OCI error responses follow the standard `{"errors":[{"code":"...","message":"...","detail":null}]}` format.

---

### `yatch-worker`

A single `src/lib.rs` containing all worker logic. Uses workers-rs 0.7 with the `d1` feature.

#### Key API Bindings

| Binding | Access | Type |
|---------|--------|------|
| `BUCKET` (R2) | `env.bucket("BUCKET")?` | `worker::Bucket` |
| `DB` (D1) | `env.d1("DB")?` | `worker::d1::D1Database` |
| `R2_PUBLIC_URL` (var) | `env.var("R2_PUBLIC_URL")?` | `worker::Var` |
| `AUTH_TOKEN` (secret) | `env.var("AUTH_TOKEN")?` | `worker::Var` |

#### R2 Bucket API (workers-rs 0.7)

```rust
let bucket = env.bucket("BUCKET")?;

// Write
bucket.put(key, data /* impl Into<Data> */)
    .http_metadata(HttpMetadata { content_type: Some("...".into()), ..Default::default() })
    .execute().await?;

// Read
let obj: Option<Object> = bucket.get(key).execute().await?;
if let Some(obj) = obj {
    let bytes: Vec<u8> = obj.body().ok_or("no body")?.bytes().await?;
}

// Metadata
let meta: Option<Object> = bucket.head(key).await?;
let size: u64 = meta.unwrap().size();

// Delete
bucket.delete(key).await?;
```

> **Note**: The `Data` enum accepts `Vec<u8>`, `String`, or a `ReadableStream`. `Vec<u8>` is the standard choice for binary blobs.

#### D1 Database API (workers-rs 0.7, `d1` feature)

```rust
let db = env.d1("DB")?;

// Execute with no return
db.prepare("INSERT INTO tags (repo, tag, digest) VALUES (?1, ?2, ?3)")
    .bind(&[JsValue::from_str(&repo), JsValue::from_str(&tag), JsValue::from_str(&digest)])?
    .run().await?;

// Fetch single row
let row: Option<MyRow> = db
    .prepare("SELECT digest FROM tags WHERE repo = ?1 AND tag = ?2")
    .bind(&[JsValue::from_str(&repo), JsValue::from_str(&tag)])?
    .first::<MyRow>(None).await?;

// Fetch many rows
let rows: Vec<MyRow> = db
    .prepare("SELECT tag FROM tags WHERE repo = ?1")
    .bind(&[JsValue::from_str(&repo)])?
    .all().await?
    .results::<MyRow>()?;
```

Bind value helpers used throughout:
```rust
fn js(s: &str) -> JsValue { JsValue::from_str(s) }
fn jsn(n: i64) -> JsValue { JsValue::from_f64(n as f64) }  // JS numbers are f64
```

Row types must derive `serde::Deserialize` with field names matching column names exactly.

---

## OCI Distribution API

All endpoints are under `/v2/`. Repository names (`<name>`) can contain `/` for multi-level namespacing (e.g., `library/ubuntu`).

### Endpoints

| Method | Path | Description | Response |
|--------|------|-------------|----------|
| `GET` | `/v2/` | API version check | `200 {}` |
| `GET` | `/v2/_catalog` | List repositories | `200 {"repositories":[...]}` |
| `GET` | `/v2/<name>/tags/list` | List tags | `200 {"name":"...","tags":[...]}` |
| `HEAD` | `/v2/<name>/manifests/<ref>` | Check manifest existence | `200` with digest header |
| `GET` | `/v2/<name>/manifests/<ref>` | Fetch manifest | `200` manifest body |
| `PUT` | `/v2/<name>/manifests/<ref>` | Push manifest | `201` |
| `DELETE` | `/v2/<name>/manifests/<ref>` | Delete manifest/tag | `202` |
| `HEAD` | `/v2/<name>/blobs/<digest>` | Check blob existence | `200` with size header |
| `GET` | `/v2/<name>/blobs/<digest>` | **→ 307 redirect to S3/R2** | `307` Location header |
| `DELETE` | `/v2/<name>/blobs/<digest>` | Delete blob | `202` |
| `POST` | `/v2/<name>/blobs/uploads/` | Initiate upload | `202` with UUID |
| `GET` | `/v2/<name>/blobs/uploads/<uuid>` | Upload progress | `204` with Range header |
| `PATCH` | `/v2/<name>/blobs/uploads/<uuid>` | Send chunk | `202` with Range header |
| `PUT` | `/v2/<name>/blobs/uploads/<uuid>?digest=` | Complete upload | `201` |
| `DELETE` | `/v2/<name>/blobs/uploads/<uuid>` | Cancel upload | `204` |

### Reference Resolution

A `<ref>` in manifest endpoints is either:
- A **tag** (e.g., `latest`, `v1.2.3`) — resolved via `tags` table to a digest
- A **digest** (e.g., `sha256:abc123`) — looked up directly in `manifests` table

### Cross-Repo Blob Mount

```
POST /v2/<name>/blobs/uploads/?mount=<digest>&from=<src-repo>
```

If the blob already exists in S3/R2 (checked by `HEAD blobs/<digest>`), returns `201 Created` immediately with no data transfer. Since blobs are content-addressed and globally stored, mounting is always free.

### OCI Error Codes

```json
{
  "errors": [
    {
      "code": "BLOB_UNKNOWN",
      "message": "blob unknown to registry",
      "detail": null
    }
  ]
}
```

| Code | HTTP Status | Meaning |
|------|-------------|---------|
| `NAME_UNKNOWN` | 404 | Repository not found |
| `MANIFEST_UNKNOWN` | 404 | Manifest not found |
| `BLOB_UNKNOWN` | 404 | Blob not found |
| `BLOB_UPLOAD_UNKNOWN` | 404 | Upload session not found |
| `DIGEST_INVALID` | 400 | Digest verification failed |
| `UNAUTHORIZED` | 401 | Missing or invalid token |
| `INTERNAL_ERROR` | 500 | Server-side failure |

---

## Storage Layer

### S3 Key Schema

```
blobs/
  sha256:<hex>                 # Content-addressed blobs, shared across all repos
                               # Never deleted by tag operations

manifests/
  <repo>/
    sha256:<hex>               # Manifest content stored per-repo
                               # e.g.: manifests/library/ubuntu/sha256:abc123

uploads/
  <uuid>                       # Temporary. Deleted when upload completes or is cancelled.
```

**Blob deduplication**: Because blobs are keyed by their SHA-256 digest globally, if the same layer is pushed to two different repositories, only one copy exists in S3. Cross-repo mounts are resolved instantly with no data movement.

### Blob Download (Cost Saving)

The registry never serves blob bytes directly. Instead:

1. `HEAD` the blob key in S3/R2 to confirm existence.
2. Generate a redirect URL (one of):
   - **Presigned URL** (standalone): `aws-sdk-s3` presigning, configurable TTL (default 1 hour).
   - **Public base URL** (both modes): if `S3_PUBLIC_URL` / `R2_PUBLIC_URL` is set, the URL is `{base}/{key}`. Suitable for public R2 buckets with a custom domain.
3. Return `307 Temporary Redirect` with `Location` and `Docker-Content-Digest` headers.

The Docker client follows the redirect and downloads directly from S3/R2.

---

## Database Schema

Both the standalone SQLite database and the Cloudflare D1 database use the same schema:

```sql
-- Tag-to-digest mapping
CREATE TABLE IF NOT EXISTS tags (
    repo    TEXT    NOT NULL,
    tag     TEXT    NOT NULL,
    digest  TEXT    NOT NULL,
    created INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    PRIMARY KEY (repo, tag)
);

-- Manifest metadata (content stored in S3/R2)
CREATE TABLE IF NOT EXISTS manifests (
    repo         TEXT    NOT NULL,
    digest       TEXT    NOT NULL,
    content_type TEXT    NOT NULL,
    size         INTEGER NOT NULL,
    created      INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    PRIMARY KEY (repo, digest)
);

-- In-progress blob upload sessions
CREATE TABLE IF NOT EXISTS uploads (
    uuid    TEXT    PRIMARY KEY,
    repo    TEXT    NOT NULL,
    offset  INTEGER NOT NULL DEFAULT 0,
    created INTEGER NOT NULL DEFAULT (strftime('%s','now'))
);
```

**Design notes**:
- `tags` and `manifests` are separate — deleting a tag does not delete the manifest row (another tag may reference the same digest).
- `manifests` stores metadata only; actual content lives in S3/R2.
- `uploads` rows are deleted on successful `PUT` or `DELETE`. Stale rows (abandoned uploads) can be cleaned up by a periodic job checking `created` age.

---

## Upload Flow

### Standard Flow (Docker CLI)

```
Client                                   Yatch                      S3/R2
  │                                        │                          │
  │  POST /v2/<name>/blobs/uploads/        │                          │
  │───────────────────────────────────────▶│                          │
  │                                        │  INSERT INTO uploads     │
  │  202 Accepted                          │  uuid = <new-uuid>       │
  │  Location: /v2/<name>/blobs/uploads/<uuid>                        │
  │◀───────────────────────────────────────│                          │
  │                                        │                          │
  │  PATCH /v2/<name>/blobs/uploads/<uuid> │                          │
  │  Content-Type: application/octet-stream│                          │
  │  [blob bytes]                          │                          │
  │───────────────────────────────────────▶│                          │
  │                                        │  PUT uploads/<uuid>      │
  │                                        │─────────────────────────▶│
  │  202 Accepted                          │                          │
  │  Range: 0-{size-1}                     │                          │
  │◀───────────────────────────────────────│  UPDATE uploads offset   │
  │                                        │                          │
  │  PUT /v2/<name>/blobs/uploads/<uuid>   │                          │
  │  ?digest=sha256:<hash>                 │                          │
  │───────────────────────────────────────▶│                          │
  │                                        │  GET uploads/<uuid>      │
  │                                        │  verify SHA-256          │
  │                                        │  PUT blobs/sha256:<hash> │
  │                                        │─────────────────────────▶│
  │                                        │  DELETE uploads/<uuid>   │
  │                                        │─────────────────────────▶│
  │  201 Created                           │  DELETE FROM uploads     │
  │  Docker-Content-Digest: sha256:<hash>  │                          │
  │◀───────────────────────────────────────│                          │
```

### Monolithic Upload (PUT only)

Some clients skip PATCH and put all blob data in the final PUT body. Yatch handles this: if the PUT body is non-empty, it is used directly (no S3 fetch needed).

### Digest Verification

On `PUT` (complete upload), the received blob bytes are passed to `verify_digest`:

```rust
pub fn verify_digest(data: &[u8], expected: &str) -> bool {
    let actual = format!("sha256:{}", hex::encode(Sha256::digest(data)));
    actual == expected
}
```

If verification fails, `400 DIGEST_INVALID` is returned and the blob is not written to S3.

---

## Authentication

Yatch supports a single, simple auth mechanism: **static Bearer token**.

Set the `AUTH_TOKEN` environment variable (standalone) or the `AUTH_TOKEN` secret (worker). All requests must include:

```
Authorization: Bearer <token>
```

If the token is absent or incorrect, the response is:

```
HTTP/1.1 401 Unauthorized
WWW-Authenticate: Bearer realm="...",service="registry"
Content-Type: application/json

{"errors":[{"code":"UNAUTHORIZED","message":"authentication required","detail":null}]}
```

**To disable auth**: leave `AUTH_TOKEN` unset. All requests are then accepted without credentials.

### Docker Login

```bash
docker login localhost:5000 -u any -p <AUTH_TOKEN>
```

Docker will send the token as a Bearer token on subsequent push/pull operations.

---

## Configuration Reference

### Standalone (`yatch-server`) — Environment Variables

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `S3_BUCKET` | **yes** | — | S3 or R2 bucket name |
| `S3_REGION` | no | `us-east-1` | AWS region |
| `S3_ENDPOINT` | no | — | Custom endpoint URL (R2, MinIO, etc.) |
| `S3_PUBLIC_URL` | no | — | Public base URL for direct blob downloads |
| `PRESIGN_TTL_SECS` | no | `3600` | Presigned URL lifetime in seconds |
| `HOST` | no | `0.0.0.0` | Bind address |
| `PORT` | no | `5000` | Bind port |
| `DB_PATH` | no | `./yatch.db` | SQLite file path |
| `AUTH_TOKEN` | no | — | Static bearer token (unset = no auth) |
| `LOG_LEVEL` | no | `info` | Tracing level (`trace`, `debug`, `info`, `warn`, `error`) |

AWS credentials are read from the standard chain: `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` environment variables, `~/.aws/credentials`, instance role, etc.

### Cloudflare Worker (`wrangler.toml`)

| Binding | Kind | Description |
|---------|------|-------------|
| `BUCKET` | R2 bucket | Blob + manifest + temp upload storage |
| `DB` | D1 database | Metadata (tags, manifests, uploads) |
| `R2_PUBLIC_URL` | Var | Optional public CDN base URL for blob redirects |
| `AUTH_TOKEN` | Secret | Optional static bearer token |

---

## Deployment

### Standalone Binary

```bash
# Build
cargo build -p yatch-server --release
./target/release/yatch

# With Cloudflare R2 as S3-compatible backend
export AWS_ACCESS_KEY_ID=<r2-access-key>
export AWS_SECRET_ACCESS_KEY=<r2-secret-key>
export S3_BUCKET=yatch-blobs
export S3_ENDPOINT=https://<account-id>.r2.cloudflarestorage.com
export S3_PUBLIC_URL=https://registry-assets.example.com  # optional
export S3_REGION=auto
export DB_PATH=/data/yatch.db
./target/release/yatch
```

#### Docker Image

```dockerfile
FROM scratch
COPY target/x86_64-unknown-linux-musl/release/yatch /yatch
ENTRYPOINT ["/yatch"]
```

Build a static binary:
```bash
cargo build -p yatch-server --release --target x86_64-unknown-linux-musl
```

### Cloudflare Worker

```bash
# 1. Create D1 database
wrangler d1 create yatch-meta
# Copy the database_id from output into wrangler.toml

# 2. Apply schema
wrangler d1 execute yatch-meta \
  --file=crates/worker/migrations/0001_init.sql

# 3. Create R2 bucket
wrangler r2 bucket create yatch-blobs

# 4. (Optional) Set auth token as a secret
wrangler secret put AUTH_TOKEN

# 5. (Optional) Set public R2 domain in wrangler.toml vars
# [vars]
# R2_PUBLIC_URL = "https://registry-assets.example.com"

# 6. Deploy
cd crates/worker
wrangler deploy

# 7. Test
curl https://yatch.<account>.workers.dev/v2/
```

#### Custom Domain

```bash
wrangler deploy --route "registry.example.com/*"
```

Then configure your Docker client:
```bash
docker push registry.example.com/myimage:latest
```

---

## Local Development

### Prerequisites

- Rust (stable, 1.75+)
- `wasm32-unknown-unknown` target: `rustup target add wasm32-unknown-unknown`
- AWS CLI or [MinIO](https://min.io/) for local S3

### Run Standalone with MinIO

```bash
# Start MinIO
docker run -p 9000:9000 -p 9001:9001 \
  -e MINIO_ROOT_USER=minio \
  -e MINIO_ROOT_PASSWORD=minio123 \
  minio/minio server /data --console-address ":9001"

# Create bucket via MinIO console at http://localhost:9001
# Or: mc alias set local http://localhost:9000 minio minio123
#     mc mb local/yatch-dev

export AWS_ACCESS_KEY_ID=minio
export AWS_SECRET_ACCESS_KEY=minio123
export S3_BUCKET=yatch-dev
export S3_ENDPOINT=http://localhost:9000
export S3_REGION=us-east-1

cargo run -p yatch-server
```

### Run Worker Locally

```bash
cd crates/worker
wrangler dev
```

This runs the worker locally with simulated D1 and R2 bindings using Miniflare.

### Push a Test Image

```bash
# Configure Docker to use the local registry (insecure)
# Add to /etc/docker/daemon.json:
# { "insecure-registries": ["localhost:5000"] }

docker pull alpine:latest
docker tag alpine:latest localhost:5000/alpine:latest
docker push localhost:5000/alpine:latest
docker pull localhost:5000/alpine:latest
```

---

## Testing

### Unit Tests

```bash
# Core tests (path parsing, digest utilities)
cargo test -p yatch-core

# Run all tests (native targets only — worker requires wasm)
cargo test -p yatch-core -p yatch-server
```

### Check Worker (WASM)

```bash
cargo check -p yatch-worker --target wasm32-unknown-unknown
```

### Integration Testing

Using [`oras`](https://oras.land/) or `crane`:

```bash
# Push and pull OCI artifact
oras push localhost:5000/test:v1 ./file.txt
oras pull localhost:5000/test:v1

# Inspect catalog and tags
curl http://localhost:5000/v2/_catalog
curl http://localhost:5000/v2/test/tags/list

# OCI conformance testing (requires go)
# https://github.com/opencontainers/distribution-spec/tree/main/conformance
```

---

## Cost Model

### Cloudflare Worker + R2

With `R2_PUBLIC_URL` set to a public R2 bucket with a custom domain:

| Operation | Cost |
|-----------|------|
| Blob upload (PATCH/PUT) | R2 Class A write operation |
| Blob download (GET) | Free egress via public R2 URL / CDN |
| Manifest read/write | D1 read/write + small R2 operation |
| Worker invocations | CF Workers free tier (100k req/day) |

Without `R2_PUBLIC_URL`, blob GET responses stream through the Worker, incurring CF Workers egress fees.

### AWS S3 (Standalone)

With `S3_PUBLIC_URL` pointing to a CloudFront distribution in front of S3:

| Operation | Cost |
|-----------|------|
| Blob upload | S3 PUT request + storage |
| Blob download | 307 → CloudFront/S3 presigned URL, no server egress |
| Manifest read | S3 GET request (small) |
| Server egress | Near-zero (only manifests and metadata responses) |

---

## Extending Yatch

### Adding a New Storage Backend

The standalone server uses `S3Store` directly. To add another backend (e.g., Azure Blob Storage), create a new file `crates/server/src/azure_storage.rs` implementing the same async methods:

```rust
pub struct AzureStore { /* ... */ }

impl AzureStore {
    pub async fn put(&self, key: &str, data: Bytes, content_type: &str) -> anyhow::Result<()>
    pub async fn get(&self, key: &str) -> anyhow::Result<Option<(Bytes, String)>>
    pub async fn head(&self, key: &str) -> anyhow::Result<Option<i64>>
    pub async fn delete(&self, key: &str) -> anyhow::Result<()>
    pub async fn blob_url(&self, key: &str) -> anyhow::Result<String>
}
```

Then update `AppState` and `main.rs` to select the backend based on configuration.

### Adding Garbage Collection

Unused blobs accumulate when manifests are deleted. To GC:

1. List all `digest` values from the `manifests` table.
2. List all `blobs/` keys in S3.
3. Delete any `blobs/` key whose digest is not referenced by any manifest.

```sql
-- Find all referenced digests
SELECT DISTINCT digest FROM manifests
```

Run as a separate CLI subcommand or scheduled job.

### Adding Image Signing Verification

To reject unsigned images on push:

1. In `put_manifest` handler, parse the manifest body.
2. Check for a signature annotation or associated signature artifact.
3. Verify the signature against a trusted key.

[Cosign](https://github.com/sigstore/cosign) signatures are stored as separate OCI artifacts in the same registry, so no storage changes are needed.

### Stale Upload Cleanup

Abandoned upload sessions leave rows in `uploads` and objects under `uploads/` in S3. Add a cleanup endpoint or background task:

```sql
-- Find uploads older than 24 hours
SELECT uuid FROM uploads WHERE created < strftime('%s','now') - 86400
```

Then call `storage.delete(upload_key(&uuid))` and `db::delete_upload(pool, &uuid)` for each.
