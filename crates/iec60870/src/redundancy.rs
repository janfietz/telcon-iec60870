//! Manage a group of redundant IEC 60870-5-104 connections.
//!
//! The standard (§5.1) allows a controlling station to maintain several TCP
//! sessions to one or more controlled stations, with at most one in
//! data-transfer state at any time. [`RedundancyServer`] applies that rule
//! on the server side: every accepted peer is tracked, but only the peer
//! that has completed the STARTDT handshake is considered "active". A
//! STARTDT from a new peer demotes the previous active one by closing its
//! TCP session (the spec forbids server-initiated STOPDT, so connection
//! teardown is the spec-conformant demote signal).
//!
//! The application talks to the active peer through [`recv_asdu`] (merged
//! inbound stream tagged with the source `SocketAddr`) and [`send_active`]
//! (outbound spontaneous send to whichever peer currently holds the slot).
//!
//! Production hardening — set both knobs unless you control every peer:
//! - [`RedundancyConfig::max_peers`] caps the number of concurrently
//!   accepted sessions; accepts beyond the cap are dropped immediately.
//! - [`RedundancyConfig::promote_filter`] gates which peer addresses are
//!   allowed to take over the data link. Rejected promotions close the
//!   peer's TCP session.
//!
//! [`recv_asdu`]: RedundancyServer::recv_asdu
//! [`send_active`]: RedundancyServer::send_active

use std::collections::HashMap;
use std::fmt;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use iec60870_proto::frame104::State;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::error::{Error, Result};
use crate::server104::{Server104, ServerEvent, ServerEvents, ServerSender};

/// Decision returned by [`RedundancyConfig::promote_filter`].
type PromoteFilter = Arc<dyn Fn(SocketAddr) -> bool + Send + Sync>;

/// Configuration for [`RedundancyServer`].
#[derive(Clone)]
pub struct RedundancyConfig {
    /// Maximum number of concurrently accepted peers. New accepts that
    /// would push the count above this are dropped, which closes their
    /// TCP session. Default: 8.
    pub max_peers: usize,
    /// Optional filter consulted whenever a peer reaches the data-transfer
    /// state. Returning `false` rejects the promotion and disconnects the
    /// peer. The filter is invoked under an internal lock — keep it cheap
    /// and non-blocking. Default: every peer is accepted.
    pub promote_filter: Option<PromoteFilter>,
}

impl Default for RedundancyConfig {
    fn default() -> Self {
        Self {
            max_peers: 8,
            promote_filter: None,
        }
    }
}

impl fmt::Debug for RedundancyConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RedundancyConfig")
            .field("max_peers", &self.max_peers)
            .field(
                "promote_filter",
                &self.promote_filter.as_ref().map(|_| "<fn>"),
            )
            .finish()
    }
}

/// Group manager that holds an unbounded number of concurrent
/// [`Server104`] connections and exposes a single "active peer" view of
/// them in line with IEC 60870-5-104 redundancy.
pub struct RedundancyServer {
    state: Arc<Mutex<Inner>>,
    asdu_rx: mpsc::Receiver<(SocketAddr, Vec<u8>)>,
    accept_task: JoinHandle<()>,
}

/// Snapshot of one moment in the manager's state. Kept tiny so the sync
/// `Mutex` is held only briefly.
#[derive(Default)]
struct Inner {
    /// Every accepted peer's cloneable send handle. Removing an entry
    /// drops its `ServerSender` which closes the driver's command channel
    /// and tears the underlying TCP connection down.
    peers: HashMap<SocketAddr, ServerSender>,
    /// The single peer (if any) that has completed STARTDT and is owning
    /// the data link.
    active: Option<SocketAddr>,
}

impl fmt::Debug for RedundancyServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let guard = lock_state(&self.state);
        f.debug_struct("RedundancyServer")
            .field("peers", &guard.peers.keys().collect::<Vec<_>>())
            .field("active", &guard.active)
            .finish()
    }
}

impl RedundancyServer {
    /// Start the redundancy manager with default configuration.
    pub fn spawn(server: Server104) -> Self {
        Self::spawn_with(server, RedundancyConfig::default())
    }

    /// Start the redundancy manager with explicit configuration.
    pub fn spawn_with(server: Server104, config: RedundancyConfig) -> Self {
        let state = Arc::new(Mutex::new(Inner::default()));
        let (asdu_tx, asdu_rx) = mpsc::channel(128);
        let accept_task = tokio::spawn(accept_loop(server, config, Arc::clone(&state), asdu_tx));
        Self {
            state,
            asdu_rx,
            accept_task,
        }
    }

    /// Receive the next ASDU from whichever peer was active at the time
    /// the bytes left the wire. Returns `(peer, bytes)`; `None` only when
    /// the manager itself has shut down.
    ///
    /// The returned `peer` reflects the active peer at decode time, not
    /// necessarily at receive time — by the time the caller reads, a
    /// failover may already have occurred. Re-check [`active_peer`] if
    /// freshness matters.
    ///
    /// [`active_peer`]: Self::active_peer
    pub async fn recv_asdu(&mut self) -> Option<(SocketAddr, Vec<u8>)> {
        self.asdu_rx.recv().await
    }

