//! Pure, untimed bit-vector transformations with structured inputs and outputs.
//!
//! ```
//! use bitlogic::{logic, bits, Bits, LogicValue};
//! #[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
//! struct State { counter: Bits<8>, enabled: bool }
//! #[logic]
//! fn step(current: State) -> State {
//!     State {
//!         counter: if current.enabled { current.counter + bits::<8>(1) }
//!                  else { current.counter },
//!         enabled: current.enabled,
//!     }
//! }
//! let input = State { counter: bits(255), enabled: true };
//! let ir = step::ir()?;
//! assert_eq!(step(input), State { counter: bits(0), enabled: true });
//! assert_eq!(ir.evaluate_typed::<_, State>(input)?, step(input));
//! let verilog = ir.to_verilog("step")?;
//! assert!(verilog.contains("i_counter"));
//! # Ok::<(), bitlogic::Error>(())
//! ```
//!
//! `#[logic]` functions may call other `#[logic]` functions, with any number of
//! arguments. Each call inlines the callee's graph; nothing is shared in time.
//! ```
//! use bitlogic::{logic, bits, Bits};
//! #[logic]
//! fn hex_digit(n: Bits<4>) -> Bits<8> {
//!     if n < bits::<4>(10) { bits::<4>(3).concat::<4, 8>(n) }
//!     else { bits::<4>(0).concat::<4, 8>(n) + bits::<8>(0x37) }
//! }
//! #[logic]
//! fn pick(sel: bool, a: Bits<8>, b: Bits<8>) -> Bits<8> { if sel { a } else { b } }
//! #[logic]
//! fn high_hex(byte: Bits<8>) -> Bits<8> {
//!     pick(byte.bit(7), hex_digit(byte.slice::<4>(4)), hex_digit(byte.slice::<4>(0)))
//! }
//! let ir = high_hex::ir()?;
//! for b in 0..256 {
//!     let b = bits::<8>(b);
//!     assert_eq!(ir.evaluate_typed::<_, Bits<8>>(b)?, high_hex(b));
//! }
//! assert_eq!(high_hex(bits(0xA3)).get(), u64::from(b'A'));
//! # Ok::<(), bitlogic::Error>(())
//! ```
//!
//! Mutation is outside the language:
//! ```compile_fail
//! use bitlogic::logic;
//! #[logic]
//! fn mutate(input: bool) -> bool { let mut value = input; value = !value; value }
//! ```
//!
//! Inputs must be owned values:
//! ```compile_fail
//! use bitlogic::logic;
//! #[logic]
//! fn borrowed(input: &bool) -> bool { *input }
//! ```
//!
//! Every field must be supplied in a returned struct:
//! ```compile_fail
//! use bitlogic::{logic, LogicValue};
//! #[derive(Clone, Copy, LogicValue)] struct Pair { a: bool, b: bool }
//! #[logic]
//! fn incomplete(input: bool) -> Pair { Pair { a: input } }
//! ```
//!
//! Branch types must agree:
//! ```compile_fail
//! use bitlogic::{logic, bits, Bits};
//! #[logic]
//! fn mismatched(input: bool) -> Bits<8> { if input { bits::<8>(1) } else { false } }
//! ```
//!
//! Arbitrary Rust leaf types are unsupported:
//! ```compile_fail
//! use bitlogic::{logic, LogicValue};
//! #[derive(Clone, Copy, LogicValue)] struct Unsupported { number: u8 }
//! #[logic]
//! fn unsupported(input: Unsupported) -> Unsupported { input }
//! ```
//!
//! Only single-input functions are complete circuits with `ir()`; functions
//! with other arities are helpers that callers inline:
//! ```compile_fail
//! use bitlogic::logic;
//! #[logic]
//! fn multiple(a: bool, b: bool) -> bool { a && b }
//! let _ = multiple::ir();
//! ```
//!
//! Callees must themselves be `#[logic]` functions:
//! ```compile_fail
//! use bitlogic::logic;
//! fn plain(a: bool) -> bool { !a }
//! #[logic]
//! fn caller(a: bool) -> bool { plain(a) }
//! ```
//!
//! Reassignment and early returns are unsupported:
//! ```compile_fail
//! use bitlogic::logic;
//! #[logic]
//! fn assign(input: bool) -> bool { let value = input; value = false; value }
//! ```
//!
//! ```compile_fail
//! use bitlogic::logic;
//! #[logic]
//! fn early(input: bool) -> bool { return input; }
//! ```
//!
//! Struct-update syntax is intentionally explicit rather than implicit:
//! ```compile_fail
//! use bitlogic::{logic, LogicValue};
//! #[derive(Clone, Copy, LogicValue)] struct Pair { a: bool, b: bool }
//! #[logic]
//! fn update(input: Pair) -> Pair { Pair { a: false, ..input } }
//! ```
//!
//! Source structs must have named fields:
//! ```compile_fail
//! use bitlogic::LogicValue;
//! #[derive(LogicValue)] struct Tuple(bool);
//! ```

