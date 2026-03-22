-- Yatch D1 schema (Cloudflare Worker mode)
-- Apply: wrangler d1 execute yatch-meta --file=migrations/0001_init.sql

CREATE TABLE IF NOT EXISTS tags (
    repo    TEXT NOT NULL,
    tag     TEXT NOT NULL,
    digest  TEXT NOT NULL,
    created INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    PRIMARY KEY (repo, tag)
);

CREATE TABLE IF NOT EXISTS manifests (
    repo         TEXT NOT NULL,
    digest       TEXT NOT NULL,
    content_type TEXT NOT NULL,
    size         INTEGER NOT NULL,
    created      INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    PRIMARY KEY (repo, digest)
);

CREATE TABLE IF NOT EXISTS uploads (
    uuid    TEXT PRIMARY KEY,
    repo    TEXT NOT NULL,
    offset  INTEGER NOT NULL DEFAULT 0,
    created INTEGER NOT NULL DEFAULT (strftime('%s','now'))
);
