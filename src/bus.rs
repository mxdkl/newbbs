//! The event bus: the single funnel every read and write passes through.
//!
//! One owner task holds the [`Db`] and the subscriber registry. Sessions never
//! touch SQLite; they send a [`Request`] and await a [`Response`]. Because the
//! owner is single-threaded, writes are serialised by construction and no lock
//! is needed anywhere else.
//!
//! Writes are appended to the log and then broadcast to every session
//! subscribed to any of the event's tags. Reads are answered directly, and are
//! themselves published as an ephemeral [`Signal::Read`] so subscribers can
//! observe access without the log filling up with non-mutations.
//!
//! What a subscriber receives is a [`Delivery`]:
//!
//! * [`Delivery::Inline`] -- the encoded payload was smaller than the 16 bytes
//!   a uuid would have cost, so the whole thing rode along. Signals are always
//!   inline: they are never logged, so there would be nothing to resolve.
//! * [`Delivery::Id`] -- just the event's uuid; resolve it with [`Bus::event`]
//!   or by re-reading the affected projection.
//! * [`Delivery::Resync`] -- the session's queue overflowed and events were
//!   dropped. Re-read state; the bus never blocks on a slow session.

use anyhow::{Result, anyhow, bail};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::config;
use crate::db::Db;
use crate::model::*;

pub type SubId = u64;

#[derive(Debug, Clone)]
pub enum Delivery {
    Inline(Payload),
    Id(Uuid),
    Resync { missed: usize },
}

#[derive(Debug, Clone)]
pub enum Payload {
    Event(Event),
    Signal(Signal),
}

// Several variants below are the reconnect path (replay from a cursor) and
// per-entity reads that only the ssh session layer will reach for. They are
// part of the designed surface, not leftovers.
#[allow(dead_code)]
#[derive(Debug)]
pub enum Request {
    Commit {
        actor: Option<UserId>,
        kind: EventKind,
    },
    Notify(Signal),
    Alloc(String),
    Event(Uuid),
    Replay {
        tags: Vec<Tag>,
        since: Option<Uuid>,
    },
    Conversations {
        user: UserId,
    },
    Conversation(ConvId),
    ConversationByName(String),
    Messages {
        conv: ConvId,
        before: Option<MessageId>,
        limit: usize,
        reader: Option<UserId>,
    },
    Members(ConvId),
    Users,
    User(UserId),
    UserByName(String),
    Roles,
    TouchLastSeen(UserId),
    LogDump(usize),
    UserKeys,
    Setting(String),
    SetSetting { key: String, value: Vec<u8> },
    ClearSetting(String),
}

#[allow(dead_code)]
#[derive(Debug)]
pub enum Response {
    Unit,
    Id(i64),
    Event(Event),
    MaybeEvent(Option<Event>),
    Events(Vec<Event>),
    Replay { events: Vec<Event>, truncated: bool },
    Conversations(Vec<Conversation>),
    MaybeConversation(Option<Conversation>),
    Messages(Vec<Message>),
    Members(Vec<UserId>),
    Users(Vec<User>),
    MaybeUser(Option<User>),
    Roles(Vec<Role>),
    UserKeys(Vec<(UserId, String)>),
    Setting(Option<Vec<u8>>),
}

enum BusMsg {
    Request {
        req: Request,
        reply: oneshot::Sender<Result<Response>>,
    },
    Subscribe {
        user: Option<UserId>,
        tags: Vec<Tag>,
        tx: mpsc::Sender<Delivery>,
        reply: oneshot::Sender<SubId>,
    },
    Retag {
        id: SubId,
        tags: Vec<Tag>,
    },
    Unsubscribe {
        id: SubId,
    },
    Shutdown,
}

/// A cheap, clonable handle to the bus.
#[derive(Clone)]
pub struct Bus {
    tx: mpsc::UnboundedSender<BusMsg>,
}

#[allow(dead_code)]
impl Bus {
    /// Open the database and start the owner task on a blocking thread
    /// (rusqlite is synchronous, and serialising access is the point).
    pub fn start(db_path: PathBuf) -> Result<(Bus, tokio::task::JoinHandle<()>)> {
        let mut db = Db::open(&db_path)?;
        // A fresh database gets the bare minimum before anyone can observe it.
        if db.is_empty()? {
            crate::db::bootstrap(&mut db)?;
        }
        let (tx, rx) = mpsc::unbounded_channel();
        let handle = tokio::task::spawn_blocking(move || {
            let mut state = BusState::default();
            let mut rx = rx;
            while let Some(msg) = rx.blocking_recv() {
                if state.handle(&mut db, msg) == Flow::Stop {
                    break;
                }
            }
            tracing::info!("bus stopped");
        });
        Ok((Bus { tx }, handle))
    }

