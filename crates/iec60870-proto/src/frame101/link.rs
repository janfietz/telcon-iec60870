//! IEC 60870-5-101 link-layer state machine (sans-I/O).
//!
//! This module provides a sans-I/O state machine for the FT 1.2 link layer.
//! The machine consumes [`Input`]s and emits [`Action`]s, leaving all I/O and
//! clock calls to the surrounding async or synchronous driver.
//!
//! ## Modes
//!
//! * **Unbalanced** (implemented) — master polls outstation; outstation only
//!   sends when polled. This is the mode used in most IEC 60870-5-101
//!   deployments over serial links.
//! * **Balanced** — peer-to-peer; both sides can initiate. Stub is present
//!   (`Mode::Balanced`) but `unimplemented!()` — to be added in a later pass.
//!
//! ## FCB (Frame Count Bit)
//!
//! The master alternates FCB on each SEND/CONFIRM cycle so the outstation can
//! detect retransmissions. FCB is set in the control field via
//! [`ControlField::Primary::fcb`](crate::frame101::control::ControlField).
//!
//! ## Timers
//!
//! The machine is entirely driven from the outside. The caller passes the
//! current [`std::time::Instant`] to every [`Connection::handle`] call; the
//! machine never reads the clock itself.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::frame101::control::{ControlField, Direction, FuncCodePrimary, FuncCodeSecondary};
use crate::frame101::frame::{Frame101, LinkAddress, LinkAddressSize, SingleChar};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Link-layer state machine configuration.
#[derive(Debug, Clone, Copy)]
pub struct Config {
    /// Link address of the local station (broadcast address excluded).
    pub link_address: LinkAddress,
    /// Width of the link address on the wire.
    pub addr_size: LinkAddressSize,
    /// Timeout waiting for an ACK after a confirmed send (default 1 s).
    pub timeout: Duration,
    /// Maximum retransmission attempts before the link is declared down
    /// (default 3).
    pub max_retries: u8,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            link_address: LinkAddress(0),
            addr_size: LinkAddressSize::One,
            timeout: Duration::from_secs(1),
            max_retries: 3,
        }
    }
}

// ---------------------------------------------------------------------------
// Role and mode
// ---------------------------------------------------------------------------

/// Connection role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Primary (master) station — initiates requests and confirmed sends.
    Master,
    /// Secondary (outstation) station — responds to requests from the master.
    Outstation,
}

/// Transfer mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Unbalanced — master controls; outstation only responds.
    Unbalanced,
    /// Balanced — peer-to-peer; both sides may initiate.
    ///
    /// # Note
    ///
    /// Not yet implemented. Constructing a `Connection` with this mode will
    /// panic.
    Balanced,
}

// ---------------------------------------------------------------------------
// Link state
// ---------------------------------------------------------------------------

/// High-level link state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkState {
    /// Link is not yet initialised (reset not completed).
    NotReady,
    /// Link is ready for data transfer.
    Ready,
    /// Link has been declared down (too many failed retries).
    Failed,
}

// ---------------------------------------------------------------------------
// Inputs and Actions
// ---------------------------------------------------------------------------

/// Inputs to the link-layer state machine.
#[derive(Debug, Clone)]
pub enum Input {
    /// A frame was received from the peer.
    FrameReceived(Frame101),
    /// A timer tick — check for timeouts.
    Tick,
    /// (Master only) Send an ASDU as a confirmed user-data frame.
    SendUserData(Vec<u8>),
    /// (Master only) Request status of link.
    RequestStatus,
    /// (Master only) Reset the remote link.
    ResetRemoteLink,
    /// (Master only) Request class-1 user data from the outstation.
    RequestUserDataClass1,
    /// (Master only) Request class-2 user data from the outstation.
    RequestUserDataClass2,
}

/// Reason for a link error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// Acknowledged timeout exhausted all retries.
    AckTimeout,
    /// Peer returned a NACK.
    Nack,
    /// Protocol violation (unexpected frame type or content).
    ProtocolError,
}

/// Actions emitted by the link-layer state machine for the I/O layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Transmit the given frame to the peer.
    SendFrame(Frame101),
    /// Deliver the ASDU payload to the application layer.
    DeliverAsdu(Vec<u8>),
    /// The link state has changed.
    LinkStateChanged(LinkState),
    /// A link-layer error occurred.
    LinkError(Reason),
}

