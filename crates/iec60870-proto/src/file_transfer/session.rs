//! File-transfer session state machine.
//!
//! See [`Session`] for the public entry point. The state machine is single-
//! file, single-section by design — sufficient for the vast majority of
//! real-world IEC 60870-5 file deployments. A multi-section extension would
//! loop on `Sr -> Sg* -> Ls` instead of jumping straight to `AwaitAck` after
//! the first `F_LS_NA_1`.

use std::time::{Duration, Instant};

use crate::asdu::header::Ioa;
use crate::asdu::types::file::{
    Afq, AfqAction, Checksum, F_AF_NA_1, F_FR_NA_1, F_LS_NA_1, F_SC_NA_1, F_SG_NA_1,
    F_SR_NA_1, Frq, LengthOfFile, LengthOfSection, Lsq, NameOfFile, NameOfSection, Scq,
    ScqAction, Srq, MAX_SEGMENT_BYTES,
};

/// Default segment payload size in bytes. Conservative enough to fit inside
/// an IEC 60870-5-104 APDU after the surrounding header (TypeID + VSQ + COT
/// + CA + IOA + NOF + NOS + length octet = ~13 bytes overhead).
pub const DEFAULT_SEGMENT_BYTES: usize = 240;

/// Tunables for one [`Session`].
#[derive(Debug, Clone, Copy)]
pub struct SessionConfig {
    /// Maximum bytes per `F_SG_NA_1` segment. Clamped to [1, [`MAX_SEGMENT_BYTES`]].
    pub max_segment_bytes: usize,
    /// Inactivity timeout — if no progress is observed within this window,
    /// the session aborts with [`FailureReason::Timeout`].
    pub idle_timeout: Duration,
    /// IOA carried in emitted ASDUs. By convention 0 for file-transfer.
    pub ioa: Ioa,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            max_segment_bytes: DEFAULT_SEGMENT_BYTES,
            idle_timeout: Duration::from_secs(30),
            ioa: Ioa(0),
        }
    }
}

/// Direction of a transfer from the local side's perspective.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    /// We want to receive a file. We drive the dialogue with `F_SC_NA_1`.
    Receiver,
    /// We hold the file and emit segments. We respond to `F_SC_NA_1` (or
    /// initiate proactively with `F_FR_NA_1`).
    Sender,
}

/// Why a session failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FailureReason {
    /// No FT ASDU was observed within [`SessionConfig::idle_timeout`].
    Timeout,
    /// Peer reported a negative ack (negative `FRQ`, `SRQ`, `AFQ`, …).
    PeerRejected,
    /// We rejected the peer's request locally (e.g. file unknown, write
    /// refused).
    LocallyRejected,
    /// `F_LS_NA_1` carried a checksum that disagrees with the bytes we
    /// observed for the section.
    ChecksumMismatch,
    /// Inbound ASDU arrived in a state where it doesn't make sense.
    ProtocolViolation,
    /// Local caller asked the session to abort.
    Aborted,
}

/// Stages of the dialogue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SessionState {
    /// Receiver: about to send `F_SC_NA_1{SelectFile}`.
    /// Sender: about to send `F_FR_NA_1` (proactive) OR awaiting it (reactive).
    Idle,
    /// Receiver: sent SELECT, awaiting `F_FR_NA_1`.
    RxAwaitFileReady,
    /// Receiver: sent REQUEST, awaiting `F_SR_NA_1`.
    RxAwaitSectionReady,
    /// Receiver: collecting `F_SG_NA_1` segments; waiting for `F_LS_NA_1`.
    RxReceivingSegments,
    /// Sender: sent `F_FR_NA_1`, awaiting `F_SC_NA_1{SelectFile}` confirmation.
    /// (Reactive senders may skip this and start at [`Self::TxAwaitRequest`].)
    TxAwaitSelect,
    /// Sender: ready to ship; awaiting `F_SC_NA_1{RequestFile}`.
    TxAwaitRequest,
    /// Sender: pushing segments. Waits for [`SessionInput::SegmentReady`]
    /// callbacks from the provider.
    TxSendingSegments,
    /// Sender: sent `F_LS_NA_1`, awaiting `F_AF_NA_1`.
    TxAwaitAck,
    /// Terminal — transfer succeeded.
    Completed,
    /// Terminal — transfer failed.
    Failed,
}

