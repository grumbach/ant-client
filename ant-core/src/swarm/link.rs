//! One WebRTC link to another client in a room: what it says it holds, the
//! records this client asked it for, and the records it asked this client for.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use ant_protocol::pqc::api::MlDsaPublicKey;
use ant_protocol::{OfferId, RoomKey};
use bytes::Bytes;
use futures::channel::mpsc;
use futures::StreamExt;
use web_time::Instant;

use super::room::{count, member_key, name_of, signed_by, Room};
use super::rtc::{ChannelEvent, ChannelSender};
use super::uploads::UploadSlot;
use super::wire::{self, Assembly, Message, MAX_HAVE_ADDRESSES};
use super::{
    count_one, or_stopped, spawn, update_usize, Counters, DiscoveryCounters, Inner, Pace, PeerRef,
    Stop,
};
use crate::data::XorName;

/// How long a new link may take to open.
pub(super) const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
/// How long a record may go without a piece arriving.
pub(super) const FETCH_STALL: std::time::Duration = std::time::Duration::from_secs(2);
/// How long one read may spend on peers, all of them together, before it asks
/// the nodes.
pub(super) const FETCH_TOTAL: std::time::Duration = std::time::Duration::from_secs(12);
/// The slowest a peer may send a record, in bytes a second: past this, the
/// nodes are likely faster. With [`FETCH_STALL`] it sets a record's deadline
/// once its size is known.
const MIN_PEER_RATE: u64 = 512 * 1024;
/// How long a peer that ran out of time is passed over.
const SLOW_PEER_PAUSE: std::time::Duration = std::time::Duration::from_secs(30);
/// How long a link closed as the duplicate of another stays connected. The
/// peer may not have this end's proof on it yet, and needs it to settle on the
/// same link itself and to learn that both reach this client: a connection
/// torn down at once looks to it like a link that failed, and it offers the
/// same member again, and again.
const SETTLE_GRACE: std::time::Duration = std::time::Duration::from_secs(2);
/// How long sending one record may take before the link is dropped: longer
/// than any asker waits for it.
pub(super) const SEND_DEADLINE: std::time::Duration = std::time::Duration::from_secs(20);
const _: () = assert!(SEND_DEADLINE.as_secs() > FETCH_TOTAL.as_secs());
/// Pieces of one record sent before it lets others have the channel: at 16
/// KiB each, a record waits at most 64 KiB behind another's.
pub(super) const PIECES_PER_TURN: usize = 4;
/// Records this client asks one link for at once. Further reads of that
/// link's records wait here for their turn, where their wait is seen, rather
/// than in the peer's queue, where it is not.
///
/// One: Chromium's data channel stalled for seconds when sent two records
/// at once, a few pieces of each at a time (a 2 MiB record took 3.6 to 5.3 s
/// that way, and 66 to 656 ms alone). A record's pieces still fill the
/// channel, and a read spreads over links.
const MAX_GETS_PER_LINK: usize = 1;
/// Reads that may wait for a turn on one link, behind the one it is asked
/// for: enough to keep the link busy back to back. Reads past that go to the
/// nodes rather than queue behind a peer that may be slower than they are
/// (on a local network, queueing made the last of four readers starting 1.5 s
/// apart 11.5 s slower than with no peers at all).
const MAX_WAITING_PER_LINK: usize = 1;
/// Records one link may be sent by this client at once; past this it is told
/// the client is busy. One more than a peer asks for at once: a record the
/// peer gave up on is still being sent until the next piece sees it, and the
/// peer's next ask may come first.
const MAX_SENDING_PER_LINK: usize = MAX_GETS_PER_LINK + 1;
/// Other names remembered for the peer on one link: one per rendezvous
/// node, twice over.
const MAX_ALIASES: usize = 2 * ant_protocol::SWARM_RENDEZVOUS_NODES;
/// Addresses remembered for what one link holds.
pub(super) const MAX_PEER_HAS: usize = 16_384;
/// Addresses sent in the first `Have` on a new link.
pub(super) const INITIAL_HAVE: usize = 4096;
/// Messages waiting to be sent on one link before it is dropped.
const MAX_OUTBOX: usize = 1024;
/// How often a sent record looks whether the channel has emptied.
const SENT_POLL: std::time::Duration = std::time::Duration::from_millis(10);

/// What became of one record asked for.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum GetOutcome {
    /// The whole record, not yet checked against its address.
    Record(Bytes),
    /// The peer does not have it.
    Missing,
    /// The peer has it but is busy, or this link has as many asks in flight
    /// as it may.
    Busy,
    /// This link has as many reads asking and waiting as it takes, or took
    /// none before the read's time ran out: nothing was asked, and the read
    /// goes elsewhere.
    Full,
    /// Once the read's turn came, the link no longer offered the record (the
    /// peer took it back, answered `Missing` for it to another read, or was
    /// lately too slow): nothing was asked.
    NotOffered,
    /// The link failed, or the record did not arrive in time.
    Failed,
}

enum GetEvent {
    /// A piece of a record `total` bytes long arrived, `received` of them
    /// so far.
    Progress {
        total: u32,
        received: u32,
    },
    Done(Bytes),
    Missing,
    Busy,
    Failed,
}

struct Get {
    address: XorName,
    assembly: Option<Assembly>,
    events: mpsc::UnboundedSender<GetEvent>,
}

#[derive(Default)]
struct State {
    open: bool,
    /// The peer's name in the room, from the key its `Hello` named, once its
    /// `Proof` showed it holds that key: an unproven link has no name, and
    /// so settles no duplicate.
    peer_nonce: Option<[u8; 16]>,
    has: PeerHas,
    gets: HashMap<u32, Get>,
    next_get: u32,
    /// The peer's asks being answered, by id, each with its transfer's
    /// number: an id cancelled and asked again names a new transfer, and the
    /// old one stops.
    serving: HashMap<u32, u64>,
    next_transfer: u64,
    /// Transfers running, cancelled ones included until they stop: the
    /// link's share of the upload turns.
    transfers: usize,
    /// Other names the peer has at the room's nodes, learnt from links to it
    /// settled away.
    aliases: VecDeque<PeerRef>,
    /// Until when the link is passed over, after a record from it ran out of
    /// time.
    slow_until: Option<Instant>,
}

