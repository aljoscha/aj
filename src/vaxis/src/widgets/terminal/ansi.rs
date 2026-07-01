//! ANSI value types shared across the emulator: the `C0` control table, the
//! parsed `CSI` sequence, and the parameter iterator that both the SGR machine
//! in [`crate::widgets::terminal::screen`] and the CSI dispatch consume.

use std::fmt;
use std::marker::PhantomData;

/// C0 control bytes. See `man 7 ascii`.
///
/// The variant names are the standard ASCII abbreviations (`Nul`, `Bel`, ...)
/// spelled in upper camel case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum C0 {
    Nul = 0x00,
    Soh = 0x01,
    Stx = 0x02,
    Etx = 0x03,
    Eot = 0x04,
    Enq = 0x05,
    Ack = 0x06,
    Bel = 0x07,
    Bs = 0x08,
    Ht = 0x09,
    Lf = 0x0a,
    Vt = 0x0b,
    Ff = 0x0c,
    Cr = 0x0d,
    So = 0x0e,
    Si = 0x0f,
    Dle = 0x10,
    Dc1 = 0x11,
    Dc2 = 0x12,
    Dc3 = 0x13,
    Dc4 = 0x14,
    Nak = 0x15,
    Syn = 0x16,
    Etb = 0x17,
    Can = 0x18,
    Em = 0x19,
    Sub = 0x1a,
    Esc = 0x1b,
    Fs = 0x1c,
    Gs = 0x1d,
    Rs = 0x1e,
    Us = 0x1f,
}

impl C0 {
    /// Maps a byte to its C0 control, or `None` when the byte is not a control
    /// byte (`> 0x1f`).
    pub fn from_u8(b: u8) -> Option<C0> {
        let c0 = match b {
            0x00 => C0::Nul,
            0x01 => C0::Soh,
            0x02 => C0::Stx,
            0x03 => C0::Etx,
            0x04 => C0::Eot,
            0x05 => C0::Enq,
            0x06 => C0::Ack,
            0x07 => C0::Bel,
            0x08 => C0::Bs,
            0x09 => C0::Ht,
            0x0a => C0::Lf,
            0x0b => C0::Vt,
            0x0c => C0::Ff,
            0x0d => C0::Cr,
            0x0e => C0::So,
            0x0f => C0::Si,
            0x10 => C0::Dle,
            0x11 => C0::Dc1,
            0x12 => C0::Dc2,
            0x13 => C0::Dc3,
            0x14 => C0::Dc4,
            0x15 => C0::Nak,
            0x16 => C0::Syn,
            0x17 => C0::Etb,
            0x18 => C0::Can,
            0x19 => C0::Em,
            0x1a => C0::Sub,
            0x1b => C0::Esc,
            0x1c => C0::Fs,
            0x1d => C0::Gs,
            0x1e => C0::Rs,
            0x1f => C0::Us,
            _ => return None,
        };
        Some(c0)
    }
}

/// A parsed CSI (Control Sequence Introducer) sequence.
///
/// `params` holds the raw parameter bytes (ASCII digits plus the `;` and `:`
/// separators) exactly as they arrived. Decode them through [`Csi::iterator`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Csi {
    pub intermediate: Option<u8>,
    pub private_marker: Option<u8>,
    /// The final byte in the range `0x40..=0xff` that ends the sequence.
    pub final_byte: u8,
    pub params: Vec<u8>,
}

impl Csi {
    /// True when `b` equals the collected intermediate byte.
    pub fn has_intermediate(&self, b: u8) -> bool {
        self.intermediate == Some(b)
    }

    /// True when `b` equals the collected private-marker byte.
    pub fn has_private_marker(&self, b: u8) -> bool {
        self.private_marker == Some(b)
    }

    /// Returns an iterator over the numeric parameters, parsed into `T`.
    ///
    /// `T` is chosen at the call site: the SGR machine uses `u8`, the CSI
    /// dispatch uses `u16`.
    pub fn iterator<T: ParamInt>(&self) -> ParamIterator<'_, T> {
        ParamIterator::new(self.params.as_slice())
    }
}

impl fmt::Display for Csi {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let params = String::from_utf8_lossy(&self.params);
        let final_ch = char::from(self.final_byte);
        match (self.private_marker, self.intermediate) {
            (None, None) => write!(f, "CSI {params} {final_ch}"),
            (Some(pm), None) => write!(f, "CSI {} {params} {final_ch}", char::from(pm)),
            (None, Some(i)) => write!(f, "CSI {params} {} {final_ch}", char::from(i)),
            (Some(pm), Some(i)) => write!(
                f,
                "CSI {} {params} {} {final_ch}",
                char::from(pm),
                char::from(i)
            ),
        }
    }
}

