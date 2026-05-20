//! IEC 60870-5-104 connection state machine (sans-I/O).
//!
//! The state machine consumes inputs ([`Input`] — incoming APDUs, user
//! requests, timer firings) and emits a queue of [`Action`]s for the I/O
//! layer to execute. It owns *all* per-connection mutable state including
//! sequence numbers, the send/receive windows, and the timer deadlines.
//!
//! Timing is modelled with [`std::time::Instant`] but every entry point
//! accepts the current instant explicitly (`now: Instant`) so unit tests can
//! drive the clock deterministically. The companion I/O crate feeds real
//! `Instant::now()`.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::frame104::apdu::{Apdu, UFunction};
use crate::frame104::seq::SeqNo;

/// Connection role. Determines who initiates the STARTDT handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Controlling station — the side that sends STARTDT_act.
    Client,
    /// Controlled station — replies to STARTDT_act with STARTDT_con.
    Server,
}

/// Connection state machine configuration. Defaults match the IEC
/// 60870-5-104 standard values (§9.6).
#[derive(Debug, Clone, Copy)]
pub struct Config {
    /// Maximum unacknowledged outgoing I-frames before send is blocked.
    pub k: u16,
    /// Maximum unacknowledged incoming I-frames before we *must* ACK.
    pub w: u16,
    /// `t1` — time to wait for an ACK of a sent I- or U-frame.
    pub t1: Duration,
    /// `t2` — time after which we proactively ACK pending I-frames.
    /// Must be strictly less than `t1`.
    pub t2: Duration,
    /// `t3` — idle timeout. If no APDU is sent or received for this long
    /// we send TESTFR_act to keep the connection alive.
    pub t3: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            k: 12,
            w: 8,
            t1: Duration::from_secs(15),
            t2: Duration::from_secs(10),
            t3: Duration::from_secs(20),
        }
    }
}

/// High-level connection state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Initial state — TCP just opened, STARTDT not yet exchanged.
    Stopped,
    /// STARTDT_act has been sent (client) and we're waiting for STARTDT_con.
    /// Only reachable on the client side.
    Starting,
    /// Data transfer is active in both directions.
    Active,
    /// STOPDT_act has been sent, waiting for STOPDT_con plus all outstanding
    /// I-frame ACKs.
    Stopping,
    /// Connection has been closed by the state machine; the I/O layer should
    /// tear down the underlying transport.
    Closed,
}

/// Inputs to the state machine.
#[derive(Debug, Clone)]
pub enum Input {
    /// An APDU was decoded from the peer.
    Apdu(Apdu),
    /// The user wants to enable data transfer (client side only).
    StartDt,
    /// The user wants to stop data transfer.
    StopDt,
    /// The user has an ASDU to send.
    SendAsdu(Vec<u8>),
    /// Time has advanced; check for timer expirations.
    Tick,
}