/// What the peer on a link says it holds: the newest [`MAX_PEER_HAS`]
/// addresses its `Have`s named, less those it said are gone or answered
/// `Missing` for. Each costs the same to add or take out, however the peer
/// mixes its `Have`s and `Gone`s.
#[derive(Default)]
struct PeerHas {
    /// Each address held, with the number its newest naming was given.
    addresses: HashMap<XorName, u64>,
    /// Those namings, oldest first. One whose address went since, or was
    /// named again under a newer number, is passed over, and swept out once
    /// as many such are kept as live ones may be.
    order: VecDeque<(XorName, u64)>,
    next: u64,
}

impl PeerHas {
    fn contains(&self, address: &XorName) -> bool {
        self.addresses.contains_key(address)
    }

    /// Notes that the peer holds `address`; past [`MAX_PEER_HAS`], the one
    /// it named longest ago is forgotten.
    fn add(&mut self, address: XorName) {
        if self.addresses.contains_key(&address) {
            return;
        }
        let number = self.next;
        self.next = self.next.wrapping_add(1);
        self.addresses.insert(address, number);
        self.order.push_back((address, number));
        while self.addresses.len() > MAX_PEER_HAS {
            let Some((oldest, named)) = self.order.pop_front() else {
                break;
            };
            if self.addresses.get(&oldest) == Some(&named) {
                self.addresses.remove(&oldest);
            }
        }
        if self.order.len() > 2 * MAX_PEER_HAS {
            let addresses = &self.addresses;
            self.order
                .retain(|(address, named)| addresses.get(address) == Some(named));
        }
    }

    /// Notes that the peer no longer holds `address`.
    fn remove(&mut self, address: &XorName) {
        self.addresses.remove(address);
    }
}

#[derive(Default)]
struct Outbox {
    control: VecDeque<Message>,
    haves: Vec<XorName>,
    /// Records announced, or that may have been, and dropped since: never
    /// also among `haves`, so the peer hears the newest of the two.
    gones: Vec<XorName>,
}

/// A link to one other client in one room.
pub(super) struct Link {
    pub(super) id: u64,
    pub(super) room: RoomKey,
    /// This end's DTLS fingerprint on the link's connection, as its own SDP
    /// gave it: how a page watching its connections tells which one an open
    /// link is.
    pub(super) fingerprint: String,
    /// What the link's room membership fetched and served.
    pub(super) room_counters: Arc<Counters>,
    /// Whether this client made the offer the link came from.
    pub(super) offered: bool,
    /// The offer's id: with the offerer's nonce, it decides which of two links
    /// between the same pair of clients stays.
    pub(super) offer_id: OfferId,
    /// This client's name in the link's room: its own in each room, so peers
    /// meeting it in two rooms cannot tell it is the same client by its name
    /// (its addresses, in its WebRTC candidates, may still let them). It also
    /// tells one membership of a room from a later one.
    pub(super) nonce: [u8; 16],
    /// The peer as the node that introduced it names it, if known.
    pub(super) peer: Option<PeerRef>,
    /// This client's proof for the peer, and the one it expects back.
    own_proof: blake3::Hash,
    peer_proof: blake3::Hash,
    /// This client's key in the room, and its signature over this link.
    own_key: Vec<u8>,
    own_signature: Vec<u8>,
    /// What the peer signs over this link.
    peer_signed: [u8; 32],
    sender: Box<dyn ChannelSender>,
    state: Mutex<State>,
    outbox: Mutex<Outbox>,
    wake: tokio::sync::Notify,
    /// This client's turns to ask the link for a record.
    asking: Arc<tokio::sync::Semaphore>,
    /// Reads waiting for one of those turns.
    waiting: AtomicUsize,
    /// Turns to send one message on the channel, handed out in order, so
    /// records sent at once interleave: in the single-threaded browser a send
    /// that need not wait never yields, and one record would take every turn.
    sending: tokio::sync::Mutex<()>,
    closed: Stop,
    /// Closed as the duplicate of a link kept.
    settled: AtomicBool,
    /// Bytes of every message handed to the channel: with what the channel
    /// still holds, how far its sending has come.
    handed: AtomicU64,
    /// Of those, the bytes the swarm has counted as gone (see
    /// [`Link::sent_since`]).
    counted: AtomicU64,
    /// Records handed to the channel whose bytes have not all left it yet,
    /// each holding its place among the client's sends.
    draining: Mutex<Drains>,
    next_draining: AtomicU64,
}

/// The records a link still drains, and how many asks the peer has made.
#[derive(Default)]
struct Drains {
    /// Asks the peer has made on the link: a send begun before the last of
    /// them has reached the peer, which asks a link for one record at a
    /// time.
    asked: u64,
    records: Vec<Draining>,
}

/// A record handed to the channel, its bytes still leaving it, and the
/// place it holds among the client's sends.
struct Draining {
    number: u64,
    _slot: UploadSlot,
}

