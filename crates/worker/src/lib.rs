//! Yatch — Cloudflare Worker entry point.
//!
//! Bindings expected in wrangler.toml:
//!   - BUCKET (R2 bucket)   — blob storage
//!   - DB    (D1 database)  — SQLite metadata
//!   - R2_PUBLIC_URL (optional var) — direct blob download base URL
//!   - AUTH_TOKEN    (optional secret)

use worker::wasm_bindgen::JsValue;
use worker::*;
use yatch_core::*;

const API_VERSION: &str = "registry/2.0";

fn js(s: &str) -> JsValue { JsValue::from_str(s) }
fn jsn(n: i64) -> JsValue { JsValue::from_f64(n as f64) }

// ── Entry Point ──────────────────────────────────────────────────────────────

#[event(fetch)]
pub async fn main(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    console_error_panic_hook::set_once();

    let path = req.path();
    let method = req.method();

    if !path.starts_with("/v2") {
        return not_found("only /v2 paths are supported");
    }

    // Optional bearer auth
    if let Ok(token) = env.var("AUTH_TOKEN") {
        let expected = token.to_string();
        if !expected.is_empty() && !check_auth(&req, &expected) {
            return auth_challenge();
        }
    }

    // /v2/ — API version check
    if path == "/v2/" || path == "/v2" {
        let mut resp = Response::from_json(&serde_json::json!({}))?;
        resp.headers_mut()
            .set("Docker-Distribution-API-Version", API_VERSION)?;
        return Ok(resp);
    }

    // /v2/_catalog
    if path == "/v2/_catalog" && method == Method::Get {
        return catalog(&env).await;
    }

    // Parse OCI path (strip /v2/)
    let oci_part = path.trim_start_matches("/v2/");
    match parse_oci_path(oci_part) {
        Some(OciPath::Manifests { name, reference }) => match method {
            Method::Get => get_manifest(&env, name, reference).await,
            Method::Head => head_manifest(&env, name, reference).await,
            Method::Put => put_manifest(&env, req, name, reference).await,
            Method::Delete => delete_manifest(&env, name, reference).await,
            _ => method_not_allowed(),
        },

        Some(OciPath::Blob { name, digest }) => match method {
            Method::Get => get_blob(&env, name, digest).await,
            Method::Head => head_blob(&env, name, digest).await,
            Method::Delete => delete_blob(&env, name, digest).await,
            _ => method_not_allowed(),
        },

        Some(OciPath::BlobUploadStart { name }) => {
            if method == Method::Post {
                let url = Url::parse(&req.url()?.to_string())?;
                let mount = url
                    .query_pairs()
                    .find(|(k, _)| k == "mount")
                    .map(|(_, v)| v.to_string());
                initiate_upload(&env, name, mount).await
            } else {
                method_not_allowed()
            }
        }

        Some(OciPath::BlobUpload { name, uuid }) => match method {
            Method::Get => upload_status(&env, name, uuid).await,
            Method::Patch => patch_upload(&env, req, name, uuid).await,
            Method::Put => {
                let url = Url::parse(&req.url()?.to_string())?;
                let digest = url
                    .query_pairs()
                    .find(|(k, _)| k == "digest")
                    .map(|(_, v)| v.to_string());
                complete_upload(&env, req, name, uuid, digest).await
            }
            Method::Delete => cancel_upload(&env, name, uuid).await,
            _ => method_not_allowed(),
        },

        Some(OciPath::TagsList { name }) => {
            if method == Method::Get {
                list_tags(&env, name).await
            } else {
                method_not_allowed()
            }
        }

        None => not_found("path not recognized"),
    }
}

// ── Catalog ──────────────────────────────────────────────────────────────────

async fn catalog(env: &Env) -> Result<Response> {
    let db = env.d1("DB")?;
    let rows = db
        .prepare("SELECT DISTINCT repo FROM manifests ORDER BY repo")
        .all()
        .await?
        .results::<RepoRow>()?;
    let repos: Vec<String> = rows.into_iter().map(|r| r.repo).collect();
    json_response(200, &serde_json::json!({ "repositories": repos }))
}