/// Actions emitted by the state machine for the I/O layer to perform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Write the given APDU to the transport.
    SendApdu(Apdu),
    /// Deliver an ASDU to the application.
    DeliverAsdu(Vec<u8>),
    /// State has changed; report to the user / hook.
    StateChanged(State),
    /// Connection should be torn down.
    Disconnect(DisconnectReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisconnectReason {
    /// `t1` expired without an ACK from the peer.
    AckTimeout,
    /// Protocol error (sequence-number mismatch, malformed frame, ...).
    ProtocolError,
    /// User requested STOPDT and outstanding frames are now drained.
    StoppedCleanly,
}

/// IEC 60870-5-104 connection state machine.
#[derive(Debug)]
pub struct Connection {
    role: Role,
    config: Config,
    state: State,

    // Sequence numbers
    send_next: SeqNo, // N(S) for the next I-frame we will send
    recv_next: SeqNo, // N(R) — we expect this from the peer next
    /// Send-side window: sent but unacknowledged frames (in send order).
    /// Each entry is the N(S) value used and the wall-clock time when the
    /// frame was sent (used to schedule the t1 deadline).
    sent: VecDeque<(SeqNo, Instant)>,
    /// Number of received I-frames not yet acknowledged via an S-frame
    /// (or piggy-backed I-frame N(R)).
    unacked_recv: u16,
    /// Timestamp of the most recent received I-frame (resets the t2 deadline).
    last_recv_iframe: Option<Instant>,
    /// Timestamp of the most recent successful send (any APDU; resets t3).
    last_send_any: Option<Instant>,
    /// Timestamp of the most recent received APDU (resets t3 idle-test).
    last_recv_any: Option<Instant>,
    /// If a TESTFR_act has been sent and is awaiting confirm, this is when.
    test_sent: Option<Instant>,

    /// Pending ASDUs queued because the send window is full.
    pending: VecDeque<Vec<u8>>,
    /// Actions to emit on the next drain.
    actions: VecDeque<Action>,
}

impl Connection {
    pub fn new(role: Role, config: Config) -> Self {
        Self {
            role,
            config,
            state: State::Stopped,
            send_next: SeqNo::new(0),
            recv_next: SeqNo::new(0),
            sent: VecDeque::new(),
            unacked_recv: 0,
            last_recv_iframe: None,
            last_send_any: None,
            last_recv_any: None,
            test_sent: None,
            pending: VecDeque::new(),
            actions: VecDeque::new(),
        }
    }

    pub fn role(&self) -> Role {
        self.role
    }
    pub fn state(&self) -> State {
        self.state
    }
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Drive the state machine with an input. Returns the sequence of
    /// actions that should be performed by the I/O layer.
    pub fn handle(&mut self, input: Input, now: Instant) -> Vec<Action> {
        match input {
            Input::Apdu(apdu) => self.on_apdu(apdu, now),
            Input::StartDt => self.on_user_startdt(now),
            Input::StopDt => self.on_user_stopdt(now),
            Input::SendAsdu(asdu) => self.on_user_send(asdu, now),
            Input::Tick => {} // timer logic runs unconditionally below
        }
        self.run_timers(now);
        std::mem::take(&mut self.actions).into_iter().collect()
    }

    // -----------------------------------------------------------------
    // APDU handling
    // -----------------------------------------------------------------

    fn on_apdu(&mut self, apdu: Apdu, now: Instant) {
        self.last_recv_any = Some(now);
        match apdu {
            Apdu::U { function } => self.on_u_frame(function, now),
            Apdu::S { recv } => {
                if matches!(self.state, State::Active | State::Stopping) {
                    self.acknowledge(recv);
                }
            }
            Apdu::I { send, recv, asdu } => self.on_i_frame(send, recv, asdu, now),
        }
    }

    fn on_u_frame(&mut self, function: UFunction, now: Instant) {
        match (function, self.role, self.state) {
            // Client receives STARTDT_con
            (UFunction::StartDtCon, Role::Client, State::Starting) => {
                self.transition(State::Active);
            }
            // Server receives STARTDT_act → reply with con and go active
            (UFunction::StartDtAct, Role::Server, State::Stopped) => {
                self.emit_apdu(Apdu::U {
                    function: UFunction::StartDtCon,
                });
                self.last_send_any = Some(now);
                self.transition(State::Active);
            }
            // Symmetric STOPDT handshake
            (UFunction::StopDtAct, Role::Server, _) => {
                self.emit_apdu(Apdu::U {
                    function: UFunction::StopDtCon,
                });
                self.last_send_any = Some(now);
                self.transition(State::Stopped);
            }
            (UFunction::StopDtCon, Role::Client, State::Stopping) => {
                if self.sent.is_empty() {
                    self.transition(State::Stopped);
                    self.actions
                        .push_back(Action::Disconnect(DisconnectReason::StoppedCleanly));
                }
            }
            // TESTFR keepalive
            (UFunction::TestFrAct, _, _) => {
                self.emit_apdu(Apdu::U {
                    function: UFunction::TestFrCon,
                });
                self.last_send_any = Some(now);
            }
            (UFunction::TestFrCon, _, _) => {
                self.test_sent = None;
            }
            // Anything else is a protocol error
            _ => {
                self.actions
                    .push_back(Action::Disconnect(DisconnectReason::ProtocolError));
                self.transition(State::Closed);
            }
        }
    }

    fn on_i_frame(&mut self, send: SeqNo, recv: SeqNo, asdu: Vec<u8>, now: Instant) {
        if !matches!(self.state, State::Active | State::Stopping) {
            // I-frames outside of the data-transfer phase are illegal.
            self.actions
                .push_back(Action::Disconnect(DisconnectReason::ProtocolError));
            self.transition(State::Closed);
            return;
        }
        if send != self.recv_next {
            // Out-of-sequence I-frame. The standard requires immediate
            // disconnect; the peer has lost track.
            self.actions
                .push_back(Action::Disconnect(DisconnectReason::ProtocolError));
            self.transition(State::Closed);
            return;
        }
        self.recv_next = self.recv_next.next();
        self.unacked_recv = self.unacked_recv.saturating_add(1);
        self.last_recv_iframe = Some(now);
        self.acknowledge(recv);
        self.actions.push_back(Action::DeliverAsdu(asdu));
        if self.unacked_recv >= self.config.w {
            self.emit_supervisory(now);
        }
    }

    /// Process an incoming N(R) — peer has acknowledged everything sent with
    /// `N(S) < n_r`. Drop those entries from the `sent` queue.
    fn acknowledge(&mut self, nr: SeqNo) {
        while let Some(&(seq, _)) = self.sent.front() {
            // seq < nr in the cyclic ordering iff distance(seq, nr) is small
            // and non-zero. Use distance(seq, nr): if seq == nr the front is
            // *not* yet acked (peer's N(R) is the next-expected, not last-
            // received).
            let d = seq.distance(nr);
            // d == 0 means nr == seq (peer has not yet acked this one).
            // 0 < d <= window means the peer has acknowledged seq.
            if d == 0 || d > SeqNo::MAX / 2 {
                break;
            }
            self.sent.pop_front();
        }
        // Sent queue may now be drainable; try to flush pending.
        self.flush_pending(self.last_recv_any);
    }

    fn flush_pending(&mut self, fallback_now: Option<Instant>) {
        while !self.pending.is_empty() && self.sent.len() < self.config.k as usize {
            let asdu = self.pending.pop_front().unwrap();
            let now = fallback_now.unwrap_or_else(Instant::now);
            self.send_iframe(asdu, now);
        }
    }

    // -----------------------------------------------------------------
    // User-initiated transitions
    // -----------------------------------------------------------------

    fn on_user_startdt(&mut self, now: Instant) {
        if self.role == Role::Client && self.state == State::Stopped {
            self.emit_apdu(Apdu::U {
                function: UFunction::StartDtAct,
            });
            self.last_send_any = Some(now);
            self.transition(State::Starting);
        }
    }

    fn on_user_stopdt(&mut self, now: Instant) {
        if self.state == State::Active {
            if self.role == Role::Client {
                self.emit_apdu(Apdu::U {
                    function: UFunction::StopDtAct,
                });
                self.last_send_any = Some(now);
                self.transition(State::Stopping);
            } else {
                // Server can't initiate STOPDT in the standard; just go
                // stopped locally and let the client notice on the next
                // attempt to use the link.
                self.transition(State::Stopped);
            }
        }
    }

    fn on_user_send(&mut self, asdu: Vec<u8>, now: Instant) {
        if self.state != State::Active {
            // Queue silently; will be flushed when STARTDT_con arrives.
            self.pending.push_back(asdu);
            return;
        }
        if self.sent.len() >= self.config.k as usize {
            self.pending.push_back(asdu);
            return;
        }
        self.send_iframe(asdu, now);
    }

    fn send_iframe(&mut self, asdu: Vec<u8>, now: Instant) {
        let send = self.send_next;
        self.send_next = self.send_next.next();
        self.sent.push_back((send, now));
        self.unacked_recv = 0;
        self.last_send_any = Some(now);
        self.actions.push_back(Action::SendApdu(Apdu::I {
            send,
            recv: self.recv_next,
            asdu,
        }));
    }

    fn emit_supervisory(&mut self, now: Instant) {
        self.actions.push_back(Action::SendApdu(Apdu::S {
            recv: self.recv_next,
        }));
        self.unacked_recv = 0;
        self.last_send_any = Some(now);
    }

    fn emit_apdu(&mut self, apdu: Apdu) {
        self.actions.push_back(Action::SendApdu(apdu));
    }

    fn transition(&mut self, to: State) {
        if self.state != to {
            self.state = to;
            self.actions.push_back(Action::StateChanged(to));
        }
    }

    // -----------------------------------------------------------------
    // Timer logic — invoked at the tail of every handle()
    // -----------------------------------------------------------------

    fn run_timers(&mut self, now: Instant) {
        // t1: oldest sent I-frame must be acked within config.t1
        if let Some(&(_, sent_at)) = self.sent.front() {
            if now.saturating_duration_since(sent_at) >= self.config.t1 {
                self.actions
                    .push_back(Action::Disconnect(DisconnectReason::AckTimeout));
                self.transition(State::Closed);
                return;
            }
        }
        // t1 on outstanding TESTFR_act
        if let Some(ts) = self.test_sent {
            if now.saturating_duration_since(ts) >= self.config.t1 {
                self.actions
                    .push_back(Action::Disconnect(DisconnectReason::AckTimeout));
                self.transition(State::Closed);
                return;
            }
        }
        // t2: acknowledge pending received I-frames
        if self.unacked_recv > 0 {
            if let Some(last) = self.last_recv_iframe {
                if now.saturating_duration_since(last) >= self.config.t2 {
                    self.emit_supervisory(now);
                }
            }
        }
        // t3: idle test
        if matches!(self.state, State::Active | State::Stopping) && self.test_sent.is_none() {
            let last_io = [self.last_send_any, self.last_recv_any]
                .into_iter()
                .flatten()
                .max();
            if let Some(t) = last_io {
                if now.saturating_duration_since(t) >= self.config.t3 {
                    self.emit_apdu(Apdu::U {
                        function: UFunction::TestFrAct,
                    });
                    self.last_send_any = Some(now);
                    self.test_sent = Some(now);
                }
            }
        }
        // If we entered Stopping and there are no outstanding frames, close.
        if self.state == State::Stopping && self.sent.is_empty() && self.pending.is_empty() {
            self.transition(State::Stopped);
            self.actions
                .push_back(Action::Disconnect(DisconnectReason::StoppedCleanly));
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn at(now: Instant, sec: u64) -> Instant {
        now + Duration::from_secs(sec)
    }

    #[test]
    fn client_startdt_handshake() {
        let t0 = Instant::now();
        let mut c = Connection::new(Role::Client, Config::default());
        let actions = c.handle(Input::StartDt, t0);
        assert_eq!(
            actions,
            vec![
                Action::SendApdu(Apdu::U {
                    function: UFunction::StartDtAct
                }),
                Action::StateChanged(State::Starting),
            ]
        );
        let actions = c.handle(
            Input::Apdu(Apdu::U {
                function: UFunction::StartDtCon,
            }),
            at(t0, 1),
        );
        assert_eq!(actions, vec![Action::StateChanged(State::Active)]);
        assert_eq!(c.state(), State::Active);
    }

    #[test]
    fn server_replies_to_startdt() {
        let t0 = Instant::now();
        let mut s = Connection::new(Role::Server, Config::default());
        let actions = s.handle(
            Input::Apdu(Apdu::U {
                function: UFunction::StartDtAct,
            }),
            t0,
        );
        assert_eq!(
            actions,
            vec![
                Action::SendApdu(Apdu::U {
                    function: UFunction::StartDtCon
                }),
                Action::StateChanged(State::Active),
            ]
        );
    }

    #[test]
    fn testfr_act_echoed_as_con() {
        let t0 = Instant::now();
        let mut c = Connection::new(Role::Server, Config::default());
        // Get to active first
        c.handle(
            Input::Apdu(Apdu::U {
                function: UFunction::StartDtAct,
            }),
            t0,
        );
        let actions = c.handle(
            Input::Apdu(Apdu::U {
                function: UFunction::TestFrAct,
            }),
            at(t0, 1),
        );
        assert_eq!(
            actions,
            vec![Action::SendApdu(Apdu::U {
                function: UFunction::TestFrCon
            })]
        );
    }

    #[test]
    fn t3_idle_sends_testfr_act() {
        let config = Config {
            t3: Duration::from_secs(5),
            ..Config::default()
        };
        let t0 = Instant::now();
        let mut c = Connection::new(Role::Client, config);
        c.handle(Input::StartDt, t0);
        c.handle(
            Input::Apdu(Apdu::U {
                function: UFunction::StartDtCon,
            }),
            at(t0, 1),
        );
        // 7 seconds later (t3=5) with no traffic
        let actions = c.handle(Input::Tick, at(t0, 8));
        assert!(actions.iter().any(|a| matches!(
            a,
            Action::SendApdu(Apdu::U {
                function: UFunction::TestFrAct
            })
        )));
    }

    #[test]
    fn iframe_is_acknowledged_after_w_received() {
        let t0 = Instant::now();
        let config = Config {
            w: 2,
            ..Config::default()
        };
        let mut c = Connection::new(Role::Server, config);
        c.handle(
            Input::Apdu(Apdu::U {
                function: UFunction::StartDtAct,
            }),
            t0,
        );

        let asdu = vec![0xAA];
        // First I-frame: delivered, no ACK yet (1 < w)
        let actions = c.handle(
            Input::Apdu(Apdu::I {
                send: SeqNo::new(0),
                recv: SeqNo::new(0),
                asdu: asdu.clone(),
            }),
            at(t0, 1),
        );
        assert_eq!(actions, vec![Action::DeliverAsdu(asdu.clone())]);

        // Second I-frame: triggers S-frame ACK because unacked_recv == w
        let actions = c.handle(
            Input::Apdu(Apdu::I {
                send: SeqNo::new(1),
                recv: SeqNo::new(0),
                asdu: asdu.clone(),
            }),
            at(t0, 2),
        );
        assert_eq!(
            actions,
            vec![
                Action::DeliverAsdu(asdu),
                Action::SendApdu(Apdu::S {
                    recv: SeqNo::new(2)
                })
            ]
        );
    }

    #[test]
    fn out_of_order_iframe_disconnects() {
        let t0 = Instant::now();
        let mut c = Connection::new(Role::Server, Config::default());
        c.handle(
            Input::Apdu(Apdu::U {
                function: UFunction::StartDtAct,
            }),
            t0,
        );
        // Peer skips N(S)=0 and sends N(S)=1 directly
        let actions = c.handle(
            Input::Apdu(Apdu::I {
                send: SeqNo::new(1),
                recv: SeqNo::new(0),
                asdu: vec![],
            }),
            at(t0, 1),
        );
        assert!(actions
            .iter()
            .any(|a| matches!(a, Action::Disconnect(DisconnectReason::ProtocolError))));
        assert_eq!(c.state(), State::Closed);
    }

    #[test]
    fn t1_expiry_on_unacked_iframe() {
        let config = Config {
            t1: Duration::from_secs(2),
            ..Config::default()
        };
        let t0 = Instant::now();
        let mut c = Connection::new(Role::Client, config);
        c.handle(Input::StartDt, t0);
        c.handle(
            Input::Apdu(Apdu::U {
                function: UFunction::StartDtCon,
            }),
            at(t0, 1),
        );
        c.handle(Input::SendAsdu(vec![1, 2, 3]), at(t0, 2));
        // No ACK by t0 + 5 (= sent_at(2) + t1(2) + 1)
        let actions = c.handle(Input::Tick, at(t0, 5));
        assert!(actions
            .iter()
            .any(|a| matches!(a, Action::Disconnect(DisconnectReason::AckTimeout))));
    }

    #[test]
    fn k_window_backpressure_queues_sends() {
        let config = Config {
            k: 2,
            ..Config::default()
        };
        let t0 = Instant::now();
        let mut c = Connection::new(Role::Client, config);
        c.handle(Input::StartDt, t0);
        c.handle(
            Input::Apdu(Apdu::U {
                function: UFunction::StartDtCon,
            }),
            at(t0, 1),
        );
        // Submit three ASDUs; only two should be sent before k saturates.
        c.handle(Input::SendAsdu(vec![1]), at(t0, 2));
        c.handle(Input::SendAsdu(vec![2]), at(t0, 2));
        let actions = c.handle(Input::SendAsdu(vec![3]), at(t0, 2));
        assert!(!actions
            .iter()
            .any(|a| matches!(a, Action::SendApdu(Apdu::I { .. }))));

        // Peer ACKs the first one (N(R) = 1).
        let actions = c.handle(
            Input::Apdu(Apdu::S {
                recv: SeqNo::new(1),
            }),
            at(t0, 3),
        );
        // The queued ASDU should now flush.
        assert_eq!(
            actions,
            vec![Action::SendApdu(Apdu::I {
                send: SeqNo::new(2),
                recv: SeqNo::new(0),
                asdu: vec![3],
            })]
        );
    }
}
