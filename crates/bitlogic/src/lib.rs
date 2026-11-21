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
//! Fixed-length arrays index with a literal (`t[2]`) or, through
//! [`LogicArray`], with a run-time `Bits` index: `at` reads (the zero value
//! past the end) and `set` returns an updated copy. Fieldless enums derive
//! `LogicValue` and are matched with constant patterns.
//! ```
//! use bitlogic::{logic, bits, Bits, LogicValue};
//! #[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
//! enum Op { Read, Write, Clear }
//! #[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
//! struct Regs { file: [Bits<8>; 4], op: Op, at: Bits<2>, data: Bits<8> }
//! #[logic]
//! fn regs(r: Regs) -> Regs {
//!     let file = match r.op {
//!         Op::Read => r.file,
//!         Op::Write => r.file.set(r.at, r.data),
//!         Op::Clear => [bits::<8>(0); 4],
//!     };
//!     Regs { file, op: r.op, at: r.at, data: r.file.at(r.at) }
//! }
//! let r = Regs { file: [bits(1), bits(2), bits(3), bits(4)], op: Op::Write,
//!                at: bits(2), data: bits(9) };
//! let want = Regs { file: [bits(1), bits(2), bits(9), bits(4)], data: bits(3), ..r };
//! assert_eq!(regs(r), want);
//! assert_eq!(regs::ir()?.evaluate_typed::<_, Regs>(r)?, want);
//! # Ok::<(), bitlogic::Error>(())
//! ```
//!
//! `fixed_set!` and `fixed_map!` generate a module holding a fixed-capacity
//! container and its `#[logic]` operations (`find`, `contains`, `insert`,
//! `remove`, `count`, `store`, and for maps `get` and `update`). Lookups
//! compare every slot in parallel; a full container replaces round-robin.
//! ```
//! use bitlogic::{bits, fixed_map, fixed_set, Bits, LogicValue};
//! fixed_set!(mod flows: Bits<43>, capacity = 16);
//! fixed_map!(mod arp: Bits<32> => Bits<48>, capacity = 8);
//! let s = flows::insert(flows::Set::zeroed(), true, bits(0x7ff_0000_0001));
//! assert!(flows::contains(s, bits(0x7ff_0000_0001)));
//! assert_eq!(flows::count(s).get(), 1);
//! let m = arp::insert(arp::Map::zeroed(), true, bits(0x0a00_0001), bits(0x0200_0000_0001));
//! assert_eq!(arp::get(m, bits(0x0a00_0001)).value.get(), 0x0200_0000_0001);
//! assert!(!arp::get(arp::remove(m, true, bits(0x0a00_0001)), bits(0x0a00_0001)).hit);
//! # Ok::<(), bitlogic::Error>(())
//! ```
//!
//! `system!` composes single-input step functions into a clocked system.
//! Each unit's input and output structs have a `state` field, held in a
//! register. Unit inputs connect only to registered values (params, inputs,
//! `unit.state.field`, `memory.rdata`); a unit's other outputs drive memory
//! ports and registered outputs. One declaration generates a cycle model
//! (`Core`) and the Verilog (`verilog()`).
//! ```
//! use bitlogic::{bits, logic, Bits, LogicValue, Native};
//! #[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
//! pub struct Count { n: Bits<4> }
//! #[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
//! pub struct CountIn { state: Count, enable: bool, seen: Bits<4> }
//! #[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
//! pub struct CountOut { state: Count, we: bool, addr: Bits<4> }
//! #[logic]
//! fn count(i: CountIn) -> CountOut {
//!     let n = i.state.n;
//!     CountOut {
//!         state: Count { n: if i.enable { n + bits::<4>(1) } else { n } },
//!         we: i.enable,
//!         addr: n,
//!     }
//! }
//! bitlogic::system! {
//!     mod counter {
//!         input enable: bool;
//!         ram log: Bits<4>, addr_bits = 4;
//!         unit c: Count = count(CountIn { state, enable, seen: log.rdata });
//!         write log(c.we, c.addr, c.state.n);
//!         read log(c.addr);
//!         output n: Bits<4> = c.state.n;
//!     }
//! }
//! # fn main() -> Result<(), bitlogic::Error> {
//! let mut core = counter::Core::new(Native);
//! for _ in 0..3 {
//!     core.clock(counter::Inputs { enable: true });
//! }
//! assert_eq!(core.outputs().n.get(), 3);
//! assert_eq!(core.log.cells[..3], [bits(0), bits(1), bits(2)]);
//! let files = counter::verilog()?;
//! assert_eq!(files[0].0, "counter.v");
//! assert!(files[0].1.contains("c_unit c_i ("));
//! # Ok(())
//! # }
//! ```
//!
//! A set has no value type:
//! ```compile_fail
//! bitlogic::fixed_set!(mod s: bitlogic::Bits<8> => bool, capacity = 4);
//! ```
//!
//! A map needs one:
//! ```compile_fail
//! bitlogic::fixed_map!(mod m: bitlogic::Bits<8>, capacity = 4);
//! ```
//!
//! Capacity is 1 to 256 (the fold limit):
//! ```compile_fail
//! bitlogic::fixed_set!(mod s: bitlogic::Bits<8>, capacity = 0);
//! ```
//!
//! A run-time index needs `at`, not `[]`:
//! ```compile_fail
//! use bitlogic::{logic, Bits};
//! #[logic]
//! fn pick(t: [Bits<8>; 4]) -> Bits<8> { t[t[0]] }
//! ```
//!
//! Match guards are unsupported:
//! ```compile_fail
//! use bitlogic::logic;
//! #[logic]
//! fn guarded(a: bool) -> bool { match a { true if a => false, _ => true } }
//! ```
//!
//! Variant fields must be logic values:
//! ```compile_fail
//! use bitlogic::LogicValue;
//! #[derive(LogicValue)] enum Tagged { A(u8), B }
//! ```
//!
//! Variant fields bind to plain names; nested patterns are unsupported:
//! ```compile_fail
//! use bitlogic::{logic, LogicValue};
//! #[derive(Clone, Copy, LogicValue)] enum Tagged { A(bool), B }
//! #[logic]
//! fn nested(t: Tagged) -> bool { match t { Tagged::A(true) => true, _ => false } }
//! ```
//!
//! Bindings cannot be shared between `|` alternatives:
//! ```compile_fail
//! use bitlogic::{logic, LogicValue};
//! #[derive(Clone, Copy, LogicValue)] enum Tagged { A(bool), B(bool) }
//! #[logic]
//! fn either(t: Tagged) -> bool { match t { Tagged::A(x) | Tagged::B(x) => x } }
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
//! Arbitrary Rust leaf types (here `u8`) are unsupported:
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
pub mod system;
mod value;
pub use bitlogic_macros::{LogicValue, fixed_map, fixed_set, logic, system};
pub use system::{Evaluate, Interpreted, LogicFn, Memory, Native};
pub use value::{LogicArray, LogicValue, Schema};
#[doc(hidden)]
pub mod __private {
    pub use crate::value::Symbolic;
    /// Nesting limit for inlined `#[logic]` calls; catches recursion.
    pub const MAX_CALL_DEPTH: usize = 64;
    /// The schema of the value a constructor expression builds. The closure is
    /// never called; it lets rustc resolve whether `A::B { .. }` names a struct
    /// or an enum variant.
    pub fn schema_of<T: crate::LogicValue>(_: impl FnOnce() -> T) -> crate::Schema {
        T::schema()
    }
    /// A placeholder field value inside a [`schema_of`] closure.
    pub fn never<T>() -> T {
        unreachable!("schema probes are never evaluated")
    }
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