/// An unsigned integer a [`ParamIterator`] can accumulate decimal digits into.
///
/// Implemented for the two widths the emulator parses parameters as: `u8` for
/// SGR and `u16` for CSI dispatch.
pub trait ParamInt: Copy {
    const ZERO: Self;

    /// Returns `self * 10 + digit`.
    ///
    /// NOTE: We wrap on overflow to mirror upstream's release-mode arithmetic.
    /// Well-formed parameters never overflow the chosen width.
    fn accumulate(self, digit: u8) -> Self;
}

impl ParamInt for u8 {
    const ZERO: Self = 0;

    fn accumulate(self, digit: u8) -> u8 {
        self.wrapping_mul(10).wrapping_add(digit)
    }
}

impl ParamInt for u16 {
    const ZERO: Self = 0;

    fn accumulate(self, digit: u8) -> u16 {
        self.wrapping_mul(10).wrapping_add(u16::from(digit))
    }
}

/// Iterates the numeric parameters of a CSI sequence.
///
/// Parameters are separated by `;`, and sub-parameters by `:`. After each
/// [`ParamIterator::next`] the two public flags describe the separator that
/// ended the parameter just returned: [`ParamIterator::next_is_sub`] is set
/// when it was a `:` (so the next parameter is a sub-parameter of this one),
/// and [`ParamIterator::is_empty`] is set when the parameter itself was the
/// empty string (as in the `::` of `38:2::r:g:b`). The SGR machine reads both
/// flags to decode colored underlines and RGB colors.
pub struct ParamIterator<'a, T: ParamInt> {
    bytes: &'a [u8],
    idx: usize,
    /// True when the parameter just returned was terminated by `:`.
    pub next_is_sub: bool,
    /// True when the parameter just returned was the empty string.
    pub is_empty: bool,
    _marker: PhantomData<T>,
}

