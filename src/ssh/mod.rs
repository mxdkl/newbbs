//! The ssh front door.
//!
//! One process serves every session: russh terminates the connection, the key
//! identifies the account, and each session runs the same [`crate::ui::App`]
//! the `console` subcommand does -- rendering into the ssh channel instead of
//! a local terminal.
//!
//! Access is invite-only. A key that matches an account is let in; a key that
//! does not is shown its own fingerprint so its owner can ask to be added, and
//! then disconnected.

mod backend;
mod input;

use anyhow::{Context, Result};
use russh::keys::{Algorithm, HashAlg, PrivateKey, PublicKey, ssh_key};
use russh::server::{Auth, Config, Handler, Msg, Server, Session};
use russh::server::ChannelOpenHandle;
use russh::{Channel, ChannelId, Pty};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::{Mutex, mpsc};

use crate::bus::Bus;
use crate::model::UserId;
use crate::ui::{self, SessionEvent};
use backend::{ChannelWriter, PtySize, SshBackend};
use input::InputParser;

/// The ssh username everyone connects as. Identity comes from the key, so this
/// is just a fixed, documented way in: `ssh -p 2222 newbbs@host`.
pub const LOGIN_NAME: &str = "newbbs";

/// Where the host key lives. Keeping it in the database means the database
/// file is the entire server -- one thing to mount, one thing to back up.
const HOST_KEY_SETTING: &str = "ssh.host_key";

/// Switch the client's terminal into the state ratatui expects, and back.
const ENTER_SCREEN: &[u8] = b"\x1b[?1049h\x1b[?7l\x1b[?25l";
const LEAVE_SCREEN: &[u8] = b"\x1b[?25h\x1b[?7h\x1b[?1049l\x1b[0m";

/// Bind the listening socket.
///
/// Separate from [`serve`] so a port already in use is reported by the caller
/// rather than from inside a task.
pub async fn bind(listen: SocketAddr) -> Result<tokio::net::TcpListener> {
    tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("binding {listen}"))
}

pub async fn serve(bus: Bus, socket: tokio::net::TcpListener) -> Result<()> {
    let listen = socket.local_addr().context("reading the listening address")?;
    let host_key = host_key(&bus).await?;
    let fingerprint = host_key.public_key().fingerprint(HashAlg::Sha256);

    let config = Arc::new(Config {
        // Identity is the key and nothing else, so do not advertise methods
        // we have no implementation for.
        methods: russh::MethodSet::from(&[russh::MethodKind::PublicKey][..]),
        inactivity_timeout: Some(std::time::Duration::from_secs(60 * 60)),
        auth_rejection_time: std::time::Duration::from_secs(1),
        keys: vec![host_key],
        nodelay: true,
        ..Default::default()
    });

    let mut server = SshServer {
        bus,
        sessions: Arc::new(Sessions::default()),
    };
    tracing::info!(%listen, %fingerprint, "ssh listener started");
    eprintln!("newbbs listening on {listen}");
    eprintln!("  host key {fingerprint}");
    eprintln!("  connect with: ssh -p {} {LOGIN_NAME}@<host>", listen.port());
    server
        .run_on_socket(config, &socket)
        .await
        .context("running the ssh listener")
}

/// Load the host key, generating one the first time. A stable key means
/// clients never see a host-key-changed warning.
async fn host_key(bus: &Bus) -> Result<PrivateKey> {
    if let Some(stored) = bus.setting(HOST_KEY_SETTING).await? {
        let text = String::from_utf8(stored).context("host key is not valid utf-8")?;
        return PrivateKey::from_openssh(&text).context("parsing the stored host key");
    }
    let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)
        .context("generating an ssh host key")?;
    let text = key
        .to_openssh(ssh_key::LineEnding::LF)
        .context("encoding the host key")?;
    bus.set_setting(HOST_KEY_SETTING, text.as_bytes().to_vec())
        .await?;
    tracing::info!(
        fingerprint = %key.public_key().fingerprint(HashAlg::Sha256),
        "generated a new ssh host key"
    );
    Ok(key)
}

// ---------------------------------------------------------------------------
// one session per person
// ---------------------------------------------------------------------------

/// Who is currently connected, so a second connection can displace the first.
#[derive(Default)]
struct Sessions {
    live: Mutex<HashMap<UserId, (u64, mpsc::UnboundedSender<SessionEvent>)>>,
    next_id: AtomicU64,
}

impl Sessions {
    /// Register a session, ending whatever that user had open before.
    async fn claim(&self, user: UserId, tx: mpsc::UnboundedSender<SessionEvent>) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let previous = self.live.lock().await.insert(user, (id, tx));
        if let Some((_, old)) = previous {
            let _ = old.send(SessionEvent::Disconnect(
                "disconnected: you connected from somewhere else".into(),
            ));
        }
        id
    }

    /// Release, but only if we are still the session holding the slot -- a
    /// session that was kicked must not evict the one that replaced it.
    async fn release(&self, user: UserId, id: u64) {
        let mut live = self.live.lock().await;
        if live.get(&user).is_some_and(|(held, _)| *held == id) {
            live.remove(&user);
        }
    }
}

