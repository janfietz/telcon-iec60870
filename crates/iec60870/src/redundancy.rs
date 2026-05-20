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
//! [`recv_asdu`]: RedundancyServer::recv_asdu
//! [`send_active`]: RedundancyServer::send_active

use std::collections::HashMap;
use std::fmt;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use iec60870_proto::frame104::State;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::error::{Error, Result};
use crate::server104::{Server104, ServerEvent, ServerEvents, ServerSender};

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
        let guard = self.state.lock().expect("poisoned");
        f.debug_struct("RedundancyServer")
            .field("peers", &guard.peers.keys().collect::<Vec<_>>())
            .field("active", &guard.active)
            .finish()
    }
}

impl RedundancyServer {
    /// Start the redundancy manager around a bound [`Server104`]. Spawns
    /// the accept loop on the current Tokio runtime; the returned handle
    /// is the only way to observe events from the group.
    pub fn spawn(server: Server104) -> Self {
        let state = Arc::new(Mutex::new(Inner::default()));
        let (asdu_tx, asdu_rx) = mpsc::channel(128);
        let accept_task = tokio::spawn(accept_loop(server, Arc::clone(&state), asdu_tx));
        Self {
            state,
            asdu_rx,
            accept_task,
        }
    }

    /// Receive the next ASDU from whichever peer is currently active.
    /// Returns `(peer, bytes)` so the caller knows the source. Returns
    /// `None` only when the manager itself has shut down.
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
            let guard = self.state.lock().expect("poisoned");
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
        self.state.lock().expect("poisoned").active
    }

    /// Snapshot of every connected peer's address, in unspecified order.
    pub async fn peers(&self) -> Vec<SocketAddr> {
        self.state
            .lock()
            .expect("poisoned")
            .peers
            .keys()
            .copied()
            .collect()
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
        if let Ok(mut guard) = self.state.lock() {
            guard.peers.clear();
            guard.active = None;
        }
    }
}

async fn accept_loop(
    server: Server104,
    state: Arc<Mutex<Inner>>,
    asdu_tx: mpsc::Sender<(SocketAddr, Vec<u8>)>,
) {
    loop {
        let conn = match server.accept().await {
            Ok(c) => c,
            Err(_) => break,
        };
        let peer = conn.peer();
        let (sender, events) = conn.split();
        state.lock().expect("poisoned").peers.insert(peer, sender);
        let st = Arc::clone(&state);
        let tx = asdu_tx.clone();
        tokio::spawn(drive_peer(peer, events, st, tx));
    }
}

async fn drive_peer(
    peer: SocketAddr,
    mut events: ServerEvents,
    state: Arc<Mutex<Inner>>,
    asdu_tx: mpsc::Sender<(SocketAddr, Vec<u8>)>,
) {
    while let Some(evt) = events.recv().await {
        match evt {
            ServerEvent::StateChanged(State::Active) => {
                // Promote this peer; demote the previous active one by
                // dropping its sender (which closes its TCP session).
                let mut guard = state.lock().expect("poisoned");
                let prev = guard.active.replace(peer);
                if let Some(p) = prev {
                    if p != peer {
                        guard.peers.remove(&p);
                    }
                }
            }
            ServerEvent::Asdu(bytes) => {
                // Only forward inbound traffic from the active peer.
                // Anything else is dropped silently — by spec, an inactive
                // peer should not be sending data.
                let is_active = state.lock().expect("poisoned").active == Some(peer);
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
    let mut guard = state.lock().expect("poisoned");
    guard.peers.remove(&peer);
    if guard.active == Some(peer) {
        guard.active = None;
    }
}
