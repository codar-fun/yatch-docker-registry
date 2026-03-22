# yatch-core

Shared types and utilities for the Yatch OCI registry. Used by both `yatch-server` and `yatch-worker`.

## What's in here

- **OCI path parsing** — extracts repo name, reference, digest, and upload UUID from URL paths (handles multi-segment names like `library/ubuntu`)
- **S3 key naming** — stable `blob_key`, `manifest_key`, `upload_key` functions shared across both deployment targets
- **Digest utilities** — `compute_digest` (SHA-256) and `verify_digest`
- **OCI response types** — `ManifestInfo`, `TagList`, `Catalog`, `OciErrors` with standard error codes

No async, no I/O. Pure Rust, compatible with both native and `wasm32-unknown-unknown` targets.