// ---------------------------------------------------------------------------
// server
// ---------------------------------------------------------------------------

struct SshServer {
    bus: Bus,
    sessions: Arc<Sessions>,
}

impl Server for SshServer {
    type Handler = Connection;

    fn new_client(&mut self, peer: Option<SocketAddr>) -> Connection {
        Connection {
            bus: self.bus.clone(),
            sessions: self.sessions.clone(),
            peer,
            identity: None,
            offered: None,
            pty: None,
            term: String::new(),
            input: None,
            parser: InputParser::new(),
        }
    }

    fn handle_session_error(&mut self, error: anyhow::Error) {
        tracing::warn!(%error, "ssh session ended with an error");
    }
}

pub struct Connection {
    bus: Bus,
    sessions: Arc<Sessions>,
    peer: Option<SocketAddr>,
    /// Set once a key matches an account.
    identity: Option<UserId>,
    /// The fingerprint offered by someone we do not know, so we can show it
    /// to them.
    offered: Option<String>,
    pty: Option<Arc<PtySize>>,
    /// The client's TERM, which decides how much colour we may send.
    term: String,
    input: Option<mpsc::UnboundedSender<SessionEvent>>,
    parser: InputParser,
}

impl Connection {
    /// Match an offered key against the invited accounts. Small servers, so a
    /// scan beats maintaining a fingerprint index.
    async fn identify(&self, offered: &PublicKey) -> Result<Option<UserId>> {
        for (id, stored) in self.bus.user_keys().await? {
            match PublicKey::from_openssh(&stored) {
                Ok(key) if key.key_data() == offered.key_data() => return Ok(Some(id)),
                Ok(_) => {}
                Err(err) => {
                    tracing::warn!(user = id, %err, "stored key does not parse; ignoring it");
                }
            }
        }
        Ok(None)
    }
}

impl Handler for Connection {
    type Error = anyhow::Error;

    async fn auth_publickey(&mut self, user: &str, key: &PublicKey) -> Result<Auth> {
        let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();
        if user != LOGIN_NAME {
            tracing::info!(
                peer = ?self.peer, user, %fingerprint,
                "rejected: wrong login name"
            );
            return Ok(Auth::reject());
        }
        match self.identify(key).await? {
            Some(id) => {
                self.identity = Some(id);
                tracing::info!(peer = ?self.peer, user = id, %fingerprint, "authenticated");
            }
            None => {
                // Accepted far enough to be told they are not invited, and no
                // further -- `shell_request` never starts a session for them.
                self.offered = Some(fingerprint.clone());
                tracing::warn!(peer = ?self.peer, %fingerprint, "connection from an uninvited key");
            }
        }
        Ok(Auth::Accept)
    }

