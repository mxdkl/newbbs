//! What a brand new database gets: the bare minimum for the board to be
//! usable, and nothing else.
//!
//! No demo users, no sample conversation. Just the pieces that cannot be
//! created from inside the BBS without already existing -- an admin role, an
//! account holding it, and somewhere to talk.

use anyhow::Result;

use super::Db;
use crate::model::*;

/// The account the `console` subcommand attaches as. It has no ssh key and
/// never gets one, so it can only ever be used from the machine running the
/// server -- if admin is online, someone is sitting at the box.
pub const ADMIN_NAME: &str = "admin";

/// Set up an empty database. Returns the id of the admin account.
pub fn bootstrap(db: &mut Db) -> Result<UserId> {
    // Each event gets its own millisecond so the log reads in the order it
    // was written rather than however the uuids happened to fall.
    let base = now_millis();
    let mut tick = 0;
    let mut at = || {
        tick += 1;
        base + tick
    };

    let sysop = db.alloc("role")?;
    db.commit_at(
        None,
        EventKind::RoleCreated {
            role: sysop,
            name: "sysop".into(),
            color: 0xe06c75,
            priority: 100,
            hoisted: true,
            admin: true,
        },
        at(),
    )?;

    let admin = db.alloc("user")?;
    db.commit_at(
        None,
        EventKind::UserCreated {
            user: admin,
            name: ADMIN_NAME.into(),
            // Deliberately keyless: `user_keys` skips empty keys, so no ssh
            // connection can ever authenticate as this account.
            pubkey: String::new(),
        },
        at(),
    )?;
    db.commit_at(
        None,
        EventKind::RoleGranted {
            user: admin,
            role: sysop,
        },
        at(),
    )?;

    let general = db.alloc("conv")?;
    db.commit_at(
        None,
        EventKind::ConvCreated {
            conv: general,
            kind: ConvKind::Channel,
            name: "general".into(),
        },
        at(),
    )?;

    tracing::info!("bootstrapped an empty board");
    Ok(admin)
}
