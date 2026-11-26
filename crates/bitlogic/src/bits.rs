//! `Bits<N>`: unsigned values of any width from 1 to [`MAX_WIDTH`], stored in
//! `⌈N/64⌉` little-endian words.
use crate::bitvec::{self, MAX_WIDTH, words_for};
use std::cmp::Ordering;
use std::fmt;
use std::ops::{Add, BitAnd, BitOr, BitXor, Mul, Not, Shl, Shr, Sub};

/// The width `N` of a `Bits<N>`, as a type. [`Storage`] is implemented for
/// every `Width<N>` with `N` in `1..=MAX_WIDTH`.
pub struct Width<const N: usize>;

/// The storage of `Bits<N>`: `[u64; ⌈N/64⌉]`. Generic code over a width
/// states `where Width<N>: Storage`; concrete widths need nothing.
///
/// # Safety
///
/// `Repr` must be `[u64; K]` with `K = ⌈N/64⌉ ≥ 1`: [`bits`] writes the first
/// word through a pointer.
pub unsafe trait Storage {
    type Repr: Copy
        + Eq
        + std::hash::Hash
        + fmt::Debug
        + AsRef<[u64]>
        + AsMut<[u64]>
        + Send
        + Sync
        + 'static;
    const ZERO: Self::Repr;
}

bitlogic_macros::__bits_storage!();

/// An unsigned, fixed-width value. Arithmetic wraps at N bits in every build
/// profile.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Bits<const N: usize>(<Width<N> as Storage>::Repr)
where
    Width<N>: Storage;

/// `value`, cut to N bits (zero-extended when N > 64).
pub const fn bits<const N: usize>(value: u64) -> Bits<N>
where
    Width<N>: Storage,
{
    let mut repr = <Width<N> as Storage>::ZERO;
    let first = &mut repr as *mut <Width<N> as Storage>::Repr as *mut u64;
    let value = if N >= 64 {
        value
    } else {
        value & (u64::MAX >> (64 - N))
    };
    // SAFETY: `Storage` guarantees `Repr` is a non-empty `[u64; K]`.
    unsafe { *first = value };
    Bits(repr)
}

