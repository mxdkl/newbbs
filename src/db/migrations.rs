//! Schema migrations, applied on boot by comparing `PRAGMA user_version`.
//!
//! Append new steps to the end of the list; never edit or reorder an existing
//! one, because databases in the wild have already applied it.

use anyhow::{Context, Result};
use rusqlite::Connection;

const MIGRATIONS: &[&str] = &[
    // 1: event log, routing tags, and the projections derived from them.
    r#"
    CREATE TABLE events (
        id             BLOB    PRIMARY KEY,
        at             INTEGER NOT NULL,
        actor          INTEGER,
        schema_version INTEGER NOT NULL,
        payload        BLOB    NOT NULL
    );

    -- One row per tag per event, so replay can filter by subscription.
    CREATE TABLE event_tags (
        event_id BLOB    NOT NULL REFERENCES events(id) ON DELETE CASCADE,
        kind     INTEGER NOT NULL,
        ref_id   INTEGER,
        PRIMARY KEY (event_id, kind, ref_id)
    ) WITHOUT ROWID;
    CREATE INDEX event_tags_lookup ON event_tags(kind, ref_id);

    CREATE TABLE counters (
        name  TEXT    PRIMARY KEY,
        value INTEGER NOT NULL
    );

    CREATE TABLE users (
        id        INTEGER PRIMARY KEY,
        name      TEXT    NOT NULL UNIQUE,
        bio       TEXT    NOT NULL DEFAULT '',
        pubkey    TEXT    NOT NULL DEFAULT '',
        joined_at INTEGER NOT NULL,
        last_seen INTEGER NOT NULL
    );

    CREATE TABLE roles (
        id       INTEGER PRIMARY KEY,
        name     TEXT    NOT NULL,
        color    INTEGER NOT NULL,
        priority INTEGER NOT NULL,
        hoisted  INTEGER NOT NULL,
        admin    INTEGER NOT NULL
    );

    CREATE TABLE user_roles (
        user_id INTEGER NOT NULL REFERENCES users(id),
        role_id INTEGER NOT NULL REFERENCES roles(id),
        PRIMARY KEY (user_id, role_id)
    ) WITHOUT ROWID;

    CREATE TABLE convs (
        id         INTEGER PRIMARY KEY,
        kind       INTEGER NOT NULL,
        name       TEXT    NOT NULL,
        topic      TEXT    NOT NULL DEFAULT '',
        created_at INTEGER NOT NULL
    );

    CREATE TABLE conv_members (
        conv_id   INTEGER NOT NULL REFERENCES convs(id),
        user_id   INTEGER NOT NULL REFERENCES users(id),
        joined_at INTEGER NOT NULL,
        PRIMARY KEY (conv_id, user_id)
    ) WITHOUT ROWID;

    CREATE TABLE messages (
        id         BLOB    PRIMARY KEY,
        conv_id    INTEGER NOT NULL REFERENCES convs(id),
        author     INTEGER NOT NULL REFERENCES users(id),
        body       TEXT    NOT NULL,
        created_at INTEGER NOT NULL,
        edited_at  INTEGER,
        deleted    INTEGER NOT NULL DEFAULT 0
    );
    CREATE INDEX messages_by_conv ON messages(conv_id, created_at);
    "#,
];

pub fn apply(conn: &mut Connection) -> Result<()> {
    let current: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    let target = MIGRATIONS.len() as i64;
    if current > target {
        anyhow::bail!(
            "database is at schema version {current}, but this build only knows {target} -- \
             it was written by a newer newbbs"
        );
    }
    for (idx, sql) in MIGRATIONS.iter().enumerate().skip(current as usize) {
        let version = idx as i64 + 1;
        let tx = conn.transaction()?;
        tx.execute_batch(sql)
            .with_context(|| format!("applying migration {version}"))?;
        // PRAGMA does not accept a bound parameter.
        tx.execute_batch(&format!("PRAGMA user_version = {version}"))?;
        tx.commit()?;
        tracing::info!(version, "applied migration");
    }
    Ok(())
}
