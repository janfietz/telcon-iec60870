//! Async driver-side runner for file-transfer sessions.
//!
//! Each instance owns one [`FileTransferProvider`] and routes incoming FT
//! ASDUs into the right [`iec60870_proto::file_transfer::Session`]. Outgoing
//! ASDUs are encoded and sent back through the connection driver's command
//! channel.
//!
//! For simplicity the v1 implementation supports a single concurrent
//! transfer per service instance — most field deployments transfer files
//! sequentially anyway, and the rest can spawn a fresh connection.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::BytesMut;
use iec60870_proto::asdu::cot::{Cause, Cot};
use iec60870_proto::asdu::types::file::{
    NameOfFile, F_AF_NA_1, F_FR_NA_1, F_LS_NA_1, F_SC_NA_1, F_SG_NA_1, F_SR_NA_1,
};
use iec60870_proto::asdu::{Asdu, AsduAddressing, AsduPayload, CommonAddress, Vsq};
use iec60870_proto::file_transfer::{
    FailureReason, Role, Session, SessionAction, SessionConfig, SessionInput,
};
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::driver::Command;

use super::events::{FileTransferEvent, FileTransferOutcome};
use super::provider::{
    FileReader, FileTransferConfig, FileTransferError, FileTransferProvider, FileWriter,
};

/// Type-erased provider handle used by the service. Users normally pass a
/// concrete provider into the builder hooks on [`crate::Client104`] /
/// [`crate::Server104`]; those builders wrap it in this newtype.
#[derive(Clone)]
pub struct ProviderObject {
    inner: Arc<dyn FileTransferProvider>,
}

impl ProviderObject {
    pub fn new<P: FileTransferProvider>(provider: P) -> Self {
        Self {
            inner: Arc::new(provider),
        }
    }

    pub(crate) fn as_ref(&self) -> &dyn FileTransferProvider {
        self.inner.as_ref()
    }
}

impl std::fmt::Debug for ProviderObject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderObject").finish()
    }
}

/// High-level user-facing handle on the file-transfer service.
///
/// Obtained via [`crate::Client104::file_transfer`] /
/// [`crate::ServerConnection::file_transfer`]. Methods are infallible at the
/// channel level (the underlying connection task is alive as long as the
/// `Client104` / `ServerConnection` is alive); the [`Result`] only carries
/// file-transfer protocol failures.
#[derive(Debug, Clone)]
pub struct FileTransferHandle {
    intent_tx: mpsc::Sender<Intent>,
    events: broadcast::Sender<FileTransferEvent>,
}

impl FileTransferHandle {
    /// Fetch a file from the peer. The bytes are streamed into the configured
    /// provider's [`FileTransferProvider::open_write`] sink.
    pub async fn fetch(
        &self,
        ca: CommonAddress,
        nof: NameOfFile,
    ) -> Result<u32, FileTransferError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.intent_tx
            .send(Intent::Fetch {
                ca,
                nof,
                reply: reply_tx,
            })
            .await
            .map_err(|_| FileTransferError::Other("file-transfer service stopped".into()))?;
        reply_rx
            .await
            .map_err(|_| FileTransferError::Other("file-transfer service dropped reply".into()))?
    }

    /// Proactively push a file to the peer using the configured provider's
    /// [`FileTransferProvider::open_read`] as the source. Returns total bytes
    /// sent.
    pub async fn push(&self, ca: CommonAddress, nof: NameOfFile) -> Result<u32, FileTransferError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.intent_tx
            .send(Intent::Push {
                ca,
                nof,
                reply: reply_tx,
            })
            .await
            .map_err(|_| FileTransferError::Other("file-transfer service stopped".into()))?;
        reply_rx
            .await
            .map_err(|_| FileTransferError::Other("file-transfer service dropped reply".into()))?
    }

    /// Subscribe to lifecycle events. Late subscribers receive only events
    /// emitted after they call this method.
    pub fn subscribe(&self) -> broadcast::Receiver<FileTransferEvent> {
        self.events.subscribe()
    }
}

