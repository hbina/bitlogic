//! Multi-word unsigned arithmetic shared by [`Bits`](crate::Bits), the IR
//! interpreter and [`BitVec`], the run-time form of leaf and port values.
//!
//! A value of width `w` occupies `words_for(w)` little-endian 64-bit words;
//! bits at and above `w` are zero. Operand slices may be shorter than the
//! result (missing words read as zero), so trimmed [`BitVec`] words and
//! fixed-size storage mix freely.
use std::cmp::Ordering;
use std::fmt;

/// The widest value: leaves, ports, IR nodes and `Bits<N>`.
pub const MAX_WIDTH: u16 = 4096;

/// 64-bit words needed for `width` bits.
pub const fn words_for(width: usize) -> usize {
    width.div_ceil(64)
}

fn word(words: &[u64], index: usize) -> u64 {
    words.get(index).copied().unwrap_or(0)
}

/// Clear the bits at and above `width`.
pub(crate) fn mask_top(words: &mut [u64], width: usize) {
    for (i, w) in words.iter_mut().enumerate() {
        let low = i * 64;
        if low >= width {
            *w = 0;
        } else if width - low < 64 {
            *w &= u64::MAX >> (64 - (width - low));
        }
    }
}

/// Whether `words` has no set bit at or above `width`.
pub(crate) fn fits(words: &[u64], width: usize) -> bool {
    words.iter().enumerate().all(|(i, &w)| {
        let low = i * 64;
        if low >= width {
            w == 0
        } else if width - low < 64 {
            w >> (width - low) == 0
        } else {
            true
        }
    })
}

pub(crate) fn copy(out: &mut [u64], a: &[u64]) {
    for (i, o) in out.iter_mut().enumerate() {
        *o = word(a, i);
    }
}

pub(crate) fn not(out: &mut [u64], a: &[u64], width: usize) {
    for (i, o) in out.iter_mut().enumerate() {
        *o = !word(a, i);
    }
    mask_top(out, width);
}

pub(crate) fn bitwise(out: &mut [u64], a: &[u64], b: &[u64], f: impl Fn(u64, u64) -> u64) {
    for (i, o) in out.iter_mut().enumerate() {
        *o = f(word(a, i), word(b, i));
    }
}

/// `a + b` (or `a - b`), wrapping at `width` bits.
pub(crate) fn add(out: &mut [u64], a: &[u64], b: &[u64], width: usize, subtract: bool) {
    let mut carry = subtract;
    for (i, o) in out.iter_mut().enumerate() {
        let b = if subtract { !word(b, i) } else { word(b, i) };
        let (sum, c1) = word(a, i).overflowing_add(b);
        let (sum, c2) = sum.overflowing_add(u64::from(carry));
        *o = sum;
        carry = c1 || c2;
    }
    mask_top(out, width);
}

pub(crate) fn compare(a: &[u64], b: &[u64]) -> Ordering {
    let n = a.len().max(b.len());
    (0..n)
        .rev()
        .map(|i| word(a, i).cmp(&word(b, i)))
        .find(|o| o.is_ne())
        .unwrap_or(Ordering::Equal)
}

/// Bits `offset..` of `a`, into `out`, cut to `width` bits.
pub(crate) fn shr(out: &mut [u64], a: &[u64], offset: usize, width: usize) {
    let (skip, shift) = (offset / 64, offset % 64);
    for (i, o) in out.iter_mut().enumerate() {
        let low = word(a, i + skip) >> shift;
        let high = if shift == 0 {
            0
        } else {
            word(a, i + skip + 1) << (64 - shift)
        };
        *o = low | high;
    }
    mask_top(out, width);
}

/// `a << amount`, cut to `width` bits.
pub(crate) fn shl(out: &mut [u64], a: &[u64], amount: usize, width: usize) {
    let (skip, shift) = (amount / 64, amount % 64);
    for (i, o) in out.iter_mut().enumerate() {
        *o = if i < skip {
            0
        } else {
            let low = word(a, i - skip) << shift;
            let carried = if shift == 0 || i == skip {
                0
            } else {
                word(a, i - skip - 1) >> (64 - shift)
            };
            low | carried
        };
    }
    mask_top(out, width);
}

/// `{high, low}` where `low` is `low_width` bits wide, cut to `width` bits.
pub(crate) fn concat(out: &mut [u64], high: &[u64], low: &[u64], low_width: usize, width: usize) {
    shl(out, high, low_width, width);
    for (i, o) in out.iter_mut().enumerate() {
        *o |= word(low, i);
    }
    mask_top(out, width);
}