    async fn request(&self, req: Request) -> Result<Response> {
        let (reply, wait) = oneshot::channel();
        self.tx
            .send(BusMsg::Request { req, reply })
            .map_err(|_| anyhow!("bus is gone"))?;
        wait.await.map_err(|_| anyhow!("bus dropped the request"))?
    }

    pub fn shutdown(&self) {
        let _ = self.tx.send(BusMsg::Shutdown);
    }

    /// Subscribe to a set of tags. `user` is the identity whose presence this
    /// subscription represents, if any.
    pub async fn subscribe(&self, user: Option<UserId>, tags: Vec<Tag>) -> Result<Subscription> {
        let (tx, rx) = mpsc::channel(config::SESSION_QUEUE);
        let (reply, wait) = oneshot::channel();
        self.tx
            .send(BusMsg::Subscribe {
                user,
                tags,
                tx,
                reply,
            })
            .map_err(|_| anyhow!("bus is gone"))?;
        let id = wait.await.map_err(|_| anyhow!("bus is gone"))?;
        Ok(Subscription {
            id,
            rx,
            bus: self.clone(),
        })
    }

    // -- typed wrappers ---------------------------------------------------

    pub async fn commit(&self, actor: Option<UserId>, kind: EventKind) -> Result<Event> {
        match self.request(Request::Commit { actor, kind }).await? {
            Response::Event(e) => Ok(e),
            other => bail!("unexpected response {other:?}"),
        }
    }

    pub async fn notify(&self, signal: Signal) -> Result<()> {
        self.request(Request::Notify(signal)).await.map(|_| ())
    }

    pub async fn alloc(&self, counter: &str) -> Result<i64> {
        match self.request(Request::Alloc(counter.to_string())).await? {
            Response::Id(id) => Ok(id),
            other => bail!("unexpected response {other:?}"),
        }
    }

    pub async fn event(&self, id: Uuid) -> Result<Option<Event>> {
        match self.request(Request::Event(id)).await? {
            Response::MaybeEvent(e) => Ok(e),
            other => bail!("unexpected response {other:?}"),
        }
    }

    pub async fn replay(&self, tags: Vec<Tag>, since: Option<Uuid>) -> Result<(Vec<Event>, bool)> {
        match self.request(Request::Replay { tags, since }).await? {
            Response::Replay { events, truncated } => Ok((events, truncated)),
            other => bail!("unexpected response {other:?}"),
        }
    }

    pub async fn conversations(&self, user: UserId) -> Result<Vec<Conversation>> {
        match self.request(Request::Conversations { user }).await? {
            Response::Conversations(c) => Ok(c),
            other => bail!("unexpected response {other:?}"),
        }
    }

    pub async fn conversation(&self, conv: ConvId) -> Result<Option<Conversation>> {
        match self.request(Request::Conversation(conv)).await? {
            Response::MaybeConversation(c) => Ok(c),
            other => bail!("unexpected response {other:?}"),
        }
    }

    pub async fn conversation_by_name(&self, name: &str) -> Result<Option<Conversation>> {
        match self
            .request(Request::ConversationByName(name.to_string()))
            .await?
        {
            Response::MaybeConversation(c) => Ok(c),
            other => bail!("unexpected response {other:?}"),
        }
    }

    pub async fn messages(
        &self,
        conv: ConvId,
        before: Option<MessageId>,
        limit: usize,
        reader: Option<UserId>,
    ) -> Result<Vec<Message>> {
        match self
            .request(Request::Messages {
                conv,
                before,
                limit,
                reader,
            })
            .await?
        {
            Response::Messages(m) => Ok(m),
            other => bail!("unexpected response {other:?}"),
        }
    }

    pub async fn members(&self, conv: ConvId) -> Result<Vec<UserId>> {
        match self.request(Request::Members(conv)).await? {
            Response::Members(m) => Ok(m),
            other => bail!("unexpected response {other:?}"),
        }
    }

    pub async fn users(&self) -> Result<Vec<User>> {
        match self.request(Request::Users).await? {
            Response::Users(u) => Ok(u),
            other => bail!("unexpected response {other:?}"),
        }
    }

    pub async fn user(&self, id: UserId) -> Result<Option<User>> {
        match self.request(Request::User(id)).await? {
            Response::MaybeUser(u) => Ok(u),
            other => bail!("unexpected response {other:?}"),
        }
    }

