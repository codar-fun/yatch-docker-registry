//! OCI Distribution Spec route handlers.
//!
//! Routing strategy: a single `/v2/*path` catch-all dispatches to named handlers
//! after parsing the multi-segment repository name from the URL path.

use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{header, HeaderMap, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::{any, get},
    Router,
};
use std::collections::HashMap;
use uuid::Uuid;
use yatch_core::*;

use crate::state::AppState;

const API_VERSION_HEADER: &str = "registry/2.0";
const CONTENT_TYPE_JSON: &str = "application/json";

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/v2/", get(api_version_check))
        .route("/v2/_catalog", get(catalog))
        .route("/v2/{*path}", any(dispatch))
        .layer(DefaultBodyLimit::disable())
        .with_state(state)
}

// ── Version Check ────────────────────────────────────────────────────────────

async fn api_version_check() -> impl IntoResponse {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, CONTENT_TYPE_JSON)
        .header("Docker-Distribution-API-Version", API_VERSION_HEADER)
        .body(axum::body::Body::from("{}"))
        .unwrap()
}

// ── Catalog ──────────────────────────────────────────────────────────────────

async fn catalog(State(state): State<AppState>) -> Response {
    match crate::db::list_repos(&state.db).await {
        Ok(repos) => {
            let body = serde_json::json!({ "repositories": repos });
            json_response(StatusCode::OK, &body)
        }
        Err(e) => internal_error(&e.to_string()),
    }
}

// ── Dispatcher ───────────────────────────────────────────────────────────────

async fn dispatch(
    method: Method,
    Path(path): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    State(state): State<AppState>,
    body: Bytes,
) -> Response {
    // Optional bearer auth
    if let Some(token) = &state.config.auth_token {
        if !check_auth(&headers, token) {
            return auth_challenge();
        }
    }

    match parse_oci_path(&path) {
        Some(OciPath::Manifests { name, reference }) => match method {
            Method::GET => get_manifest(state, name, reference).await,
            Method::HEAD => head_manifest(state, name, reference).await,
            Method::PUT => put_manifest(state, name, reference, headers, body).await,
            Method::DELETE => delete_manifest(state, name, reference).await,
            _ => method_not_allowed(),
        },

        Some(OciPath::Blob { name, digest }) => match method {
            Method::GET => get_blob(state, name, digest).await,
            Method::HEAD => head_blob(state, name, digest).await,
            Method::DELETE => delete_blob(state, name, digest).await,
            _ => method_not_allowed(),
        },

        Some(OciPath::BlobUploadStart { name }) => {
            if method == Method::POST {
                initiate_upload(state, name, query).await
            } else {
                method_not_allowed()
            }
        }

        Some(OciPath::BlobUpload { name, uuid }) => match method {
            Method::GET => upload_status(state, name, uuid).await,
            Method::PATCH => patch_upload(state, name, uuid, headers, body).await,
            Method::PUT => complete_upload(state, name, uuid, query, body).await,
            Method::DELETE => cancel_upload(state, name, uuid).await,
            _ => method_not_allowed(),
        },

        Some(OciPath::TagsList { name }) => {
            if method == Method::GET {
                list_tags(state, name).await
            } else {
                method_not_allowed()
            }
        }

        None => not_found("path not found"),
    }
}

// ── Manifest Handlers ────────────────────────────────────────────────────────

async fn get_manifest(state: AppState, repo: String, reference: String) -> Response {
    let meta = match crate::db::get_manifest(&state.db, &repo, &reference).await {
        Ok(Some(m)) => m,
        Ok(None) => return oci_error(StatusCode::NOT_FOUND, OciErrors::manifest_unknown()),
        Err(e) => return internal_error(&e.to_string()),
    };

    let key = manifest_key(&repo, &meta.digest);
    match state.s3.get(&key).await {
        Ok(Some((data, _))) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, &meta.content_type)
            .header("Docker-Content-Digest", &meta.digest)
            .header(header::CONTENT_LENGTH, data.len())
            .header("Docker-Distribution-API-Version", API_VERSION_HEADER)
            .body(axum::body::Body::from(data))
            .unwrap(),
        Ok(None) => oci_error(StatusCode::NOT_FOUND, OciErrors::manifest_unknown()),
        Err(e) => internal_error(&e.to_string()),
    }
}

