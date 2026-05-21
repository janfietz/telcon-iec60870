//! JSON request, response, and event shapes exchanged on the control socket.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// One ASDU type we care about for the test rig's point store. Maps 1:1 to the
/// IEC 60870-5 `TypeID` namespace.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum PointKind {
    /// M_SP_NA_1 (TypeID 1) — single-point information.
    SpNa,
    /// M_DP_NA_1 (TypeID 3) — double-point information.
    DpNa,
    /// M_ME_NA_1 (TypeID 9) — measured value, normalized.
    MeNa,
    /// M_ME_NB_1 (TypeID 11) — measured value, scaled.
    MeNb,
    /// M_ME_NC_1 (TypeID 13) — measured value, short float.
    MeNc,
    /// M_SP_TB_1 (TypeID 30) — single-point + CP56Time2a.
    SpTb,
    /// M_DP_TB_1 (TypeID 31) — double-point + CP56Time2a.
    DpTb,
    /// M_ME_TD_1 (TypeID 34) — normalized + CP56Time2a.
    MeTd,
    /// M_ME_TE_1 (TypeID 35) — scaled + CP56Time2a.
    MeTe,
    /// M_ME_TF_1 (TypeID 36) — float + CP56Time2a.
    MeTf,
}

impl PointKind {
    pub const ALL: [PointKind; 10] = [
        PointKind::SpNa,
        PointKind::DpNa,
        PointKind::MeNa,
        PointKind::MeNb,
        PointKind::MeNc,
        PointKind::SpTb,
        PointKind::DpTb,
        PointKind::MeTd,
        PointKind::MeTe,
        PointKind::MeTf,
    ];

    pub const fn type_id(self) -> u8 {
        match self {
            PointKind::SpNa => 1,
            PointKind::DpNa => 3,
            PointKind::MeNa => 9,
            PointKind::MeNb => 11,
            PointKind::MeNc => 13,
            PointKind::SpTb => 30,
            PointKind::DpTb => 31,
            PointKind::MeTd => 34,
            PointKind::MeTe => 35,
            PointKind::MeTf => 36,
        }
    }

    pub fn from_type_id(t: u8) -> Option<Self> {
        Self::ALL.iter().copied().find(|k| k.type_id() == t)
    }

    pub const fn mnemonic(self) -> &'static str {
        match self {
            PointKind::SpNa => "M_SP_NA_1",
            PointKind::DpNa => "M_DP_NA_1",
            PointKind::MeNa => "M_ME_NA_1",
            PointKind::MeNb => "M_ME_NB_1",
            PointKind::MeNc => "M_ME_NC_1",
            PointKind::SpTb => "M_SP_TB_1",
            PointKind::DpTb => "M_DP_TB_1",
            PointKind::MeTd => "M_ME_TD_1",
            PointKind::MeTe => "M_ME_TE_1",
            PointKind::MeTf => "M_ME_TF_1",
        }
    }

    pub const fn has_timestamp(self) -> bool {
        matches!(
            self,
            PointKind::SpTb
                | PointKind::DpTb
                | PointKind::MeTd
                | PointKind::MeTe
                | PointKind::MeTf
        )
    }
}

/// Untagged double-point state. Serializes as a lowercase string.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum DoublePointWire {
    #[default]
    Intermediate,
    Off,
    On,
    Indeterminate,
}

/// JSON-friendly point value. Discriminated by `kind` so the wire format is
/// unambiguous (e.g. `{"kind": "float", "value": 42.5}`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum PointValue {
    Single(bool),
    Double(DoublePointWire),
    Normalized(f32),
    Scaled(i16),
    Float(f32),
}