pub(crate) fn is_zero(a: &[u64]) -> bool {
    a.iter().all(|&w| w == 0)
}

/// Hexadecimal digits without leading zeros (`0` for zero).
pub(crate) fn hex(a: &[u64]) -> String {
    let top = a.iter().rposition(|&w| w != 0);
    match top {
        None => "0".into(),
        Some(top) => {
            let mut text = format!("{:x}", a[top]);
            for w in a[..top].iter().rev() {
                text.push_str(&format!("{w:016x}"));
            }
            text
        }
    }
}

/// An unsigned value of up to [`MAX_WIDTH`] bits whose width is known only at
/// run time: a leaf of a flattened value, a port value, or an IR constant.
/// Equality, ordering and hashing compare numeric values, and a `BitVec`
/// equals a `u64` of the same value.
#[derive(Clone)]
pub struct BitVec {
    words: Words,
}

/// Little-endian words without trailing zero words. Values that fit one word
/// (almost every leaf) are stored inline, without a heap allocation.
#[derive(Clone)]
enum Words {
    /// Zero (no words) or one non-zero word.
    Inline(u64),
    /// Two or more words, the last non-zero.
    Heap(Box<[u64]>),
}

impl Default for BitVec {
    fn default() -> Self {
        Self {
            words: Words::Inline(0),
        }
    }
}

impl BitVec {
    pub fn zero() -> Self {
        Self::default()
    }
    /// The value of little-endian `words`.
    pub fn from_words(words: &[u64]) -> Self {
        let len = words.iter().rposition(|&w| w != 0).map_or(0, |top| top + 1);
        let words = match len {
            0 => Words::Inline(0),
            1 => Words::Inline(words[0]),
            _ => Words::Heap(words[..len].into()),
        };
        Self { words }
    }
    /// Little-endian words, without trailing zero words (empty for zero).
    pub fn words(&self) -> &[u64] {
        match &self.words {
            Words::Inline(0) => &[],
            Words::Inline(w) => std::slice::from_ref(w),
            Words::Heap(words) => words,
        }
    }
    /// The value as a `u64`, if it fits.
    pub fn to_u64(&self) -> Option<u64> {
        match self.words() {
            [] => Some(0),
            [w] => Some(*w),
            _ => None,
        }
    }
    pub fn is_zero(&self) -> bool {
        self.words().is_empty()
    }
    /// Whether the value fits in `width` bits.
    pub fn fits(&self, width: u16) -> bool {
        fits(self.words(), usize::from(width))
    }
    pub fn bit(&self, index: usize) -> bool {
        word(self.words(), index / 64) >> (index % 64) & 1 != 0
    }
    /// The value cut to `width` bits.
    pub fn masked(&self, width: u16) -> Self {
        let mut words = self.words().to_vec();
        mask_top(&mut words, usize::from(width));
        Self::from_words(&words)
    }
    /// A Verilog literal of `width` bits.
    pub fn to_verilog(&self, width: u16) -> String {
        format!("{width}'h{}", hex(self.words()))
    }
    /// `width` one bits.
    pub fn ones(width: u16) -> Self {
        let mut words = vec![u64::MAX; words_for(usize::from(width))];
        mask_top(&mut words, usize::from(width));
        Self::from_words(&words)
    }
    /// Bits `offset..offset + width`.
    pub fn extract(&self, offset: u16, width: u16) -> Self {
        let mut words = vec![0; words_for(usize::from(width))];
        shr(
            &mut words,
            self.words(),
            usize::from(offset),
            usize::from(width),
        );
        Self::from_words(&words)
    }
    /// `(value, width)` parts laid out from bit 0 upwards, in a `width`-bit
    /// value. Each part is cut to its width.
    pub fn pack(parts: &[(&BitVec, u16)], width: u16) -> Self {
        let size = words_for(usize::from(width));
        let (mut words, mut part) = (vec![0; size], vec![0; size]);
        let mut offset = 0;
        for &(value, part_width) in parts {
            let mut cut = value.words().to_vec();
            mask_top(&mut cut, usize::from(part_width));
            shl(&mut part, &cut, offset, usize::from(width));
            for (w, p) in words.iter_mut().zip(&part) {
                *w |= p;
            }
            offset += usize::from(part_width);
        }
        Self::from_words(&words)
    }
}