/// Local intents the user-facing handle pushes onto the service.
#[derive(Debug)]
enum Intent {
    Fetch {
        ca: CommonAddress,
        nof: NameOfFile,
        reply: oneshot::Sender<Result<u32, FileTransferError>>,
    },
    Push {
        ca: CommonAddress,
        nof: NameOfFile,
        reply: oneshot::Sender<Result<u32, FileTransferError>>,
    },
}

/// Build a fresh service + paired handle. Returns the handle (for the user)
/// and the service runner that should be dropped onto a tokio task by the
/// owning driver.
pub(crate) fn build(
    provider: ProviderObject,
    config: FileTransferConfig,
    cmd_tx: mpsc::Sender<Command>,
    asdu_rx: mpsc::Receiver<(CommonAddress, Asdu)>,
) -> (FileTransferHandle, FileTransferService) {
    let (intent_tx, intent_rx) = mpsc::channel(8);
    let (events_tx, _) = broadcast::channel(64);
    let handle = FileTransferHandle {
        intent_tx,
        events: events_tx.clone(),
    };
    let service = FileTransferService {
        provider,
        config,
        cmd_tx,
        asdu_rx,
        intent_rx,
        events: events_tx,
        sessions: HashMap::new(),
    };
    (handle, service)
}

/// One active transfer's bookkeeping (one per `(ca, nof)`).
struct ActiveSession {
    session: Session,
    #[allow(dead_code)]
    ca: CommonAddress,
    reader: Option<Box<dyn FileReader + Send>>,
    writer: Option<Box<dyn FileWriter + Send>>,
    reply: Option<oneshot::Sender<Result<u32, FileTransferError>>>,
    last_tick: Instant,
}

/// The service task body. Spawned by [`build`] returns it; the connection
/// owner calls `run` inside `tokio::spawn`.
pub(crate) struct FileTransferService {
    provider: ProviderObject,
    config: FileTransferConfig,
    cmd_tx: mpsc::Sender<Command>,
    asdu_rx: mpsc::Receiver<(CommonAddress, Asdu)>,
    intent_rx: mpsc::Receiver<Intent>,
    events: broadcast::Sender<FileTransferEvent>,
    sessions: HashMap<(CommonAddress, NameOfFile), ActiveSession>,
}