/// Quality bits as carried on the wire. All flags default to `false`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct QualityWire {
    #[serde(default)]
    pub overflow: bool,
    #[serde(default)]
    pub blocked: bool,
    #[serde(default)]
    pub substituted: bool,
    #[serde(default)]
    pub not_topical: bool,
    #[serde(default)]
    pub invalid: bool,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StepDir {
    Lower,
    Higher,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SetpointKind {
    Normalized,
    Scaled,
    Float,
}

/// Simulator schedule for one point. `None` disables autonomous evolution
/// (the value is held until set explicitly via the control socket).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SimSchedule {
    /// Hold the current value indefinitely.
    None,
    /// Flip a single-point value every `interval_ms`. Phase offset is applied
    /// per-IOA so a range doesn't all toggle in lockstep.
    Toggle {
        interval_ms: u64,
        #[serde(default)]
        phase_ms: u64,
    },
    /// Rotate a double-point value Off → On → Off every `interval_ms`.
    Rotate { interval_ms: u64 },
    /// Random-walk a float-valued point by `step` per tick, clamped to
    /// `[min, max]`.
    RandomWalk {
        interval_ms: u64,
        step: f32,
        min: f32,
        max: f32,
    },
    /// Integer step-up by `step` every tick, wrapping at `wrap_at`.
    StepUp {
        interval_ms: u64,
        step: i32,
        wrap_at: i32,
    },
    /// Sinusoid sampled every `interval_ms`; one full period over `period_ms`.
    Sine {
        interval_ms: u64,
        period_ms: u64,
        amplitude: f32,
        offset: f32,
    },
}

/// Control-plane request. Discriminated by `op`.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    // -- server side --
    Get {
        ioa: u32,
    },
    Set {
        ioa: u32,
        value: PointValue,
        #[serde(default)]
        quality: Option<QualityWire>,
    },
    List {
        #[serde(default)]
        type_id: Option<u8>,
    },
    SimGet {
        ioa: u32,
    },
    SimSet {
        ioa: u32,
        schedule: SimSchedule,
    },

    // -- client side --
    Interrogate {
        #[serde(default)]
        group: Option<u8>,
        #[serde(default)]
        ca: Option<u16>,
        #[serde(default)]
        timeout_ms: Option<u64>,
    },
    CmdSingle {
        ioa: u32,
        on: bool,
        #[serde(default)]
        ca: Option<u16>,
    },
    CmdDouble {
        ioa: u32,
        on: bool,
        #[serde(default)]
        ca: Option<u16>,
    },
    CmdRegulating {
        ioa: u32,
        step: StepDir,
        #[serde(default)]
        ca: Option<u16>,
    },
    CmdSetpoint {
        ioa: u32,
        kind: SetpointKind,
        value: f64,
        #[serde(default)]
        ca: Option<u16>,
    },
    Read {
        ioa: u32,
        #[serde(default)]
        type_id: Option<u8>,
    },
    FileGet {
        nof: u16,
        out: PathBuf,
        #[serde(default)]
        ca: Option<u16>,
    },
    FilePut {
        nof: u16,
        input: PathBuf,
        #[serde(default)]
        ca: Option<u16>,
    },

    // -- shared --
    Status,
    Shutdown,
    /// Open a long-lived subscription. The daemon responds with an initial
    /// `{"ok": true}` and then streams `Event` objects, one per line, until
    /// the connection is closed.
    Events,
}

/// Control-plane response. Successful responses set `ok = true` and may
/// carry arbitrary additional fields under `data`; failures set `ok = false`
/// and populate `error`.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Response {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub error: Option<String>,
    #[serde(flatten, default = "serde_json::Map::new")]
    pub data: serde_json::Map<String, serde_json::Value>,
}

impl Response {
    pub fn ok(data: serde_json::Value) -> Self {
        let data = match data {
            serde_json::Value::Object(map) => map,
            other => {
                let mut m = serde_json::Map::new();
                m.insert("result".into(), other);
                m
            }
        };
        Self {
            ok: true,
            error: None,
            data,
        }
    }

    pub fn ok_empty() -> Self {
        Self {
            ok: true,
            error: None,
            data: serde_json::Map::new(),
        }
    }

    pub fn err(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(message.into()),
            data: serde_json::Map::new(),
        }
    }
}

/// One asynchronous notification streamed on an `Events` subscription.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// The daemon emitted an ASDU (either spontaneous, interrogated, or
    /// command confirmation). `value` is the typed payload as JSON.
    AsduSent {
        cot: String,
        type_id: u8,
        ioa: u32,
        value: serde_json::Value,
    },
    /// The daemon received an ASDU from its peer.
    AsduReceived {
        cot: String,
        type_id: u8,
        ioa: u32,
        value: serde_json::Value,
    },
    /// Underlying transport state transitioned (e.g. STARTDT_CON for 104,
    /// LinkState::Ready for 101).
    StateChanged {
        state: String,
    },
    /// Simulator updated a point internally.
    SimTick {
        ioa: u32,
        kind: String,
    },
    Connected,
    Disconnected {
        #[serde(default)]
        reason: Option<String>,
    },
}
