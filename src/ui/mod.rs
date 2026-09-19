//! The ratatui session: three panes, vim modes, and a subscription to the bus.
//!
//! A session owns no shared state. It reads through the bus, holds a local view
//! of what it is currently showing, and reacts to deliveries -- fetching the
//! event behind a uuid, or re-reading state when it is told it fell behind.

mod ansi;
mod command;
mod keys;
mod markdown;
mod render;
mod switcher;
pub mod theme;

use anyhow::Result;
use std::collections::HashMap;
use tokio::sync::mpsc;

use crate::bus::{Bus, Delivery, Payload, Subscription};
use crate::config;
use crate::model::*;
use switcher::Switcher;
use theme::Theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Insert,
    Command,
}

/// What is drawn on top of the three panes, if anything.
pub enum Overlay {
    Switcher(Switcher),
    Profile(User),
    Help,
}

pub struct App {
    bus: Bus,
    me: UserId,
    pub theme: Theme,
    pub mode: Mode,

    /// Conversations this user can see, in sidebar order.
    pub convs: Vec<Conversation>,
    /// Sidebar labels -- DMs are named after the other member, not stored.
    pub labels: HashMap<ConvId, String>,
    pub active: usize,

    pub messages: Vec<Message>,
    pub members: Vec<UserId>,
    pub users: HashMap<UserId, User>,

    pub input: String,
    /// Caret position in `input`, in characters.
    pub cursor: usize,
    pub command: String,
    pub status: Option<String>,
    /// While this is in the future the bar flashes red -- a refused command.
    pub flash_until: Option<std::time::Instant>,
    /// Lines scrolled back from the bottom of the conversation.
    pub scroll: usize,
    /// A half-typed multi-key sequence, e.g. the first `g` of `gg`.
    pub pending: Option<char>,
    pub overlay: Option<Overlay>,

    /// Written by the renderer so key handling can clamp scrolling to what is
    /// actually on screen.
    pub view_height: usize,
    pub total_lines: usize,

    quit: bool,
}

/// What drives a session. The local terminal and an ssh channel produce the
/// same events, so the loop below does not know which it is serving.
#[derive(Debug)]
pub enum SessionEvent {
    Key(crossterm::event::KeyEvent),
    Resize,
    /// The server is ending this session; the reason is shown on the way out.
    Disconnect(String),
}

/// Hold the splash until the visitor presses something. Returns false if they
/// went away instead.
///
/// The art and message come from settings, falling back to the compiled-in
/// defaults, so a board that has never been configured still looks right.
pub async fn splash<B: ratatui::backend::Backend>(
    terminal: &mut ratatui::Terminal<B>,
    events: &mut mpsc::UnboundedReceiver<SessionEvent>,
    bus: &Bus,
    theme: &Theme,
) -> Result<bool>
where
    B::Error: Send + Sync + 'static,
{
    let setting = |key: &'static str, fallback: &'static str| {
        let bus = bus.clone();
        async move {
            match bus.setting(key).await {
                Ok(Some(bytes)) => String::from_utf8(bytes).unwrap_or_else(|_| fallback.into()),
                Ok(None) => fallback.into(),
                Err(err) => {
                    tracing::warn!(key, %err, "could not read setting; using the default");
                    fallback.into()
                }
            }
        }
    };
    let art = setting(config::SETTING_ART, config::DEFAULT_ART).await;
    let motd = setting(config::SETTING_MOTD, config::DEFAULT_MOTD).await;

    loop {
        terminal.draw(|frame| render::draw_splash(frame, &art, &motd, theme))?;
        match events.recv().await {
            Some(SessionEvent::Key(_)) => return Ok(true),
            // Redraw at the new size and keep waiting.
            Some(SessionEvent::Resize) => continue,
            Some(SessionEvent::Disconnect(_)) | None => return Ok(false),
        }
    }
}

/// (width, coloured, escapes-look-stripped) -- for `newbbs art` to report.
pub fn art_summary(art: &str) -> (usize, bool, bool) {
    let parsed = ansi::parse(art, ratatui::style::Style::default());
    (
        parsed.width,
        parsed.colored,
        ansi::looks_like_stripped_escapes(art),
    )
}