impl<const N: usize> Bits<N>
where
    Width<N>: Storage,
{
    /// The value as a `u64`. Only for `N <= 64` (a compile-time error
    /// otherwise); use [`Bits::words`] or [`Bits::to_u64`] for wider values.
    pub const fn get(self) -> u64 {
        const { assert!(N <= 64, "Bits::get needs N <= 64; use words()") };
        let first = &self.0 as *const <Width<N> as Storage>::Repr as *const u64;
        // SAFETY: `Storage` guarantees `Repr` is a non-empty `[u64; K]`.
        unsafe { *first }
    }
    /// The value as a `u64`, if it fits.
    pub fn to_u64(self) -> Option<u64> {
        let words = self.words();
        words[1..].iter().all(|&w| w == 0).then_some(words[0])
    }
    /// Little-endian words: `⌈N/64⌉` of them.
    pub fn words(&self) -> &[u64] {
        self.0.as_ref()
    }
    /// The value of little-endian `words`, cut to N bits.
    pub fn from_words(words: &[u64]) -> Self {
        let mut result = Self(<Width<N> as Storage>::ZERO);
        bitvec::copy(result.0.as_mut(), words);
        bitvec::mask_top(result.0.as_mut(), N);
        result
    }
    /// The value of `value`, which must fit in N bits.
    pub fn from_bitvec(value: &bitvec::BitVec) -> Option<Self> {
        value
            .fits(u16::try_from(N).ok()?)
            .then(|| Self::from_words(value.words()))
    }
    pub fn to_bitvec(self) -> bitvec::BitVec {
        bitvec::BitVec::from_words(self.words())
    }
    pub fn bit(self, index: usize) -> bool {
        assert!(index < N, "bit index out of bounds");
        self.words()[index / 64] >> (index % 64) & 1 != 0
    }
    pub fn slice<const M: usize>(self, offset: usize) -> Bits<M>
    where
        Width<M>: Storage,
    {
        assert!(M > 0 && M <= N && offset <= N - M, "slice out of bounds");
        if N <= 64 {
            return bits(self.low() >> offset);
        }
        let mut result = Bits::<M>(<Width<M> as Storage>::ZERO);
        bitvec::shr(result.0.as_mut(), self.words(), offset, M);
        result
    }
    /// Concatenate self as the high bits and rhs as the low bits.
    pub fn concat<const M: usize, const O: usize>(self, rhs: Bits<M>) -> Bits<O>
    where
        Width<M>: Storage,
        Width<O>: Storage,
    {
        assert!(N.checked_add(M) == Some(O), "concatenation width mismatch");
        if O <= 64 {
            return bits((self.low() << M) | rhs.low());
        }
        let mut result = Bits::<O>(<Width<O> as Storage>::ZERO);
        bitvec::concat(result.0.as_mut(), self.words(), rhs.words(), M, O);
        result
    }
    /// The full unsigned product, `O = N + M` bits wide (it never wraps).
    pub fn mul_wide<const M: usize, const O: usize>(self, rhs: Bits<M>) -> Bits<O>
    where
        Width<M>: Storage,
        Width<O>: Storage,
    {
        assert!(N.checked_add(M) == Some(O), "product width mismatch");
        if O <= 64 {
            return bits(self.low() * rhs.low());
        }
        let mut result = Bits::<O>(<Width<O> as Storage>::ZERO);
        bitvec::mul(
            result.0.as_mut(),
            (self.words(), N),
            (rhs.words(), M),
            O,
            false,
        );
        result
    }
    /// The full product of both values read as two's complement, `O = N + M`
    /// bits wide (it never wraps), as two's complement.
    pub fn smul_wide<const M: usize, const O: usize>(self, rhs: Bits<M>) -> Bits<O>
    where
        Width<M>: Storage,
        Width<O>: Storage,
    {
        assert!(N.checked_add(M) == Some(O), "product width mismatch");
        if O <= 64 {
            let a = bitvec::sign_extend(self.low(), N as u32);
            let b = bitvec::sign_extend(rhs.low(), M as u32);
            return bits(a.wrapping_mul(b) as u64);
        }
        let mut result = Bits::<O>(<Width<O> as Storage>::ZERO);
        bitvec::mul(
            result.0.as_mut(),
            (self.words(), N),
            (rhs.words(), M),
            O,
            true,
        );
        result
    }
    /// The top bit: set for a negative value read as two's complement.
    fn negative(self) -> bool {
        self.bit(N - 1)
    }
    /// Both values read as two's complement, ordered.
    fn signed_cmp(self, rhs: Self) -> Ordering {
        match (self.negative(), rhs.negative()) {
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
            _ => self.cmp(&rhs),
        }
    }
    /// Signed `<`: both values read as two's complement.
    pub fn slt(self, rhs: Self) -> bool {
        self.signed_cmp(rhs).is_lt()
    }
    /// Signed `<=`.
    pub fn sle(self, rhs: Self) -> bool {
        self.signed_cmp(rhs).is_le()
    }
    /// Signed `>`.
    pub fn sgt(self, rhs: Self) -> bool {
        self.signed_cmp(rhs).is_gt()
    }
    /// Signed `>=`.
    pub fn sge(self, rhs: Self) -> bool {
        self.signed_cmp(rhs).is_ge()
    }
    /// Arithmetic shift right: `>>` filling with copies of the top bit, so
    /// a two's complement value is divided by `2^amount`, rounding down.
    pub fn sar(self, amount: u32) -> Self {
        if self.negative() {
            !(!self >> amount)
        } else {
            self >> amount
        }
    }
    /// The low word. With `N <= 64` it is the whole value: the `if N <= 64`
    /// fast paths below are resolved at compile time.
    #[inline(always)]
    fn low(self) -> u64 {
        self.words()[0]
    }
    fn map(self, f: impl FnOnce(&mut [u64], &[u64])) -> Self {
        let mut result = Self(<Width<N> as Storage>::ZERO);
        f(result.0.as_mut(), self.words());
        result
    }
}