async fn head_manifest(state: AppState, repo: String, reference: String) -> Response {
    match crate::db::get_manifest(&state.db, &repo, &reference).await {
        Ok(Some(m)) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, &m.content_type)
            .header("Docker-Content-Digest", &m.digest)
            .header(header::CONTENT_LENGTH, m.size)
            .header("Docker-Distribution-API-Version", API_VERSION_HEADER)
            .body(axum::body::Body::empty())
            .unwrap(),
        Ok(None) => oci_error(StatusCode::NOT_FOUND, OciErrors::manifest_unknown()),
        Err(e) => internal_error(&e.to_string()),
    }
}

async fn put_manifest(
    state: AppState,
    repo: String,
    reference: String,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/vnd.docker.distribution.manifest.v2+json")
        .to_string();

    let digest = compute_digest(&body);
    let size = body.len() as i64;
    let key = manifest_key(&repo, &digest);

    if let Err(e) = state.s3.put(&key, body, &content_type).await {
        return internal_error(&e.to_string());
    }

    if let Err(e) = crate::db::put_manifest(&state.db, &repo, &digest, &content_type, size).await {
        return internal_error(&e.to_string());
    }

    // If reference is a tag (not a digest), record the tag → digest mapping
    if !reference.starts_with("sha256:") {
        if let Err(e) = crate::db::put_tag(&state.db, &repo, &reference, &digest).await {
            return internal_error(&e.to_string());
        }
    }

    Response::builder()
        .status(StatusCode::CREATED)
        .header("Docker-Content-Digest", &digest)
        .header("Location", format!("/v2/{}/manifests/{}", repo, digest))
        .header(header::CONTENT_LENGTH, "0")
        .body(axum::body::Body::empty())
        .unwrap()
}

async fn delete_manifest(state: AppState, repo: String, reference: String) -> Response {
    match crate::db::get_manifest(&state.db, &repo, &reference).await {
        Ok(None) => return oci_error(StatusCode::NOT_FOUND, OciErrors::manifest_unknown()),
        Err(e) => return internal_error(&e.to_string()),
        Ok(Some(m)) => {
            let key = manifest_key(&repo, &m.digest);
            let _ = state.s3.delete(&key).await; // best-effort
        }
    }

    if let Err(e) = crate::db::delete_manifest(&state.db, &repo, &reference).await {
        return internal_error(&e.to_string());
    }

    StatusCode::ACCEPTED.into_response()
}

// ── Blob Handlers ────────────────────────────────────────────────────────────

/// Blob GET: returns a 307 redirect to a presigned S3/R2 URL.
/// The client downloads directly from S3 — no blob data passes through Yatch.
async fn get_blob(state: AppState, _repo: String, digest: String) -> Response {
    let key = blob_key(&digest);
    match state.s3.head(&key).await {
        Ok(None) => return oci_error(StatusCode::NOT_FOUND, OciErrors::blob_unknown()),
        Err(e) => return internal_error(&e.to_string()),
        Ok(Some(_)) => {}
    }

    match state.s3.blob_url(&key).await {
        Ok(url) => Response::builder()
            .status(StatusCode::TEMPORARY_REDIRECT)
            .header(header::LOCATION, url)
            .header("Docker-Content-Digest", &digest)
            .body(axum::body::Body::empty())
            .unwrap(),
        Err(e) => internal_error(&e.to_string()),
    }
}

async fn head_blob(state: AppState, _repo: String, digest: String) -> Response {
    let key = blob_key(&digest);
    match state.s3.head(&key).await {
        Ok(Some(size)) => Response::builder()
            .status(StatusCode::OK)
            .header("Docker-Content-Digest", &digest)
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .header(header::CONTENT_LENGTH, size)
            .body(axum::body::Body::empty())
            .unwrap(),
        Ok(None) => oci_error(StatusCode::NOT_FOUND, OciErrors::blob_unknown()),
        Err(e) => internal_error(&e.to_string()),
    }
}