// ---------------------------------------------------------------------------
// Pending confirmed send
// ---------------------------------------------------------------------------

/// Tracks an in-flight SEND/CONFIRM exchange.
#[derive(Debug, Clone)]
struct PendingSend {
    /// The full frame that was sent (kept for retransmission).
    frame: Frame101,
    /// Wall-clock time when the frame was last transmitted.
    sent_at: Instant,
    /// Number of transmissions so far (1 on first send).
    attempts: u8,
}

// ---------------------------------------------------------------------------
// Connection
// ---------------------------------------------------------------------------

/// IEC 60870-5-101 link-layer state machine.
///
/// # Example (master reset handshake)
///
/// ```
/// use std::time::{Duration, Instant};
/// use iec60870_proto::frame101::link::{
///     Action, Config, Connection, Input, LinkState, Mode, Role,
/// };
/// use iec60870_proto::frame101::frame::{Frame101, LinkAddress, LinkAddressSize, SingleChar};
///
/// let mut master = Connection::new(
///     Role::Master,
///     Mode::Unbalanced,
///     Config {
///         link_address: LinkAddress(1),
///         addr_size: LinkAddressSize::One,
///         ..Config::default()
///     },
/// );
/// let t0 = Instant::now();
/// let actions = master.handle(Input::ResetRemoteLink, t0);
/// assert!(actions.iter().any(|a| matches!(a, Action::SendFrame(Frame101::Fixed { .. }))));
/// ```
#[derive(Debug)]
pub struct Connection {
    role: Role,
    #[allow(dead_code)]
    mode: Mode,
    config: Config,
    link_state: LinkState,
    /// Current FCB value (master tracks the next FCB to send).
    fcb: bool,
    /// Whether the outstation's last FCB value matched what we expected
    /// (outstation side only; used to detect duplicate frames).
    last_fcb_seen: Option<bool>,
    /// In-flight confirmed-send, if any.
    pending: Option<PendingSend>,
    /// Queue of user-data ASDUs waiting to be sent.
    send_queue: VecDeque<Vec<u8>>,
    /// Buffered actions to drain on the next `handle` return.
    actions: VecDeque<Action>,
}

impl Connection {
    /// Create a new link-layer connection.
    ///
    /// # Panics
    ///
    /// Panics if `mode == Mode::Balanced` (not yet implemented).
    pub fn new(role: Role, mode: Mode, config: Config) -> Self {
        assert!(
            mode != Mode::Balanced,
            "balanced mode is not yet implemented (TODO)"
        );
        Self {
            role,
            mode,
            config,
            link_state: LinkState::NotReady,
            fcb: false,
            last_fcb_seen: None,
            pending: None,
            send_queue: VecDeque::new(),
            actions: VecDeque::new(),
        }
    }

    /// Current link state.
    pub fn link_state(&self) -> LinkState {
        self.link_state
    }

    /// Drive the state machine with a single input.
    ///
    /// Returns the sequence of actions the I/O layer must perform. The `now`
    /// parameter is used for timeout tracking; callers should pass
    /// `Instant::now()` in production code and a synthetic value in tests.
    pub fn handle(&mut self, input: Input, now: Instant) -> Vec<Action> {
        match input {
            Input::FrameReceived(frame) => self.on_frame(frame, now),
            Input::Tick => {}
            Input::SendUserData(asdu) => self.on_send_user_data(asdu, now),
            Input::RequestStatus => self.on_request_status(now),
            Input::ResetRemoteLink => self.on_reset_remote_link(now),
            Input::RequestUserDataClass1 => {
                self.on_request_class(FuncCodePrimary::RequestUserDataClass1, now)
            }
            Input::RequestUserDataClass2 => {
                self.on_request_class(FuncCodePrimary::RequestUserDataClass2, now)
            }
        }
        self.run_timers(now);
        std::mem::take(&mut self.actions).into_iter().collect()
    }

    // -----------------------------------------------------------------------
    // Input handlers
    // -----------------------------------------------------------------------

    fn on_frame(&mut self, frame: Frame101, now: Instant) {
        match self.role {
            Role::Master => self.master_on_frame(frame, now),
            Role::Outstation => self.outstation_on_frame(frame, now),
        }
    }