// ── Manifest Handlers ────────────────────────────────────────────────────────

async fn get_manifest(env: &Env, repo: String, reference: String) -> Result<Response> {
    let meta = match db_get_manifest(env, &repo, &reference).await? {
        Some(m) => m,
        None => return oci_error(404, "MANIFEST_UNKNOWN", "manifest unknown"),
    };

    let bucket = env.bucket("BUCKET")?;
    let key = manifest_key(&repo, &meta.digest);

    match bucket.get(key).execute().await? {
        Some(obj) => {
            let data = obj.body().ok_or("no body")?.bytes().await?;
            let mut resp = Response::from_bytes(data)?;
            let h = resp.headers_mut();
            h.set("Content-Type", &meta.content_type)?;
            h.set("Docker-Content-Digest", &meta.digest)?;
            h.set("Docker-Distribution-API-Version", API_VERSION)?;
            Ok(resp)
        }
        None => oci_error(404, "MANIFEST_UNKNOWN", "manifest not found in storage"),
    }
}

async fn head_manifest(env: &Env, repo: String, reference: String) -> Result<Response> {
    match db_get_manifest(env, &repo, &reference).await? {
        Some(m) => {
            let mut resp = Response::empty()?.with_status(200);
            let h = resp.headers_mut();
            h.set("Content-Type", &m.content_type)?;
            h.set("Docker-Content-Digest", &m.digest)?;
            h.set("Content-Length", &m.size.to_string())?;
            h.set("Docker-Distribution-API-Version", API_VERSION)?;
            Ok(resp)
        }
        None => oci_error(404, "MANIFEST_UNKNOWN", "manifest unknown"),
    }
}

async fn put_manifest(
    env: &Env,
    mut req: Request,
    repo: String,
    reference: String,
) -> Result<Response> {
    let content_type = req
        .headers()
        .get("Content-Type")?
        .unwrap_or_else(|| "application/vnd.docker.distribution.manifest.v2+json".into());

    let body = req.bytes().await?;
    let digest = compute_digest(&body);
    let size = body.len() as i64;

    let bucket = env.bucket("BUCKET")?;
    let key = manifest_key(&repo, &digest);
    bucket
        .put(key, body)
        .http_metadata(HttpMetadata {
            content_type: Some(content_type.clone()),
            ..Default::default()
        })
        .execute()
        .await?;

    let db = env.d1("DB")?;
    db.prepare(
        "INSERT OR REPLACE INTO manifests (repo, digest, content_type, size) \
         VALUES (?1, ?2, ?3, ?4)",
    )
    .bind(&[js(&repo), js(&digest), js(&content_type), jsn(size)])?
    .run()
    .await?;

    if !reference.starts_with("sha256:") {
        db.prepare("INSERT OR REPLACE INTO tags (repo, tag, digest) VALUES (?1, ?2, ?3)")
            .bind(&[js(&repo), js(&reference), js(&digest)])?
            .run()
            .await?;
    }

    let mut resp = Response::empty()?.with_status(201);
    resp.headers_mut()
        .set("Docker-Content-Digest", &digest)?;
    resp.headers_mut()
        .set("Location", &format!("/v2/{}/manifests/{}", repo, digest))?;
    Ok(resp)
}

async fn delete_manifest(env: &Env, repo: String, reference: String) -> Result<Response> {
    let meta = match db_get_manifest(env, &repo, &reference).await? {
        Some(m) => m,
        None => return oci_error(404, "MANIFEST_UNKNOWN", "manifest unknown"),
    };

    let bucket = env.bucket("BUCKET")?;
    let _ = bucket.delete(manifest_key(&repo, &meta.digest)).await;

    let db = env.d1("DB")?;
    if reference.starts_with("sha256:") {
        db.prepare("DELETE FROM manifests WHERE repo = ?1 AND digest = ?2")
            .bind(&[js(&repo), js(&reference)])?
            .run()
            .await?;
        db.prepare("DELETE FROM tags WHERE repo = ?1 AND digest = ?2")
            .bind(&[js(&repo), js(&reference)])?
            .run()
            .await?;
    } else {
        db.prepare("DELETE FROM tags WHERE repo = ?1 AND tag = ?2")
            .bind(&[js(&repo), js(&reference)])?
            .run()
            .await?;
    }

    Ok(Response::empty()?.with_status(202))
}