pub async fn run(bus: Bus, me: UserId) -> Result<()> {
    let mut app = App::new(bus, me).await?;
    let mut terminal = init_terminal()?;
    let (tx, mut events) = mpsc::unbounded_channel();
    spawn_input_reader(tx);
    let result = app.event_loop(&mut terminal, &mut events).await;
    restore_terminal();
    if let Some(reason) = result.as_ref().ok().and_then(|r| r.clone()) {
        println!("{reason}");
    }
    result.map(|_| ())
}

/// Like `ratatui::init`, plus one thing it does not do: turn off line wrap.
///
/// With wrap on, writing the bottom-right cell makes the terminal scroll a
/// line, which shunts the whole screen up and tramples whatever sits below --
/// a tmux status line, for instance. Nothing wrote that cell until the status
/// bar gained a background, because unstyled trailing spaces never made it
/// into the diff.
fn init_terminal() -> Result<ratatui::DefaultTerminal> {
    use crossterm::terminal::{DisableLineWrap, EnterAlternateScreen, enable_raw_mode};

    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        hook(info);
    }));

    enable_raw_mode()?;
    crossterm::execute!(std::io::stdout(), EnterAlternateScreen, DisableLineWrap)?;
    Ok(ratatui::Terminal::new(
        ratatui::backend::CrosstermBackend::new(std::io::stdout()),
    )?)
}

fn restore_terminal() {
    use crossterm::terminal::{EnableLineWrap, LeaveAlternateScreen, disable_raw_mode};

    // Raw mode goes first: it has the wider side effects. ResetColor before
    // leaving the alternate screen so no colour of ours survives into whatever
    // repaints the terminal afterwards.
    let _ = disable_raw_mode();
    let _ = crossterm::execute!(
        std::io::stdout(),
        crossterm::style::ResetColor,
        EnableLineWrap,
        LeaveAlternateScreen,
        crossterm::style::ResetColor,
    );
}

impl App {
    pub(crate) async fn new(bus: Bus, me: UserId) -> Result<App> {
        let mut app = App {
            bus,
            me,
            theme: Theme::new(),
            mode: Mode::Normal,
            convs: Vec::new(),
            labels: HashMap::new(),
            active: 0,
            messages: Vec::new(),
            members: Vec::new(),
            users: HashMap::new(),
            input: String::new(),
            cursor: 0,
            command: String::new(),
            status: None,
            flash_until: None,
            scroll: 0,
            pending: None,
            overlay: None,
            view_height: 0,
            total_lines: 0,
            quit: false,
        };
        app.reload().await?;
        Ok(app)
    }

    pub fn me(&self) -> UserId {
        self.me
    }

    pub fn bus(&self) -> &Bus {
        &self.bus
    }

    pub fn quit(&mut self) {
        self.quit = true;
    }

    // -- state ------------------------------------------------------------

    /// Re-read everything this session displays. Called at startup, after a
    /// resync marker, and whenever an event arrives that is easier to absorb by
    /// re-reading than by patching.
    pub async fn reload(&mut self) -> Result<()> {
        self.convs = self.bus.conversations(self.me).await?;
        for user in self.bus.users().await? {
            self.users.insert(user.id, user);
        }
        self.labels.clear();
        for conv in &self.convs {
            let label = match conv.kind {
                ConvKind::Channel => format!("#{}", conv.name),
                ConvKind::Group if !conv.name.is_empty() => format!("&{}", conv.name),
                _ => {
                    let members = self.bus.members(conv.id).await?;
                    let names: Vec<String> = members
                        .iter()
                        .filter(|id| **id != self.me)
                        .map(|id| {
                            self.users
                                .get(id)
                                .map(|u| u.name.clone())
                                .unwrap_or_else(|| format!("user{id}"))
                        })
                        .collect();
                    if names.is_empty() {
                        "@(empty)".to_string()
                    } else {
                        format!("@{}", names.join(", "))
                    }
                }
            };
            self.labels.insert(conv.id, label);
        }
        if self.active >= self.convs.len() {
            self.active = self.convs.len().saturating_sub(1);
        }
        self.load_active().await
    }