impl FileTransferService {
    pub(crate) async fn run(mut self) {
        let mut ticker = tokio::time::interval(Duration::from_millis(250));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                Some(intent) = self.intent_rx.recv() => {
                    self.on_intent(intent).await;
                }
                Some((ca, asdu)) = self.asdu_rx.recv() => {
                    self.on_incoming(ca, asdu).await;
                }
                _ = ticker.tick() => {
                    self.on_tick().await;
                }
                else => break,
            }
        }
    }

    fn session_config(&self) -> SessionConfig {
        SessionConfig {
            max_segment_bytes: self.config.max_segment_bytes,
            idle_timeout: self.config.idle_timeout,
            ioa: iec60870_proto::asdu::Ioa(0),
        }
    }

    async fn on_intent(&mut self, intent: Intent) {
        match intent {
            Intent::Fetch { ca, nof, reply } => {
                let key = (ca, nof);
                if self.sessions.contains_key(&key) {
                    let _ = reply.send(Err(FileTransferError::InvalidState(
                        "another transfer is already in progress for this (ca, nof)".into(),
                    )));
                    return;
                }
                if self.sessions.len() >= self.config.max_concurrent_sessions {
                    let _ = reply.send(Err(FileTransferError::InvalidState(
                        "max_concurrent_sessions reached".into(),
                    )));
                    return;
                }
                // Open the sink up-front so the caller fails fast on storage errors.
                let writer = match self.provider.as_ref().open_write(nof, 0).await {
                    Ok(w) => w,
                    Err(e) => {
                        let _ = reply.send(Err(e));
                        return;
                    }
                };
                let mut active = ActiveSession {
                    session: Session::new(Role::Receiver, nof, self.session_config()),
                    ca,
                    reader: None,
                    writer: Some(writer),
                    reply: Some(reply),
                    last_tick: Instant::now(),
                };
                self.emit(FileTransferEvent::Started {
                    peer: ca,
                    nof,
                    role: Role::Receiver,
                });
                let acts = active
                    .session
                    .step(SessionInput::Start { length: 0 }, active.last_tick);
                self.sessions.insert(key, active);
                self.apply_actions(key, acts).await;
            }
            Intent::Push { ca, nof, reply } => {
                let key = (ca, nof);
                if self.sessions.contains_key(&key) {
                    let _ = reply.send(Err(FileTransferError::InvalidState(
                        "another transfer is already in progress for this (ca, nof)".into(),
                    )));
                    return;
                }
                if self.sessions.len() >= self.config.max_concurrent_sessions {
                    let _ = reply.send(Err(FileTransferError::InvalidState(
                        "max_concurrent_sessions reached".into(),
                    )));
                    return;
                }
                // Look up file metadata and open the read stream.
                let meta = match self.provider.as_ref().lookup(nof).await {
                    Ok(Some(m)) => m,
                    Ok(None) => {
                        let _ = reply.send(Err(FileTransferError::NotFound { nof }));
                        return;
                    }
                    Err(e) => {
                        let _ = reply.send(Err(e));
                        return;
                    }
                };
                let reader = match self.provider.as_ref().open_read(nof).await {
                    Ok(r) => r,
                    Err(e) => {
                        let _ = reply.send(Err(e));
                        return;
                    }
                };
                let mut active = ActiveSession {
                    session: Session::new(Role::Sender, nof, self.session_config()),
                    ca,
                    reader: Some(reader),
                    writer: None,
                    reply: Some(reply),
                    last_tick: Instant::now(),
                };
                self.emit(FileTransferEvent::Started {
                    peer: ca,
                    nof,
                    role: Role::Sender,
                });
                let acts = active.session.step(
                    SessionInput::Start {
                        length: meta.length,
                    },
                    active.last_tick,
                );
                self.sessions.insert(key, active);
                self.apply_actions(key, acts).await;
            }
        }
    }

    async fn on_incoming(&mut self, ca: CommonAddress, asdu: Asdu) {
        let type_id = asdu.type_id();
        let nof = match peek_nof(&asdu) {
            Some(n) => n,
            None => return, // F_DR_TA_1 / unsupported — ignore for routing
        };
        let key = (ca, nof);
        if !self.sessions.contains_key(&key) {
            // DoS guard: refuse new sessions once the configured cap is reached.
            // Existing sessions continue; the peer must retry after one finishes
            // or times out.
            if self.sessions.len() >= self.config.max_concurrent_sessions {
                tracing::warn!(
                    target: "iec60870::ft",
                    sessions = self.sessions.len(),
                    cap = self.config.max_concurrent_sessions,
                    "refusing new file-transfer session, concurrency cap reached"
                );
                return;
            }
            // Auto-create a passive session for unsolicited inbound flows.
            match type_id {
                120 => {
                    // F_FR_NA_1 unsolicited → peer is pushing a file at us.
                    // Validate the announced length up-front to bound disk use.
                    if let Ok(fr) = asdu.decode_payload::<F_FR_NA_1>(AsduAddressing::IEC104) {
                        if fr.lof.0 > self.config.max_inbound_file_bytes {
                            tracing::warn!(
                                target: "iec60870::ft",
                                lof = fr.lof.0,
                                cap = self.config.max_inbound_file_bytes,
                                "refusing inbound file: LOF exceeds max_inbound_file_bytes"
                            );
                            return;
                        }
                    } else {
                        // Malformed F_FR_NA_1 — don't create a session.
                        return;
                    }
                    let writer = match self.provider.as_ref().open_write(nof, 0).await {
                        Ok(w) => w,
                        Err(e) => {
                            tracing::warn!(target: "iec60870::ft", "open_write failed: {e:?}");
                            return;
                        }
                    };
                    let active = ActiveSession {
                        session: Session::new(Role::Receiver, nof, self.session_config()),
                        ca,
                        reader: None,
                        writer: Some(writer),
                        reply: None,
                        last_tick: Instant::now(),
                    };
                    self.emit(FileTransferEvent::Started {
                        peer: ca,
                        nof,
                        role: Role::Receiver,
                    });
                    self.sessions.insert(key, active);
                }
                122 => {
                    // F_SC_NA_1 unsolicited → peer wants a file from us.
                    let meta = match self.provider.as_ref().lookup(nof).await {
                        Ok(Some(m)) => m,
                        _ => return,
                    };
                    let reader = match self.provider.as_ref().open_read(nof).await {
                        Ok(r) => r,
                        Err(_) => return,
                    };
                    let mut session = Session::new(Role::Sender, nof, self.session_config());
                    // Pre-load total file length so the SELECT handler can
                    // emit F_FR_NA_1 with the right LOF. We deliberately
                    // ignore the actions from Start — the SELECT we received
                    // already drives the session to TxAwaitRequest in the
                    // next step() call below.
                    let _ = session.step(
                        SessionInput::Start {
                            length: meta.length,
                        },
                        Instant::now(),
                    );
                    let active = ActiveSession {
                        session,
                        ca,
                        reader: Some(reader),
                        writer: None,
                        reply: None,
                        last_tick: Instant::now(),
                    };
                    self.emit(FileTransferEvent::Started {
                        peer: ca,
                        nof,
                        role: Role::Sender,
                    });
                    self.sessions.insert(key, active);
                }
                _ => return,
            }
        }
        let input = match decode_ft_asdu(&asdu) {
            Some(i) => i,
            None => return,
        };
        let active = self.sessions.get_mut(&key).expect("session inserted above");
        active.last_tick = Instant::now();
        let acts = active.session.step(input, active.last_tick);
        self.apply_actions(key, acts).await;
    }

    async fn on_tick(&mut self) {
        let now = Instant::now();
        let keys: Vec<_> = self.sessions.keys().cloned().collect();
        for key in keys {
            let acts = match self.sessions.get_mut(&key) {
                Some(s) => s.session.step(SessionInput::Tick, now),
                None => continue,
            };
            if !acts.is_empty() {
                self.apply_actions(key, acts).await;
            }
        }
    }

    async fn apply_actions(
        &mut self,
        key: (CommonAddress, NameOfFile),
        actions: Vec<SessionAction>,
    ) {
        // The session never produces nested actions on its own — but
        // `RequestNextSegment` triggers a `pump_reader` call that itself
        // steps the session and yields more actions. We keep a flat work
        // queue to avoid async recursion.
        let mut queue: std::collections::VecDeque<SessionAction> = actions.into_iter().collect();
        while let Some(action) = queue.pop_front() {
            match action {
                SessionAction::SendFileReady(a) => self.send(key.0, &a).await,
                SessionAction::SendSectionReady(a) => self.send(key.0, &a).await,
                SessionAction::SendSelectCall(a) => self.send(key.0, &a).await,
                SessionAction::SendLastSection(a) => self.send(key.0, &a).await,
                SessionAction::SendAckFile(a) => self.send(key.0, &a).await,
                SessionAction::SendSegment(a) => self.send(key.0, &a).await,
                SessionAction::RequestNextSegment { max_bytes } => {
                    let extra = self.pump_reader(key, max_bytes).await;
                    for a in extra {
                        queue.push_back(a);
                    }
                }
                SessionAction::DeliverSegment(data) => {
                    self.deliver_segment(key, data).await;
                }
                SessionAction::Completed { bytes } => {
                    self.finalize_session(key, FileTransferOutcome::Completed { bytes })
                        .await;
                }
                SessionAction::Failed(reason) => {
                    self.finalize_session(key, FileTransferOutcome::Failed(reason))
                        .await;
                }
            }
        }
    }

    async fn send<P: AsduPayload>(&mut self, ca: CommonAddress, payload: &P) {
        let asdu = Asdu::from_payload(
            Cot::with(Cause::FILE_TRANSFER),
            ca,
            Vsq::single(1),
            payload,
            AsduAddressing::IEC104,
        );
        let mut buf = BytesMut::new();
        asdu.encode(&mut buf, AsduAddressing::IEC104);
        let _ = self.cmd_tx.send(Command::SendAsdu(buf.to_vec())).await;
    }

    async fn pump_reader(
        &mut self,
        key: (CommonAddress, NameOfFile),
        max_bytes: usize,
    ) -> Vec<SessionAction> {
        let active = match self.sessions.get_mut(&key) {
            Some(s) => s,
            None => return Vec::new(),
        };
        let chunk = match active.reader.as_mut() {
            Some(r) => match r.read_segment(max_bytes).await {
                Ok(Some(c)) => c,
                Ok(None) => Vec::new(),
                Err(e) => {
                    tracing::warn!(target: "iec60870::ft", "reader error: {e:?}");
                    Vec::new()
                }
            },
            None => return Vec::new(),
        };
        let now = Instant::now();
        active.session.step(SessionInput::SegmentReady(chunk), now)
    }

    async fn deliver_segment(&mut self, key: (CommonAddress, NameOfFile), data: Vec<u8>) {
        let active = match self.sessions.get_mut(&key) {
            Some(s) => s,
            None => return,
        };
        if let Some(writer) = active.writer.as_mut() {
            if let Err(e) = writer.write_segment(&data).await {
                tracing::warn!(target: "iec60870::ft", "writer error: {e:?}");
            }
        }
        let _ = active;
        self.emit(FileTransferEvent::SegmentTransferred {
            peer: key.0,
            nof: key.1,
            bytes_total: data.len() as u32,
        });
    }

    async fn finalize_session(
        &mut self,
        key: (CommonAddress, NameOfFile),
        outcome: FileTransferOutcome,
    ) {
        let mut active = match self.sessions.remove(&key) {
            Some(s) => s,
            None => return,
        };
        let success = matches!(outcome, FileTransferOutcome::Completed { .. });
        if let Some(writer) = active.writer.take() {
            if let Err(e) = writer.finalize(success).await {
                tracing::warn!(target: "iec60870::ft", "finalize error: {e:?}");
            }
        }
        // Drop reader implicitly.
        active.reader.take();
        let result = match outcome {
            FileTransferOutcome::Completed { bytes } => Ok(bytes),
            FileTransferOutcome::Failed(reason) => Err(failure_to_error(reason)),
        };
        if let Some(reply) = active.reply.take() {
            let _ = reply.send(result);
        }
        self.emit(FileTransferEvent::Finished {
            peer: key.0,
            nof: key.1,
            outcome,
        });
    }

    fn emit(&self, ev: FileTransferEvent) {
        let _ = self.events.send(ev);
    }
}