async fn delete_blob(state: AppState, _repo: String, digest: String) -> Response {
    let key = blob_key(&digest);
    match state.s3.head(&key).await {
        Ok(None) => return oci_error(StatusCode::NOT_FOUND, OciErrors::blob_unknown()),
        Err(e) => return internal_error(&e.to_string()),
        _ => {}
    }
    if let Err(e) = state.s3.delete(&key).await {
        return internal_error(&e.to_string());
    }
    StatusCode::ACCEPTED.into_response()
}

// ── Blob Upload Handlers ─────────────────────────────────────────────────────

/// POST /v2/<name>/blobs/uploads/ — initiate a blob upload session.
///
/// Also handles cross-repo blob mount:
///   POST /v2/<name>/blobs/uploads/?mount=<digest>&from=<repo>
async fn initiate_upload(
    state: AppState,
    repo: String,
    query: HashMap<String, String>,
) -> Response {
    // Cross-repo blob mount
    if let Some(digest) = query.get("mount") {
        let key = blob_key(digest);
        if let Ok(Some(_)) = state.s3.head(&key).await {
            // Blob already exists globally — just accept the mount
            return Response::builder()
                .status(StatusCode::CREATED)
                .header("Docker-Content-Digest", digest)
                .header("Location", format!("/v2/{}/blobs/{}", repo, digest))
                .header(header::CONTENT_LENGTH, "0")
                .body(axum::body::Body::empty())
                .unwrap();
        }
        // Fall through to normal upload if blob doesn't exist
    }

    let uuid = Uuid::new_v4().to_string();
    if let Err(e) = crate::db::create_upload(&state.db, &uuid, &repo).await {
        return internal_error(&e.to_string());
    }

    Response::builder()
        .status(StatusCode::ACCEPTED)
        .header("Docker-Upload-UUID", &uuid)
        .header("Location", format!("/v2/{}/blobs/uploads/{}", repo, uuid))
        .header("Range", "0-0")
        .header(header::CONTENT_LENGTH, "0")
        .body(axum::body::Body::empty())
        .unwrap()
}

async fn upload_status(state: AppState, repo: String, uuid: String) -> Response {
    match crate::db::get_upload(&state.db, &uuid).await {
        Ok(Some(u)) => Response::builder()
            .status(StatusCode::NO_CONTENT)
            .header("Docker-Upload-UUID", &uuid)
            .header("Location", format!("/v2/{}/blobs/uploads/{}", repo, uuid))
            .header("Range", format!("0-{}", u.offset.saturating_sub(1)))
            .body(axum::body::Body::empty())
            .unwrap(),
        Ok(None) => not_found("upload not found"),
        Err(e) => internal_error(&e.to_string()),
    }
}

/// PATCH — receive blob data chunk and store to S3 under uploads/<uuid>.
/// Most Docker clients send the entire blob in a single PATCH.
async fn patch_upload(
    state: AppState,
    repo: String,
    uuid: String,
    _headers: HeaderMap,
    body: Bytes,
) -> Response {
    match crate::db::get_upload(&state.db, &uuid).await {
        Ok(None) => return not_found("upload not found"),
        Err(e) => return internal_error(&e.to_string()),
        _ => {}
    }

    let length = body.len() as i64;
    let key = upload_key(&uuid);

    if let Err(e) = state.s3.put(&key, body, "application/octet-stream").await {
        return internal_error(&e.to_string());
    }

    if let Err(e) = crate::db::update_upload_offset(&state.db, &uuid, length).await {
        return internal_error(&e.to_string());
    }

    Response::builder()
        .status(StatusCode::ACCEPTED)
        .header("Docker-Upload-UUID", &uuid)
        .header("Location", format!("/v2/{}/blobs/uploads/{}", repo, uuid))
        .header("Range", format!("0-{}", length.saturating_sub(1)))
        .header(header::CONTENT_LENGTH, "0")
        .body(axum::body::Body::empty())
        .unwrap()
}