    pub async fn user_by_name(&self, name: &str) -> Result<Option<User>> {
        match self.request(Request::UserByName(name.to_string())).await? {
            Response::MaybeUser(u) => Ok(u),
            other => bail!("unexpected response {other:?}"),
        }
    }

    pub async fn roles(&self) -> Result<Vec<Role>> {
        match self.request(Request::Roles).await? {
            Response::Roles(r) => Ok(r),
            other => bail!("unexpected response {other:?}"),
        }
    }

    pub async fn touch_last_seen(&self, user: UserId) -> Result<()> {
        self.request(Request::TouchLastSeen(user)).await.map(|_| ())
    }

    pub async fn user_keys(&self) -> Result<Vec<(UserId, String)>> {
        match self.request(Request::UserKeys).await? {
            Response::UserKeys(keys) => Ok(keys),
            other => bail!("unexpected response {other:?}"),
        }
    }

    pub async fn setting(&self, key: &str) -> Result<Option<Vec<u8>>> {
        match self.request(Request::Setting(key.to_string())).await? {
            Response::Setting(value) => Ok(value),
            other => bail!("unexpected response {other:?}"),
        }
    }

    pub async fn set_setting(&self, key: &str, value: Vec<u8>) -> Result<()> {
        self.request(Request::SetSetting {
            key: key.to_string(),
            value,
        })
        .await
        .map(|_| ())
    }

    pub async fn clear_setting(&self, key: &str) -> Result<()> {
        self.request(Request::ClearSetting(key.to_string()))
            .await
            .map(|_| ())
    }

    pub async fn log_dump(&self, limit: usize) -> Result<Vec<Event>> {
        match self.request(Request::LogDump(limit)).await? {
            Response::Events(e) => Ok(e),
            other => bail!("unexpected response {other:?}"),
        }
    }
}

/// A live subscription. Dropping it unsubscribes and marks the user offline.
pub struct Subscription {
    pub id: SubId,
    pub rx: mpsc::Receiver<Delivery>,
    bus: Bus,
}

impl Subscription {
    /// Replace the tag set -- used when a session switches conversation.
    pub fn retag(&self, tags: Vec<Tag>) {
        let _ = self.bus.tx.send(BusMsg::Retag { id: self.id, tags });
    }

    pub async fn recv(&mut self) -> Option<Delivery> {
        self.rx.recv().await
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        let _ = self.bus.tx.send(BusMsg::Unsubscribe { id: self.id });
    }
}

// ---------------------------------------------------------------------------
// owner task
// ---------------------------------------------------------------------------

#[derive(PartialEq, Eq)]
enum Flow {
    Continue,
    Stop,
}

struct Subscriber {
    user: Option<UserId>,
    tags: HashSet<Tag>,
    tx: mpsc::Sender<Delivery>,
    /// Events dropped since the last resync marker this session accepted.
    missed: usize,
}

#[derive(Default)]
struct BusState {
    subs: HashMap<SubId, Subscriber>,
    online: HashMap<UserId, usize>,
    next_id: SubId,
}

impl BusState {
    fn handle(&mut self, db: &mut Db, msg: BusMsg) -> Flow {
        match msg {
            BusMsg::Shutdown => return Flow::Stop,
            BusMsg::Subscribe {
                user,
                tags,
                tx,
                reply,
            } => {
                self.next_id += 1;
                let id = self.next_id;
                self.subs.insert(
                    id,
                    Subscriber {
                        user,
                        tags: tags.into_iter().collect(),
                        tx,
                        missed: 0,
                    },
                );
                if let Some(user) = user {
                    let count = self.online.entry(user).or_insert(0);
                    *count += 1;
                    if *count == 1 {
                        self.publish_signal(Signal::Presence { user, online: true });
                    }
                }
                let _ = reply.send(id);
            }
            BusMsg::Retag { id, tags } => {
                if let Some(sub) = self.subs.get_mut(&id) {
                    sub.tags = tags.into_iter().collect();
                }
            }
            BusMsg::Unsubscribe { id } => {
                if let Some(sub) = self.subs.remove(&id)
                    && let Some(user) = sub.user
                {
                    if let Some(count) = self.online.get_mut(&user) {
                        *count = count.saturating_sub(1);
                        if *count == 0 {
                            self.online.remove(&user);
                            let _ = db.touch_last_seen(user);
                            self.publish_signal(Signal::Presence { user, online: false });
                        }
                    }
                }
            }
            BusMsg::Request { req, reply } => {
                let result = self.serve(db, req);
                let _ = reply.send(result);
            }
        }
        Flow::Continue
    }

