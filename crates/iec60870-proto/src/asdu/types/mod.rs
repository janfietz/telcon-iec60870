//! Built-in typed ASDU payloads.
//!
//! Each submodule implements one Type ID per IEC 60870-5-101 §7.3 by providing
//! a struct that implements [`crate::asdu::AsduPayload`].

mod monitor;

pub use monitor::{
    M_DP_NA_1, M_DP_TB_1, M_ME_NA_1, M_ME_NB_1, M_ME_NC_1, M_ME_TD_1, M_ME_TE_1, M_ME_TF_1,
    M_SP_NA_1, M_SP_TB_1,
};