/// PUT — complete the upload. Digest is verified; blob moved to permanent key.
async fn complete_upload(
    state: AppState,
    repo: String,
    uuid: String,
    query: HashMap<String, String>,
    body: Bytes,
) -> Response {
    let expected_digest = match query.get("digest") {
        Some(d) => d.clone(),
        None => return bad_request("digest query parameter required"),
    };

    match crate::db::get_upload(&state.db, &uuid).await {
        Ok(None) => return not_found("upload not found"),
        Err(e) => return internal_error(&e.to_string()),
        _ => {}
    };

    // Determine blob data source: PUT body or previously PATCHed data
    let blob_data: Bytes = if !body.is_empty() {
        body
    } else {
        // Fetch the previously uploaded chunk
        let tmp_key = upload_key(&uuid);
        match state.s3.get(&tmp_key).await {
            Ok(Some((data, _))) => data,
            Ok(None) => return bad_request("no blob data found for upload"),
            Err(e) => return internal_error(&e.to_string()),
        }
    };

    // Verify digest
    if !verify_digest(&blob_data, &expected_digest) {
        return oci_error(StatusCode::BAD_REQUEST, OciErrors::digest_invalid());
    }

    // Store blob at its permanent content-addressed key
    let dst_key = blob_key(&expected_digest);
    if let Err(e) = state
        .s3
        .put(&dst_key, blob_data, "application/octet-stream")
        .await
    {
        return internal_error(&e.to_string());
    }

    // Clean up temp upload object and DB record
    let _ = state.s3.delete(&upload_key(&uuid)).await;
    let _ = crate::db::delete_upload(&state.db, &uuid).await;

    Response::builder()
        .status(StatusCode::CREATED)
        .header("Docker-Content-Digest", &expected_digest)
        .header("Location", format!("/v2/{}/blobs/{}", repo, expected_digest))
        .header(header::CONTENT_LENGTH, "0")
        .body(axum::body::Body::empty())
        .unwrap()
}

async fn cancel_upload(state: AppState, _repo: String, uuid: String) -> Response {
    let _ = state.s3.delete(&upload_key(&uuid)).await;
    let _ = crate::db::delete_upload(&state.db, &uuid).await;
    StatusCode::NO_CONTENT.into_response()
}

// ── Tag Listing ──────────────────────────────────────────────────────────────

async fn list_tags(state: AppState, repo: String) -> Response {
    match crate::db::list_tags(&state.db, &repo).await {
        Ok(tags) => {
            let body = serde_json::json!({ "name": repo, "tags": tags });
            json_response(StatusCode::OK, &body)
        }
        Err(e) => internal_error(&e.to_string()),
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn json_response(status: StatusCode, body: &serde_json::Value) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, CONTENT_TYPE_JSON)],
        serde_json::to_string(body).unwrap_or_default(),
    )
        .into_response()
}

fn oci_error(status: StatusCode, errors: OciErrors) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, CONTENT_TYPE_JSON)],
        errors.to_json(),
    )
        .into_response()
}

fn not_found(detail: &str) -> Response {
    oci_error(StatusCode::NOT_FOUND, OciErrors::not_found(detail))
}

fn bad_request(msg: &str) -> Response {
    oci_error(StatusCode::BAD_REQUEST, OciErrors::new("UNSUPPORTED", msg))
}

fn internal_error(msg: &str) -> Response {
    tracing::error!("Internal error: {}", msg);
    oci_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        OciErrors::new("INTERNAL_ERROR", msg),
    )
}

fn method_not_allowed() -> Response {
    StatusCode::METHOD_NOT_ALLOWED.into_response()
}

fn auth_challenge() -> Response {
    Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .header("WWW-Authenticate", r#"Bearer realm="/v2/token",service="yatch""#)
        .header(header::CONTENT_TYPE, CONTENT_TYPE_JSON)
        .body(axum::body::Body::from(
            OciErrors::new("UNAUTHORIZED", "authentication required").to_json(),
        ))
        .unwrap()
}

fn check_auth(headers: &HeaderMap, expected_token: &str) -> bool {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|t| t == expected_token)
        .unwrap_or(false)
}
