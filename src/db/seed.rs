//! Demo content for a fresh database: enough users, roles, channels, DMs and
//! backdated messages to exercise grouping, day separators, markdown, flair,
//! member-list sections and scrollback.

use anyhow::Result;

use super::Db;
use crate::model::*;

const MINUTE: Millis = 60 * 1000;
const HOUR: Millis = 60 * MINUTE;

/// Setup events (roles, accounts, channel creation) all happen "at once", but
/// each gets its own millisecond so the log reads in the order it was written.
struct Clock {
    base: Millis,
    tick: Millis,
}

impl Clock {
    fn setup(&mut self) -> Millis {
        self.tick += 1;
        self.base + self.tick
    }

    fn at(&self, minutes: Millis) -> Millis {
        self.base + minutes * MINUTE
    }
}

/// Populate an empty database. Returns the id of the admin account.
pub fn seed_demo(db: &mut Db) -> Result<UserId> {
    // Backdate the whole conversation so there is a day boundary in view.
    let mut clock = Clock {
        base: now_millis() - 26 * HOUR,
        tick: 0,
    };

    let admin_role = role(db, &mut clock, "sysop", 0xe06c75, 100, true, true)?;
    let mod_role = role(db, &mut clock, "op", 0x61afef, 50, true, false)?;
    let regular_role = role(db, &mut clock, "regular", 0x98c379, 10, false, false)?;

    let admin = user(
        db,
        &mut clock,
        "admin",
        "Runs this box. Ping me for an invite.",
        Some(admin_role),
    )?;
    let alice = user(
        db,
        &mut clock,
        "alice",
        "Rust, synths, and long walks to the fridge.",
        Some(mod_role),
    )?;
    let bob = user(
        db,
        &mut clock,
        "bob",
        "just here for the gossip",
        Some(regular_role),
    )?;
    let carol = user(db, &mut clock, "carol", "", Some(regular_role))?;
    let dave = user(db, &mut clock, "dave", "new here", None)?;

    let general = conv(db, &mut clock, ConvKind::Channel, "general", "anything goes")?;
    let dev = conv(
        db,
        &mut clock,
        ConvKind::Channel,
        "dev",
        "newbbs development",
    )?;
    let random = conv(db, &mut clock, ConvKind::Channel, "random", "")?;

    let group = conv(db, &mut clock, ConvKind::Group, "weekend-plans", "")?;
    join(db, &mut clock, group, &[admin, alice, bob])?;
    let dm_alice = conv(db, &mut clock, ConvKind::Dm, "", "")?;
    join(db, &mut clock, dm_alice, &[admin, alice])?;
    let dm_bob = conv(db, &mut clock, ConvKind::Dm, "", "")?;
    join(db, &mut clock, dm_bob, &[admin, bob])?;

    // (conversation, author, minutes after base, body)
    let script: &[(ConvId, UserId, Millis, &str)] = &[
        (general, alice, 5, "morning all"),
        (general, alice, 6, "the board is back up after last night's reboot"),
        (general, bob, 12, "nice. did the *event log* survive?"),
        (
            general,
            alice,
            14,
            "all of it. it's append-only, there's nothing to corrupt",
        ),
        (general, carol, 45, "is there a keybinding cheatsheet anywhere?"),
        (
            general,
            admin,
            52,
            "`:help` in command mode. it's the short list for now",
        ),
        (general, dave, 90, "hi, just got my invite. what is this place"),
        (
            general,
            alice,
            95,
            "a BBS with delusions of being discord. welcome",
        ),
        (
            general,
            alice,
            96,
            "read the topic, then say something in #random",
        ),
        (general, bob, 300, "~~lunch~~ dinner?"),
        // ... and the next day
        (general, carol, 25 * 60, "so did anyone actually eat"),
        (general, bob, 25 * 60 + 3, "i had cereal at 2am, it counts"),
        (
            general,
            admin,
            25 * 60 + 40,
            "new build is live. **vim keys**: `i` to talk, `esc` to scroll, `/` to jump",
        ),
        (
            general,
            alice,
            25 * 60 + 44,
            "the fuzzy switcher is the good part",
        ),
        (dev, admin, 30, "starting on the event bus today"),
        (
            dev,
            admin,
            31,
            "every read and write goes through it, so subscriptions are free",
        ),
        (dev, alice, 60, "what happens when a session can't keep up?"),
        (
            dev,
            admin,
            65,
            "bounded queue, and on overflow the session gets a resync marker instead of blocking everyone else",
        ),
        (dev, alice, 70, "> bounded queue\n\nsensible. what size?"),
        (
            dev,
            admin,
            72,
            "256 for now:\n\n```rust\npub const SESSION_QUEUE: usize = 256;\n```",
        ),
        (
            dev,
            bob,
            25 * 60 + 10,
            "does the 16-byte inline thing actually fire?",
        ),
        (
            dev,
            admin,
            25 * 60 + 15,
            "for presence and typing, yes. and for short ones. a long `MessageSent` doesn't fit, so it ships as a uuid",
        ),
        (
            random,
            bob,
            120,
            "this terminal renders _italics_ properly and i'm unreasonably happy about it",
        ),
        (random, carol, 130, "wait until you see the day separator"),
        (
            random,
            dave,
            25 * 60 + 100,
            "found the fuzzy finder. i'm never using a mouse again",
        ),
        (group, alice, 200, "saturday still on?"),
        (group, bob, 205, "yep"),
        (
            group,
            admin,
            25 * 60 + 5,
            "i'll bring the laptop, we can hack on the ssh layer",
        ),
        (
            dm_alice,
            alice,
            80,
            "can you bump my role? i want the blue name",
        ),
        (dm_alice, admin, 85, "done. try `:info alice`"),
        (dm_alice, alice, 86, "oh that's nice"),
        (dm_bob, bob, 25 * 60 + 30, "is the ssh server done yet"),
        (dm_bob, admin, 25 * 60 + 31, "no"),
    ];

    for (conv, author, minutes, body) in script {
        db.commit_at(
            Some(*author),
            EventKind::MessageSent {
                conv: *conv,
                author: *author,
                body: (*body).into(),
            },
            clock.at(*minutes),
        )?;
    }

    tracing::info!(users = 5, channels = 3, "seeded demo content");
    Ok(admin)
}