/// Driver inputs.
#[derive(Debug)]
pub enum SessionInput {
    /// Local kick-off. For [`Role::Receiver`], `length` is ignored. For
    /// [`Role::Sender`], `length` is the total file size in bytes.
    Start { length: u32 },
    /// An `F_FR_NA_1` arrived from the peer.
    FileReady(F_FR_NA_1),
    /// An `F_SR_NA_1` arrived from the peer.
    SectionReady(F_SR_NA_1),
    /// An `F_SC_NA_1` arrived from the peer.
    SelectCall(F_SC_NA_1),
    /// An `F_LS_NA_1` arrived from the peer.
    LastSection(F_LS_NA_1),
    /// An `F_AF_NA_1` arrived from the peer.
    AckFile(F_AF_NA_1),
    /// An `F_SG_NA_1` segment arrived from the peer (receiver side).
    Segment(F_SG_NA_1),
    /// The provider produced the next chunk of bytes (sender side). An empty
    /// slice signals EOF — the session emits `F_LS_NA_1` and advances.
    SegmentReady(Vec<u8>),
    /// Wall clock advanced. The driver should send this every ~250 ms.
    Tick,
    /// Local caller wants to give up.
    Abort,
}

/// Driver outputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionAction {
    /// Encode and ship `F_FR_NA_1`.
    SendFileReady(F_FR_NA_1),
    /// Encode and ship `F_SR_NA_1`.
    SendSectionReady(F_SR_NA_1),
    /// Encode and ship `F_SC_NA_1`.
    SendSelectCall(F_SC_NA_1),
    /// Encode and ship `F_LS_NA_1`.
    SendLastSection(F_LS_NA_1),
    /// Encode and ship `F_AF_NA_1`.
    SendAckFile(F_AF_NA_1),
    /// Encode and ship `F_SG_NA_1`.
    SendSegment(F_SG_NA_1),
    /// Sender side: ask the provider for the next chunk (up to `max_bytes`).
    /// When the provider answers via [`SessionInput::SegmentReady`], the
    /// session resumes.
    RequestNextSegment { max_bytes: usize },
    /// Receiver side: hand the bytes to the provider for storage. The driver
    /// is expected to feed them into a `FileWriter` immediately.
    DeliverSegment(Vec<u8>),
    /// Terminal success: the transfer completed and the session can be dropped.
    Completed { bytes: u32 },
    /// Terminal failure: drop the session and surface the reason.
    Failed(FailureReason),
}

/// One file-transfer session.
#[derive(Debug)]
pub struct Session {
    role: Role,
    nof: NameOfFile,
    state: SessionState,
    config: SessionConfig,

    /// File length. For Sender: total to ship. For Receiver: announced by
    /// `F_FR_NA_1` (zero until then).
    file_length: u32,
    /// Section length (we use one section per file). Equal to `file_length`
    /// once known.
    section_length: u32,
    /// How many segment bytes have been pushed (sender) or accepted (receiver).
    bytes_transferred: u32,
    /// Incremental checksum over the current section's segment payloads.
    section_checksum: Checksum,
    /// Latest time at which an FT ASDU made progress (resets the idle timer).
    last_progress: Option<Instant>,
    /// Sender-only: most recent `RequestNextSegment` is outstanding. Used to
    /// avoid double-requesting on consecutive `Tick`s.
    awaiting_provider: bool,

    actions: Vec<SessionAction>,
}

impl Session {
    /// Build a session. The session does *not* drive itself; the caller must
    /// invoke [`Self::step`] for each input. Use [`SessionInput::Start`] to
    /// initiate.
    pub fn new(role: Role, nof: NameOfFile, config: SessionConfig) -> Self {
        Self {
            role,
            nof,
            state: SessionState::Idle,
            config,
            file_length: 0,
            section_length: 0,
            bytes_transferred: 0,
            section_checksum: Checksum::default(),
            last_progress: None,
            awaiting_provider: false,
            actions: Vec::new(),
        }
    }

    /// Current role.
    pub fn role(&self) -> Role {
        self.role
    }