    /// Load the messages and members of the selected conversation, and point
    /// the subscription at it.
    pub async fn load_active(&mut self) -> Result<()> {
        let Some(conv) = self.convs.get(self.active).map(|c| c.id) else {
            self.messages.clear();
            self.members.clear();
            return Ok(());
        };
        self.messages = self
            .bus
            .messages(conv, None, config::MESSAGE_WINDOW, Some(self.me))
            .await?;
        self.members = self.bus.members(conv).await?;
        self.scroll = 0;
        Ok(())
    }

    pub fn active_conv(&self) -> Option<&Conversation> {
        self.convs.get(self.active)
    }

    pub fn label(&self, conv: ConvId) -> &str {
        self.labels.get(&conv).map(String::as_str).unwrap_or("?")
    }

    pub fn user(&self, id: UserId) -> Option<&User> {
        self.users.get(&id)
    }

    pub fn user_name(&self, id: UserId) -> String {
        self.users
            .get(&id)
            .map(|u| u.name.clone())
            .unwrap_or_else(|| format!("user{id}"))
    }

    /// Flair: the highest-priority role colours the name everywhere.
    pub fn user_color(&self, id: UserId) -> ratatui::style::Color {
        match self.users.get(&id).and_then(|u| u.roles.first()) {
            Some(role) => self.theme.color(role.color),
            None => self.theme.fg(),
        }
    }

    pub fn set_status(&mut self, text: impl Into<String>) {
        self.status = Some(text.into());
        self.flash_until = None;
    }

    /// A refused command: same message, but the bar goes red for a beat.
    pub fn set_error(&mut self, text: impl Into<String>) {
        self.status = Some(text.into());
        self.flash_until = Some(std::time::Instant::now() + config::ERROR_FLASH);
    }

    pub fn flashing(&self) -> bool {
        self.flash_until
            .is_some_and(|until| until > std::time::Instant::now())
    }

    /// Select a conversation by index and retarget the subscription.
    pub async fn select(&mut self, index: usize, sub: &Subscription) -> Result<()> {
        if index >= self.convs.len() {
            return Ok(());
        }
        self.active = index;
        self.load_active().await?;
        sub.retag(self.tags());
        Ok(())
    }

    fn tags(&self) -> Vec<Tag> {
        let mut tags = vec![
            Tag::Directory,
            Tag::Presence,
            Tag::Roles,
            Tag::User(self.me),
        ];
        if let Some(conv) = self.active_conv() {
            tags.push(Tag::Conv(conv.id));
        }
        tags
    }

    pub async fn send_message(&mut self) -> Result<()> {
        let body = self.input.trim().to_string();
        self.input.clear();
        self.cursor = 0;
        if body.is_empty() {
            return Ok(());
        }
        let Some(conv) = self.active_conv().map(|c| c.id) else {
            self.set_error("no conversation selected");
            return Ok(());
        };
        self.bus
            .commit(
                Some(self.me),
                EventKind::MessageSent {
                    conv,
                    author: self.me,
                    body,
                },
            )
            .await?;
        self.scroll = 0;
        Ok(())
    }

    // -- event loop -------------------------------------------------------

    /// Drive a session to completion. Returns the reason it ended, if the
    /// server supplied one.
    pub(crate) async fn event_loop<B: ratatui::backend::Backend>(
        &mut self,
        terminal: &mut ratatui::Terminal<B>,
        events: &mut mpsc::UnboundedReceiver<SessionEvent>,
    ) -> Result<Option<String>>
    where
        B::Error: Send + Sync + 'static,
    {
        let mut sub = self.bus.subscribe(Some(self.me), self.tags()).await?;
        let mut ended = None;

        loop {
            terminal.draw(|frame| render::draw(frame, self))?;
            if self.quit {
                break;
            }
            tokio::select! {
                event = events.recv() => match event {
                    Some(SessionEvent::Key(key)) => keys::handle(self, key, &sub).await?,
                    // A redraw is all a resize needs: the backend reports the
                    // new size and `draw` resizes the buffers to match.
                    Some(SessionEvent::Resize) => {}
                    Some(SessionEvent::Disconnect(reason)) => {
                        ended = Some(reason);
                        break;
                    }
                    // The input source is gone; nothing more can arrive.
                    None => break,
                },
                delivery = sub.recv() => match delivery {
                    Some(delivery) => self.on_delivery(delivery).await?,
                    None => break,
                },
                () = expire(self.flash_until) => self.flash_until = None,
            }
        }
        Ok(ended)
    }

