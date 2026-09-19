//! SQLite storage: the append-only event log plus the projections derived from
//! it. Appending an event and applying it to the projections happen inside one
//! transaction, so a read can never observe a log entry that has not landed in
//! the tables yet.
//!
//! Everything here is synchronous and single-owner -- the bus task is the only
//! thing that touches a [`Db`].

mod bootstrap;
mod migrations;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;
use uuid::Uuid;

use crate::config;
use crate::model::*;

pub struct Db {
    conn: Connection,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let mut conn =
            Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        // `newbbs invite` runs against the same file while the server holds
        // it, so a writer has to wait rather than fail outright.
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        migrations::apply(&mut conn)?;
        Ok(Self { conn })
    }

    /// Reserve the next id for one of the integer-keyed entities.
    pub fn alloc(&mut self, counter: &str) -> Result<i64> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO counters(name, value) VALUES (?1, 1)
             ON CONFLICT(name) DO UPDATE SET value = value + 1",
            params![counter],
        )?;
        let id: i64 = tx.query_row(
            "SELECT value FROM counters WHERE name = ?1",
            params![counter],
            |r| r.get(0),
        )?;
        tx.commit()?;
        Ok(id)
    }

    /// Append an event and apply it to the projections, atomically.
    pub fn commit(&mut self, actor: Option<UserId>, kind: EventKind) -> Result<Event> {
        self.commit_at(actor, kind, now_millis())
    }

    /// As [`Db::commit`], but with an explicit timestamp. Bootstrapping uses
    /// it to give each of its events a distinct millisecond. The uuid is v7
    /// derived from `at`, so the log still sorts by time.
    pub fn commit_at(
        &mut self,
        actor: Option<UserId>,
        kind: EventKind,
        at: Millis,
    ) -> Result<Event> {
        let event = Event {
            id: uuid_at(at),
            at,
            actor,
            kind,
        };
        let payload = postcard::to_stdvec(&event.kind).context("encoding event payload")?;
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO events(id, at, actor, schema_version, payload)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                event.id.as_bytes().as_slice(),
                event.at,
                event.actor,
                config::EVENT_SCHEMA_VERSION,
                payload
            ],
        )?;
        for tag in event.tags() {
            let (kind_code, ref_id) = encode_tag(tag);
            tx.execute(
                "INSERT OR IGNORE INTO event_tags(event_id, kind, ref_id) VALUES (?1, ?2, ?3)",
                params![event.id.as_bytes().as_slice(), kind_code, ref_id],
            )?;
        }
        apply_projection(&tx, &event)?;
        tx.commit()?;
        Ok(event)
    }

    pub fn event(&self, id: Uuid) -> Result<Option<Event>> {
        self.conn
            .query_row(
                "SELECT id, at, actor, payload FROM events WHERE id = ?1",
                params![id.as_bytes().as_slice()],
                row_to_event,
            )
            .optional()
            .context("loading event")?
            .transpose()
    }

    /// Events matching any of `tags` that happened after `since`, newest-capped
    /// and age-capped. The boolean reports whether the cap truncated the range,
    /// i.e. whether the caller is looking at a gap.
    pub fn replay(&self, tags: &[Tag], since: Option<Uuid>) -> Result<(Vec<Event>, bool)> {
        if tags.is_empty() {
            return Ok((Vec::new(), false));
        }
        let floor = now_millis() - config::REPLAY_AGE_MILLIS;
        let since_bytes = since.map(|u| u.as_bytes().to_vec());
        let placeholders = (0..tags.len())
            .map(|i| format!("(?{}, ?{})", i * 2 + 1, i * 2 + 2))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT DISTINCT e.id, e.at, e.actor, e.payload
               FROM events e JOIN event_tags t ON t.event_id = e.id
              WHERE (t.kind, COALESCE(t.ref_id, -1)) IN ({placeholders})
                AND e.at >= ?{floor_idx}
                AND (?{since_idx} IS NULL OR e.id > ?{since_idx})
              ORDER BY e.id ASC
              LIMIT ?{limit_idx}",
            floor_idx = tags.len() * 2 + 1,
            since_idx = tags.len() * 2 + 2,
            limit_idx = tags.len() * 2 + 3,
        );
        let mut values: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        for tag in tags {
            let (kind_code, ref_id) = encode_tag(*tag);
            values.push(Box::new(kind_code));
            values.push(Box::new(ref_id.unwrap_or(-1)));
        }
        values.push(Box::new(floor));
        values.push(Box::new(since_bytes));
        // One extra row tells us whether the cap truncated the range.
        values.push(Box::new(config::REPLAY_EVENT_CAP as i64 + 1));

        let mut stmt = self.conn.prepare(&sql)?;
        let params = rusqlite::params_from_iter(values.iter().map(|v| v.as_ref()));
        let mut events = stmt
            .query_map(params, row_to_event)?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .collect::<Result<Vec<Event>>>()?;
        let truncated = events.len() > config::REPLAY_EVENT_CAP;
        events.truncate(config::REPLAY_EVENT_CAP);
        Ok((events, truncated))
    }

    // -- projections ------------------------------------------------------

    pub fn user(&self, id: UserId) -> Result<Option<User>> {
        let mut user = match self
            .conn
            .query_row(
                "SELECT id, name, bio, joined_at, last_seen FROM users WHERE id = ?1",
                params![id],
                row_to_user,
            )
            .optional()?
        {
            Some(u) => u,
            None => return Ok(None),
        };
        user.roles = self.roles_of(id)?;
        Ok(Some(user))
    }

    pub fn user_by_name(&self, name: &str) -> Result<Option<User>> {
        let id: Option<UserId> = self
            .conn
            .query_row(
                "SELECT id FROM users WHERE name = ?1 COLLATE NOCASE",
                params![name],
                |r| r.get(0),
            )
            .optional()?;
        match id {
            Some(id) => self.user(id),
            None => Ok(None),
        }
    }

    pub fn users(&self) -> Result<Vec<User>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, name, bio, joined_at, last_seen FROM users ORDER BY name")?;
        let mut users = stmt
            .query_map([], row_to_user)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for user in &mut users {
            user.roles = self.roles_of(user.id)?;
        }
        Ok(users)
    }

    pub fn roles_of(&self, user: UserId) -> Result<Vec<Role>> {
        let mut stmt = self.conn.prepare(
            "SELECT r.id, r.name, r.color, r.priority, r.hoisted, r.admin
               FROM roles r JOIN user_roles ur ON ur.role_id = r.id
              WHERE ur.user_id = ?1
              ORDER BY r.priority DESC, r.id ASC",
        )?;
        Ok(stmt
            .query_map(params![user], row_to_role)?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn roles(&self) -> Result<Vec<Role>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, color, priority, hoisted, admin FROM roles
              ORDER BY priority DESC, id ASC",
        )?;
        Ok(stmt
            .query_map([], row_to_role)?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Channels are visible to everyone; DMs and groups only to their members.
    pub fn conversations(&self, user: UserId) -> Result<Vec<Conversation>> {
        let mut stmt = self.conn.prepare(
            "SELECT c.id, c.kind, c.name, c.topic, c.created_at
               FROM convs c
              WHERE c.kind = 0
                 OR EXISTS (SELECT 1 FROM conv_members m
                             WHERE m.conv_id = c.id AND m.user_id = ?1)
              ORDER BY c.kind ASC, c.name ASC",
        )?;
        Ok(stmt
            .query_map(params![user], row_to_conv)?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn conversation(&self, conv: ConvId) -> Result<Option<Conversation>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, kind, name, topic, created_at FROM convs WHERE id = ?1",
                params![conv],
                row_to_conv,
            )
            .optional()?)
    }

    pub fn conversation_by_name(&self, name: &str) -> Result<Option<Conversation>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, kind, name, topic, created_at FROM convs
                  WHERE name = ?1 COLLATE NOCASE",
                params![name],
                row_to_conv,
            )
            .optional()?)
    }

    /// Everyone in a channel (all users) or the members of a DM/group.
    pub fn members(&self, conv: ConvId) -> Result<Vec<UserId>> {
        let kind: Option<i64> = self
            .conn
            .query_row("SELECT kind FROM convs WHERE id = ?1", params![conv], |r| {
                r.get(0)
            })
            .optional()?;
        let sql = match kind {
            Some(0) => "SELECT id FROM users ORDER BY name",
            Some(_) => {
                "SELECT u.id FROM users u JOIN conv_members m ON m.user_id = u.id
                  WHERE m.conv_id = ?1 ORDER BY u.name"
            }
            None => return Ok(Vec::new()),
        };
        let mut stmt = self.conn.prepare(sql)?;
        let rows = if kind == Some(0) {
            stmt.query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        } else {
            stmt.query_map(params![conv], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        Ok(rows)
    }

    /// The newest `limit` messages in a conversation, oldest first. `before`
    /// pages backwards through history.
    pub fn messages(
        &self,
        conv: ConvId,
        before: Option<MessageId>,
        limit: usize,
    ) -> Result<Vec<Message>> {
        let before_bytes = before.map(|u| u.as_bytes().to_vec());
        let mut stmt = self.conn.prepare(
            "SELECT id, conv_id, author, body, created_at, edited_at
               FROM messages
              WHERE conv_id = ?1 AND deleted = 0
                AND (?2 IS NULL OR id < ?2)
              ORDER BY id DESC
              LIMIT ?3",
        )?;
        let mut rows = stmt
            .query_map(params![conv, before_bytes, limit as i64], row_to_message)?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .collect::<Result<Vec<Message>>>()?;
        rows.reverse();
        Ok(rows)
    }

    pub fn touch_last_seen(&mut self, user: UserId) -> Result<()> {
        self.conn.execute(
            "UPDATE users SET last_seen = ?2 WHERE id = ?1",
            params![user, now_millis()],
        )?;
        Ok(())
    }

    /// Every account that has a key, for authenticating an ssh connection.
    /// Small servers only, which is what this is: the caller compares parsed
    /// keys rather than us indexing a fingerprint.
    pub fn user_keys(&self) -> Result<Vec<(UserId, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, pubkey FROM users WHERE pubkey <> '' ORDER BY id")?;
        Ok(stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn setting(&self, key: &str) -> Result<Option<Vec<u8>>> {
        Ok(self
            .conn
            .query_row(
                "SELECT value FROM settings WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn set_setting(&mut self, key: &str, value: &[u8]) -> Result<()> {
        self.conn.execute(
            "INSERT INTO settings(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn is_empty(&self) -> Result<bool> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))?;
        Ok(count == 0)
    }

    /// Every logged event, oldest first -- for `newbbs log`.
    pub fn all_events(&self, limit: usize) -> Result<Vec<Event>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, at, actor, payload FROM events ORDER BY id ASC LIMIT ?1")?;
        stmt.query_map(params![limit as i64], row_to_event)?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .collect()
    }
}

// ---------------------------------------------------------------------------
// projection application
// ---------------------------------------------------------------------------

/// Fold one event into the projection tables. Runs inside the same transaction
/// that appended the event.
fn apply_projection(tx: &rusqlite::Transaction<'_>, event: &Event) -> Result<()> {
    use EventKind::*;
    match &event.kind {
        UserCreated { user, name, pubkey } => {
            tx.execute(
                "INSERT INTO users(id, name, bio, pubkey, joined_at, last_seen)
                 VALUES (?1, ?2, '', ?3, ?4, ?4)",
                params![user, name, pubkey, event.at],
            )?;
        }
        UserRenamed { user, name } => {
            tx.execute(
                "UPDATE users SET name = ?2 WHERE id = ?1",
                params![user, name],
            )?;
        }
        BioSet { user, bio } => {
            tx.execute("UPDATE users SET bio = ?2 WHERE id = ?1", params![user, bio])?;
        }
        RoleCreated {
            role,
            name,
            color,
            priority,
            hoisted,
            admin,
        } => {
            tx.execute(
                "INSERT INTO roles(id, name, color, priority, hoisted, admin)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![role, name, color, priority, hoisted, admin],
            )?;
        }
        RoleGranted { user, role } => {
            tx.execute(
                "INSERT OR IGNORE INTO user_roles(user_id, role_id) VALUES (?1, ?2)",
                params![user, role],
            )?;
        }
        RoleRevoked { user, role } => {
            tx.execute(
                "DELETE FROM user_roles WHERE user_id = ?1 AND role_id = ?2",
                params![user, role],
            )?;
        }
        ConvCreated { conv, kind, name } => {
            tx.execute(
                "INSERT INTO convs(id, kind, name, topic, created_at)
                 VALUES (?1, ?2, ?3, '', ?4)",
                params![conv, conv_kind_code(*kind), name, event.at],
            )?;
        }
        ConvRemoved { conv } => {
            tx.execute("DELETE FROM conv_members WHERE conv_id = ?1", params![conv])?;
            tx.execute("DELETE FROM messages WHERE conv_id = ?1", params![conv])?;
            tx.execute("DELETE FROM convs WHERE id = ?1", params![conv])?;
        }
        TopicSet { conv, topic } => {
            tx.execute(
                "UPDATE convs SET topic = ?2 WHERE id = ?1",
                params![conv, topic],
            )?;
        }
        MemberJoined { conv, user } => {
            tx.execute(
                "INSERT OR IGNORE INTO conv_members(conv_id, user_id, joined_at)
                 VALUES (?1, ?2, ?3)",
                params![conv, user, event.at],
            )?;
        }
        MemberLeft { conv, user } => {
            tx.execute(
                "DELETE FROM conv_members WHERE conv_id = ?1 AND user_id = ?2",
                params![conv, user],
            )?;
        }
        MessageSent { conv, author, body } => {
            tx.execute(
                "INSERT INTO messages(id, conv_id, author, body, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![event.id.as_bytes().as_slice(), conv, author, body, event.at],
            )?;
        }
        MessageEdited { message, body } => {
            tx.execute(
                "UPDATE messages SET body = ?2, edited_at = ?3 WHERE id = ?1",
                params![message.as_bytes().as_slice(), body, event.at],
            )?;
        }
        MessageDeleted { message } => {
            tx.execute(
                "UPDATE messages SET deleted = 1, body = '' WHERE id = ?1",
                params![message.as_bytes().as_slice()],
            )?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// row mapping
// ---------------------------------------------------------------------------

/// A UUIDv7 whose embedded timestamp is `at`, so ordering by id is ordering by
/// time even for events given an explicit timestamp.
fn uuid_at(at: Millis) -> Uuid {
    let secs = at.div_euclid(1000) as u64;
    let nanos = (at.rem_euclid(1000) * 1_000_000) as u32;
    Uuid::new_v7(uuid::Timestamp::from_unix(uuid::NoContext, secs, nanos))
}

fn encode_tag(tag: Tag) -> (i64, Option<i64>) {
    match tag {
        Tag::Conv(id) => (0, Some(id)),
        Tag::User(id) => (1, Some(id)),
        Tag::Presence => (2, None),
        Tag::Roles => (3, None),
        Tag::Directory => (4, None),
    }
}

fn conv_kind_code(kind: ConvKind) -> i64 {
    match kind {
        ConvKind::Channel => 0,
        ConvKind::Dm => 1,
        ConvKind::Group => 2,
    }
}

fn conv_kind_from_code(code: i64) -> ConvKind {
    match code {
        1 => ConvKind::Dm,
        2 => ConvKind::Group,
        _ => ConvKind::Channel,
    }
}

fn uuid_from_row(bytes: Vec<u8>) -> Result<Uuid> {
    let array: [u8; 16] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("uuid column was {} bytes, expected 16", bytes.len()))?;
    Ok(Uuid::from_bytes(array))
}

fn row_to_event(row: &rusqlite::Row<'_>) -> rusqlite::Result<Result<Event>> {
    let id: Vec<u8> = row.get(0)?;
    let at: i64 = row.get(1)?;
    let actor: Option<i64> = row.get(2)?;
    let payload: Vec<u8> = row.get(3)?;
    Ok((|| {
        Ok(Event {
            id: uuid_from_row(id)?,
            at,
            actor,
            kind: postcard::from_bytes(&payload).context("decoding event payload")?,
        })
    })())
}

fn row_to_message(row: &rusqlite::Row<'_>) -> rusqlite::Result<Result<Message>> {
    let id: Vec<u8> = row.get(0)?;
    let conv: i64 = row.get(1)?;
    let author: i64 = row.get(2)?;
    let body: String = row.get(3)?;
    let created_at: i64 = row.get(4)?;
    let edited_at: Option<i64> = row.get(5)?;
    Ok(uuid_from_row(id).map(|id| Message {
        id,
        conv,
        author,
        body,
        created_at,
        edited_at,
    }))
}

fn row_to_user(row: &rusqlite::Row<'_>) -> rusqlite::Result<User> {
    Ok(User {
        id: row.get(0)?,
        name: row.get(1)?,
        bio: row.get(2)?,
        joined_at: row.get(3)?,
        last_seen: row.get(4)?,
        online: false,
        roles: Vec::new(),
    })
}

fn row_to_role(row: &rusqlite::Row<'_>) -> rusqlite::Result<Role> {
    let color: i64 = row.get(2)?;
    Ok(Role {
        id: row.get(0)?,
        name: row.get(1)?,
        color: color as u32,
        priority: row.get(3)?,
        hoisted: row.get(4)?,
        admin: row.get(5)?,
    })
}

fn row_to_conv(row: &rusqlite::Row<'_>) -> rusqlite::Result<Conversation> {
    let kind: i64 = row.get(1)?;
    Ok(Conversation {
        id: row.get(0)?,
        kind: conv_kind_from_code(kind),
        name: row.get(2)?,
        topic: row.get(3)?,
        created_at: row.get(4)?,
    })
}

pub use bootstrap::{ADMIN_NAME, bootstrap};