impl Link {
    /// A link in `room` for the offer `offer_id`, which this client made if
    /// `offered`, over the connection `binding` names; `None` if this
    /// client's signature over it cannot be made.
    pub(super) fn new(
        id: u64,
        room: &Room,
        offered: bool,
        offer_id: OfferId,
        binding: &Binding,
        peer: Option<PeerRef>,
        sender: Box<dyn ChannelSender>,
    ) -> Option<Self> {
        let fingerprint = binding.fingerprint.clone();
        let binding = &binding.proof;
        let own_signature = room.identity.sign(&signed(binding, &offer_id, offered))?;
        Some(Self {
            id,
            room: room.key,
            fingerprint,
            room_counters: Arc::clone(&room.counters),
            offered,
            offer_id,
            nonce: room.nonce,
            peer,
            own_proof: proof(&room.secret, binding, &offer_id, offered),
            peer_proof: proof(&room.secret, binding, &offer_id, !offered),
            own_key: room.identity.public().to_vec(),
            own_signature,
            peer_signed: signed(binding, &offer_id, !offered),
            sender,
            state: Mutex::default(),
            outbox: Mutex::default(),
            wake: tokio::sync::Notify::new(),
            asking: Arc::new(tokio::sync::Semaphore::new(MAX_GETS_PER_LINK)),
            waiting: AtomicUsize::new(0),
            sending: tokio::sync::Mutex::new(()),
            closed: Stop::default(),
            settled: AtomicBool::new(false),
            handed: AtomicU64::new(0),
            counted: AtomicU64::new(0),
            draining: Mutex::default(),
            next_draining: AtomicU64::new(0),
        })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(super) fn is_open(&self) -> bool {
        self.state().open && !self.closed.is_stopped()
    }

    pub(super) fn is_closed(&self) -> bool {
        self.closed.is_stopped()
    }

    /// Whether the peer said it holds `address`, and has not lately been too
    /// slow to ask.
    pub(super) fn offers(&self, address: &XorName) -> bool {
        let state = self.state();
        let slow = state.slow_until.is_some_and(|until| Instant::now() < until);
        !slow && state.has.contains(address)
    }

    #[cfg(test)]
    pub(super) fn transfers_running(&self) -> usize {
        self.state().transfers
    }

    #[cfg(test)]
    pub(super) fn has(&self, address: &XorName) -> bool {
        self.state().has.contains(address)
    }

    /// Reads waiting for a turn to ask this link.
    #[cfg(test)]
    pub(super) fn reads_waiting(&self) -> usize {
        self.waiting.load(Ordering::Acquire)
    }

    pub(super) fn peer_nonce(&self) -> Option<[u8; 16]> {
        self.state().peer_nonce
    }

    /// Notes another name the peer has, at another rendezvous node: its
    /// newest at each node, for the newest few nodes, as a long read's
    /// rendezvous nodes change.
    pub(super) fn also_known_as(&self, peer: Option<PeerRef>) {
        let Some(peer) = peer else {
            return;
        };
        let mut state = self.state();
        if self.peer != Some(peer) {
            state.aliases.retain(|(node, _)| *node != peer.0);
            if state.aliases.len() >= MAX_ALIASES {
                state.aliases.pop_front();
            }
            state.aliases.push_back(peer);
        }
    }

    /// Whether the peer is a member of rendezvous node `node`, by a name
    /// this client knows.
    pub(super) fn known_at(&self, node: &[u8; 32]) -> bool {
        self.peer.is_some_and(|(at, _)| at == *node)
            || self.state().aliases.iter().any(|(at, _)| at == node)
    }

    /// Closes the link as the duplicate of another to the same peer, kept:
    /// a read of it goes on to that one. The link stops at once; its
    /// connection goes after [`SETTLE_GRACE`], and until then the link stays
    /// among `swarm`'s links, closed, so it still counts against their
    /// limits.
    pub(super) fn settle_away(self: &Arc<Self>, swarm: &Weak<Inner>) {
        // The first to settle it sees to its end, even of a link stopped
        // already (by a room left meanwhile, say), which `finish` now leaves
        // to the timer; a later call leaves that grace be.
        if self.settled.swap(true, Ordering::AcqRel) {
            return;
        }
        self.stop();
        let link = Arc::clone(self);
        let timer_swarm = Weak::clone(swarm);
        let later = spawn(async move {
            crate::runtime::sleep(SETTLE_GRACE).await;
            link.sender.close();
            if let Some(inner) = timer_swarm.upgrade() {
                inner.remove_link(link.id);
            }
        });
        if !later {
            self.sender.close();
            if let Some(inner) = swarm.upgrade() {
                inner.remove_link(self.id);
            }
        }
    }

    /// Whether the link was closed as the duplicate of one kept.
    pub(super) fn settled_away(&self) -> bool {
        self.settled.load(Ordering::Acquire)
    }

    /// Closes the link and its connection, and fails every record asked of
    /// it.
    pub(super) fn close(&self) {
        self.stop();
        self.sender.close();
    }

    /// Stops the link, leaving its connection be, and fails every record
    /// asked of it. Returns whether it was running until now.
    fn stop(&self) -> bool {
        if self.closed.is_stopped() {
            return false;
        }
        self.closed.stop();
        let gets = std::mem::take(&mut self.state().gets);
        for get in gets.into_values() {
            let _ = get.events.unbounded_send(GetEvent::Failed);
        }
        self.wake.notify_waiters();
        true
    }

    /// Queues a message for the writer; a link that cannot keep up is closed.
    fn queue(&self, message: Message) -> bool {
        {
            let mut outbox = self.outbox.lock().unwrap_or_else(PoisonError::into_inner);
            if outbox.control.len() >= MAX_OUTBOX {
                drop(outbox);
                self.close();
                return false;
            }
            outbox.control.push_back(message);
        }
        self.wake.notify_one();
        true
    }

    /// Tells the peer about records this client holds.
    pub(super) fn announce(&self, addresses: &[XorName]) {
        {
            let mut outbox = self.outbox.lock().unwrap_or_else(PoisonError::into_inner);
            if !outbox.gones.is_empty() {
                outbox.gones.retain(|address| !addresses.contains(address));
            }
            outbox.haves.extend_from_slice(addresses);
            let excess = outbox.haves.len().saturating_sub(MAX_PEER_HAS);
            outbox.haves.drain(..excess);
        }
        self.wake.notify_one();
    }

    /// Tells the peer that records this client announced, or may have, are
    /// no longer held: dropped from the store to make room. Without this the
    /// peer would go on asking for them, and be answered `Missing` each time.
    /// A link with more than [`MAX_PEER_HAS`] of them still unsent is closed,
    /// as one whose other messages back up is: leaving any out would leave
    /// the peer asking for a record gone.
    pub(super) fn retract(&self, addresses: &[XorName]) {
        if addresses.is_empty() {
            return;
        }
        let behind = {
            let mut outbox = self.outbox.lock().unwrap_or_else(PoisonError::into_inner);
            outbox.haves.retain(|address| !addresses.contains(address));
            outbox.gones.extend_from_slice(addresses);
            outbox.gones.len() > MAX_PEER_HAS
        };
        if behind {
            self.close();
            return;
        }
        self.wake.notify_one();
    }

    /// Whether a read may ask this link now: a turn is free, or fewer reads
    /// wait for one than may.
    pub(super) fn takes_asks(&self) -> bool {
        self.asking.available_permits() > 0
            || self.waiting.load(Ordering::Acquire) < MAX_WAITING_PER_LINK
    }

    /// Asks the peer for a record and waits for it, until `deadline` at the
    /// latest. A peer that runs out of time is passed over for a while.
    pub(super) async fn get(&self, address: XorName, deadline: Instant, pace: &Pace) -> GetOutcome {
        pace.note(0, 0);
        // A turn now, or a place among the few waiting for one while the
        // link's ask finishes; past that the read goes elsewhere.
        let _turn = match Arc::clone(&self.asking).try_acquire_owned() {
            Ok(turn) => turn,
            Err(_) => {
                let Some(_waiting) = Waiting::enter(&self.waiting) else {
                    return GetOutcome::Full;
                };
                let left = deadline.saturating_duration_since(Instant::now());
                let turn = crate::runtime::timeout(
                    left,
                    or_stopped(&self.closed, Arc::clone(&self.asking).acquire_owned()),
                )
                .await;
                match turn {
                    Ok(Some(Ok(turn))) => turn,
                    Err(_) => return GetOutcome::Full,
                    Ok(_) => return GetOutcome::Failed,
                }
            }
        };
        if !self.offers(&address) {
            return GetOutcome::NotOffered;
        }
        let (sender, mut events) = mpsc::unbounded();
        let id = {
            let mut state = self.state();
            if !state.open || state.gets.len() >= MAX_GETS_PER_LINK {
                return GetOutcome::Busy;
            }
            let mut id = state.next_get;
            while state.gets.contains_key(&id) {
                id = id.wrapping_add(1);
            }
            state.next_get = id.wrapping_add(1);
            state.gets.insert(
                id,
                Get {
                    address,
                    assembly: None,
                    events: sender,
                },
            );
            id
        };
        // The reader forgets an ask the peer answered; one still known when
        // this returns was given up on, and the peer is told.
        let _guard = GetGuard { link: self, id };
        if !self.queue(Message::Get { id, address }) {
            return GetOutcome::Failed;
        }
        let started = Instant::now();
        let mut deadline = deadline;
        // The time the peer's record may take, once its size is known.
        let mut sending_by: Option<Instant> = None;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let wait = FETCH_STALL.min(left);
            match crate::runtime::timeout(wait, events.next()).await {
                Ok(Some(GetEvent::Progress { total, received })) => {
                    pace.note(u64::from(received), u64::from(total));
                    if sending_by.is_none() {
                        let sending = std::time::Duration::from_millis(
                            u64::from(total).saturating_mul(1000) / MIN_PEER_RATE,
                        );
                        let by = started + FETCH_STALL + sending;
                        sending_by = Some(by);
                        deadline = deadline.min(by);
                    }
                }
                Ok(Some(GetEvent::Done(record))) => return GetOutcome::Record(record),
                Ok(Some(GetEvent::Missing)) => return GetOutcome::Missing,
                Ok(Some(GetEvent::Busy)) => return GetOutcome::Busy,
                Ok(Some(GetEvent::Failed) | None) => return GetOutcome::Failed,
                Err(_) => {
                    // A peer that sent nothing for a whole stall, or ran past
                    // the time its record's size allows, is passed over a
                    // while; the read's own time running out says nothing of
                    // the peer.
                    let stalled = wait >= FETCH_STALL;
                    let overran = sending_by.is_some_and(|by| Instant::now() >= by);
                    if stalled || overran {
                        self.state().slow_until = Some(Instant::now() + SLOW_PEER_PAUSE);
                    }
                    return GetOutcome::Failed;
                }
            }
        }
    }