    async fn on_delivery(&mut self, delivery: Delivery) -> Result<()> {
        match delivery {
            Delivery::Inline(Payload::Event(event)) => self.apply_event(event).await?,
            Delivery::Inline(Payload::Signal(signal)) => self.apply_signal(signal),
            Delivery::Id(id) => {
                if let Some(event) = self.bus.event(id).await? {
                    self.apply_event(event).await?;
                }
            }
            Delivery::Resync { missed } => {
                tracing::warn!(missed, "session fell behind, resyncing");
                self.set_status(format!("fell behind ({missed} events) -- resynced"));
                self.reload().await?;
            }
        }
        Ok(())
    }

    async fn apply_event(&mut self, event: Event) -> Result<()> {
        let active = self.active_conv().map(|c| c.id);
        match &event.kind {
            EventKind::MessageSent { conv, author, body } if Some(*conv) == active => {
                self.messages.push(Message {
                    id: event.id,
                    conv: *conv,
                    author: *author,
                    body: body.clone(),
                    created_at: event.at,
                    edited_at: None,
                });
                if self.messages.len() > config::MESSAGE_WINDOW {
                    self.messages.remove(0);
                }
            }
            EventKind::MessageSent { .. } => {}
            EventKind::TopicSet { conv, topic } => {
                if let Some(c) = self.convs.iter_mut().find(|c| c.id == *conv) {
                    c.topic = topic.clone();
                }
            }
            // Everything else is cheaper to absorb by re-reading than by
            // hand-patching each projection in the session.
            _ => self.reload().await?,
        }
        Ok(())
    }

    fn apply_signal(&mut self, signal: Signal) {
        match signal {
            Signal::Presence { user, online } => {
                if let Some(u) = self.users.get_mut(&user) {
                    u.online = online;
                    if !online {
                        u.last_seen = now_millis();
                    }
                }
            }
            // Typing indicators and read observations have no UI yet.
            Signal::Typing { .. } | Signal::Read { .. } => {}
        }
    }
}

/// Completes when the flash is due to end, or never if nothing is flashing.
async fn expire(deadline: Option<std::time::Instant>) {
    match deadline {
        Some(until) => {
            tokio::time::sleep(until.saturating_duration_since(std::time::Instant::now())).await;
        }
        None => std::future::pending().await,
    }
}

/// crossterm's reader is blocking, so it lives on its own thread and feeds the
/// async loop through a channel.
fn spawn_input_reader(tx: mpsc::UnboundedSender<SessionEvent>) {
    std::thread::spawn(move || {
        loop {
            let event = match crossterm::event::read() {
                Ok(event) => event,
                Err(err) => {
                    tracing::error!(%err, "terminal input reader stopped");
                    break;
                }
            };
            let event = match event {
                crossterm::event::Event::Key(key) => SessionEvent::Key(key),
                crossterm::event::Event::Resize(_, _) => SessionEvent::Resize,
                _ => continue,
            };
            if tx.send(event).is_err() {
                break;
            }
        }
    });
}

