//! Command mode (`:`) -- the verbs that have no natural keybinding.

use anyhow::Result;

use super::{App, Overlay};
use crate::bus::Subscription;
use crate::model::*;

pub async fn run(app: &mut App, line: &str, sub: &Subscription) -> Result<()> {
    let line = line.trim();
    if line.is_empty() {
        return Ok(());
    }
    let (verb, rest) = match line.split_once(char::is_whitespace) {
        Some((verb, rest)) => (verb, rest.trim()),
        None => (line, ""),
    };
    let args: Vec<&str> = rest.split_whitespace().collect();

    match verb {
        "q" | "quit" => app.quit(),
        "help" | "h" => app.overlay = Some(Overlay::Help),
        "reload" => {
            app.reload().await?;
            app.set_status("reloaded");
        }
        "theme" => theme(app, &args),
        "info" => info(app, &args).await?,
        "nick" => nick(app, &args).await?,
        "bio" => bio(app, rest).await?,
        "topic" => topic(app, rest).await?,
        "join" => join(app, &args, sub).await?,
        "dm" => dm(app, &args, sub).await?,
        "group" => group(app, &args, sub).await?,
        "invite" => invite(app, rest).await?,
        "mkchan" => mkchan(app, &args, sub).await?,
        "rmchan" => rmchan(app, &args).await?,
        "mkrole" => mkrole(app, &args).await?,
        "grant" => grant(app, &args, true).await?,
        "revoke" => grant(app, &args, false).await?,
        "ban" => app.set_error("ban needs the ssh auth layer, which is not built yet"),
        other => app.set_error(format!("unknown command :{other} -- try :help")),
    }
    Ok(())
}

fn theme(app: &mut App, args: &[&str]) {
    match args.first() {
        None => {
            let names = super::theme::Theme::names().join(", ");
            app.set_status(format!("theme is {} -- available: {names}", app.theme.name));
        }
        Some(name) => {
            if app.theme.set(name) {
                app.set_status(format!("theme set to {name}"));
            } else {
                app.set_error(format!("no theme called {name}"));
            }
        }
    }
}

async fn info(app: &mut App, args: &[&str]) -> Result<()> {
    let name = match args.first() {
        Some(name) => name.trim_start_matches('@').to_string(),
        None => app.user_name(app.me()),
    };
    match app.bus().user_by_name(&name).await? {
        Some(user) => app.overlay = Some(Overlay::Profile(user)),
        None => app.set_error(format!("no user called {name}")),
    }
    Ok(())
}

async fn nick(app: &mut App, args: &[&str]) -> Result<()> {
    let Some(name) = args.first() else {
        return Ok(app.set_error("usage: :nick <name>"));
    };
    if app.bus().user_by_name(name).await?.is_some() {
        return Ok(app.set_error(format!("{name} is taken")));
    }
    let me = app.me();
    app.bus()
        .commit(
            Some(me),
            EventKind::UserRenamed {
                user: me,
                name: (*name).to_string(),
            },
        )
        .await?;
    app.set_status(format!("you are now {name}"));
    Ok(())
}

async fn bio(app: &mut App, rest: &str) -> Result<()> {
    let me = app.me();
    app.bus()
        .commit(
            Some(me),
            EventKind::BioSet {
                user: me,
                bio: rest.to_string(),
            },
        )
        .await?;
    app.set_status("bio updated");
    Ok(())
}

async fn topic(app: &mut App, rest: &str) -> Result<()> {
    let Some(conv) = app.active_conv().map(|c| c.id) else {
        return Ok(app.set_error("no conversation selected"));
    };
    let me = app.me();
    app.bus()
        .commit(
            Some(me),
            EventKind::TopicSet {
                conv,
                topic: rest.to_string(),
            },
        )
        .await?;
    app.set_status("topic set");
    Ok(())
}

async fn join(app: &mut App, args: &[&str], sub: &Subscription) -> Result<()> {
    let Some(name) = args.first() else {
        return Ok(app.set_error("usage: :join #channel"));
    };
    let wanted = name.trim_start_matches('#');
    match app
        .convs
        .iter()
        .position(|c| c.kind == ConvKind::Channel && c.name == wanted)
    {
        Some(index) => app.select(index, sub).await?,
        None => app.set_error(format!("no channel called #{wanted}")),
    }
    Ok(())
}

async fn dm(app: &mut App, args: &[&str], sub: &Subscription) -> Result<()> {
    let Some(name) = args.first() else {
        return Ok(app.set_error("usage: :dm <user>"));
    };
    let name = name.trim_start_matches('@');
    let Some(other) = app.bus().user_by_name(name).await? else {
        return Ok(app.set_error(format!("no user called {name}")));
    };
    if other.id == app.me() {
        return Ok(app.set_error("you cannot DM yourself"));
    }

    // Reuse the existing DM with this person if there is one.
    for (index, conv) in app.convs.iter().enumerate() {
        if conv.kind != ConvKind::Dm {
            continue;
        }
        let members = app.bus().members(conv.id).await?;
        if members.len() == 2 && members.contains(&other.id) && members.contains(&app.me()) {
            return app.select(index, sub).await;
        }
    }

    let me = app.me();
    let conv = app.bus().alloc("conv").await?;
    app.bus()
        .commit(
            Some(me),
            EventKind::ConvCreated {
                conv,
                kind: ConvKind::Dm,
                name: String::new(),
            },
        )
        .await?;
    for user in [me, other.id] {
        app.bus()
            .commit(Some(me), EventKind::MemberJoined { conv, user })
            .await?;
    }
    app.reload().await?;
    if let Some(index) = app.convs.iter().position(|c| c.id == conv) {
        app.select(index, sub).await?;
    }
    Ok(())
}