    // ---- Master receive path -----------------------------------------------

    fn master_on_frame(&mut self, frame: Frame101, _now: Instant) {
        match &frame {
            Frame101::Single(sc) => {
                match sc {
                    SingleChar::Ack => {
                        // Clear the pending confirmed send.
                        if self.pending.take().is_some() {
                            // If there's more to send, flush.
                            self.maybe_flush_queue(_now);
                        }
                    }
                    SingleChar::Nack => {
                        self.pending = None;
                        self.emit_link_error(Reason::Nack);
                    }
                }
            }
            Frame101::Fixed {
                control,
                address: _,
            } => {
                // Decode the control field as a secondary frame.
                if let Some(cf) = ControlField::decode(*control, Direction::Secondary) {
                    self.master_on_secondary_fixed(cf, _now);
                } else {
                    self.emit_link_error(Reason::ProtocolError);
                }
            }
            Frame101::Variable {
                control,
                address: _,
                asdu,
            } => {
                if let Some(ControlField::Secondary {
                    func: FuncCodeSecondary::RespondUserData,
                    ..
                }) = ControlField::decode(*control, Direction::Secondary)
                {
                    // Clear any pending request.
                    self.pending = None;
                    self.actions.push_back(Action::DeliverAsdu(asdu.clone()));
                } else {
                    self.emit_link_error(Reason::ProtocolError);
                }
            }
        }
    }

    fn master_on_secondary_fixed(&mut self, cf: ControlField, now: Instant) {
        match cf {
            ControlField::Secondary {
                func: FuncCodeSecondary::Ack,
                ..
            } => {
                if self.pending.take().is_some() {
                    if self.link_state == LinkState::NotReady {
                        self.transition_link(LinkState::Ready);
                    }
                    self.maybe_flush_queue(now);
                }
            }
            ControlField::Secondary {
                func: FuncCodeSecondary::Nack,
                ..
            } => {
                self.pending = None;
                self.emit_link_error(Reason::Nack);
            }
            ControlField::Secondary {
                func: FuncCodeSecondary::StatusOfLink,
                ..
            } => {
                // Treat status response same as ACK for our purposes.
                self.pending = None;
                if self.link_state == LinkState::NotReady {
                    self.transition_link(LinkState::Ready);
                }
            }
            ControlField::Secondary {
                func: FuncCodeSecondary::NackNoData,
                ..
            } => {
                // Outstation has no data for the request — treat as end of exchange.
                self.pending = None;
            }
            _ => {
                self.emit_link_error(Reason::ProtocolError);
            }
        }
    }

    // ---- Outstation receive path ------------------------------------------

    fn outstation_on_frame(&mut self, frame: Frame101, now: Instant) {
        match frame {
            Frame101::Fixed { control, address } => {
                if address != self.config.link_address {
                    // Not addressed to us; ignore.
                    return;
                }
                if let Some(cf) = ControlField::decode(control, Direction::Primary) {
                    self.outstation_on_primary_fixed(cf, now);
                }
                // Unknown function code — ignore silently per the standard.
            }
            Frame101::Variable {
                control,
                address,
                asdu,
            } => {
                if address != self.config.link_address {
                    return;
                }
                if let Some(cf) = ControlField::decode(control, Direction::Primary) {
                    self.outstation_on_primary_variable(cf, asdu, now);
                }
            }
            // Single-char frames (ACK/NACK) are only sent by secondary; ignore.
            Frame101::Single(_) => {}
        }
    }

    fn outstation_on_primary_fixed(&mut self, cf: ControlField, now: Instant) {
        match cf {
            ControlField::Primary {
                func: FuncCodePrimary::ResetRemoteLink,
                ..
            } => {
                self.last_fcb_seen = None;
                self.send_ack(now);
                self.transition_link(LinkState::Ready);
            }
            ControlField::Primary {
                func: FuncCodePrimary::RequestStatus,
                ..
            } => {
                self.send_status(now);
            }
            ControlField::Primary {
                func: FuncCodePrimary::RequestUserDataClass1,
                ..
            }
            | ControlField::Primary {
                func: FuncCodePrimary::RequestUserDataClass2,
                ..
            } => {
                // Serve the next queued ASDU if available; otherwise NACK.
                if let Some(asdu) = self.send_queue.pop_front() {
                    self.send_respond_user_data(asdu, now);
                } else {
                    self.send_nack_no_data(now);
                }
            }
            _ => {
                // Ignore unrecognised primary fixed frames.
            }
        }
    }