/// Render one frame off-screen and return it as plain text.
///
/// The TUI needs a terminal, which CI, a pipe and a container build do not
/// have -- this renders the same widgets into a buffer so the layout can be
/// inspected anywhere.
pub async fn snapshot(
    bus: Bus,
    me: UserId,
    width: u16,
    height: u16,
    overlay: Option<&str>,
) -> Result<String> {
    let mut app = App::new(bus.clone(), me).await?;
    // Subscribing marks this user online, the same as a real session would.
    let mut sub = bus.subscribe(Some(me), app.tags()).await?;
    while let Ok(delivery) = sub.rx.try_recv() {
        app.on_delivery(delivery).await?;
    }
    app.overlay = match overlay {
        Some("help") => Some(Overlay::Help),
        Some("switcher") => Some(Overlay::Switcher(Switcher::new(&app))),
        Some("profile") => app.user(me).cloned().map(Overlay::Profile),
        _ => None,
    };

    let backend = ratatui::backend::TestBackend::new(width, height);
    let mut terminal = ratatui::Terminal::new(backend)?;
    let frame = terminal.draw(|frame| render::draw(frame, &mut app))?;

    let mut out = String::new();
    for y in 0..frame.area.height {
        for x in 0..frame.area.width {
            if let Some(cell) = frame.buffer.cell((x, y)) {
                out.push_str(cell.symbol());
            }
        }
        out.push('\n');
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Render the bottom row of the status bar in one mode.
    async fn status_row(app: &mut App, mode: Mode) -> String {
        app.mode = mode;
        let backend = ratatui::backend::TestBackend::new(90, 20);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let frame = terminal.draw(|frame| render::draw(frame, app)).unwrap();
        let y = frame.area.height - 1;
        (0..frame.area.width)
            .map(|x| frame.buffer.cell((x, y)).unwrap().symbol())
            .collect()
    }

    /// A refused command paints the bar red until the flash lapses, without
    /// disturbing anything else on it.
    #[tokio::test]
    async fn a_refused_command_flashes_the_bar_red() {
        let path = std::env::temp_dir().join(format!("newbbs-flash-{}.db", uuid::Uuid::now_v7()));
        let (bus, _owner) = Bus::start(path.clone()).unwrap();
        let me = bus.user_by_name("admin").await.unwrap().unwrap().id;
        let mut app = App::new(bus.clone(), me).await.unwrap();

        let (calm, label) = bar(&mut app).await;
        assert_eq!(calm, app.theme.bar_color(app.theme.palette.mode_normal));
        assert!(label.starts_with("NORMAL"), "got {label:?}");

        app.set_error("unknown command :nonsense -- try :help");
        assert!(app.flashing());
        let (angry, label) = bar(&mut app).await;
        assert_eq!(angry, app.theme.bar_color(app.theme.palette.mode_error));
        assert!(
            label.starts_with("ERROR"),
            "a red bar should not still claim to be in normal mode, got {label:?}"
        );

        // Once the flash lapses the bar goes back on its own, with no keypress.
        app.flash_until = Some(std::time::Instant::now() - std::time::Duration::from_secs(1));
        assert!(!app.flashing());
        let (settled, label) = bar(&mut app).await;
        assert_eq!(settled, calm);
        assert!(label.starts_with("NORMAL"), "got {label:?}");

        bus.shutdown();
        let _ = std::fs::remove_file(&path);
    }

    /// The status bar's background colour and its text.
    async fn bar(app: &mut App) -> (ratatui::style::Color, String) {
        let backend = ratatui::backend::TestBackend::new(90, 20);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let frame = terminal.draw(|frame| render::draw(frame, app)).unwrap();
        let y = frame.area.height - 1;
        let text = (0..frame.area.width)
            .map(|x| frame.buffer.cell((x, y)).unwrap().symbol())
            .collect();
        (frame.buffer.cell((0, y)).unwrap().bg, text)
    }

    /// The mode name is flush left, and the conversation is centred on the bar
    /// rather than on what is left of it -- so COMMAND being a letter longer
    /// than NORMAL does not shove the conversation sideways.
    #[tokio::test]
    async fn the_conversation_does_not_move_when_the_mode_changes() {
        let path = std::env::temp_dir().join(format!("newbbs-ui-{}.db", uuid::Uuid::now_v7()));
        let (bus, _owner) = Bus::start(path.clone()).unwrap();
        let me = bus.user_by_name("admin").await.unwrap().unwrap().id;
        let mut app = App::new(bus.clone(), me).await.unwrap();

        let mut centres = Vec::new();
        for (mode, label) in [
            (Mode::Normal, "NORMAL"),
            (Mode::Insert, "INSERT"),
            (Mode::Command, "COMMAND"),
        ] {
            let row = status_row(&mut app, mode).await;
            assert!(
                row.starts_with(label),
                "{label} should sit in the first column, got {row:?}"
            );
            centres.push(row.find('#').expect("the conversation name"));
        }
        assert_eq!(
            centres[0], centres[1],
            "the conversation moved between NORMAL and INSERT"
        );
        assert_eq!(
            centres[0], centres[2],
            "the conversation moved between NORMAL and COMMAND"
        );

        bus.shutdown();
        let _ = std::fs::remove_file(&path);
    }
}