async fn group(app: &mut App, args: &[&str], sub: &Subscription) -> Result<()> {
    if args.len() < 2 {
        return Ok(app.set_error("usage: :group <name> <user> [user...]"));
    }
    let me = app.me();
    let mut members = vec![me];
    for name in &args[1..] {
        match app.bus().user_by_name(name.trim_start_matches('@')).await? {
            Some(user) if !members.contains(&user.id) => members.push(user.id),
            Some(_) => {}
            None => return Ok(app.set_error(format!("no user called {name}"))),
        }
    }
    let conv = app.bus().alloc("conv").await?;
    app.bus()
        .commit(
            Some(me),
            EventKind::ConvCreated {
                conv,
                kind: ConvKind::Group,
                name: args[0].to_string(),
            },
        )
        .await?;
    for user in members {
        app.bus()
            .commit(Some(me), EventKind::MemberJoined { conv, user })
            .await?;
    }
    app.reload().await?;
    if let Some(index) = app.convs.iter().position(|c| c.id == conv) {
        app.select(index, sub).await?;
    }
    Ok(())
}

/// `:invite <name> <ssh public key>` -- the key runs to the end of the line,
/// comment and all, so it can be pasted straight from a .pub file.
async fn invite(app: &mut App, rest: &str) -> Result<()> {
    if !require_admin(app) {
        return Ok(());
    }
    let Some((name, key)) = rest.split_once(char::is_whitespace) else {
        return Ok(app.set_error("usage: :invite <name> <ssh public key>"));
    };
    let me = app.me();
    match crate::ssh::invite(app.bus(), Some(me), name, key).await {
        Ok(fingerprint) => {
            app.reload().await?;
            app.set_status(format!("invited {name} ({fingerprint})"));
        }
        Err(err) => app.set_error(format!("{err}")),
    }
    Ok(())
}

async fn mkchan(app: &mut App, args: &[&str], sub: &Subscription) -> Result<()> {
    if !require_admin(app) {
        return Ok(());
    }
    let Some(name) = args.first() else {
        return Ok(app.set_error("usage: :mkchan <name>"));
    };
    let name = name.trim_start_matches('#').to_string();
    if app.bus().conversation_by_name(&name).await?.is_some() {
        return Ok(app.set_error(format!("#{name} already exists")));
    }
    let me = app.me();
    let conv = app.bus().alloc("conv").await?;
    app.bus()
        .commit(
            Some(me),
            EventKind::ConvCreated {
                conv,
                kind: ConvKind::Channel,
                name: name.clone(),
            },
        )
        .await?;
    app.reload().await?;
    if let Some(index) = app.convs.iter().position(|c| c.id == conv) {
        app.select(index, sub).await?;
    }
    app.set_status(format!("created #{name}"));
    Ok(())
}

async fn rmchan(app: &mut App, args: &[&str]) -> Result<()> {
    if !require_admin(app) {
        return Ok(());
    }
    let Some(name) = args.first() else {
        return Ok(app.set_error("usage: :rmchan <name>"));
    };
    let name = name.trim_start_matches('#');
    let Some(conv) = app.bus().conversation_by_name(name).await? else {
        return Ok(app.set_error(format!("no channel called #{name}")));
    };
    let me = app.me();
    app.bus()
        .commit(Some(me), EventKind::ConvRemoved { conv: conv.id })
        .await?;
    app.active = 0;
    app.reload().await?;
    app.set_status(format!("removed #{name}"));
    Ok(())
}

async fn mkrole(app: &mut App, args: &[&str]) -> Result<()> {
    if !require_admin(app) {
        return Ok(());
    }
    if args.len() < 2 {
        return Ok(app.set_error("usage: :mkrole <name> <#rrggbb> [priority] [hoist]"));
    }
    let Ok(color) = u32::from_str_radix(args[1].trim_start_matches('#'), 16) else {
        return Ok(app.set_error("colour must look like #61afef"));
    };
    let priority = args.get(2).and_then(|p| p.parse().ok()).unwrap_or(10);
    let hoisted = args.get(3).is_some_and(|h| *h == "hoist" || *h == "true");
    let me = app.me();
    let role = app.bus().alloc("role").await?;
    app.bus()
        .commit(
            Some(me),
            EventKind::RoleCreated {
                role,
                name: args[0].to_string(),
                color,
                priority,
                hoisted,
                admin: false,
            },
        )
        .await?;
    app.set_status(format!("created role {}", args[0]));
    Ok(())
}

async fn grant(app: &mut App, args: &[&str], granting: bool) -> Result<()> {
    if !require_admin(app) {
        return Ok(());
    }
    if args.len() < 2 {
        let verb = if granting { "grant" } else { "revoke" };
        return Ok(app.set_error(format!("usage: :{verb} <user> <role>")));
    }
    let Some(user) = app
        .bus()
        .user_by_name(args[0].trim_start_matches('@'))
        .await?
    else {
        return Ok(app.set_error(format!("no user called {}", args[0])));
    };
    let roles = app.bus().roles().await?;
    let Some(role) = roles.iter().find(|r| r.name == args[1]) else {
        return Ok(app.set_error(format!("no role called {}", args[1])));
    };
    let me = app.me();
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
    app.bus().commit(Some(me), kind).await?;
    let verb = if granting { "granted" } else { "revoked" };
    app.set_status(format!("{verb} {} for {}", role.name, user.name));
    Ok(())
}

/// Admin commands are gated on holding a role marked admin.
fn require_admin(app: &mut App) -> bool {
    let is_admin = app
        .user(app.me())
        .is_some_and(|u| u.roles.iter().any(|r| r.admin));
    if !is_admin {
        app.set_error("that command needs an admin role");
    }
    is_admin
}