impl From<u64> for BitVec {
    fn from(value: u64) -> Self {
        Self::from_words(&[value])
    }
}
impl From<bool> for BitVec {
    fn from(value: bool) -> Self {
        Self::from(u64::from(value))
    }
}
impl From<&BitVec> for BitVec {
    fn from(value: &BitVec) -> Self {
        value.clone()
    }
}
impl PartialEq for BitVec {
    fn eq(&self, other: &Self) -> bool {
        self.words() == other.words()
    }
}
impl Eq for BitVec {}
impl std::hash::Hash for BitVec {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.words().hash(state);
    }
}
impl PartialEq<u64> for BitVec {
    fn eq(&self, other: &u64) -> bool {
        self.to_u64() == Some(*other)
    }
}
impl PartialEq<BitVec> for u64 {
    fn eq(&self, other: &BitVec) -> bool {
        other == self
    }
}
impl Ord for BitVec {
    fn cmp(&self, other: &Self) -> Ordering {
        compare(self.words(), other.words())
    }
}
impl PartialOrd for BitVec {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl fmt::Debug for BitVec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.to_u64() {
            Some(v) => write!(f, "{v}"),
            None => write!(f, "0x{}", hex(self.words())),
        }
    }
}
impl fmt::Display for BitVec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}
impl fmt::LowerHex for BitVec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad_integral(true, "0x", &hex(self.words()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reference arithmetic on u128 for widths up to 128.
    fn check(width: usize, a: u128, b: u128) {
        let m = if width == 128 {
            u128::MAX
        } else {
            (1u128 << width) - 1
        };
        let (a, b) = (a & m, b & m);
        let words = |v: u128| [v as u64, (v >> 64) as u64];
        let get = |w: &[u64]| u128::from(w[0]) | (u128::from(*w.get(1).unwrap_or(&0)) << 64);
        let n = words_for(width);
        let mut out = vec![0; n];
        add(&mut out, &words(a), &words(b), width, false);
        assert_eq!(get(&out), a.wrapping_add(b) & m, "{width} {a:x}+{b:x}");
        add(&mut out, &words(a), &words(b), width, true);
        assert_eq!(get(&out), a.wrapping_sub(b) & m, "{width} {a:x}-{b:x}");
        not(&mut out, &words(a), width);
        assert_eq!(get(&out), !a & m);
        assert_eq!(compare(&words(a), &words(b)), a.cmp(&b));
        for amount in [0, 1, 13, 63, 64, 65, 100, 127] {
            shl(&mut out, &words(a), amount, width);
            assert_eq!(get(&out), (a << amount) & m, "{width} {a:x}<<{amount}");
            shr(&mut out, &words(a), amount, width);
            assert_eq!(get(&out), (a >> amount) & m, "{width} {a:x}>>{amount}");
        }
        assert_eq!(hex(&words(a)), format!("{a:x}"));
    }

    #[test]
    fn word_arithmetic_matches_u128() {
        let samples = [
            0,
            1,
            u64::MAX as u128,
            1 << 64,
            u128::MAX,
            0x0123_4567_89ab_cdef_fedc_ba98_7654_3210,
            0x8000_0000_0000_0000_0000_0000_0000_0001,
        ];
        for width in [1, 7, 63, 64, 65, 100, 127, 128] {
            for &a in &samples {
                for &b in &samples {
                    check(width, a, b);
                }
            }
        }
    }

    #[test]
    fn concat_and_slice_cross_words() {
        let high = [0xabcd];
        let low = [u64::MAX, 0x1];
        let mut out = vec![0; 3];
        concat(&mut out, &high, &low, 65, 81);
        assert_eq!(out, [u64::MAX, 0x1 | (0xabcd << 1), 0]);
        let mut slice = vec![0; 1];
        shr(&mut slice, &out, 60, 8);
        // Bits 60..68: the top nibble of word 0 (0xf), then 0xb from word 1.
        assert_eq!(slice, [0xbf]);
    }

    #[test]
    fn bitvec_compares_by_value() {
        assert_eq!(BitVec::from_words(&[5, 0, 0]), BitVec::from(5));
        assert_eq!(BitVec::from(5), 5);
        assert!(BitVec::from_words(&[0, 1]) > BitVec::from_words(&[u64::MAX]));
        assert_eq!(BitVec::from_words(&[0, 1]).to_u64(), None);
        assert!(!BitVec::from_words(&[0, 1]).fits(64));
        assert!(BitVec::from_words(&[0, 1]).fits(65));
        assert_eq!(
            format!("{:x}", BitVec::from_words(&[1, 2])),
            "20000000000000001"
        );
        assert_eq!(BitVec::from(3).to_verilog(70), "70'h3");
    }
}