    /// File the session refers to.
    pub fn nof(&self) -> NameOfFile {
        self.nof
    }

    /// Current high-level state.
    pub fn state(&self) -> SessionState {
        self.state
    }

    /// `true` once the session has reached [`SessionState::Completed`] or
    /// [`SessionState::Failed`].
    pub fn is_terminal(&self) -> bool {
        matches!(self.state, SessionState::Completed | SessionState::Failed)
    }

    /// Drive the state machine with one input. Returns the actions to apply.
    pub fn step(&mut self, input: SessionInput, now: Instant) -> Vec<SessionAction> {
        if self.is_terminal() {
            return Vec::new();
        }
        match input {
            SessionInput::Start { length } => self.on_start(length),
            SessionInput::FileReady(asdu) => self.on_file_ready(asdu, now),
            SessionInput::SectionReady(asdu) => self.on_section_ready(asdu, now),
            SessionInput::SelectCall(asdu) => self.on_select_call(asdu, now),
            SessionInput::LastSection(asdu) => self.on_last_section(asdu, now),
            SessionInput::AckFile(asdu) => self.on_ack_file(asdu, now),
            SessionInput::Segment(asdu) => self.on_segment(asdu, now),
            SessionInput::SegmentReady(data) => self.on_segment_ready(data, now),
            SessionInput::Tick => {}
            SessionInput::Abort => self.fail(FailureReason::Aborted),
        }
        self.check_timeout(now);
        std::mem::take(&mut self.actions)
    }

    // ---- Input handlers ---------------------------------------------------

    fn on_start(&mut self, length: u32) {
        if self.state != SessionState::Idle {
            return;
        }
        match self.role {
            Role::Receiver => {
                self.emit(SessionAction::SendSelectCall(F_SC_NA_1 {
                    ioa: self.config.ioa,
                    nof: self.nof,
                    nos: NameOfSection::WHOLE_FILE,
                    scq: Scq::new(ScqAction::SelectFile, 0),
                }));
                self.state = SessionState::RxAwaitFileReady;
            }
            Role::Sender => {
                self.file_length = length;
                self.section_length = length;
                self.emit(SessionAction::SendFileReady(F_FR_NA_1 {
                    ioa: self.config.ioa,
                    nof: self.nof,
                    lof: LengthOfFile(length),
                    frq: Frq::READY,
                }));
                self.state = SessionState::TxAwaitSelect;
            }
        }
    }

    fn on_file_ready(&mut self, asdu: F_FR_NA_1, now: Instant) {
        // Only the receiver consumes F_FR_NA_1. A sender that sees one is
        // looking at its own echo — ignore.
        if self.role != Role::Receiver {
            return;
        }
        if self.state != SessionState::RxAwaitFileReady {
            self.fail(FailureReason::ProtocolViolation);
            return;
        }
        self.last_progress = Some(now);
        if asdu.frq.negative {
            self.fail(FailureReason::PeerRejected);
            return;
        }
        self.file_length = asdu.lof.0;
        self.section_length = asdu.lof.0;
        self.emit(SessionAction::SendSelectCall(F_SC_NA_1 {
            ioa: self.config.ioa,
            nof: self.nof,
            nos: NameOfSection::WHOLE_FILE,
            scq: Scq::new(ScqAction::RequestFile, 0),
        }));
        self.state = SessionState::RxAwaitSectionReady;
    }

    fn on_section_ready(&mut self, asdu: F_SR_NA_1, now: Instant) {
        if self.role != Role::Receiver || self.state != SessionState::RxAwaitSectionReady {
            self.fail(FailureReason::ProtocolViolation);
            return;
        }
        self.last_progress = Some(now);
        if asdu.srq.negative {
            self.fail(FailureReason::PeerRejected);
            return;
        }
        self.section_length = asdu.los.0;
        self.bytes_transferred = 0;
        self.section_checksum = Checksum::default();
        self.state = SessionState::RxReceivingSegments;
    }

