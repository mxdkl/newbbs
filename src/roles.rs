//! Role operations shared by the BBS commands and the CLI, so both behave
//! identically and only differ in who is allowed to reach them.

use anyhow::{Result, bail};

use crate::bus::Bus;
use crate::model::{EventKind, Role, UserId};

/// Look a role up by exact name.
pub async fn find(bus: &Bus, name: &str) -> Result<Role> {
    match bus.roles().await?.into_iter().find(|r| r.name == name) {
        Some(role) => Ok(role),
        None => bail!("no role called {name}"),
    }
}

/// Grant or revoke, returning what to tell whoever asked.
pub async fn grant(
    bus: &Bus,
    actor: Option<UserId>,
    user: &str,
    role: &str,
    granting: bool,
) -> Result<String> {
    let user = match bus.user_by_name(user.trim_start_matches('@')).await? {
        Some(user) => user,
        None => bail!("no user called {user}"),
    };
    let role = find(bus, role).await?;

    let kind = if granting {
        EventKind::RoleGranted {
            user: user.id,
            role: role.id,
        }
    } else {
        EventKind::RoleRevoked {
            user: user.id,
            role: role.id,
        }
    };
    bus.commit(actor, kind).await?;

    let verb = if granting { "granted" } else { "revoked" };
    Ok(format!("{verb} {} for {}", role.name, user.name))
}

/// Remove a role. Holders lose it in the same transaction, so a name can
/// never be coloured by a role that no longer exists.
pub async fn remove(bus: &Bus, actor: Option<UserId>, name: &str) -> Result<String> {
    let role = find(bus, name).await?;
    let holders = bus
        .users()
        .await?
        .iter()
        .filter(|u| u.roles.iter().any(|r| r.id == role.id))
        .count();
    bus.commit(actor, EventKind::RoleRemoved { role: role.id })
        .await?;
    Ok(match holders {
        0 => format!("removed role {name}"),
        1 => format!("removed role {name}, revoked from 1 person"),
        n => format!("removed role {name}, revoked from {n} people"),
    })
}

/// Create a cosmetic role. Never admin -- the only admin role is the one
/// bootstrapped with the board.
pub async fn create(
    bus: &Bus,
    actor: Option<UserId>,
    name: &str,
    color_text: &str,
    priority: i64,
    hoisted: bool,
) -> Result<String> {
    if name.is_empty() || name.contains(char::is_whitespace) {
        bail!("a role name cannot be empty or contain spaces");
    }
    if bus.roles().await?.iter().any(|r| r.name == name) {
        bail!("there is already a role called {name}");
    }
    let color = parse_color(color_text)?;
    let role = bus.alloc("role").await?;
    bus.commit(
        actor,
        EventKind::RoleCreated {
            role,
            name: name.to_string(),
            color,
            priority,
            hoisted,
            admin: false,
        },
    )
    .await?;
    Ok(format!("created role {name}"))
}

/// Exactly six hex digits, with an optional leading `#`.
///
/// Without the length check `#abc` parses as 0x000abc -- near-black, nothing
/// like the `#aabbcc` the author meant.
pub fn parse_color(text: &str) -> Result<u32> {
    let digits = text.trim_start_matches('#');
    if digits.len() != 6 || !digits.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("colour must be six hex digits, like #61afef");
    }
    Ok(u32::from_str_radix(digits, 16).expect("six hex digits"))
}

#[cfg(test)]
mod tests {
    use super::parse_color;

    #[test]
    fn colours_must_be_six_digits() {
        assert_eq!(parse_color("#61afef").unwrap(), 0x61afef);
        assert_eq!(parse_color("61afef").unwrap(), 0x61afef);
        // The shorthand would otherwise silently become near-black.
        assert!(parse_color("#abc").is_err());
        assert!(parse_color("#1234567").is_err());
        assert!(parse_color("#zzzzzz").is_err());
        assert!(parse_color("").is_err());
    }
}