    /// Reads the link until it closes. `swarm` is the swarm it belongs to.
    pub(super) async fn run(
        self: Arc<Self>,
        swarm: Weak<Inner>,
        mut events: mpsc::Receiver<ChannelEvent>,
    ) {
        // One deadline for the channel to open and the peer to prove itself.
        // A link this client answered starts before the offerer has even had
        // the answer, so it waits for that too.
        let within = if self.offered {
            CONNECT_TIMEOUT
        } else {
            super::room::ANSWERED_OPEN_TIMEOUT
        };
        let deadline = Instant::now() + within;
        // A message comes only on an open channel, and some stacks report
        // the opening after it (webrtc-rs runs its open callback on a task of
        // its own, beside the one reading messages): a message before `Open`
        // opens the link, and is read first.
        let opened = crate::runtime::timeout(
            within,
            or_stopped(&self.closed, async {
                match events.next().await {
                    Some(ChannelEvent::Open) => Some(None),
                    Some(ChannelEvent::Message(bytes)) => Some(Some(bytes)),
                    Some(ChannelEvent::Closed) | None => None,
                }
            }),
        )
        .await;
        let Ok(Some(Some(mut early))) = opened else {
            count(&swarm, 1, |discovery| &discovery.links_failed);
            self.finish(&swarm);
            return;
        };
        count(&swarm, 1, |discovery| &discovery.channels_opened);
        // This end's name and proof go out before the peer's are read, not
        // through the writer: a link settled away as a duplicate the moment
        // the peer's proof is read has still given the peer its own, so the
        // peer settles the same way rather than offering again.
        let introduced = crate::runtime::timeout(
            deadline.saturating_duration_since(Instant::now()),
            or_stopped(&self.closed, async {
                self.send(
                    Message::Hello {
                        key: self.own_key.clone(),
                    }
                    .encode(),
                )
                .await?;
                self.send(
                    Message::Proof {
                        mac: *self.own_proof.as_bytes(),
                        signature: self.own_signature.clone(),
                    }
                    .encode(),
                )
                .await
            }),
        )
        .await;
        if !matches!(introduced, Ok(Some(Ok(())))) {
            count(&swarm, 1, |discovery| &discovery.links_failed);
            self.finish(&swarm);
            return;
        }
        if !spawn(Arc::clone(&self).write()) {
            count(&swarm, 1, |discovery| &discovery.links_failed);
            self.finish(&swarm);
            return;
        }
        // The peer proves it knows the room's secret, and holds the key it
        // names itself by, for this connection, before anything else: until
        // then nothing of the room is said or served, and its name settles no
        // duplicate.
        let proven = crate::runtime::timeout(
            deadline.saturating_duration_since(Instant::now()),
            or_stopped(&self.closed, async {
                // The name and key the peer's `Hello` gave, until its proof.
                let mut named: Option<([u8; 16], MlDsaPublicKey)> = None;
                loop {
                    let event = match early.take() {
                        Some(bytes) => ChannelEvent::Message(bytes),
                        None => match events.next().await {
                            Some(event) => event,
                            None => return false,
                        },
                    };
                    let bytes = match event {
                        ChannelEvent::Message(bytes) => bytes,
                        ChannelEvent::Open => continue,
                        ChannelEvent::Closed => return false,
                    };
                    // `Hello` first, once, then `Proof`: anything else ends
                    // the link, so every open link has its peer's name.
                    match Message::decode(&bytes) {
                        Ok(Message::Hello { key }) => {
                            if named.is_some() {
                                return false;
                            }
                            let Some(peer_key) = member_key(&key) else {
                                return false;
                            };
                            named = Some((name_of(&key), peer_key));
                        }
                        Ok(Message::Proof { mac, signature }) => {
                            let Some((name, peer_key)) = named.take() else {
                                return false;
                            };
                            if blake3::Hash::from_bytes(mac) != self.peer_proof
                                || !signed_by(&peer_key, &self.peer_signed, &signature)
                            {
                                return false;
                            }
                            self.state().peer_nonce = Some(name);
                            return true;
                        }
                        _ => return false,
                    }
                }
            }),
        )
        .await;
        if !matches!(proven, Ok(Some(true))) {
            count(&swarm, 1, |discovery| &discovery.links_failed);
            self.finish(&swarm);
            return;
        }
        let Some(inner) = swarm.upgrade() else {
            self.close();
            return;
        };
        DiscoveryCounters::add(&inner.discovery.links_proven, 1);
        self.state().open = true;
        inner.settle_duplicates(&self);
        // Only the records of this link's room's file, listed and announced
        // in one turn with the store's changes: a record dropped meanwhile is
        // either left out here or taken back after.
        if let Some(room) = inner.room(&self.room) {
            let _publishing = inner.publishing();
            self.announce(
                &inner
                    .store
                    .newest_where(INITIAL_HAVE, |address| room.holds(address)),
            );
        }
        drop(inner);
        while let Some(Some(event)) = or_stopped(&self.closed, events.next()).await {
            let keep = match event {
                ChannelEvent::Message(bytes) => match Message::decode(&bytes) {
                    Ok(message) => self.handle(message, &swarm),
                    Err(_) => false,
                },
                ChannelEvent::Open => true,
                ChannelEvent::Closed => false,
            };
            if !keep {
                break;
            }
        }
        self.finish(&swarm);
    }

