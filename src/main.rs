//! newbbs -- a BBS you reach over ssh.

mod bus;
mod config;
mod db;
mod model;
mod ssh;
mod ui;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::net::SocketAddr;
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "newbbs", version, about = "A BBS you reach over ssh")]
struct Cli {
    /// Database file (default: $XDG_DATA_HOME/newbbs/newbbs.db).
    #[arg(long, global = true, value_name = "PATH")]
    db: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the server.
    Serve {
        /// Address to listen on for ssh.
        #[arg(long, default_value = "0.0.0.0:2222", value_name = "ADDR")]
        listen: SocketAddr,

        /// Also attach a TUI session on this terminal, as the admin account.
        #[arg(long)]
        ui: bool,
    },
    /// Create an account bound to an ssh public key.
    Invite {
        /// The name they will appear as.
        name: String,
        /// Their public key, or a path to a .pub file.
        key: String,
    },
    /// Render one frame of the TUI to stdout as text, without needing a tty.
    Snapshot {
        #[arg(long, default_value_t = 100)]
        width: u16,
        #[arg(long, default_value_t = 32)]
        height: u16,
        /// Draw an overlay on top: help, switcher or profile.
        #[arg(long)]
        overlay: Option<String>,
    },
    /// Dump the event log as JSON (the log itself is postcard on disk).
    Log {
        /// How many events, oldest first.
        #[arg(long, default_value_t = 200)]
        limit: usize,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let db_path = cli.db.clone().unwrap_or_else(config::default_db_path);

    // The TUI owns the terminal, so logs go to a file either way.
    let log_dir = config::log_dir();
    std::fs::create_dir_all(&log_dir).with_context(|| format!("creating {}", log_dir.display()))?;
    let appender = tracing_appender::rolling::daily(&log_dir, "newbbs.log");
    let (writer, _guard) = tracing_appender::non_blocking(appender);
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with_writer(writer)
        .with_ansi(false)
        .init();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        match cli.command {
            Command::Serve { listen, ui } => serve(db_path, listen, ui).await,
            Command::Invite { name, key } => invite(db_path, name, key).await,
            Command::Snapshot {
                width,
                height,
                overlay,
            } => snapshot(db_path, width, height, overlay).await,
            Command::Log { limit } => dump_log(db_path, limit).await,
        }
    })
}

async fn serve(db_path: PathBuf, listen: SocketAddr, with_ui: bool) -> Result<()> {
    let (bus, owner) = bus::Bus::start(db_path)?;
    let admin = ensure_seeded(&bus).await?;

    let listener = tokio::spawn({
        let bus = bus.clone();
        async move { ssh::serve(bus, listen).await }
    });

    let result = if with_ui {
        // The local session runs alongside the listener; when it quits, so
        // does the server.
        ui::run(bus.clone(), admin).await
    } else {
        // Nothing else to do here -- wait on the listener.
        match listener.await {
            Ok(result) => result,
            Err(err) => Err(err.into()),
        }
    };

    bus.shutdown();
    let _ = owner.await;
    result
}

async fn invite(db_path: PathBuf, name: String, key: String) -> Result<()> {
    let (bus, owner) = bus::Bus::start(db_path)?;
    let key = ssh::read_key_argument(&key)?;
    let result = ssh::invite(&bus, None, &name, &key).await;
    bus.shutdown();
    let _ = owner.await;

    let fingerprint = result?;
    println!("invited {name}");
    println!("  key {fingerprint}");
    // The listener's port belongs to `serve`, not to this command, so don't
    // claim one here.
    println!("  they connect as: ssh -p <port> {}@<host>", ssh::LOGIN_NAME);
    Ok(())
}

async fn snapshot(
    db_path: PathBuf,
    width: u16,
    height: u16,
    overlay: Option<String>,
) -> Result<()> {
    let (bus, owner) = bus::Bus::start(db_path)?;
    let admin = ensure_seeded(&bus).await?;
    let text = ui::snapshot(bus.clone(), admin, width, height, overlay.as_deref()).await?;
    print!("{text}");
    bus.shutdown();
    let _ = owner.await;
    Ok(())
}

async fn dump_log(db_path: PathBuf, limit: usize) -> Result<()> {
    let (bus, owner) = bus::Bus::start(db_path)?;
    for event in bus.log_dump(limit).await? {
        println!(
            "{}",
            serde_json::json!({
                "id": event.id.to_string(),
                "at": chrono::DateTime::from_timestamp_millis(event.at)
                    .map(|t| t.to_rfc3339())
                    .unwrap_or_default(),
                "actor": event.actor,
                "kind": event.kind,
            })
        );
    }
    bus.shutdown();
    let _ = owner.await;
    Ok(())
}

/// The account the local `--ui` session logs in as. The bus seeds demo content
/// into an empty database on startup, so this is always present.
async fn ensure_seeded(bus: &bus::Bus) -> Result<model::UserId> {
    match bus.user_by_name("admin").await? {
        Some(admin) => Ok(admin.id),
        None => anyhow::bail!("no admin account in this database"),
    }
}