    /// Send an ASDU to the currently active peer. Returns the address it
    /// was routed to, or [`Error::NoActivePeer`] when no peer holds the
    /// slot.
    pub async fn send_active(&self, asdu: Vec<u8>) -> Result<SocketAddr> {
        // Lock just long enough to clone the sender; the actual send can
        // await freely without blocking other state mutations.
        let sender = {
            let guard = lock_state(&self.state);
            let peer = guard.active.ok_or(Error::NoActivePeer)?;
            guard.peers.get(&peer).cloned().ok_or(Error::NoActivePeer)?
        };
        let peer = sender.peer();
        sender.send_asdu(asdu).await?;
        Ok(peer)
    }

    /// The peer that currently holds the data-link slot, or `None` if no
    /// connected peer has completed STARTDT.
    pub async fn active_peer(&self) -> Option<SocketAddr> {
        lock_state(&self.state).active
    }

    /// Snapshot of every connected peer's address, in unspecified order.
    pub async fn peers(&self) -> Vec<SocketAddr> {
        lock_state(&self.state).peers.keys().copied().collect()
    }
}

impl Drop for RedundancyServer {
    fn drop(&mut self) {
        // Stop accepting new peers.
        self.accept_task.abort();
        // Tear down every remaining session by dropping its sender. The
        // matching driver task will observe the closed cmd channel and
        // exit; that in turn closes the event channel and drains any
        // still-running per-peer drive task.
        let mut guard = lock_state(&self.state);
        guard.peers.clear();
        guard.active = None;
    }
}

/// Lock the state, recovering from poisoning by logging and using the
/// inner value. We prefer continuing over panicking because the inner
/// mutations are atomic enough that a recovered state is still consistent
/// for subsequent operations.
fn lock_state(m: &Mutex<Inner>) -> MutexGuard<'_, Inner> {
    m.lock()
        .unwrap_or_else(|e: PoisonError<MutexGuard<'_, Inner>>| {
            tracing::error!(
                target: "iec60870::redundancy",
                "redundancy state mutex was poisoned — recovering inner state",
            );
            e.into_inner()
        })
}

async fn accept_loop(
    server: Server104,
    config: RedundancyConfig,
    state: Arc<Mutex<Inner>>,
    asdu_tx: mpsc::Sender<(SocketAddr, Vec<u8>)>,
) {
    let filter = config.promote_filter.clone();
    loop {
        let conn = match server.accept().await {
            Ok(c) => c,
            Err(err) => {
                // Transient kernel-level conditions (EMFILE, ENFILE,
                // ECONNABORTED) shouldn't permanently stop the listener.
                // Back off briefly and try again. If the listener itself
                // is fatally gone the next accept will return immediately
                // and we'll keep looping cheaply — the only realistic
                // unrecoverable case is task cancellation via Drop.
                tracing::warn!(
                    target: "iec60870::redundancy",
                    "accept error, retrying: {err}",
                );
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let peer = conn.peer();

        // Enforce the connection cap. Dropping `conn` here closes the
        // command channel; the driver task observes the closure and tears
        // the TCP session down without a STARTDT_con ever being sent.
        if lock_state(&state).peers.len() >= config.max_peers {
            tracing::warn!(
                target: "iec60870::redundancy",
                peer = %peer,
                cap = config.max_peers,
                "rejecting connection: max_peers cap reached",
            );
            drop(conn);
            continue;
        }

        let (sender, events) = conn.split();
        lock_state(&state).peers.insert(peer, sender);
        let st = Arc::clone(&state);
        let tx = asdu_tx.clone();
        let f = filter.clone();
        tokio::spawn(drive_peer(peer, events, st, tx, f));
    }
}

async fn drive_peer(
    peer: SocketAddr,
    mut events: ServerEvents,
    state: Arc<Mutex<Inner>>,
    asdu_tx: mpsc::Sender<(SocketAddr, Vec<u8>)>,
    promote_filter: Option<PromoteFilter>,
) {
    while let Some(evt) = events.recv().await {
        match evt {
            ServerEvent::StateChanged(State::Active) => {
                // Optional authorization gate. Reject under-lock to avoid
                // a window where a rejected peer is briefly "active".
                if let Some(filter) = promote_filter.as_ref() {
                    if !filter(peer) {
                        tracing::warn!(
                            target: "iec60870::redundancy",
                            peer = %peer,
                            "promotion rejected by filter",
                        );
                        lock_state(&state).peers.remove(&peer);
                        break;
                    }
                }
                // Promote this peer; demote the previous active one by
                // dropping its sender (which closes its TCP session).
                let mut guard = lock_state(&state);
                let prev = guard.active.replace(peer);
                if let Some(p) = prev {
                    if p != peer {
                        guard.peers.remove(&p);
                    }
                }
            }
            ServerEvent::Asdu(bytes) => {
                // Only forward inbound traffic from the active peer.
                // Anything else (a still-draining demoted peer's last
                // I-frame, for instance) is dropped silently.
                let is_active = lock_state(&state).active == Some(peer);
                if is_active && asdu_tx.send((peer, bytes)).await.is_err() {
                    break;
                }
            }
            ServerEvent::Closed(_) => break,
            ServerEvent::StateChanged(_) => {}
        }
    }
    // Peer disconnected. Remove its sender and clear the active slot if
    // we were holding it.
    let mut guard = lock_state(&state);
    guard.peers.remove(&peer);
    if guard.active == Some(peer) {
        guard.active = None;
    }
}