impl<const N: usize> Ord for Bits<N>
where
    Width<N>: Storage,
{
    fn cmp(&self, other: &Self) -> Ordering {
        if N <= 64 {
            return self.low().cmp(&other.low());
        }
        bitvec::compare(self.words(), other.words())
    }
}
impl<const N: usize> PartialOrd for Bits<N>
where
    Width<N>: Storage,
{
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl<const N: usize> fmt::Debug for Bits<N>
where
    Width<N>: Storage,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.to_u64() {
            Some(v) => write!(f, "Bits({v})"),
            None => write!(f, "Bits(0x{})", bitvec::hex(self.words())),
        }
    }
}

macro_rules! binary {
    ($trait:ident, $method:ident, $small:expr, |$out:ident, $a:ident, $b:ident| $body:expr) => {
        impl<const N: usize> $trait for Bits<N>
        where
            Width<N>: Storage,
        {
            type Output = Self;
            #[inline]
            fn $method(self, rhs: Self) -> Self {
                if N <= 64 {
                    return bits(($small)(self.low(), rhs.low()));
                }
                self.map(|$out, $a| {
                    let $b = rhs.words();
                    $body
                })
            }
        }
    };
}
binary!(BitAnd, bitand, |x, y| x & y, |o, a, b| {
    bitvec::bitwise(o, a, b, |x, y| x & y)
});
binary!(BitOr, bitor, |x, y| x | y, |o, a, b| {
    bitvec::bitwise(o, a, b, |x, y| x | y)
});
binary!(BitXor, bitxor, |x, y| x ^ y, |o, a, b| {
    bitvec::bitwise(o, a, b, |x, y| x ^ y)
});
binary!(Add, add, u64::wrapping_add, |o, a, b| {
    bitvec::add(o, a, b, N, false)
});
binary!(Sub, sub, u64::wrapping_sub, |o, a, b| {
    bitvec::add(o, a, b, N, true)
});
/// The product, wrapping at N bits (the same bits signed or unsigned).
impl<const N: usize> Mul for Bits<N>
where
    Width<N>: Storage,
{
    type Output = Self;
    #[inline]
    fn mul(self, rhs: Self) -> Self {
        if N <= 64 {
            return bits(self.low().wrapping_mul(rhs.low()));
        }
        self.map(|o, a| bitvec::mul(o, (a, N), (rhs.words(), N), N, false))
    }
}
impl<const N: usize> Not for Bits<N>
where
    Width<N>: Storage,
{
    type Output = Self;
    fn not(self) -> Self {
        if N <= 64 {
            return bits(!self.low());
        }
        self.map(|o, a| bitvec::not(o, a, N))
    }
}
impl<const N: usize> Shl<u32> for Bits<N>
where
    Width<N>: Storage,
{
    type Output = Self;
    fn shl(self, rhs: u32) -> Self {
        if N <= 64 {
            return bits(self.low().checked_shl(rhs).unwrap_or(0));
        }
        let amount = usize::try_from(rhs).unwrap_or(usize::MAX).min(N);
        self.map(|o, a| bitvec::shl(o, a, amount, N))
    }
}
impl<const N: usize> Shr<u32> for Bits<N>
where
    Width<N>: Storage,
{
    type Output = Self;
    fn shr(self, rhs: u32) -> Self {
        if N <= 64 {
            return bits(self.low().checked_shr(rhs).unwrap_or(0));
        }
        let amount = usize::try_from(rhs).unwrap_or(usize::MAX).min(N);
        self.map(|o, a| bitvec::shr(o, a, amount, N))
    }
}

const _: () = assert!(words_for(MAX_WIDTH as usize) == 64);
