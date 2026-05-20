//! 15-bit sequence number with wrapping arithmetic.

/// A 15-bit sequence number (`N(S)` or `N(R)`) used in I- and S-frame APCIs.
///
/// Values are always in the range `0..=32_767`; arithmetic wraps modulo 2^15.
/// On the wire the value is packed as the upper 15 bits of a little-endian
/// u16 — i.e. `wire = value << 1`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SeqNo(u16);

impl SeqNo {
    pub const MAX: u16 = (1 << 15) - 1;
    pub const MODULUS: u32 = 1 << 15;

    /// Construct from a raw u16, masking down to 15 bits.
    pub const fn new(v: u16) -> Self {
        Self(v & Self::MAX)
    }

    /// Underlying 15-bit value.
    pub const fn value(self) -> u16 {
        self.0
    }

    /// Wrapping addition modulo 2^15.
    pub const fn add(self, n: u16) -> Self {
        Self(self.0.wrapping_add(n) & Self::MAX)
    }

    /// Wrapping increment.
    pub const fn next(self) -> Self {
        self.add(1)
    }

    /// Number of outstanding frames between `self` and `later`, modulo 2^15.
    /// Returns `(later - self) mod 2^15`.
    pub const fn distance(self, later: Self) -> u16 {
        (later.0 + Self::MODULUS as u16 - self.0) & Self::MAX
    }

    /// Pack into the 16-bit little-endian wire form (`value << 1`).
    pub const fn to_wire(self) -> u16 {
        self.0 << 1
    }

    /// Recover from the 16-bit wire form: shifts down by 1, masking off the
    /// type bit. The caller must already have checked the format discriminator.
    pub const fn from_wire(w: u16) -> Self {
        Self((w >> 1) & Self::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn wraps_at_2_15() {
        let s = SeqNo::new(SeqNo::MAX);
        assert_eq!(s.next(), SeqNo::new(0));
    }

    #[test]
    fn wire_form_shifts_left_by_one() {
        let s = SeqNo::new(0x1234);
        assert_eq!(s.to_wire(), 0x1234 << 1);
        assert_eq!(SeqNo::from_wire(s.to_wire()), s);
    }

    #[test]
    fn distance_handles_wrap() {
        let a = SeqNo::new(SeqNo::MAX - 2);
        let b = SeqNo::new(3);
        // a is 5 steps behind b (32765, 32766, 32767, 0, 1, 2, 3)
        assert_eq!(a.distance(b), 6);
    }

    #[test]
    fn new_masks_input() {
        // SeqNo is only 15 bits; the constructor must mask off bit 15.
        let s = SeqNo::new(0xFFFF);
        assert_eq!(s.value(), SeqNo::MAX);
    }

    proptest! {
        #[test]
        fn prop_wire_roundtrip(v in 0u16..=SeqNo::MAX) {
            let s = SeqNo::new(v);
            prop_assert_eq!(SeqNo::from_wire(s.to_wire()), s);
        }

        #[test]
        fn prop_add_wraps(v in 0u16..=SeqNo::MAX, n in 0u16..=SeqNo::MAX) {
            let s = SeqNo::new(v);
            let r = s.add(n);
            prop_assert!(r.value() <= SeqNo::MAX);
            prop_assert_eq!(r.value(), ((v as u32 + n as u32) % SeqNo::MODULUS) as u16);
        }
    }
}