fn role(
    db: &mut Db,
    clock: &mut Clock,
    name: &str,
    color: u32,
    priority: i64,
    hoisted: bool,
    admin: bool,
) -> Result<RoleId> {
    let id = db.alloc("role")?;
    db.commit_at(
        None,
        EventKind::RoleCreated {
            role: id,
            name: name.into(),
            color,
            priority,
            hoisted,
            admin,
        },
        clock.setup(),
    )?;
    Ok(id)
}

fn user(
    db: &mut Db,
    clock: &mut Clock,
    name: &str,
    bio: &str,
    role: Option<RoleId>,
) -> Result<UserId> {
    let id = db.alloc("user")?;
    db.commit_at(
        None,
        EventKind::UserCreated {
            user: id,
            name: name.into(),
            pubkey: format!("ssh-ed25519 AAAAC3Nza-demo-{name}"),
        },
        clock.setup(),
    )?;
    if !bio.is_empty() {
        db.commit_at(
            Some(id),
            EventKind::BioSet {
                user: id,
                bio: bio.into(),
            },
            clock.setup(),
        )?;
    }
    if let Some(role) = role {
        db.commit_at(None, EventKind::RoleGranted { user: id, role }, clock.setup())?;
    }
    Ok(id)
}

fn conv(
    db: &mut Db,
    clock: &mut Clock,
    kind: ConvKind,
    name: &str,
    topic: &str,
) -> Result<ConvId> {
    let id = db.alloc("conv")?;
    db.commit_at(
        None,
        EventKind::ConvCreated {
            conv: id,
            kind,
            name: name.into(),
        },
        clock.setup(),
    )?;
    if !topic.is_empty() {
        db.commit_at(
            None,
            EventKind::TopicSet {
                conv: id,
                topic: topic.into(),
            },
            clock.setup(),
        )?;
    }
    Ok(id)
}

fn join(db: &mut Db, clock: &mut Clock, conv: ConvId, users: &[UserId]) -> Result<()> {
    for user in users {
        db.commit_at(
            None,
            EventKind::MemberJoined { conv, user: *user },
            clock.setup(),
        )?;
    }
    Ok(())
}
