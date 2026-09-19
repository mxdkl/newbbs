//! Tunables that were decided up front, gathered in one place.

use std::path::PathBuf;

/// A UUIDv7 is 16 bytes. An encoded payload smaller than that is cheaper to
/// ship whole than to ship an id the receiver has to resolve, so the bus
/// inlines it.
pub const INLINE_MAX_BYTES: usize = 16;

/// Per-session delivery queue. On overflow the session is handed a resync
/// marker instead of blocking the bus.
pub const SESSION_QUEUE: usize = 256;

/// Reconnect replays at most this many events before falling back to a plain
/// state read and reporting a gap.
pub const REPLAY_EVENT_CAP: usize = 500;

/// ...and never reaches further back than this.
pub const REPLAY_AGE_MILLIS: i64 = 7 * 24 * 60 * 60 * 1000;

/// Consecutive messages from one author group under a single timestamp until
/// this much time passes (or the day changes).
pub const GROUP_GAP_MILLIS: i64 = 5 * 60 * 1000;

/// How many messages a conversation view holds before older ones are dropped
/// from memory and re-fetched on scroll.
pub const MESSAGE_WINDOW: usize = 500;

/// How long the status bar flashes red after a command is refused.
pub const ERROR_FLASH: std::time::Duration = std::time::Duration::from_millis(1000);

/// Bumped only if the on-disk postcard encoding of an event has to change
/// shape incompatibly.
pub const EVENT_SCHEMA_VERSION: i64 = 1;

/// `$XDG_DATA_HOME/newbbs` (or `~/.local/share/newbbs`).
pub fn data_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME")
        && !xdg.is_empty()
    {
        return PathBuf::from(xdg).join("newbbs");
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".local/share/newbbs")
}

pub fn default_db_path() -> PathBuf {
    data_dir().join("newbbs.db")
}

/// The TUI owns the terminal, so logs go to a file.
pub fn log_dir() -> PathBuf {
    data_dir().join("logs")
}

// ---------------------------------------------------------------------------
// the splash shown to an ssh session before the board
// ---------------------------------------------------------------------------

/// Settings keys. Both fall back to the defaults below when unset, so a fresh
/// board looks right without any setup.
pub const SETTING_ART: &str = "splash.art";
pub const SETTING_MOTD: &str = "splash.motd";

pub const DEFAULT_ART: &str = concat!(
    " _____           _____ _____ _____ \n",
    "|   | |___ _ _ _| __  | __  |   __|\n",
    "| | | | -_| | | | __ -| __ -|__   |\n",
    "|_|___|___|_____|_____|_____|_____|",
);

pub const DEFAULT_MOTD: &str = "powered by newbbs";