    fn serve(&mut self, db: &mut Db, req: Request) -> Result<Response> {
        match req {
            Request::Commit { actor, kind } => {
                let event = db.commit(actor, kind)?;
                self.publish_event(&event);
                Ok(Response::Event(event))
            }
            Request::Notify(signal) => {
                self.publish_signal(signal);
                Ok(Response::Unit)
            }
            Request::Alloc(counter) => Ok(Response::Id(db.alloc(&counter)?)),
            Request::Event(id) => Ok(Response::MaybeEvent(db.event(id)?)),
            Request::Replay { tags, since } => {
                let (events, truncated) = db.replay(&tags, since)?;
                Ok(Response::Replay { events, truncated })
            }
            Request::Conversations { user } => {
                self.publish_signal(Signal::Read { user, conv: None });
                Ok(Response::Conversations(db.conversations(user)?))
            }
            Request::Conversation(conv) => Ok(Response::MaybeConversation(db.conversation(conv)?)),
            Request::ConversationByName(name) => {
                Ok(Response::MaybeConversation(db.conversation_by_name(&name)?))
            }
            Request::Messages {
                conv,
                before,
                limit,
                reader,
            } => {
                if let Some(user) = reader {
                    self.publish_signal(Signal::Read {
                        user,
                        conv: Some(conv),
                    });
                }
                Ok(Response::Messages(db.messages(conv, before, limit)?))
            }
            Request::Members(conv) => Ok(Response::Members(db.members(conv)?)),
            Request::Users => {
                let mut users = db.users()?;
                for user in &mut users {
                    user.online = self.online.contains_key(&user.id);
                }
                Ok(Response::Users(users))
            }
            Request::User(id) => {
                let user = db.user(id)?.map(|mut u| {
                    u.online = self.online.contains_key(&u.id);
                    u
                });
                Ok(Response::MaybeUser(user))
            }
            Request::UserByName(name) => {
                let user = db.user_by_name(&name)?.map(|mut u| {
                    u.online = self.online.contains_key(&u.id);
                    u
                });
                Ok(Response::MaybeUser(user))
            }
            Request::Roles => Ok(Response::Roles(db.roles()?)),
            Request::TouchLastSeen(user) => {
                db.touch_last_seen(user)?;
                Ok(Response::Unit)
            }
            Request::LogDump(limit) => Ok(Response::Events(db.all_events(limit)?)),
            Request::UserKeys => Ok(Response::UserKeys(db.user_keys()?)),
            Request::Setting(key) => Ok(Response::Setting(db.setting(&key)?)),
            Request::SetSetting { key, value } => {
                db.set_setting(&key, &value)?;
                Ok(Response::Unit)
            }
            Request::ClearSetting(key) => {
                db.clear_setting(&key)?;
                Ok(Response::Unit)
            }
        }
    }

    /// A logged event: inline it if the encoded payload undercuts a uuid,
    /// otherwise send the uuid and let the session resolve it.
    fn publish_event(&mut self, event: &Event) {
        let encoded_len = postcard::to_stdvec(&event.kind).map(|v| v.len()).unwrap_or(usize::MAX);
        let delivery = if encoded_len < config::INLINE_MAX_BYTES {
            Delivery::Inline(Payload::Event(event.clone()))
        } else {
            Delivery::Id(event.id)
        };
        self.fan_out(&event.tags(), delivery);
    }

    /// Signals are never logged, so there is nothing to fetch by uuid: they
    /// always travel whole.
    fn publish_signal(&mut self, signal: Signal) {
        let tags = signal.tags();
        self.fan_out(&tags, Delivery::Inline(Payload::Signal(signal)));
    }