// ── Blob Handlers ────────────────────────────────────────────────────────────

/// GET blob → 307 redirect to public R2 URL (zero egress), or stream through worker.
async fn get_blob(env: &Env, _repo: String, digest: String) -> Result<Response> {
    let bucket = env.bucket("BUCKET")?;
    let key = blob_key(&digest);

    if bucket.head(&key).await?.is_none() {
        return oci_error(404, "BLOB_UNKNOWN", "blob unknown to registry");
    }

    // Use public R2 domain if configured (free egress via public bucket)
    if let Ok(base) = env.var("R2_PUBLIC_URL") {
        let base_url = base.to_string();
        if !base_url.is_empty() {
            let url = format!("{}/{}", base_url.trim_end_matches('/'), key);
            let mut resp = Response::empty()?.with_status(307);
            resp.headers_mut().set("Location", &url)?;
            resp.headers_mut().set("Docker-Content-Digest", &digest)?;
            return Ok(resp);
        }
    }

    // Fallback: stream blob through worker
    let obj = bucket.get(key).execute().await?.ok_or("blob not found")?;
    let data = obj.body().ok_or("no body")?.bytes().await?;
    let mut resp = Response::from_bytes(data)?;
    resp.headers_mut().set("Docker-Content-Digest", &digest)?;
    resp.headers_mut()
        .set("Content-Type", "application/octet-stream")?;
    Ok(resp)
}

async fn head_blob(env: &Env, _repo: String, digest: String) -> Result<Response> {
    let bucket = env.bucket("BUCKET")?;
    let key = blob_key(&digest);

    match bucket.head(&key).await? {
        Some(obj) => {
            let mut resp = Response::empty()?.with_status(200);
            let h = resp.headers_mut();
            h.set("Docker-Content-Digest", &digest)?;
            h.set("Content-Type", "application/octet-stream")?;
            h.set("Content-Length", &obj.size().to_string())?;
            Ok(resp)
        }
        None => oci_error(404, "BLOB_UNKNOWN", "blob unknown to registry"),
    }
}

async fn delete_blob(env: &Env, _repo: String, digest: String) -> Result<Response> {
    let bucket = env.bucket("BUCKET")?;
    let key = blob_key(&digest);
    if bucket.head(&key).await?.is_none() {
        return oci_error(404, "BLOB_UNKNOWN", "blob unknown");
    }
    bucket.delete(key).await?;
    Ok(Response::empty()?.with_status(202))
}

// ── Upload Handlers ──────────────────────────────────────────────────────────

async fn initiate_upload(
    env: &Env,
    repo: String,
    mount_digest: Option<String>,
) -> Result<Response> {
    // Cross-repo blob mount: blob already exists globally → fast path
    if let Some(ref digest) = mount_digest {
        let bucket = env.bucket("BUCKET")?;
        if bucket.head(&blob_key(digest)).await?.is_some() {
            let mut resp = Response::empty()?.with_status(201);
            resp.headers_mut().set("Docker-Content-Digest", digest)?;
            resp.headers_mut()
                .set("Location", &format!("/v2/{}/blobs/{}", repo, digest))?;
            return Ok(resp);
        }
    }

    let uuid = uuid::Uuid::new_v4().to_string();
    let db = env.d1("DB")?;
    db.prepare("INSERT INTO uploads (uuid, repo, offset) VALUES (?1, ?2, 0)")
        .bind(&[js(&uuid), js(&repo)])?
        .run()
        .await?;

    let mut resp = Response::empty()?.with_status(202);
    let h = resp.headers_mut();
    h.set("Docker-Upload-UUID", &uuid)?;
    h.set("Location", &format!("/v2/{}/blobs/uploads/{}", repo, uuid))?;
    h.set("Range", "0-0")?;
    h.set("Content-Length", "0")?;
    Ok(resp)
}

