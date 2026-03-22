# yatch-worker

Yatch as a Cloudflare Worker — OCI registry on the edge backed by R2 (blobs) and D1 (metadata).

## Deploy

```bash
# 1. Create D1 database and copy the database_id into wrangler.toml
wrangler d1 create yatch-meta

# 2. Apply schema
wrangler d1 execute yatch-meta --file=migrations/0001_init.sql

# 3. Create R2 bucket
wrangler r2 bucket create yatch-blobs

# 4. Optional: set auth token
wrangler secret put AUTH_TOKEN

# 5. Deploy
wrangler deploy
```

## Bindings (`wrangler.toml`)

| Binding | Kind | Description |
|---------|------|-------------|
| `BUCKET` | R2 | Blob + manifest storage |
| `DB` | D1 | Tags, manifests, upload sessions |
| `R2_PUBLIC_URL` | Var | Public CDN URL for free blob egress |
| `AUTH_TOKEN` | Secret | Static bearer token (optional) |

## Local dev

```bash
wrangler dev
```