    fn outstation_on_primary_variable(&mut self, cf: ControlField, asdu: Vec<u8>, now: Instant) {
        match cf {
            ControlField::Primary {
                fcb,
                fcv,
                func: FuncCodePrimary::UserDataConfirmed,
            } => {
                // FCB duplicate detection: if FCV=1 and FCB matches last seen,
                // this is a retransmission — still ACK but don't deliver again.
                let duplicate = if fcv {
                    self.last_fcb_seen == Some(fcb)
                } else {
                    false
                };
                if fcv {
                    self.last_fcb_seen = Some(fcb);
                }
                self.send_ack(now);
                if !duplicate {
                    self.actions.push_back(Action::DeliverAsdu(asdu));
                }
            }
            ControlField::Primary {
                func: FuncCodePrimary::UserDataUnconfirmed,
                ..
            } => {
                // No ACK required.
                self.actions.push_back(Action::DeliverAsdu(asdu));
            }
            _ => {
                // Ignore.
            }
        }
    }

    // ---- Master send path --------------------------------------------------

    fn on_send_user_data(&mut self, asdu: Vec<u8>, now: Instant) {
        match self.role {
            Role::Master => {
                self.send_queue.push_back(asdu);
                self.maybe_flush_queue(now);
            }
            Role::Outstation => {
                // Outstation queues data for the master's next poll.
                // Transmitted in response to RequestUserDataClass1/2.
                self.send_queue.push_back(asdu);
            }
        }
    }

    fn on_request_status(&mut self, now: Instant) {
        if self.role != Role::Master {
            return;
        }
        if self.pending.is_some() {
            return; // already waiting
        }
        let cf = ControlField::Primary {
            fcb: false,
            fcv: false,
            func: FuncCodePrimary::RequestStatus,
        };
        let frame = self.build_fixed(cf);
        self.send_with_confirm(frame, now);
    }

    fn on_reset_remote_link(&mut self, now: Instant) {
        if self.role != Role::Master {
            return;
        }
        // Cancel any in-flight send.
        self.pending = None;
        let cf = ControlField::Primary {
            fcb: false,
            fcv: false,
            func: FuncCodePrimary::ResetRemoteLink,
        };
        let frame = self.build_fixed(cf);
        self.send_with_confirm(frame, now);
    }

    fn on_request_class(&mut self, func: FuncCodePrimary, now: Instant) {
        if self.role != Role::Master {
            return;
        }
        if self.pending.is_some() {
            return;
        }
        let cf = ControlField::Primary {
            fcb: false,
            fcv: false,
            func,
        };
        let frame = self.build_fixed(cf);
        self.send_with_confirm(frame, now);
    }

    // ---- Timer logic -------------------------------------------------------

    fn run_timers(&mut self, now: Instant) {
        if let Some(ref pending) = self.pending {
            let elapsed = now.saturating_duration_since(pending.sent_at);
            if elapsed >= self.config.timeout {
                let attempts = pending.attempts;
                if attempts > self.config.max_retries {
                    self.pending = None;
                    if self.link_state != LinkState::Failed {
                        self.link_state = LinkState::Failed;
                        self.actions
                            .push_back(Action::LinkStateChanged(LinkState::Failed));
                    }
                    self.actions
                        .push_back(Action::LinkError(Reason::AckTimeout));
                } else {
                    // Retransmit.
                    let frame = self.pending.as_ref().unwrap().frame.clone();
                    if let Some(ref mut p) = self.pending {
                        p.attempts += 1;
                        p.sent_at = now;
                    }
                    self.actions.push_back(Action::SendFrame(frame));
                }
            }
        }
    }

    // ---- Helpers -----------------------------------------------------------