mod ir;
mod value;
pub use bitlogic_macros::{LogicValue, logic};
pub use value::{LogicValue, Schema};
#[doc(hidden)]
pub mod __private {
    pub use crate::value::Symbolic;
    /// Nesting limit for inlined `#[logic]` calls; catches recursion.
    pub const MAX_CALL_DEPTH: usize = 64;
}
pub use ir::{Binary, Builder, Error, Logic, Node, NodeId, Operation, Port};
use std::ops::{Add, BitAnd, BitOr, BitXor, Not, Shl, Shr, Sub};

/// An unsigned, fixed-width value. Arithmetic wraps at N bits in every build profile.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Bits<const N: usize>(u64);

pub const fn bits<const N: usize>(value: u64) -> Bits<N> {
    assert!(N > 0 && N <= 64, "bit widths must be in 1..=64");
    Bits(value & (u64::MAX >> (64 - N)))
}

impl<const N: usize> Bits<N> {
    pub const fn get(self) -> u64 {
        self.0
    }
    pub fn bit(self, index: usize) -> bool {
        assert!(index < N, "bit index out of bounds");
        (self.0 >> index) & 1 != 0
    }
    pub fn slice<const M: usize>(self, offset: usize) -> Bits<M> {
        assert!(M > 0 && M <= N && offset <= N - M, "slice out of bounds");
        bits(self.0 >> offset)
    }
    /// Concatenate self as the high bits and rhs as the low bits.
    pub fn concat<const M: usize, const O: usize>(self, rhs: Bits<M>) -> Bits<O> {
        assert!(
            O <= 64 && N.checked_add(M) == Some(O),
            "concatenation width mismatch"
        );
        bits((self.0 << M) | rhs.0)
    }
}

macro_rules! binary {
    ($trait:ident, $method:ident, $op:expr) => {
        impl<const N: usize> $trait for Bits<N> {
            type Output = Self;
            fn $method(self, rhs: Self) -> Self {
                bits(($op)(self.0, rhs.0))
            }
        }
    };
}
binary!(BitAnd, bitand, |a, b| a & b);
binary!(BitOr, bitor, |a, b| a | b);
binary!(BitXor, bitxor, |a, b| a ^ b);
binary!(Add, add, u64::wrapping_add);
binary!(Sub, sub, u64::wrapping_sub);
impl<const N: usize> Not for Bits<N> {
    type Output = Self;
    fn not(self) -> Self {
        bits(!self.0)
    }
}
impl<const N: usize> Shl<u32> for Bits<N> {
    type Output = Self;
    fn shl(self, rhs: u32) -> Self {
        bits(self.0.checked_shl(rhs).unwrap_or(0))
    }
}
impl<const N: usize> Shr<u32> for Bits<N> {
    type Output = Self;
    fn shr(self, rhs: u32) -> Self {
        bits(self.0.checked_shr(rhs).unwrap_or(0))
    }
}