async fn upload_status(env: &Env, repo: String, uuid: String) -> Result<Response> {
    let db = env.d1("DB")?;
    let row = db
        .prepare("SELECT offset FROM uploads WHERE uuid = ?1")
        .bind(&[js(&uuid)])?
        .first::<OffsetRow>(None)
        .await?;

    match row {
        Some(r) => {
            let mut resp = Response::empty()?.with_status(204);
            resp.headers_mut()
                .set("Range", &format!("0-{}", r.offset.saturating_sub(1)))?;
            resp.headers_mut()
                .set("Location", &format!("/v2/{}/blobs/uploads/{}", repo, uuid))?;
            Ok(resp)
        }
        None => oci_error(404, "BLOB_UPLOAD_UNKNOWN", "upload not found"),
    }
}

/// PATCH — store chunk in R2 temporary object.
async fn patch_upload(env: &Env, mut req: Request, repo: String, uuid: String) -> Result<Response> {
    let db = env.d1("DB")?;
    let exists = db
        .prepare("SELECT uuid FROM uploads WHERE uuid = ?1")
        .bind(&[js(&uuid)])?
        .first::<UuidRow>(None)
        .await?
        .is_some();

    if !exists {
        return oci_error(404, "BLOB_UPLOAD_UNKNOWN", "upload not found");
    }

    let body = req.bytes().await?;
    let length = body.len() as i64;

    let bucket = env.bucket("BUCKET")?;
    bucket.put(upload_key(&uuid), body).execute().await?;

    db.prepare("UPDATE uploads SET offset = ?1 WHERE uuid = ?2")
        .bind(&[jsn(length), js(&uuid)])?
        .run()
        .await?;

    let mut resp = Response::empty()?.with_status(202);
    let h = resp.headers_mut();
    h.set("Docker-Upload-UUID", &uuid)?;
    h.set("Location", &format!("/v2/{}/blobs/uploads/{}", repo, uuid))?;
    h.set("Range", &format!("0-{}", length.saturating_sub(1)))?;
    Ok(resp)
}

/// PUT — verify digest and finalize upload.
async fn complete_upload(
    env: &Env,
    mut req: Request,
    repo: String,
    uuid: String,
    digest: Option<String>,
) -> Result<Response> {
    let expected = match digest {
        Some(d) => d,
        None => return oci_error(400, "DIGEST_INVALID", "digest query parameter required"),
    };

    let db = env.d1("DB")?;
    let upload_exists = db
        .prepare("SELECT uuid FROM uploads WHERE uuid = ?1")
        .bind(&[js(&uuid)])?
        .first::<UuidRow>(None)
        .await?
        .is_some();

    if !upload_exists {
        return oci_error(404, "BLOB_UPLOAD_UNKNOWN", "upload not found");
    }

    let bucket = env.bucket("BUCKET")?;

    // Get blob data: PUT body or previously PATCHed R2 object
    let body_bytes = req.bytes().await?;
    let blob_data: Vec<u8> = if !body_bytes.is_empty() {
        body_bytes
    } else {
        match bucket.get(upload_key(&uuid)).execute().await? {
            Some(obj) => obj.body().ok_or("no body")?.bytes().await?,
            None => return oci_error(400, "BLOB_UPLOAD_UNKNOWN", "no blob data for upload"),
        }
    };

    if !verify_digest(&blob_data, &expected) {
        return oci_error(400, "DIGEST_INVALID", "digest mismatch");
    }

    // Write to permanent content-addressed key
    bucket.put(blob_key(&expected), blob_data).execute().await?;

    // Cleanup temp upload
    let _ = bucket.delete(upload_key(&uuid)).await;
    db.prepare("DELETE FROM uploads WHERE uuid = ?1")
        .bind(&[js(&uuid)])?
        .run()
        .await?;

    let mut resp = Response::empty()?.with_status(201);
    resp.headers_mut()
        .set("Docker-Content-Digest", &expected)?;
    resp.headers_mut()
        .set("Location", &format!("/v2/{}/blobs/{}", repo, expected))?;
    Ok(resp)
}