    fn maybe_flush_queue(&mut self, now: Instant) {
        if self.pending.is_some() {
            return; // wait for ACK first
        }
        if let Some(asdu) = self.send_queue.pop_front() {
            let cf = ControlField::Primary {
                fcb: self.fcb,
                fcv: true,
                func: FuncCodePrimary::UserDataConfirmed,
            };
            let frame = Frame101::Variable {
                control: cf.encode(),
                address: self.config.link_address,
                asdu,
            };
            self.send_with_confirm(frame, now);
            // Toggle FCB for the next send.
            self.fcb = !self.fcb;
        }
    }

    fn send_with_confirm(&mut self, frame: Frame101, now: Instant) {
        self.actions.push_back(Action::SendFrame(frame.clone()));
        self.pending = Some(PendingSend {
            frame,
            sent_at: now,
            attempts: 1,
        });
    }

    fn build_fixed(&self, cf: ControlField) -> Frame101 {
        Frame101::Fixed {
            control: cf.encode(),
            address: self.config.link_address,
        }
    }

    fn send_ack(&mut self, _now: Instant) {
        let cf = ControlField::Secondary {
            acd: false,
            dfc: false,
            func: FuncCodeSecondary::Ack,
        };
        let frame = Frame101::Fixed {
            control: cf.encode(),
            address: self.config.link_address,
        };
        self.actions.push_back(Action::SendFrame(frame));
    }

    fn send_status(&mut self, _now: Instant) {
        let cf = ControlField::Secondary {
            acd: false,
            dfc: false,
            func: FuncCodeSecondary::StatusOfLink,
        };
        let frame = Frame101::Fixed {
            control: cf.encode(),
            address: self.config.link_address,
        };
        self.actions.push_back(Action::SendFrame(frame));
    }

    fn send_respond_user_data(&mut self, asdu: Vec<u8>, _now: Instant) {
        let cf = ControlField::Secondary {
            acd: false,
            dfc: false,
            func: FuncCodeSecondary::RespondUserData,
        };
        let frame = Frame101::Variable {
            control: cf.encode(),
            address: self.config.link_address,
            asdu,
        };
        self.actions.push_back(Action::SendFrame(frame));
    }

    fn send_nack_no_data(&mut self, _now: Instant) {
        let cf = ControlField::Secondary {
            acd: false,
            dfc: false,
            func: FuncCodeSecondary::NackNoData,
        };
        let frame = Frame101::Fixed {
            control: cf.encode(),
            address: self.config.link_address,
        };
        self.actions.push_back(Action::SendFrame(frame));
    }

    fn transition_link(&mut self, new_state: LinkState) {
        if self.link_state != new_state {
            self.link_state = new_state;
            self.actions.push_back(Action::LinkStateChanged(new_state));
        }
    }