    fn on_select_call(&mut self, asdu: F_SC_NA_1, now: Instant) {
        if self.role != Role::Sender {
            self.fail(FailureReason::ProtocolViolation);
            return;
        }
        self.last_progress = Some(now);
        match (self.state, asdu.scq.action) {
            (SessionState::Idle, ScqAction::SelectFile)
            | (SessionState::TxAwaitSelect, ScqAction::SelectFile) => {
                // Reactive start: respond with F_FR_NA_1, await REQUEST.
                self.emit(SessionAction::SendFileReady(F_FR_NA_1 {
                    ioa: self.config.ioa,
                    nof: self.nof,
                    lof: LengthOfFile(self.file_length),
                    frq: Frq::READY,
                }));
                self.state = SessionState::TxAwaitRequest;
            }
            (SessionState::TxAwaitRequest, ScqAction::RequestFile) => {
                // Master wants the data: announce the section, ask the
                // provider for the first chunk.
                self.section_checksum = Checksum::default();
                self.bytes_transferred = 0;
                self.emit(SessionAction::SendSectionReady(F_SR_NA_1 {
                    ioa: self.config.ioa,
                    nof: self.nof,
                    nos: NameOfSection(1),
                    los: LengthOfSection(self.section_length),
                    srq: Srq::READY,
                }));
                self.request_next_segment();
                self.state = SessionState::TxSendingSegments;
            }
            (_, ScqAction::DeactivateFile)
            | (_, ScqAction::DeleteFile)
            | (_, ScqAction::DeactivateSection) => {
                self.fail(FailureReason::Aborted);
            }
            _ => {
                self.fail(FailureReason::ProtocolViolation);
            }
        }
    }

    fn on_last_section(&mut self, asdu: F_LS_NA_1, now: Instant) {
        if self.role != Role::Receiver || self.state != SessionState::RxReceivingSegments {
            self.fail(FailureReason::ProtocolViolation);
            return;
        }
        self.last_progress = Some(now);
        if asdu.chs.0 != self.section_checksum.0 {
            // Tell the peer we rejected the section, then mark failure.
            self.emit(SessionAction::SendAckFile(F_AF_NA_1 {
                ioa: self.config.ioa,
                nof: self.nof,
                nos: asdu.nos,
                afq: Afq::new(AfqAction::NegativeSection, 0),
            }));
            self.fail(FailureReason::ChecksumMismatch);
            return;
        }
        // ACK section, ACK file (single-section model).
        self.emit(SessionAction::SendAckFile(F_AF_NA_1 {
            ioa: self.config.ioa,
            nof: self.nof,
            nos: asdu.nos,
            afq: Afq::new(AfqAction::PositiveFile, 0),
        }));
        let bytes = self.bytes_transferred;
        self.state = SessionState::Completed;
        self.emit(SessionAction::Completed { bytes });
    }

    fn on_ack_file(&mut self, asdu: F_AF_NA_1, now: Instant) {
        if self.role != Role::Sender || self.state != SessionState::TxAwaitAck {
            self.fail(FailureReason::ProtocolViolation);
            return;
        }
        self.last_progress = Some(now);
        match asdu.afq.action {
            AfqAction::PositiveFile | AfqAction::PositiveSection => {
                let bytes = self.bytes_transferred;
                self.state = SessionState::Completed;
                self.emit(SessionAction::Completed { bytes });
            }
            _ => self.fail(FailureReason::PeerRejected),
        }
    }

    fn on_segment(&mut self, asdu: F_SG_NA_1, now: Instant) {
        if self.role != Role::Receiver || self.state != SessionState::RxReceivingSegments {
            self.fail(FailureReason::ProtocolViolation);
            return;
        }
        self.last_progress = Some(now);
        self.section_checksum.update_slice(&asdu.segment);
        self.bytes_transferred = self.bytes_transferred.saturating_add(asdu.segment.len() as u32);
        self.emit(SessionAction::DeliverSegment(asdu.segment));
    }

