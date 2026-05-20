//! Helpers for encoding and decoding the information-objects section
//! of an ASDU, common to every Type ID.
//!
//! The shape of an information-objects section depends on the SQ bit of
//! the VSQ:
//!
//! * `SQ = 0` — each object carries its own IOA, followed by the type-specific
//!   information element(s).
//! * `SQ = 1` — only the *first* object carries an IOA; subsequent objects
//!   are implicitly addressed at `IOA + i` and only the information element
//!   is on the wire.

use bytes::{Buf, BufMut};

use crate::asdu::header::{decode_ioa, encode_ioa, AsduAddressing, Ioa, Vsq};
use crate::error::Result;

/// Encode a `[(Ioa, E)]` slice using the SQ rule.
pub(crate) fn encode_io_list<B, E, F>(
    buf: &mut B,
    objects: &[(Ioa, E)],
    vsq: Vsq,
    addressing: AsduAddressing,
    mut encode_element: F,
) where
    B: BufMut,
    F: FnMut(&mut B, &E),
{
    if vsq.sequence {
        if let Some((ioa, first)) = objects.first() {
            encode_ioa(buf, *ioa, addressing.ioa_size);
            encode_element(buf, first);
            for (_, e) in &objects[1..] {
                encode_element(buf, e);
            }
        }
    } else {
        for (ioa, e) in objects {
            encode_ioa(buf, *ioa, addressing.ioa_size);
            encode_element(buf, e);
        }
    }
}

/// Decode `vsq.count` information objects into `Vec<(Ioa, E)>` using the SQ rule.
pub(crate) fn decode_io_list<B, E, F>(
    buf: &mut B,
    vsq: Vsq,
    addressing: AsduAddressing,
    mut decode_element: F,
) -> Result<Vec<(Ioa, E)>>
where
    B: Buf,
    F: FnMut(&mut B) -> Result<E>,
{
    let count = vsq.count as usize;
    let mut objects = Vec::with_capacity(count);
    if vsq.sequence {
        if count == 0 {
            return Ok(objects);
        }
        let base = decode_ioa(buf, addressing.ioa_size)?;
        for i in 0..count {
            let e = decode_element(buf)?;
            objects.push((Ioa(base.0.wrapping_add(i as u32)), e));
        }
    } else {
        for _ in 0..count {
            let ioa = decode_ioa(buf, addressing.ioa_size)?;
            let e = decode_element(buf)?;
            objects.push((ioa, e));
        }
    }
    Ok(objects)
}