    fn fan_out(&mut self, tags: &[Tag], delivery: Delivery) {
        let mut dead = Vec::new();
        for (id, sub) in self.subs.iter_mut() {
            if !tags.iter().any(|t| sub.tags.contains(t)) {
                continue;
            }
            // A session that fell behind gets told to resync before it is sent
            // anything else, so it never applies deltas to stale state.
            if sub.missed > 0 {
                match sub.tx.try_send(Delivery::Resync { missed: sub.missed }) {
                    Ok(()) => sub.missed = 0,
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        sub.missed += 1;
                        continue;
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        dead.push(*id);
                        continue;
                    }
                }
            }
            match sub.tx.try_send(delivery.clone()) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    sub.missed += 1;
                    tracing::warn!(sub = id, "session queue full, dropping delivery");
                }
                Err(mpsc::error::TrySendError::Closed(_)) => dead.push(*id),
            }
        }
        for id in dead {
            self.subs.remove(&id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ConvKind, EventKind, Tag};

    fn temp_db() -> PathBuf {
        std::env::temp_dir().join(format!("newbbs-test-{}.db", Uuid::now_v7()))
    }

    /// The rule the whole delivery design turns on: a payload that undercuts a
    /// uuid travels whole, anything bigger travels as an id to resolve.
    #[tokio::test]
    async fn payload_smaller_than_a_uuid_is_inlined() {
        let path = temp_db();
        let (bus, _owner) = Bus::start(path.clone()).unwrap();
        let admin = bus.user_by_name("admin").await.unwrap().unwrap();
        let general = bus
            .conversation_by_name("general")
            .await
            .unwrap()
            .expect("#general exists on every board");
        let mut sub = bus
            .subscribe(None, vec![Tag::Conv(general.id)])
            .await
            .unwrap();

        bus.commit(
            Some(admin.id),
            EventKind::MessageSent {
                conv: general.id,
                author: admin.id,
                body: "hi".into(),
            },
        )
        .await
        .unwrap();
        match sub.recv().await.expect("delivery") {
            Delivery::Inline(Payload::Event(event)) => {
                assert!(matches!(event.kind, EventKind::MessageSent { .. }));
            }
            other => panic!("expected a short message to ride inline, got {other:?}"),
        }

        bus.commit(
            Some(admin.id),
            EventKind::MessageSent {
                conv: general.id,
                author: admin.id,
                body: "this one is comfortably longer than sixteen bytes".into(),
            },
        )
        .await
        .unwrap();
        match sub.recv().await.expect("delivery") {
            Delivery::Id(id) => {
                let event = bus.event(id).await.unwrap().expect("resolvable by uuid");
                assert!(matches!(event.kind, EventKind::MessageSent { .. }));
            }
            other => panic!("expected a long message to ship as a uuid, got {other:?}"),
        }

        bus.shutdown();
        let _ = std::fs::remove_file(&path);
    }

    /// Subscriptions are by tag: an event in another conversation must not
    /// reach a session that is not watching it.
    #[tokio::test]
    async fn untagged_events_do_not_reach_a_session() {
        let path = temp_db();
        let (bus, _owner) = Bus::start(path.clone()).unwrap();
        let admin = bus.user_by_name("admin").await.unwrap().unwrap();
        let general = bus.conversation_by_name("general").await.unwrap().unwrap();
        // A second channel of our own, rather than relying on whatever
        // happens to exist on a fresh board.
        let elsewhere = bus.alloc("conv").await.unwrap();
        bus.commit(
            Some(admin.id),
            EventKind::ConvCreated {
                conv: elsewhere,
                kind: ConvKind::Channel,
                name: "elsewhere".into(),
            },
        )
        .await
        .unwrap();
        let mut sub = bus
            .subscribe(None, vec![Tag::Conv(general.id)])
            .await
            .unwrap();

        bus.commit(
            Some(admin.id),
            EventKind::MessageSent {
                conv: elsewhere,
                author: admin.id,
                body: "not for you".into(),
            },
        )
        .await
        .unwrap();
        bus.commit(
            Some(admin.id),
            EventKind::MessageSent {
                conv: general.id,
                author: admin.id,
                body: "for you".into(),
            },
        )
        .await
        .unwrap();

        // The first thing to arrive must be the one that was tagged for us.
        match sub.recv().await.expect("delivery") {
            Delivery::Inline(Payload::Event(Event {
                kind: EventKind::MessageSent { conv, .. },
                ..
            })) => assert_eq!(conv, general.id),
            other => panic!("expected the #general message, got {other:?}"),
        }

        bus.shutdown();
        let _ = std::fs::remove_file(&path);
    }

    /// Appending an event and updating the projections is one transaction, so a
    /// read straight after a write always sees it.
    #[tokio::test]
    async fn a_write_is_visible_to_the_next_read() {
        let path = temp_db();
        let (bus, _owner) = Bus::start(path.clone()).unwrap();
        let admin = bus.user_by_name("admin").await.unwrap().unwrap();
        let conv = bus.alloc("conv").await.unwrap();
        bus.commit(
            Some(admin.id),
            EventKind::ConvCreated {
                conv,
                kind: ConvKind::Channel,
                name: "fresh".into(),
            },
        )
        .await
        .unwrap();

        let found = bus.conversation_by_name("fresh").await.unwrap();
        assert_eq!(found.map(|c| c.id), Some(conv));

        bus.shutdown();
        let _ = std::fs::remove_file(&path);
    }
}