    fn emit_link_error(&mut self, reason: Reason) {
        self.actions.push_back(Action::LinkError(reason));
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn at(base: Instant, secs: u64) -> Instant {
        base + Duration::from_secs(secs)
    }

    fn master(addr: u16) -> Connection {
        Connection::new(
            Role::Master,
            Mode::Unbalanced,
            Config {
                link_address: LinkAddress(addr),
                addr_size: LinkAddressSize::One,
                timeout: Duration::from_secs(1),
                max_retries: 3,
            },
        )
    }

    fn outstation(addr: u16) -> Connection {
        Connection::new(
            Role::Outstation,
            Mode::Unbalanced,
            Config {
                link_address: LinkAddress(addr),
                addr_size: LinkAddressSize::One,
                timeout: Duration::from_secs(1),
                max_retries: 3,
            },
        )
    }

    // ---- Scenario: master reset → ACK → ready ----------------------------

    #[test]
    fn master_reset_ack_ready() {
        let t0 = Instant::now();
        let mut m = master(1);

        // Master sends reset.
        let actions = m.handle(Input::ResetRemoteLink, t0);
        assert!(actions.iter().any(|a| matches!(
            a,
            Action::SendFrame(Frame101::Fixed { control, .. })
            if ControlField::decode(*control, Direction::Primary)
                == Some(ControlField::Primary {
                    fcb: false, fcv: false,
                    func: FuncCodePrimary::ResetRemoteLink,
                })
        )));

        // Outstation ACKs with a fixed secondary frame.
        let ack_cf = ControlField::Secondary {
            acd: false,
            dfc: false,
            func: FuncCodeSecondary::Ack,
        };
        let ack_frame = Frame101::Fixed {
            control: ack_cf.encode(),
            address: LinkAddress(1),
        };
        let actions = m.handle(Input::FrameReceived(ack_frame), at(t0, 0));
        assert!(actions.contains(&Action::LinkStateChanged(LinkState::Ready)));
        assert_eq!(m.link_state(), LinkState::Ready);
    }

    // ---- Scenario: master sends USER_DATA_CONFIRMED → outstation ACKs -----

    #[test]
    fn master_user_data_confirmed_acked() {
        let t0 = Instant::now();
        let mut m = master(1);

        // First get to Ready state.
        m.handle(Input::ResetRemoteLink, t0);
        let ack_cf = ControlField::Secondary {
            acd: false,
            dfc: false,
            func: FuncCodeSecondary::Ack,
        };
        m.handle(
            Input::FrameReceived(Frame101::Fixed {
                control: ack_cf.encode(),
                address: LinkAddress(1),
            }),
            at(t0, 0),
        );
        assert_eq!(m.link_state(), LinkState::Ready);

        // Send user data.
        let asdu = vec![0x01, 0x02, 0x03];
        let actions = m.handle(Input::SendUserData(asdu.clone()), at(t0, 1));
        // Should emit a Variable frame with UserDataConfirmed.
        let sent = actions.iter().find_map(|a| match a {
            Action::SendFrame(f @ Frame101::Variable { .. }) => Some(f.clone()),
            _ => None,
        });
        assert!(sent.is_some(), "expected a Variable frame to be sent");

        // ACK with a single-char 0xE5.
        let actions = m.handle(
            Input::FrameReceived(Frame101::Single(SingleChar::Ack)),
            at(t0, 1),
        );
        // No error, no further send.
        assert!(!actions.iter().any(|a| matches!(a, Action::LinkError(_))));
    }

    // ---- Scenario: FCB toggles on consecutive confirmed sends -------------

    #[test]
    fn fcb_alternates_on_consecutive_sends() {
        let t0 = Instant::now();
        let mut m = master(1);

        // Get to Ready.
        m.handle(Input::ResetRemoteLink, t0);
        let ack_cf = ControlField::Secondary {
            acd: false,
            dfc: false,
            func: FuncCodeSecondary::Ack,
        };
        m.handle(
            Input::FrameReceived(Frame101::Fixed {
                control: ack_cf.encode(),
                address: LinkAddress(1),
            }),
            t0,
        );

        // First send — FCB should be false (initial).
        let actions = m.handle(Input::SendUserData(vec![0xAA]), at(t0, 1));
        let first_control = extract_variable_control(&actions);

        // ACK the first send.
        m.handle(
            Input::FrameReceived(Frame101::Single(SingleChar::Ack)),
            at(t0, 1),
        );

        // Second send — FCB should have toggled.
        let actions = m.handle(Input::SendUserData(vec![0xBB]), at(t0, 2));
        let second_control = extract_variable_control(&actions);

        let cf1 = ControlField::decode(first_control, Direction::Primary).unwrap();
        let cf2 = ControlField::decode(second_control, Direction::Primary).unwrap();

        let fcb1 = match cf1 {
            ControlField::Primary { fcb, .. } => fcb,
            _ => panic!("expected primary"),
        };
        let fcb2 = match cf2 {
            ControlField::Primary { fcb, .. } => fcb,
            _ => panic!("expected primary"),
        };
        assert_ne!(fcb1, fcb2, "FCB should toggle between sends");
    }

    fn extract_variable_control(actions: &[Action]) -> u8 {
        actions
            .iter()
            .find_map(|a| match a {
                Action::SendFrame(Frame101::Variable { control, .. }) => Some(*control),
                _ => None,
            })
            .expect("expected a Variable frame")
    }

    // ---- Scenario: outstation responds to REQUEST_USER_DATA_CLASS_1 -------

    #[test]
    fn master_class1_request_delivers_asdu() {
        let t0 = Instant::now();
        let mut m = master(1);

        // Get to Ready.
        m.handle(Input::ResetRemoteLink, t0);
        let ack_cf = ControlField::Secondary {
            acd: false,
            dfc: false,
            func: FuncCodeSecondary::Ack,
        };
        m.handle(
            Input::FrameReceived(Frame101::Fixed {
                control: ack_cf.encode(),
                address: LinkAddress(1),
            }),
            t0,
        );

        // Master requests class-1 data.
        let actions = m.handle(Input::RequestUserDataClass1, at(t0, 1));
        assert!(actions.iter().any(|a| matches!(a, Action::SendFrame(_))));

        // Outstation replies with user data.
        let resp_cf = ControlField::Secondary {
            acd: false,
            dfc: false,
            func: FuncCodeSecondary::RespondUserData,
        };
        let asdu = vec![0xDE, 0xAD];
        let actions = m.handle(
            Input::FrameReceived(Frame101::Variable {
                control: resp_cf.encode(),
                address: LinkAddress(1),
                asdu: asdu.clone(),
            }),
            at(t0, 1),
        );
        assert!(actions.contains(&Action::DeliverAsdu(asdu.clone())));
    }

    // ---- Scenario: ACK timeout triggers retry then LinkError ---------------

    #[test]
    fn ack_timeout_retries_then_fails() {
        let t0 = Instant::now();
        let timeout = Duration::from_secs(1);
        let max_retries = 3;
        let mut m = Connection::new(
            Role::Master,
            Mode::Unbalanced,
            Config {
                link_address: LinkAddress(1),
                addr_size: LinkAddressSize::One,
                timeout,
                max_retries,
            },
        );

        // Get to Ready.
        m.handle(Input::ResetRemoteLink, t0);
        let ack_cf = ControlField::Secondary {
            acd: false,
            dfc: false,
            func: FuncCodeSecondary::Ack,
        };
        m.handle(
            Input::FrameReceived(Frame101::Fixed {
                control: ack_cf.encode(),
                address: LinkAddress(1),
            }),
            t0,
        );

        // Send user data — first attempt.
        m.handle(Input::SendUserData(vec![0x01]), at(t0, 1));

        // No ACK — first retry at t0+2.
        let actions = m.handle(Input::Tick, at(t0, 2));
        assert!(actions.iter().any(|a| matches!(a, Action::SendFrame(_))));

        // Second retry at t0+3.
        let actions = m.handle(Input::Tick, at(t0, 3));
        assert!(actions.iter().any(|a| matches!(a, Action::SendFrame(_))));

        // Third retry at t0+4.
        let actions = m.handle(Input::Tick, at(t0, 4));
        assert!(actions.iter().any(|a| matches!(a, Action::SendFrame(_))));

        // Fourth tick: attempts exhausted → LinkError.
        let actions = m.handle(Input::Tick, at(t0, 5));
        assert!(actions.contains(&Action::LinkError(Reason::AckTimeout)));
        assert_eq!(m.link_state(), LinkState::Failed);
    }

    // ---- Outstation: reset → ack, then handles confirmed user data --------

    #[test]
    fn outstation_reset_then_delivers_user_data() {
        let t0 = Instant::now();
        let mut o = outstation(1);

        // Receive a reset from the master.
        let reset_cf = ControlField::Primary {
            fcb: false,
            fcv: false,
            func: FuncCodePrimary::ResetRemoteLink,
        };
        let actions = o.handle(
            Input::FrameReceived(Frame101::Fixed {
                control: reset_cf.encode(),
                address: LinkAddress(1),
            }),
            t0,
        );
        // Should ACK and transition to Ready.
        assert!(actions.iter().any(|a| matches!(a, Action::SendFrame(_))));
        assert!(actions.contains(&Action::LinkStateChanged(LinkState::Ready)));

        // Receive user data (UserDataConfirmed).
        let data_cf = ControlField::Primary {
            fcb: false,
            fcv: true,
            func: FuncCodePrimary::UserDataConfirmed,
        };
        let asdu = vec![0x01, 0x02];
        let actions = o.handle(
            Input::FrameReceived(Frame101::Variable {
                control: data_cf.encode(),
                address: LinkAddress(1),
                asdu: asdu.clone(),
            }),
            at(t0, 1),
        );
        // Should ACK and deliver the ASDU.
        assert!(actions.iter().any(|a| matches!(a, Action::SendFrame(_))));
        assert!(actions.contains(&Action::DeliverAsdu(asdu.clone())));
    }
}