impl<'a, T: ParamInt> ParamIterator<'a, T> {
    fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            idx: 0,
            next_is_sub: false,
            is_empty: false,
            _marker: PhantomData,
        }
    }

    /// Returns the next parameter, or `None` when the parameters are exhausted
    /// or a non-numeric, non-separator byte is hit.
    pub fn next(&mut self) -> Option<T> {
        self.next_is_sub = false;
        self.is_empty = false;

        let start = self.idx;
        let mut val = T::ZERO;
        while self.idx < self.bytes.len() {
            // `cur` is the current byte's index. We advance `self.idx` past it
            // before returning, mirroring upstream's `defer self.idx += 1`.
            let cur = self.idx;
            let b = self.bytes[cur];
            match b {
                0x30..=0x39 => {
                    val = val.accumulate(b - 0x30);
                    self.idx = cur + 1;
                    if cur == self.bytes.len() - 1 {
                        return Some(val);
                    }
                }
                b':' | b';' => {
                    self.next_is_sub = b == b':';
                    self.is_empty = cur == start;
                    self.idx = cur + 1;
                    return Some(val);
                }
                _ => {
                    self.idx = cur + 1;
                    return None;
                }
            }
        }
        None
    }

    /// Returns true when at least `n` more parameters remain.
    ///
    /// Restores the read position afterward, so it is a pure lookahead over the
    /// count. NOTE: it does not restore [`ParamIterator::next_is_sub`] or
    /// [`ParamIterator::is_empty`], matching upstream, so a following `next`
    /// starts with those flags cleared regardless.
    pub fn has_at_least(&mut self, n: usize) -> bool {
        let start = self.idx;
        let mut i = 0;
        while self.next().is_some() {
            i += 1;
            if i >= n {
                break;
            }
        }
        self.idx = start;
        i >= n
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect_u8(params: &[u8]) -> Vec<u8> {
        let csi = Csi {
            intermediate: None,
            private_marker: None,
            final_byte: b'm',
            params: params.to_vec(),
        };
        let mut iter = csi.iterator::<u8>();
        let mut out = Vec::new();
        while let Some(v) = iter.next() {
            out.push(v);
        }
        out
    }

    #[test]
    fn param_iterator_semicolons() {
        assert_eq!(collect_u8(b"1;2;3"), vec![1, 2, 3]);
    }

    #[test]
    fn param_iterator_single_value() {
        assert_eq!(collect_u8(b"38"), vec![38]);
    }

    #[test]
    fn param_iterator_sub_params_and_empty() {
        // Walk "38:2::10:20:30" and assert the values plus the sub-param and
        // empty flags after each step. The "::" yields one empty parameter.
        let csi = Csi {
            intermediate: None,
            private_marker: None,
            final_byte: b'm',
            params: b"38:2::10:20:30".to_vec(),
        };
        let mut iter = csi.iterator::<u8>();

        assert_eq!(iter.next(), Some(38));
        assert!(iter.next_is_sub);
        assert!(!iter.is_empty);

        assert_eq!(iter.next(), Some(2));
        assert!(iter.next_is_sub);
        assert!(!iter.is_empty);

        // The empty parameter between the two colons.
        assert_eq!(iter.next(), Some(0));
        assert!(iter.next_is_sub);
        assert!(iter.is_empty);

        assert_eq!(iter.next(), Some(10));
        assert!(iter.next_is_sub);
        assert!(!iter.is_empty);

        assert_eq!(iter.next(), Some(20));
        assert!(iter.next_is_sub);

        assert_eq!(iter.next(), Some(30));
        assert_eq!(iter.next(), None);
    }

    #[test]
    fn param_iterator_leading_empty() {
        let csi = Csi {
            intermediate: None,
            private_marker: None,
            final_byte: b'm',
            params: b";5".to_vec(),
        };
        let mut iter = csi.iterator::<u8>();
        assert_eq!(iter.next(), Some(0));
        assert!(iter.is_empty);
        assert!(!iter.next_is_sub);
        assert_eq!(iter.next(), Some(5));
        assert_eq!(iter.next(), None);
    }

    #[test]
    fn param_iterator_empty_params() {
        assert_eq!(collect_u8(b""), Vec::<u8>::new());
    }

    #[test]
    fn param_iterator_u16_widens() {
        let csi = Csi {
            intermediate: None,
            private_marker: None,
            final_byte: b'H',
            params: b"1000;2".to_vec(),
        };
        let mut iter = csi.iterator::<u16>();
        assert_eq!(iter.next(), Some(1000u16));
        assert_eq!(iter.next(), Some(2u16));
        assert_eq!(iter.next(), None);
    }

    #[test]
    fn param_iterator_has_at_least() {
        let csi = Csi {
            intermediate: None,
            private_marker: None,
            final_byte: b'm',
            params: b"1;2;3".to_vec(),
        };
        let mut iter = csi.iterator::<u8>();
        assert!(iter.has_at_least(3));
        assert!(!iter.has_at_least(4));
        // Read position is restored, so we still see all three.
        assert_eq!(iter.next(), Some(1));
        assert_eq!(iter.next(), Some(2));
        assert_eq!(iter.next(), Some(3));
    }

    #[test]
    fn c0_mapping() {
        assert_eq!(C0::from_u8(0x00), Some(C0::Nul));
        assert_eq!(C0::from_u8(0x07), Some(C0::Bel));
        assert_eq!(C0::from_u8(0x0a), Some(C0::Lf));
        assert_eq!(C0::from_u8(0x1b), Some(C0::Esc));
        assert_eq!(C0::from_u8(0x1f), Some(C0::Us));
        assert_eq!(C0::from_u8(0x20), None);
        assert_eq!(C0::from_u8(0xff), None);
    }

    #[test]
    fn csi_marker_queries() {
        let csi = Csi {
            intermediate: Some(b' '),
            private_marker: Some(b'?'),
            final_byte: b'h',
            params: b"1049".to_vec(),
        };
        assert!(csi.has_private_marker(b'?'));
        assert!(!csi.has_private_marker(b'>'));
        assert!(csi.has_intermediate(b' '));
        assert!(!csi.has_intermediate(b'!'));
    }

    #[test]
    fn csi_display() {
        let plain = Csi {
            intermediate: None,
            private_marker: None,
            final_byte: b'H',
            params: b"1;2".to_vec(),
        };
        assert_eq!(plain.to_string(), "CSI 1;2 H");

        let private = Csi {
            intermediate: None,
            private_marker: Some(b'?'),
            final_byte: b'h',
            params: b"1049".to_vec(),
        };
        assert_eq!(private.to_string(), "CSI ? 1049 h");
    }
}
