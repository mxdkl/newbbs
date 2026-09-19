//! Core domain types, the event vocabulary, and the tags events are routed by.
//!
//! The event log is the source of truth: every state change in the BBS is an
//! [`Event`] appended to the log, and the projection tables are derived from it.
//! [`Signal`]s are the ephemeral counterpart -- observations that are published
//! on the bus but never persisted, because they do not change state.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub type UserId = i64;
pub type ConvId = i64;
pub type RoleId = i64;

/// A message is identified by the uuid of the `MessageSent` event that created
/// it, so message ids inherit UUIDv7's chronological ordering for free.
pub type MessageId = Uuid;

/// Milliseconds since the unix epoch, UTC.
pub type Millis = i64;

// ---------------------------------------------------------------------------
// entities (projections of the log)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    pub id: UserId,
    pub name: String,
    pub bio: String,
    pub joined_at: Millis,
    pub last_seen: Millis,
    pub online: bool,
    /// Sorted by descending priority; the first one supplies the name colour.
    pub roles: Vec<Role>,
}

/// A purely cosmetic (for now) grouping that colours a name and sections the
/// member list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Role {
    pub id: RoleId,
    pub name: String,
    /// Packed 0xRRGGBB. Degraded to whatever the terminal supports at render.
    pub color: u32,
    /// Higher wins when a user holds several roles.
    pub priority: i64,
    /// Roles above this line get their own member-list section.
    pub hoisted: bool,
    pub admin: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConvKind {
    Channel,
    Dm,
    Group,
}

/// Channels, DMs and group DMs are all conversations; only the kind and the
/// membership rules differ.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conversation {
    pub id: ConvId,
    pub kind: ConvKind,
    pub name: String,
    pub topic: String,
    pub created_at: Millis,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub id: MessageId,
    pub conv: ConvId,
    pub author: UserId,
    pub body: String,
    pub created_at: Millis,
    pub edited_at: Option<Millis>,
}

// ---------------------------------------------------------------------------
// routing tags
// ---------------------------------------------------------------------------

/// Every event carries a small set of tags; a session receives the event if it
/// subscribes to any one of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Tag {
    /// Anything happening inside one conversation.
    Conv(ConvId),
    /// Anything about one user (profile, roles, membership).
    User(UserId),
    /// Online/offline transitions, server-wide.
    Presence,
    /// Role definitions changed.
    Roles,
    /// The conversation list itself changed.
    Directory,
}

// ---------------------------------------------------------------------------
// events (persisted, source of truth)
// ---------------------------------------------------------------------------

/// A logged state change. `id` is UUIDv7, so events sort chronologically by id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub id: Uuid,
    pub at: Millis,
    pub actor: Option<UserId>,
    pub kind: EventKind,
}

impl Event {
    pub fn tags(&self) -> Vec<Tag> {
        self.kind.tags()
    }
}

/// The event vocabulary.
///
/// These variants are written to disk in postcard, which is not
/// self-describing: new variants may only be appended at the end, and existing
/// ones must never be reordered or removed. `schema_version` on the row is the
/// escape hatch if that ever has to change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EventKind {
    UserCreated {
        user: UserId,
        name: String,
        pubkey: String,
    },
    UserRenamed {
        user: UserId,
        name: String,
    },
    BioSet {
        user: UserId,
        bio: String,
    },
    RoleCreated {
        role: RoleId,
        name: String,
        color: u32,
        priority: i64,
        hoisted: bool,
        admin: bool,
    },
    RoleGranted {
        user: UserId,
        role: RoleId,
    },
    RoleRevoked {
        user: UserId,
        role: RoleId,
    },
    ConvCreated {
        conv: ConvId,
        kind: ConvKind,
        name: String,
    },
    ConvRemoved {
        conv: ConvId,
    },
    TopicSet {
        conv: ConvId,
        topic: String,
    },
    MemberJoined {
        conv: ConvId,
        user: UserId,
    },
    MemberLeft {
        conv: ConvId,
        user: UserId,
    },
    MessageSent {
        conv: ConvId,
        author: UserId,
        body: String,
    },
    MessageEdited {
        message: MessageId,
        body: String,
    },
    MessageDeleted {
        message: MessageId,
    },
    /// Removing a role also strips it from everyone holding it.
    RoleRemoved {
        role: RoleId,
    },
}

impl EventKind {
    pub fn tags(&self) -> Vec<Tag> {
        use EventKind::*;
        match self {
            UserCreated { user, .. } => vec![Tag::User(*user), Tag::Presence],
            UserRenamed { user, .. } | BioSet { user, .. } => vec![Tag::User(*user)],
            RoleCreated { .. } => vec![Tag::Roles],
            RoleGranted { user, .. } | RoleRevoked { user, .. } => {
                vec![Tag::User(*user), Tag::Roles]
            }
            ConvCreated { conv, .. } | ConvRemoved { conv } => {
                vec![Tag::Conv(*conv), Tag::Directory]
            }
            TopicSet { conv, .. } => vec![Tag::Conv(*conv)],
            MemberJoined { conv, user } | MemberLeft { conv, user } => {
                vec![Tag::Conv(*conv), Tag::User(*user), Tag::Directory]
            }
            MessageSent { conv, author, .. } => vec![Tag::Conv(*conv), Tag::User(*author)],
            MessageEdited { .. } | MessageDeleted { .. } => Vec::new(),
            RoleRemoved { .. } => vec![Tag::Roles],
        }
    }
}

// ---------------------------------------------------------------------------
// signals (ephemeral, never logged)
// ---------------------------------------------------------------------------

/// Published on the bus but not appended to the log: nothing here changes
/// state, so replaying it would be meaningless. These payloads are tiny, which
/// is exactly when the bus ships them inline instead of by uuid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Signal {
    Presence { user: UserId, online: bool },
    Typing { user: UserId, conv: ConvId },
    /// A read passed through the bus. Subscribers can observe access patterns
    /// without the log filling up with non-mutations.
    Read { user: UserId, conv: Option<ConvId> },
}

impl Signal {
    pub fn tags(&self) -> Vec<Tag> {
        match self {
            Signal::Presence { user, .. } => vec![Tag::User(*user), Tag::Presence],
            Signal::Typing { conv, .. } => vec![Tag::Conv(*conv)],
            Signal::Read { conv, .. } => conv.map(|c| vec![Tag::Conv(c)]).unwrap_or_default(),
        }
    }
}

pub fn now_millis() -> Millis {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
