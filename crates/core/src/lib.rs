//! Yatch core: OCI types, path parsing, digest utils, and S3 key naming.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

// ── OCI Response Types ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestInfo {
    pub repo: String,
    pub digest: String,
    pub content_type: String,
    pub size: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadInfo {
    pub uuid: String,
    pub repo: String,
    pub offset: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TagList {
    pub name: String,
    pub tags: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Catalog {
    pub repositories: Vec<String>,
}

// ── OCI Error Format ────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
pub struct OciError {
    pub code: String,
    pub message: String,
    pub detail: serde_json::Value,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct OciErrors {
    pub errors: Vec<OciError>,
}

impl OciErrors {
    pub fn new(code: &str, message: &str) -> Self {
        Self {
            errors: vec![OciError {
                code: code.to_string(),
                message: message.to_string(),
                detail: serde_json::Value::Null,
            }],
        }
    }

    pub fn not_found(detail: &str) -> Self {
        Self::new("NAME_UNKNOWN", detail)
    }

    pub fn blob_unknown() -> Self {
        Self::new("BLOB_UNKNOWN", "blob unknown to registry")
    }

    pub fn manifest_unknown() -> Self {
        Self::new("MANIFEST_UNKNOWN", "manifest unknown to registry")
    }

    pub fn digest_invalid() -> Self {
        Self::new("DIGEST_INVALID", "provided digest did not match uploaded content")
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

// ── OCI Path Parsing ────────────────────────────────────────────────────────

#[derive(Debug, PartialEq)]
pub enum OciPath {
    /// GET/HEAD/PUT/DELETE /v2/<name>/manifests/<reference>
    Manifests { name: String, reference: String },
    /// GET/HEAD/DELETE /v2/<name>/blobs/<digest>
    Blob { name: String, digest: String },
    /// POST /v2/<name>/blobs/uploads/
    BlobUploadStart { name: String },
    /// GET/PATCH/PUT/DELETE /v2/<name>/blobs/uploads/<uuid>
    BlobUpload { name: String, uuid: String },
    /// GET /v2/<name>/tags/list
    TagsList { name: String },
}

/// Parse the path segment after `/v2/` into an [`OciPath`].
/// Handles multi-component names like `library/ubuntu`.
pub fn parse_oci_path(path: &str) -> Option<OciPath> {
    let path = path.trim_start_matches('/');

    // /manifests/ — must check before /blobs/
    if let Some(pos) = path.find("/manifests/") {
        let name = path[..pos].to_string();
        let reference = path[pos + "/manifests/".len()..].to_string();
        if !name.is_empty() && !reference.is_empty() {
            return Some(OciPath::Manifests { name, reference });
        }
    }

    // /blobs/uploads/<uuid> or /blobs/uploads/  — must check before /blobs/
    if let Some(pos) = path.find("/blobs/uploads") {
        let name = path[..pos].to_string();
        let rest = &path[pos + "/blobs/uploads".len()..];
        if !name.is_empty() {
            let uuid = rest.trim_start_matches('/').to_string();
            if uuid.is_empty() {
                return Some(OciPath::BlobUploadStart { name });
            } else {
                return Some(OciPath::BlobUpload { name, uuid });
            }
        }
    }

    // /blobs/<digest>
    if let Some(pos) = path.find("/blobs/") {
        let name = path[..pos].to_string();
        let digest = path[pos + "/blobs/".len()..].to_string();
        if !name.is_empty() && !digest.is_empty() {
            return Some(OciPath::Blob { name, digest });
        }
    }

    // /tags/list
    if let Some(path) = path.strip_suffix("/tags/list") {
        if !path.is_empty() {
            return Some(OciPath::TagsList { name: path.to_string() });
        }
    }

    None
}

// ── S3 Key Naming ───────────────────────────────────────────────────────────

/// Shared blob storage key — content-addressed, deduped across all repos.
pub fn blob_key(digest: &str) -> String {
    format!("blobs/{}", digest)
}

/// Per-repo manifest key.
pub fn manifest_key(repo: &str, digest: &str) -> String {
    format!("manifests/{}/{}", repo, digest)
}

/// Temporary upload key, deleted after the upload completes.
pub fn upload_key(uuid: &str) -> String {
    format!("uploads/{}", uuid)
}

// ── Digest Utilities ────────────────────────────────────────────────────────

pub fn compute_digest(data: &[u8]) -> String {
    let hash = Sha256::digest(data);
    format!("sha256:{}", hex::encode(hash))
}

pub fn verify_digest(data: &[u8], expected: &str) -> bool {
    compute_digest(data) == expected
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_manifests() {
        assert_eq!(
            parse_oci_path("library/ubuntu/manifests/latest"),
            Some(OciPath::Manifests {
                name: "library/ubuntu".into(),
                reference: "latest".into()
            })
        );
    }

    #[test]
    fn parse_blob() {
        let p = parse_oci_path("myrepo/blobs/sha256:deadbeef").unwrap();
        assert!(matches!(p, OciPath::Blob { .. }));
    }

    #[test]
    fn parse_upload_start() {
        let p = parse_oci_path("myrepo/blobs/uploads/").unwrap();
        assert!(matches!(p, OciPath::BlobUploadStart { .. }));
    }

    #[test]
    fn parse_upload_uuid() {
        let p = parse_oci_path("myrepo/blobs/uploads/abc-123").unwrap();
        assert!(matches!(p, OciPath::BlobUpload { .. }));
    }

    #[test]
    fn parse_tags_list() {
        let p = parse_oci_path("library/ubuntu/tags/list").unwrap();
        assert!(matches!(p, OciPath::TagsList { .. }));
    }

    #[test]
    fn digest_round_trip() {
        let data = b"hello world";
        let d = compute_digest(data);
        assert!(verify_digest(data, &d));
    }
}
