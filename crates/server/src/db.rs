//! SQLite metadata store via sqlx.
//!
//! Schema:
//!   tags      — (repo, tag) → manifest_digest
//!   manifests — (repo, digest) → content_type, size
//!   uploads   — uuid → (repo, offset) for in-progress blob uploads

use anyhow::Result;
use sqlx::{sqlite::SqlitePool, Row};

pub async fn open(path: &str) -> Result<SqlitePool> {
    // Create file if it doesn't exist
    let url = if path.starts_with("sqlite:") {
        path.to_string()
    } else {
        format!("sqlite:{}?mode=rwc", path)
    };

    let pool = SqlitePool::connect(&url).await?;
    migrate(&pool).await?;
    Ok(pool)
}

async fn migrate(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS tags (
            repo    TEXT NOT NULL,
            tag     TEXT NOT NULL,
            digest  TEXT NOT NULL,
            created INTEGER NOT NULL DEFAULT (strftime('%s','now')),
            PRIMARY KEY (repo, tag)
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS manifests (
            repo         TEXT NOT NULL,
            digest       TEXT NOT NULL,
            content_type TEXT NOT NULL,
            size         INTEGER NOT NULL,
            created      INTEGER NOT NULL DEFAULT (strftime('%s','now')),
            PRIMARY KEY (repo, digest)
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS uploads (
            uuid    TEXT PRIMARY KEY,
            repo    TEXT NOT NULL,
            offset  INTEGER NOT NULL DEFAULT 0,
            created INTEGER NOT NULL DEFAULT (strftime('%s','now'))
        )",
    )
    .execute(pool)
    .await?;

    Ok(())
}

// ── Manifests ───────────────────────────────────────────────────────────────

pub async fn put_manifest(
    pool: &SqlitePool,
    repo: &str,
    digest: &str,
    content_type: &str,
    size: i64,
) -> Result<()> {
    sqlx::query(
        "INSERT OR REPLACE INTO manifests (repo, digest, content_type, size) VALUES (?, ?, ?, ?)",
    )
    .bind(repo)
    .bind(digest)
    .bind(content_type)
    .bind(size)
    .execute(pool)
    .await?;
    Ok(())
}

pub struct ManifestRow {
    pub digest: String,
    pub content_type: String,
    pub size: i64,
}

/// Look up a manifest by digest or tag.
pub async fn get_manifest(
    pool: &SqlitePool,
    repo: &str,
    reference: &str,
) -> Result<Option<ManifestRow>> {
    // If reference is a digest, look up directly
    let digest = if reference.starts_with("sha256:") {
        reference.to_string()
    } else {
        // Look up tag
        let row = sqlx::query("SELECT digest FROM tags WHERE repo = ? AND tag = ?")
            .bind(repo)
            .bind(reference)
            .fetch_optional(pool)
            .await?;
        match row {
            Some(r) => r.try_get("digest")?,
            None => return Ok(None),
        }
    };

    let row = sqlx::query(
        "SELECT digest, content_type, size FROM manifests WHERE repo = ? AND digest = ?",
    )
    .bind(repo)
    .bind(&digest)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|r| ManifestRow {
        digest: r.try_get("digest").unwrap(),
        content_type: r.try_get("content_type").unwrap(),
        size: r.try_get("size").unwrap(),
    }))
}

pub async fn delete_manifest(pool: &SqlitePool, repo: &str, reference: &str) -> Result<()> {
    if reference.starts_with("sha256:") {
        sqlx::query("DELETE FROM manifests WHERE repo = ? AND digest = ?")
            .bind(repo)
            .bind(reference)
            .execute(pool)
            .await?;
        sqlx::query("DELETE FROM tags WHERE repo = ? AND digest = ?")
            .bind(repo)
            .bind(reference)
            .execute(pool)
            .await?;
    } else {
        sqlx::query("DELETE FROM tags WHERE repo = ? AND tag = ?")
            .bind(repo)
            .bind(reference)
            .execute(pool)
            .await?;
    }
    Ok(())
}

// ── Tags ────────────────────────────────────────────────────────────────────

pub async fn put_tag(pool: &SqlitePool, repo: &str, tag: &str, digest: &str) -> Result<()> {
    sqlx::query("INSERT OR REPLACE INTO tags (repo, tag, digest) VALUES (?, ?, ?)")
        .bind(repo)
        .bind(tag)
        .bind(digest)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn list_tags(pool: &SqlitePool, repo: &str) -> Result<Vec<String>> {
    let rows = sqlx::query("SELECT tag FROM tags WHERE repo = ? ORDER BY tag")
        .bind(repo)
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().map(|r| r.try_get("tag").unwrap()).collect())
}

// ── Repositories (catalog) ──────────────────────────────────────────────────

pub async fn list_repos(pool: &SqlitePool) -> Result<Vec<String>> {
    let rows = sqlx::query("SELECT DISTINCT repo FROM manifests ORDER BY repo")
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().map(|r| r.try_get("repo").unwrap()).collect())
}

// ── In-progress Uploads ─────────────────────────────────────────────────────

pub async fn create_upload(pool: &SqlitePool, uuid: &str, repo: &str) -> Result<()> {
    sqlx::query("INSERT INTO uploads (uuid, repo, offset) VALUES (?, ?, 0)")
        .bind(uuid)
        .bind(repo)
        .execute(pool)
        .await?;
    Ok(())
}

pub struct UploadRow {
    pub repo: String,
    pub offset: i64,
}

pub async fn get_upload(pool: &SqlitePool, uuid: &str) -> Result<Option<UploadRow>> {
    let row = sqlx::query("SELECT repo, offset FROM uploads WHERE uuid = ?")
        .bind(uuid)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|r| UploadRow {
        repo: r.try_get("repo").unwrap(),
        offset: r.try_get("offset").unwrap(),
    }))
}

pub async fn update_upload_offset(pool: &SqlitePool, uuid: &str, offset: i64) -> Result<()> {
    sqlx::query("UPDATE uploads SET offset = ? WHERE uuid = ?")
        .bind(offset)
        .bind(uuid)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn delete_upload(pool: &SqlitePool, uuid: &str) -> Result<()> {
    sqlx::query("DELETE FROM uploads WHERE uuid = ?")
        .bind(uuid)
        .execute(pool)
        .await?;
    Ok(())
}