    fn finish(&self, swarm: &Weak<Inner>) {
        // A link settled away keeps its connection, and its place among the
        // links, for the grace: its timer lets both go.
        if self.settled_away() {
            self.stop();
            return;
        }
        self.close();
        if let Some(inner) = swarm.upgrade() {
            inner.remove_link(self.id);
        }
    }

    /// Handles one message; returns whether to keep the link.
    fn handle(self: &Arc<Self>, message: Message, swarm: &Weak<Inner>) -> bool {
        match message {
            // Only before the link opens: its peer's name is fixed by then.
            Message::Hello { .. } => true,
            Message::Have { addresses } => {
                let mut state = self.state();
                for address in addresses {
                    state.has.add(address);
                }
                true
            }
            Message::Gone { addresses } => {
                let mut state = self.state();
                for address in &addresses {
                    state.has.remove(address);
                }
                true
            }
            Message::Get { id, address } => {
                self.serve(id, address, swarm);
                true
            }
            Message::Piece {
                id,
                total,
                offset,
                data,
            } => self.piece(id, total, offset, &data),
            Message::Missing { id } => {
                let mut state = self.state();
                if let Some(get) = state.gets.remove(&id) {
                    state.has.remove(&get.address);
                    let _ = get.events.unbounded_send(GetEvent::Missing);
                }
                true
            }
            Message::Busy { id } => {
                if let Some(get) = self.state().gets.remove(&id) {
                    let _ = get.events.unbounded_send(GetEvent::Busy);
                }
                true
            }
            Message::Cancel { id } => {
                self.state().serving.remove(&id);
                true
            }
            // Its proof checked out before the link opened.
            Message::Proof { .. } => true,
        }
    }