async fn cancel_upload(env: &Env, _repo: String, uuid: String) -> Result<Response> {
    let bucket = env.bucket("BUCKET")?;
    let _ = bucket.delete(upload_key(&uuid)).await;
    let db = env.d1("DB")?;
    let _ = db
        .prepare("DELETE FROM uploads WHERE uuid = ?1")
        .bind(&[js(&uuid)])?
        .run()
        .await;
    Ok(Response::empty()?.with_status(204))
}

// ── Tags ─────────────────────────────────────────────────────────────────────

async fn list_tags(env: &Env, repo: String) -> Result<Response> {
    let db = env.d1("DB")?;
    let rows = db
        .prepare("SELECT tag FROM tags WHERE repo = ?1 ORDER BY tag")
        .bind(&[js(&repo)])?
        .all()
        .await?
        .results::<TagRow>()?;
    let tags: Vec<String> = rows.into_iter().map(|r| r.tag).collect();
    json_response(200, &serde_json::json!({ "name": repo, "tags": tags }))
}

// ── DB Row Types ──────────────────────────────────────────────────────────────

#[derive(serde::Deserialize)]
struct ManifestMetaRow {
    digest: String,
    content_type: String,
    size: i64,
}

#[derive(serde::Deserialize)]
struct TagDigestRow {
    digest: String,
}

#[derive(serde::Deserialize)]
struct TagRow {
    tag: String,
}

#[derive(serde::Deserialize)]
struct RepoRow {
    repo: String,
}

#[derive(serde::Deserialize)]
struct OffsetRow {
    offset: i64,
}

#[derive(serde::Deserialize)]
struct UuidRow {
    uuid: String,
}

async fn db_get_manifest(
    env: &Env,
    repo: &str,
    reference: &str,
) -> Result<Option<ManifestMetaRow>> {
    let db = env.d1("DB")?;

    let digest: String = if reference.starts_with("sha256:") {
        reference.to_string()
    } else {
        match db
            .prepare("SELECT digest FROM tags WHERE repo = ?1 AND tag = ?2")
            .bind(&[js(repo), js(reference)])?
            .first::<TagDigestRow>(None)
            .await?
        {
            Some(r) => r.digest,
            None => return Ok(None),
        }
    };

    db.prepare(
        "SELECT digest, content_type, size FROM manifests WHERE repo = ?1 AND digest = ?2",
    )
    .bind(&[js(repo), js(&digest)])?
    .first::<ManifestMetaRow>(None)
    .await
}

// ── Response Helpers ──────────────────────────────────────────────────────────

fn json_response(status: u16, body: &serde_json::Value) -> Result<Response> {
    let mut resp = Response::from_json(body)?.with_status(status);
    resp.headers_mut()
        .set("Content-Type", "application/json")?;
    Ok(resp)
}

fn oci_error(status: u16, code: &str, msg: &str) -> Result<Response> {
    let body = serde_json::json!({
        "errors": [{ "code": code, "message": msg, "detail": null }]
    });
    let mut resp = Response::from_json(&body)?.with_status(status);
    resp.headers_mut()
        .set("Content-Type", "application/json")?;
    Ok(resp)
}

fn not_found(msg: &str) -> Result<Response> {
    oci_error(404, "NAME_UNKNOWN", msg)
}

fn method_not_allowed() -> Result<Response> {
    Ok(Response::empty()?.with_status(405))
}

fn auth_challenge() -> Result<Response> {
    let body = serde_json::json!({
        "errors": [{ "code": "UNAUTHORIZED", "message": "authentication required", "detail": null }]
    });
    let mut resp = Response::from_json(&body)?.with_status(401);
    resp.headers_mut().set(
        "WWW-Authenticate",
        r#"Bearer realm="https://registry.example.com/auth",service="registry""#,
    )?;
    Ok(resp)
}

fn check_auth(req: &Request, expected_token: &str) -> bool {
    req.headers()
        .get("Authorization")
        .ok()
        .flatten()
        .and_then(|v| v.strip_prefix("Bearer ").map(str::to_string))
        .map(|t| t == expected_token)
        .unwrap_or(false)
}