    fn on_segment_ready(&mut self, data: Vec<u8>, now: Instant) {
        if self.role != Role::Sender || self.state != SessionState::TxSendingSegments {
            return;
        }
        self.awaiting_provider = false;
        if data.is_empty() {
            // EOF — close with F_LS_NA_1 carrying the running checksum.
            self.emit(SessionAction::SendLastSection(F_LS_NA_1 {
                ioa: self.config.ioa,
                nof: self.nof,
                nos: NameOfSection(1),
                lsq: Lsq::FileWithoutDeactivate,
                chs: self.section_checksum,
            }));
            self.state = SessionState::TxAwaitAck;
            self.last_progress = Some(now);
            return;
        }
        self.section_checksum.update_slice(&data);
        self.bytes_transferred = self.bytes_transferred.saturating_add(data.len() as u32);
        self.last_progress = Some(now);
        self.emit(SessionAction::SendSegment(F_SG_NA_1 {
            ioa: self.config.ioa,
            nof: self.nof,
            nos: NameOfSection(1),
            segment: data,
        }));
        // Pump the pipeline by asking for the next chunk straight away.
        self.request_next_segment();
    }

    // ---- Helpers ----------------------------------------------------------

    fn request_next_segment(&mut self) {
        if self.awaiting_provider {
            return;
        }
        let max = self
            .config
            .max_segment_bytes
            .clamp(1, MAX_SEGMENT_BYTES);
        self.awaiting_provider = true;
        self.emit(SessionAction::RequestNextSegment { max_bytes: max });
    }

    fn check_timeout(&mut self, now: Instant) {
        if self.is_terminal() {
            return;
        }
        if matches!(self.state, SessionState::Idle) {
            return;
        }
        if let Some(last) = self.last_progress {
            if now.saturating_duration_since(last) >= self.config.idle_timeout {
                self.fail(FailureReason::Timeout);
            }
        } else {
            // No progress yet — start the timer relative to now.
            self.last_progress = Some(now);
        }
    }

    fn fail(&mut self, reason: FailureReason) {
        if self.is_terminal() {
            return;
        }
        self.state = SessionState::Failed;
        self.emit(SessionAction::Failed(reason));
    }

