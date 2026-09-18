//! The ratatui session: three panes, vim modes, and a subscription to the bus.
//!
//! A session owns no shared state. It reads through the bus, holds a local view
//! of what it is currently showing, and reacts to deliveries -- fetching the
//! event behind a uuid, or re-reading state when it is told it fell behind.

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

pub async fn run(bus: Bus, me: UserId) -> Result<()> {
    let mut app = App::new(bus, me).await?;
    let mut terminal = ratatui::init();
    let result = app.event_loop(&mut terminal).await;
    ratatui::restore();
    result
}

impl App {
    async fn new(bus: Bus, me: UserId) -> Result<App> {
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
            self.set_status("no conversation selected");
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

    async fn event_loop(&mut self, terminal: &mut ratatui::DefaultTerminal) -> Result<()> {
        let sub = self.bus.subscribe(Some(self.me), self.tags()).await?;
        let mut sub = sub;
        let mut input = spawn_input_reader();

        loop {
            terminal.draw(|frame| render::draw(frame, self))?;
            if self.quit {
                break;
            }
            tokio::select! {
                key = input.recv() => match key {
                    Some(event) => self.on_terminal_event(event, &sub).await?,
                    // The reader thread died; nothing more can arrive.
                    None => break,
                },
                delivery = sub.recv() => match delivery {
                    Some(delivery) => self.on_delivery(delivery).await?,
                    None => break,
                },
            }
        }
        Ok(())
    }

    async fn on_terminal_event(
        &mut self,
        event: crossterm::event::Event,
        sub: &Subscription,
    ) -> Result<()> {
        use crossterm::event::Event;
        match event {
            Event::Key(key) => keys::handle(self, key, sub).await?,
            Event::Resize(_, _) => {}
            _ => {}
        }
        Ok(())
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

/// crossterm's reader is blocking, so it lives on its own thread and feeds the
/// async loop through a channel.
fn spawn_input_reader() -> mpsc::UnboundedReceiver<crossterm::event::Event> {
    let (tx, rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        loop {
            match crossterm::event::read() {
                Ok(event) => {
                    if tx.send(event).is_err() {
                        break;
                    }
                }
                Err(err) => {
                    tracing::error!(%err, "terminal input reader stopped");
                    break;
                }
            }
        }
    });
    rx
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