    /// Adds a piece of a record this client asked for. A piece out of order
    /// or of the wrong size ends the link; one for an ask this client gave up
    /// on is ignored.
    fn piece(&self, id: u32, total: u32, offset: u32, data: &[u8]) -> bool {
        let mut state = self.state();
        let Some(get) = state.gets.get_mut(&id) else {
            return true;
        };
        if get.assembly.is_none() {
            get.assembly = Assembly::new(total);
        }
        let added = match get.assembly.as_mut() {
            Some(assembly) => assembly.add(total, offset, data),
            None => Err(wire::WireError::PieceOutOfBounds),
        };
        match added {
            Ok(None) => {
                let received = offset.saturating_add(u32::try_from(data.len()).unwrap_or(u32::MAX));
                let _ = get
                    .events
                    .unbounded_send(GetEvent::Progress { total, received });
                true
            }
            Ok(Some(record)) => {
                if let Some(get) = state.gets.remove(&id) {
                    let _ = get.events.unbounded_send(GetEvent::Done(record));
                }
                true
            }
            Err(_) => {
                if let Some(get) = state.gets.remove(&id) {
                    let _ = get.events.unbounded_send(GetEvent::Failed);
                }
                false
            }
        }
    }

    /// Answers the peer's ask for a record: its pieces, or why not.
    fn serve(self: &Arc<Self>, id: u32, address: XorName, swarm: &Weak<Inner>) {
        let Some(inner) = swarm.upgrade() else {
            return;
        };
        // A peer asks a link for one record at a time: asking again, it has
        // the last one, and its place is free, whatever the channel says it
        // still holds.
        let asked = self.asked_again();
        let record = if inner.room_holds(&self.room, &address) {
            inner.store.get(&address)
        } else {
            None
        };
        let counters = Arc::clone(&inner.counters);
        let Some(record) = record else {
            count_one(&counters.refused_missing);
            self.queue(Message::Missing { id });
            return;
        };
        let number = {
            let mut state = self.state();
            if state.transfers >= MAX_SENDING_PER_LINK || state.serving.contains_key(&id) {
                None
            } else {
                let number = state.next_transfer;
                state.next_transfer = state.next_transfer.wrapping_add(1);
                state.serving.insert(id, number);
                state.transfers += 1;
                Some(number)
            }
        };
        let Some(number) = number else {
            count_one(&counters.refused_busy_link);
            self.queue(Message::Busy { id });
            return;
        };
        // Gives the place back however the transfer ends, even unstarted.
        let transfer = Transfer {
            link: Arc::clone(self),
            id,
            number,
        };
        // As many records at once as the client's upload rate gives each the
        // rate a peer needs.
        let Some(slot) = inner.start_upload() else {
            drop(transfer);
            count_one(&counters.refused_busy_uploads);
            self.queue(Message::Busy { id });
            return;
        };
        let room_counters = Arc::clone(&self.room_counters);
        drop(inner);
        let link = Arc::clone(self);
        spawn(async move {
            let slot = slot;
            let transfer = transfer;
            let started = Instant::now();
            // A peer that stops reading would hold the send, and its upload
            // slot, for ever: past the deadline the link goes.
            let sending = async {
                let mut any = false;
                for (sent, piece) in wire::pieces(id, &record).enumerate() {
                    // Turns go in order only among records already waiting
                    // for one: every few pieces, this record lets the others,
                    // and a browser's newly arrived asks, have theirs.
                    if sent > 0 && sent % PIECES_PER_TURN == 0 {
                        crate::runtime::yield_now().await;
                    }
                    // Looked at last thing before each piece. An asker never
                    // asks again under an id it cancelled (its ids only
                    // grow), so a piece of a cancelled transfer already on
                    // its way can confuse only an asker that did. What it
                    // sent before still has to leave the channel.
                    if !transfer.wanted() {
                        return Handed::Part(any);
                    }
                    if link.send(piece.encode()).await.is_err() {
                        link.close();
                        return Handed::Failed;
                    }
                    any = true;
                }
                Handed::Whole
            };
            let handed = match crate::runtime::timeout(SEND_DEADLINE, sending).await {
                Ok(handed) => handed,
                Err(_) => {
                    link.close();
                    Handed::Failed
                }
            };
            drop(transfer);
            let draining = match handed {
                Handed::Whole => {
                    counters.served(record.len());
                    room_counters.served(record.len());
                    true
                }
                Handed::Part(any) => any,
                Handed::Failed => false,
            };
            // A hidden page's timers are slowed, which would hold the place
            // long after the channel emptied: there it goes once handed over.
            if !draining || crate::runtime::page_hidden() {
                return;
            }
            // The place is held until the channel holds nothing more, or the
            // peer asks again, which means the record reached it: if it has
            // asked again already, the place goes now.
            let number = link.next_draining.fetch_add(1, Ordering::Relaxed);
            if !link.hold_until_sent(number, asked, slot) {
                return;
            }
            // A page hidden meanwhile lets the place go at once, as its timers
            // slow down.
            let drain = {
                let waiting = std::pin::pin!(link.until_empty(number, started + SEND_DEADLINE));
                let hidden = std::pin::pin!(crate::runtime::until_page_hidden());
                match futures::future::select(waiting, hidden).await {
                    futures::future::Either::Left((drain, _)) => drain,
                    futures::future::Either::Right(_) => Drain::Ended,
                }
            };
            link.drained(number);
            if drain == Drain::Stalled {
                // Bytes still not gone past the deadline, with nothing newer
                // asked of the link: a peer that stopped reading, which ends
                // the link as a send that stalls does (already stopped when
                // the stall was found).
                link.close();
            }
        });
    }