    fn emit(&mut self, action: SessionAction) {
        self.actions.push(action);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> SessionConfig {
        SessionConfig {
            max_segment_bytes: 16,
            idle_timeout: Duration::from_millis(500),
            ioa: Ioa(0),
        }
    }

    fn now_plus(d: Duration) -> Instant {
        Instant::now() + d
    }

    // ---- Receiver happy path ----------------------------------------------

    #[test]
    fn receiver_happy_path() {
        let mut s = Session::new(Role::Receiver, NameOfFile(7), cfg());
        let t0 = Instant::now();

        let acts = s.step(SessionInput::Start { length: 0 }, t0);
        assert_eq!(acts.len(), 1);
        assert!(matches!(acts[0], SessionAction::SendSelectCall(_)));
        assert_eq!(s.state(), SessionState::RxAwaitFileReady);

        // Peer announces file ready with 32 bytes.
        let acts = s.step(
            SessionInput::FileReady(F_FR_NA_1 {
                ioa: Ioa(0),
                nof: NameOfFile(7),
                lof: LengthOfFile(32),
                frq: Frq::READY,
            }),
            t0,
        );
        assert!(matches!(acts[0], SessionAction::SendSelectCall(_)));
        assert_eq!(s.state(), SessionState::RxAwaitSectionReady);

        // Peer announces section ready.
        let acts = s.step(
            SessionInput::SectionReady(F_SR_NA_1 {
                ioa: Ioa(0),
                nof: NameOfFile(7),
                nos: NameOfSection(1),
                los: LengthOfSection(32),
                srq: Srq::READY,
            }),
            t0,
        );
        assert!(acts.is_empty()); // Receiver just transitions silently
        assert_eq!(s.state(), SessionState::RxReceivingSegments);

        // Two segments arrive: 16 + 16 bytes.
        let seg_a = vec![0x01u8; 16];
        let seg_b = vec![0x02u8; 16];
        let acts = s.step(
            SessionInput::Segment(F_SG_NA_1 {
                ioa: Ioa(0),
                nof: NameOfFile(7),
                nos: NameOfSection(1),
                segment: seg_a.clone(),
            }),
            t0,
        );
        assert!(matches!(acts[0], SessionAction::DeliverSegment(ref d) if *d == seg_a));

        let acts = s.step(
            SessionInput::Segment(F_SG_NA_1 {
                ioa: Ioa(0),
                nof: NameOfFile(7),
                nos: NameOfSection(1),
                segment: seg_b.clone(),
            }),
            t0,
        );
        assert!(matches!(acts[0], SessionAction::DeliverSegment(ref d) if *d == seg_b));

        // Compute expected checksum.
        let mut chs = Checksum::default();
        chs.update_slice(&seg_a);
        chs.update_slice(&seg_b);

        // Last section.
        let acts = s.step(
            SessionInput::LastSection(F_LS_NA_1 {
                ioa: Ioa(0),
                nof: NameOfFile(7),
                nos: NameOfSection(1),
                lsq: Lsq::FileWithoutDeactivate,
                chs,
            }),
            t0,
        );
        // Expect: SendAckFile + Completed
        assert_eq!(acts.len(), 2);
        assert!(matches!(acts[0], SessionAction::SendAckFile(_)));
        assert!(matches!(acts[1], SessionAction::Completed { bytes: 32 }));
        assert_eq!(s.state(), SessionState::Completed);
        assert!(s.is_terminal());
    }

    // ---- Sender happy path ------------------------------------------------

    #[test]
    fn sender_happy_path_proactive() {
        let mut s = Session::new(Role::Sender, NameOfFile(7), cfg());
        let t0 = Instant::now();

        let acts = s.step(SessionInput::Start { length: 32 }, t0);
        assert!(matches!(acts[0], SessionAction::SendFileReady(_)));
        assert_eq!(s.state(), SessionState::TxAwaitSelect);

        // Master confirms with SELECT.
        let acts = s.step(
            SessionInput::SelectCall(F_SC_NA_1 {
                ioa: Ioa(0),
                nof: NameOfFile(7),
                nos: NameOfSection::WHOLE_FILE,
                scq: Scq::new(ScqAction::SelectFile, 0),
            }),
            t0,
        );
        // Reactive SELECT after proactive FR is handled by re-sending FR.
        // The session is now in TxAwaitRequest.
        assert_eq!(s.state(), SessionState::TxAwaitRequest);
        assert!(matches!(acts[0], SessionAction::SendFileReady(_)));

        // Master requests the data.
        let acts = s.step(
            SessionInput::SelectCall(F_SC_NA_1 {
                ioa: Ioa(0),
                nof: NameOfFile(7),
                nos: NameOfSection::WHOLE_FILE,
                scq: Scq::new(ScqAction::RequestFile, 0),
            }),
            t0,
        );
        // Expect SendSectionReady + RequestNextSegment.
        assert!(matches!(acts[0], SessionAction::SendSectionReady(_)));
        assert!(matches!(acts[1], SessionAction::RequestNextSegment { max_bytes: 16 }));
        assert_eq!(s.state(), SessionState::TxSendingSegments);

        // Provider yields a 16-byte chunk.
        let chunk = vec![0xAB; 16];
        let acts = s.step(SessionInput::SegmentReady(chunk.clone()), t0);
        // Expect SendSegment + RequestNextSegment.
        assert!(matches!(acts[0], SessionAction::SendSegment(ref a) if a.segment == chunk));
        assert!(matches!(acts[1], SessionAction::RequestNextSegment { .. }));

        // Provider yields a second 16-byte chunk.
        let chunk2 = vec![0xCD; 16];
        let _ = s.step(SessionInput::SegmentReady(chunk2.clone()), t0);

        // Provider signals EOF.
        let acts = s.step(SessionInput::SegmentReady(Vec::new()), t0);
        assert!(matches!(acts[0], SessionAction::SendLastSection(_)));
        assert_eq!(s.state(), SessionState::TxAwaitAck);

        // Master acks positively.
        let acts = s.step(
            SessionInput::AckFile(F_AF_NA_1 {
                ioa: Ioa(0),
                nof: NameOfFile(7),
                nos: NameOfSection(1),
                afq: Afq::new(AfqAction::PositiveFile, 0),
            }),
            t0,
        );
        assert!(matches!(acts[0], SessionAction::Completed { bytes: 32 }));
        assert_eq!(s.state(), SessionState::Completed);
    }

    // ---- Failure modes ----------------------------------------------------

    #[test]
    fn receiver_checksum_mismatch() {
        let mut s = Session::new(Role::Receiver, NameOfFile(1), cfg());
        let t0 = Instant::now();
        let _ = s.step(SessionInput::Start { length: 0 }, t0);
        let _ = s.step(
            SessionInput::FileReady(F_FR_NA_1 {
                ioa: Ioa(0),
                nof: NameOfFile(1),
                lof: LengthOfFile(4),
                frq: Frq::READY,
            }),
            t0,
        );
        let _ = s.step(
            SessionInput::SectionReady(F_SR_NA_1 {
                ioa: Ioa(0),
                nof: NameOfFile(1),
                nos: NameOfSection(1),
                los: LengthOfSection(4),
                srq: Srq::READY,
            }),
            t0,
        );
        let _ = s.step(
            SessionInput::Segment(F_SG_NA_1 {
                ioa: Ioa(0),
                nof: NameOfFile(1),
                nos: NameOfSection(1),
                segment: vec![1, 2, 3, 4],
            }),
            t0,
        );
        // Real checksum is 10, send a wrong one.
        let acts = s.step(
            SessionInput::LastSection(F_LS_NA_1 {
                ioa: Ioa(0),
                nof: NameOfFile(1),
                nos: NameOfSection(1),
                lsq: Lsq::FileWithoutDeactivate,
                chs: Checksum(0xFF),
            }),
            t0,
        );
        // SendAckFile (negative) + Failed(ChecksumMismatch)
        assert!(matches!(acts[0], SessionAction::SendAckFile(_)));
        assert!(matches!(
            acts[1],
            SessionAction::Failed(FailureReason::ChecksumMismatch)
        ));
        assert_eq!(s.state(), SessionState::Failed);
    }

    #[test]
    fn receiver_timeout() {
        let mut s = Session::new(Role::Receiver, NameOfFile(1), cfg());
        let t0 = Instant::now();
        let _ = s.step(SessionInput::Start { length: 0 }, t0);
        // Skip forward past idle_timeout without any peer activity.
        let acts = s.step(SessionInput::Tick, now_plus(Duration::from_secs(2)));
        assert!(matches!(
            acts[0],
            SessionAction::Failed(FailureReason::Timeout)
        ));
    }

    #[test]
    fn sender_peer_rejects() {
        let mut s = Session::new(Role::Sender, NameOfFile(1), cfg());
        let t0 = Instant::now();
        let _ = s.step(SessionInput::Start { length: 4 }, t0);
        // Force sender into TxAwaitAck path quickly.
        let _ = s.step(
            SessionInput::SelectCall(F_SC_NA_1 {
                ioa: Ioa(0),
                nof: NameOfFile(1),
                nos: NameOfSection::WHOLE_FILE,
                scq: Scq::new(ScqAction::SelectFile, 0),
            }),
            t0,
        );
        let _ = s.step(
            SessionInput::SelectCall(F_SC_NA_1 {
                ioa: Ioa(0),
                nof: NameOfFile(1),
                nos: NameOfSection::WHOLE_FILE,
                scq: Scq::new(ScqAction::RequestFile, 0),
            }),
            t0,
        );
        // Provider yields all four bytes then EOF.
        let _ = s.step(SessionInput::SegmentReady(vec![1, 2, 3, 4]), t0);
        let _ = s.step(SessionInput::SegmentReady(Vec::new()), t0);
        assert_eq!(s.state(), SessionState::TxAwaitAck);

        let acts = s.step(
            SessionInput::AckFile(F_AF_NA_1 {
                ioa: Ioa(0),
                nof: NameOfFile(1),
                nos: NameOfSection(1),
                afq: Afq::new(AfqAction::NegativeFile, 0),
            }),
            t0,
        );
        assert!(matches!(
            acts[0],
            SessionAction::Failed(FailureReason::PeerRejected)
        ));
    }

    #[test]
    fn abort_is_idempotent() {
        let mut s = Session::new(Role::Receiver, NameOfFile(1), cfg());
        let t0 = Instant::now();
        let _ = s.step(SessionInput::Start { length: 0 }, t0);
        let acts = s.step(SessionInput::Abort, t0);
        assert!(matches!(
            acts[0],
            SessionAction::Failed(FailureReason::Aborted)
        ));
        // Subsequent inputs are ignored.
        let acts = s.step(
            SessionInput::FileReady(F_FR_NA_1::default()),
            t0,
        );
        assert!(acts.is_empty());
    }
}