    async fn channel_open_session(
        &mut self,
        _channel: Channel<Msg>,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<()> {
        reply.accept().await;
        Ok(())
    }

    async fn pty_request(
        &mut self,
        channel: ChannelId,
        term: &str,
        columns: u32,
        rows: u32,
        _pix_width: u32,
        _pix_height: u32,
        _modes: &[(Pty, u32)],
        session: &mut Session,
    ) -> Result<()> {
        self.pty = Some(PtySize::new(columns as u16, rows as u16));
        self.term = term.to_string();
        session.channel_success(channel)?;
        Ok(())
    }

    async fn shell_request(&mut self, channel: ChannelId, session: &mut Session) -> Result<()> {
        // No pty means no terminal to draw on. Refuse without explanation.
        let Some(size) = self.pty.clone() else {
            session.close(channel)?;
            return Ok(());
        };
        session.channel_success(channel)?;
        let handle = session.handle();

        let Some(me) = self.identity else {
            let fingerprint = self.offered.clone().unwrap_or_default();
            let _ = handle
                .data(channel, not_invited(&fingerprint).into_bytes())
                .await;
            let _ = handle.exit_status_request(channel, 1).await;
            let _ = handle.eof(channel).await;
            let _ = handle.close(channel).await;
            return Ok(());
        };

        let (tx, events) = mpsc::unbounded_channel();
        self.input = Some(tx.clone());
        let id = self.sessions.claim(me, tx).await;

        let bus = self.bus.clone();
        let sessions = self.sessions.clone();
        let term = self.term.clone();
        tokio::spawn(async move {
            if let Err(err) = run_session(bus, me, handle, channel, size, events, term).await {
                tracing::error!(user = me, %err, "session ended badly");
            }
            sessions.release(me, id).await;
        });
        Ok(())
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        _command: &[u8],
        session: &mut Session,
    ) -> Result<()> {
        // newbbs is interactive only; a command request just closes.
        session.close(channel)?;
        Ok(())
    }

    async fn data(&mut self, _channel: ChannelId, data: &[u8], _session: &mut Session) -> Result<()> {
        let Some(input) = &self.input else {
            return Ok(());
        };
        for key in self.parser.feed(data) {
            if input.send(SessionEvent::Key(key)).is_err() {
                break;
            }
        }
        Ok(())
    }

    async fn window_change_request(
        &mut self,
        _channel: ChannelId,
        columns: u32,
        rows: u32,
        _pix_width: u32,
        _pix_height: u32,
        _session: &mut Session,
    ) -> Result<()> {
        if let Some(size) = &self.pty {
            size.set(columns as u16, rows as u16);
        }
        if let Some(input) = &self.input {
            let _ = input.send(SessionEvent::Resize);
        }
        Ok(())
    }
}

/// Drive one session's UI for as long as it lives.
async fn run_session(
    bus: Bus,
    me: UserId,
    handle: russh::server::Handle,
    channel: ChannelId,
    size: Arc<PtySize>,
    mut events: mpsc::UnboundedReceiver<SessionEvent>,
    term: String,
) -> Result<()> {
    let (bytes_tx, mut bytes_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let writer_handle = handle.clone();
    let writer = tokio::spawn(async move {
        while let Some(frame) = bytes_rx.recv().await {
            if writer_handle.data(channel, frame).await.is_err() {
                break;
            }
        }
    });

    let _ = bytes_tx.send(ENTER_SCREEN.to_vec());
    let backend = SshBackend::new(ChannelWriter::new(bytes_tx.clone()), size);
    let mut terminal = ratatui::Terminal::new(backend)?;

    // ssh does not forward COLORTERM, so the client's TERM is all we have to
    // judge colour depth by. Guessing from the server's own environment would
    // send truecolor to someone sitting at a plain console.
    let theme = ui::theme::Theme::for_term(&term);

    let ended = match ui::splash(&mut terminal, &mut events, &bus, &theme).await {
        // They left while the splash was up; there is no session to run.
        Ok(false) => Ok(None),
        Ok(true) => {
            let mut app = ui::App::new(bus, me).await?;
            app.theme = theme;
            app.event_loop(&mut terminal, &mut events).await
        }
        Err(err) => Err(err),
    };

    // The terminal owns a clone of the byte sender. Until it is dropped the
    // writer task below can never see the channel close, and awaiting it would
    // block forever -- taking the shutdown that follows with it.
    drop(terminal);

    let mut farewell = LEAVE_SCREEN.to_vec();
    match &ended {
        Ok(Some(reason)) => farewell.extend_from_slice(format!("{reason}\r\n").as_bytes()),
        Ok(None) => {}
        Err(err) => {
            tracing::error!(user = me, %err, "session loop failed");
            farewell.extend_from_slice(b"newbbs hit an error and had to close the session\r\n");
        }
    }
    let _ = bytes_tx.send(farewell);
    drop(bytes_tx);
    let _ = writer.await;

    // Without an exit status the client has no reason to believe the command
    // finished, and `ssh` sits there with a closed channel instead of
    // returning you to your shell.
    let _ = handle.exit_status_request(channel, 0).await;
    let _ = handle.eof(channel).await;
    let _ = handle.close(channel).await;
    ended.map(|_| ())
}

/// Shown to a key we do not know, so its owner can ask to be added.
fn not_invited(fingerprint: &str) -> String {
    [
        "",
        "  newbbs",
        "",
        "  This board is invite only, and your key is not on the list.",
        "",
        "  Your key fingerprint:",
        &format!("    {fingerprint}"),
        "",
        "  Send that to whoever runs the board and ask them to add you.",
        "",
        "",
    ]
    .join("\r\n")
}

// ---------------------------------------------------------------------------
// invites
// ---------------------------------------------------------------------------

/// Create an account bound to a public key. Shared by the `invite`
/// subcommand and the in-BBS `:invite`, so both validate identically.
pub async fn invite(bus: &Bus, actor: Option<UserId>, name: &str, key_text: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() || name.contains(char::is_whitespace) {
        anyhow::bail!("a name cannot be empty or contain spaces");
    }
    let key = PublicKey::from_openssh(key_text.trim())
        .context("that does not look like an ssh public key")?;
    let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();

    if bus.user_by_name(name).await?.is_some() {
        anyhow::bail!("there is already an account called {name}");
    }
    // One key per account, so a key already in use would make identity
    // ambiguous at login.
    for (id, stored) in bus.user_keys().await? {
        if PublicKey::from_openssh(&stored).is_ok_and(|k| k.key_data() == key.key_data()) {
            let owner = bus.user(id).await?.map(|u| u.name).unwrap_or_default();
            anyhow::bail!("that key already belongs to {owner}");
        }
    }

    let canonical = key.to_openssh().context("re-encoding the key")?;
    let user = bus.alloc("user").await?;
    bus.commit(
        actor,
        crate::model::EventKind::UserCreated {
            user,
            name: name.to_string(),
            pubkey: canonical,
        },
    )
    .await?;
    tracing::info!(user, name, %fingerprint, "invited");
    Ok(fingerprint)
}

/// Accept either a path to a `.pub` file or the key text itself, since both
/// are natural things to have on hand.
pub fn read_key_argument(argument: &str) -> Result<String> {
    let path = std::path::Path::new(argument);
    if path.is_file() {
        return std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()));
    }
    Ok(argument.to_string())
}