    /// Sends one message in its turn.
    async fn send(&self, message: Vec<u8>) -> Result<(), super::rtc::RtcError> {
        let _turn = self.sending.lock().await;
        let bytes = u64::try_from(message.len()).unwrap_or(u64::MAX);
        self.sender.send(message).await?;
        self.handed.fetch_add(bytes, Ordering::AcqRel);
        Ok(())
    }

    fn drains(&self) -> MutexGuard<'_, Drains> {
        self.draining.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Counts the peer's new ask, and frees the places of the records the
    /// link still drains: the peer has them. Returns the count.
    fn asked_again(&self) -> u64 {
        let (asked, freed) = {
            let mut drains = self.drains();
            drains.asked = drains.asked.wrapping_add(1);
            (drains.asked, std::mem::take(&mut drains.records))
        };
        // Given back outside the lock.
        drop(freed);
        asked
    }

    /// Holds `slot` for the record `number`, sent for the peer's ask
    /// `asked`, until its channel empties; unless the peer has asked again
    /// since, which frees it now. Returns whether it is held.
    fn hold_until_sent(&self, number: u64, asked: u64, slot: UploadSlot) -> bool {
        let mut drains = self.drains();
        if drains.asked != asked {
            drop(drains);
            drop(slot);
            return false;
        }
        drains.records.push(Draining {
            number,
            _slot: slot,
        });
        true
    }

    /// Whether the draining record `number`, past its deadline, still holds
    /// its place: if so the link stops, in the same step, so a peer asking
    /// again at that moment either retires it first or finds the link
    /// stopped.
    fn stalled(&self, number: u64) -> Drain {
        let mut drains = self.drains();
        let Some(position) = drains
            .records
            .iter()
            .position(|draining| draining.number == number)
        else {
            return Drain::Ended;
        };
        let claimed = drains.records.remove(position);
        self.stop();
        drop(drains);
        drop(claimed);
        Drain::Stalled
    }

    /// Frees the place of the draining record `number`, if it still holds
    /// one.
    fn drained(&self, number: u64) {
        let freed: Vec<Draining> = {
            let mut drains = self.drains();
            let (freed, kept) = std::mem::take(&mut drains.records)
                .into_iter()
                .partition(|record| record.number == number);
            drains.records = kept;
            freed
        };
        // Given back outside the lock.
        drop(freed);
    }

    /// Waits until the channel holds nothing unsent, while the draining
    /// record `number` still holds its place: by `by`, or it stalled.
    /// Natively a channel can have its later bytes acknowledged before
    /// earlier ones are sent again, so only an empty channel tells that a
    /// record has left it whole.
    async fn until_empty(&self, number: u64, by: Instant) -> Drain {
        loop {
            let unsent = self.sender.unsent().await;
            let now = Instant::now();
            if self.is_closed() {
                return Drain::Ended;
            }
            let held = self
                .drains()
                .records
                .iter()
                .any(|draining| draining.number == number);
            // Its peer asked again: it has the record, and what the channel
            // still holds is no longer this send's.
            if !held || unsent == 0 {
                return Drain::Ended;
            }
            if now > by {
                return self.stalled(number);
            }
            crate::runtime::sleep(SENT_POLL.min(by.saturating_duration_since(now))).await;
        }
    }

    /// Bytes that have left the channel (natively, that the peer has
    /// acknowledged) since the last time this was asked.
    pub(super) async fn sent_since(&self) -> u64 {
        let handed = self.handed.load(Ordering::Acquire);
        let unsent = u64::try_from(self.sender.unsent().await).unwrap_or(u64::MAX);
        let left = handed.saturating_sub(unsent);
        left.saturating_sub(self.counted.fetch_max(left, Ordering::AcqRel))
    }

    /// Sends queued messages until the link closes, gathering the addresses
    /// waiting to be taken back or announced into as few `Gone` and `Have`
    /// messages as fit.
    async fn write(self: Arc<Self>) {
        loop {
            let notified = self.wake.notified();
            let (control, gones, haves) = {
                let mut outbox = self.outbox.lock().unwrap_or_else(PoisonError::into_inner);
                (
                    std::mem::take(&mut outbox.control),
                    std::mem::take(&mut outbox.gones),
                    std::mem::take(&mut outbox.haves),
                )
            };
            if self.closed.is_stopped() {
                return;
            }
            if control.is_empty() && gones.is_empty() && haves.is_empty() {
                if or_stopped(&self.closed, notified).await.is_none() {
                    return;
                }
                continue;
            }
            let gones = gones
                .chunks(MAX_HAVE_ADDRESSES)
                .map(|addresses| Message::Gone {
                    addresses: addresses.to_vec(),
                });
            let haves = haves
                .chunks(MAX_HAVE_ADDRESSES)
                .map(|addresses| Message::Have {
                    addresses: addresses.to_vec(),
                });
            for message in control.into_iter().chain(gones).chain(haves) {
                // A peer that stops reading would hold the writer for ever.
                let sent =
                    crate::runtime::timeout(SEND_DEADLINE, self.send(message.encode())).await;
                if !matches!(sent, Ok(Ok(()))) {
                    self.close();
                    return;
                }
            }
        }
    }
}

/// How a record's place came to end.
#[derive(Debug, PartialEq, Eq)]
enum Drain {
    /// Its channel emptied, its peer asked again, or the link closed.
    Ended,
    /// Its channel still held bytes past the send's deadline.
    Stalled,
}

/// What became of a record's send.
enum Handed {
    /// Every piece went to the channel.
    Whole,
    /// The asker cancelled it partway, after some of its pieces went, or
    /// before any did.
    Part(bool),
    /// The link failed, or the send ran out of time.
    Failed,
}

/// A read waiting for a turn on a link, counted while it waits.
struct Waiting<'a>(&'a AtomicUsize);