fn failure_to_error(reason: FailureReason) -> FileTransferError {
    match reason {
        FailureReason::Timeout => FileTransferError::Other("idle timeout".into()),
        FailureReason::PeerRejected => FileTransferError::Other("peer rejected transfer".into()),
        FailureReason::LocallyRejected => {
            FileTransferError::Other("locally rejected transfer".into())
        }
        FailureReason::ChecksumMismatch => FileTransferError::ChecksumMismatch,
        FailureReason::ProtocolViolation => FileTransferError::Other("protocol violation".into()),
        FailureReason::Aborted => FileTransferError::Other("aborted".into()),
    }
}

/// Quick peek at the NOF field of a file-transfer ASDU without decoding the
/// whole payload — used for routing to the right session.
fn peek_nof(asdu: &Asdu) -> Option<NameOfFile> {
    let bytes = asdu.payload_bytes();
    // Layout for FT ASDUs is `IOA(3) | NOF(2) | ...` under IEC104 addressing.
    if bytes.len() < 5 {
        return None;
    }
    Some(NameOfFile(u16::from_le_bytes([bytes[3], bytes[4]])))
}

fn decode_ft_asdu(asdu: &Asdu) -> Option<SessionInput> {
    let addressing = AsduAddressing::IEC104;
    Some(match asdu.type_id() {
        120 => SessionInput::FileReady(asdu.decode_payload::<F_FR_NA_1>(addressing).ok()?),
        121 => SessionInput::SectionReady(asdu.decode_payload::<F_SR_NA_1>(addressing).ok()?),
        122 => SessionInput::SelectCall(asdu.decode_payload::<F_SC_NA_1>(addressing).ok()?),
        123 => SessionInput::LastSection(asdu.decode_payload::<F_LS_NA_1>(addressing).ok()?),
        124 => SessionInput::AckFile(asdu.decode_payload::<F_AF_NA_1>(addressing).ok()?),
        125 => SessionInput::Segment(asdu.decode_payload::<F_SG_NA_1>(addressing).ok()?),
        _ => return None,
    })
}