impl<'a> Waiting<'a> {
    /// A place among those waiting, if there is one left.
    fn enter(count: &'a AtomicUsize) -> Option<Self> {
        update_usize(count, |waiting| {
            (waiting < MAX_WAITING_PER_LINK).then_some(waiting + 1)
        })
        .ok()
        .map(|_| Self(count))
    }
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// The proof a link's offerer (`offerer`) or answerer sends: that it knows
/// the room's `secret`, for this connection alone. The secret comes from the
/// file's root map, which a node that only knows the room's key does not
/// have (for a private file); `binding` comes from both ends' DTLS
/// fingerprints, so a node that put itself between two clients, with an
/// offer and an answer of its own, cannot pass one client's proof on to the
/// other.
pub(super) fn proof(
    secret: &[u8; 32],
    binding: &[u8; 32],
    offer_id: &OfferId,
    offerer: bool,
) -> blake3::Hash {
    let mut hasher = blake3::Hasher::new_keyed(secret);
    hasher.update(b"ant-swarm link proof v1");
    hasher.update(&[u8::from(offerer)]);
    hasher.update(binding);
    hasher.update(offer_id);
    hasher.finalize()
}

/// What a link's offerer (`offerer`) or answerer signs with its key in the
/// room: this connection (`binding`, see [`binding`]) and offer, and its side
/// of it, so a signature made for one link proves nothing on another, and a
/// member cannot name itself by another member's key.
pub(super) fn signed(binding: &[u8; 32], offer_id: &OfferId, offerer: bool) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new_derive_key("ant-swarm link signature v1");
    hasher.update(&[u8::from(offerer)]);
    hasher.update(binding);
    hasher.update(offer_id);
    *hasher.finalize().as_bytes()
}

/// What ties a link's proofs to its connection: the DTLS fingerprints in the
/// offer and the answer, as both ends see them. Each end's WebRTC stack
/// checks the other's DTLS certificate against these.
/// What ties a link to its connection: a hash of both ends' DTLS
/// fingerprints, which both ends' proofs cover, and this end's own
/// fingerprint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Binding {
    pub(super) proof: [u8; 32],
    pub(super) fingerprint: String,
}

impl Binding {
    /// The binding of the connection an offer and its answer make, on the
    /// end that made the offer if `offered`.
    pub(super) fn of(offer_sdp: &str, answer_sdp: &str, offered: bool) -> Self {
        let own = if offered { offer_sdp } else { answer_sdp };
        Self {
            proof: binding(offer_sdp, answer_sdp),
            fingerprint: fingerprint(own),
        }
    }
}

/// The first DTLS fingerprint `sdp` names, as `sha-256 AB:CD:...`.
pub(super) fn fingerprint(sdp: &str) -> String {
    sdp.lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix("a=fingerprint:"))
        .map(|fingerprint| fingerprint.trim().to_string())
        .unwrap_or_default()
}

pub(super) fn binding(offer_sdp: &str, answer_sdp: &str) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new_derive_key("ant-swarm link binding v1");
    for (side, sdp) in [(b'o', offer_sdp), (b'a', answer_sdp)] {
        hasher.update(&[side]);
        for fingerprint in sdp
            .lines()
            .map(str::trim)
            .filter(|line| line.starts_with("a=fingerprint:"))
        {
            hasher.update(fingerprint.as_bytes());
            hasher.update(b"\n");
        }
    }
    *hasher.finalize().as_bytes()
}

/// A record being sent to the peer: its place among the link's transfers,
/// given back however the transfer ends.
struct Transfer {
    link: Arc<Link>,
    id: u32,
    number: u64,
}

impl Transfer {
    /// Whether the peer still wants this transfer: not cancelled, nor asked
    /// again under the same id, and the link is open.
    fn wanted(&self) -> bool {
        self.link.state().serving.get(&self.id) == Some(&self.number) && !self.link.is_closed()
    }
}

impl Drop for Transfer {
    fn drop(&mut self) {
        let mut state = self.link.state();
        state.transfers = state.transfers.saturating_sub(1);
        if state.serving.get(&self.id) == Some(&self.number) {
            state.serving.remove(&self.id);
        }
    }
}

/// Forgets an ask when its waiter stops waiting, and tells the peer.
struct GetGuard<'a> {
    link: &'a Link,
    id: u32,
}

impl Drop for GetGuard<'_> {
    fn drop(&mut self) {
        let removed = self.link.state().gets.remove(&self.id).is_some();
        if removed && !self.link.is_closed() {
            self.link.queue(Message::Cancel { id: self.id });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn address(n: usize) -> XorName {
        let mut address = [0; 32];
        if let Some(head) = address.first_chunk_mut::<8>() {
            *head = u64::try_from(n).unwrap_or(u64::MAX).to_be_bytes();
        }
        address
    }

    #[test]
    fn a_peer_holds_its_newest_named_records_less_those_it_took_back() {
        let mut has = PeerHas::default();
        for n in 0..=MAX_PEER_HAS {
            has.add(address(n));
        }
        // Past the limit, the one named longest ago goes.
        assert!(!has.contains(&address(0)));
        assert!(has.contains(&address(1)));
        assert!(has.contains(&address(MAX_PEER_HAS)));
        has.remove(&address(1));
        assert!(!has.contains(&address(1)));
        // Named again, it is held as newly named: its first naming, still
        // queued, does not push it out early.
        has.add(address(1));
        has.add(address(MAX_PEER_HAS + 1));
        assert!(has.contains(&address(1)));
        assert!(!has.contains(&address(2)));
        assert_eq!(has.addresses.len(), MAX_PEER_HAS);
    }

    #[test]
    fn a_peer_naming_and_taking_back_records_over_and_over_costs_bounded_room() {
        let mut has = PeerHas::default();
        for n in 0..5 * MAX_PEER_HAS {
            has.add(address(n % 7));
            assert!(has.order.len() <= 2 * MAX_PEER_HAS);
            has.remove(&address(n % 7));
        }
        assert!(has.addresses.is_empty());
    }
}
