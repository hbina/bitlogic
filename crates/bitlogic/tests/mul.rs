//! Multiplication (`*`, `mul_wide`, `smul_wide`), signed comparisons
//! (`slt`, `sle`, `sgt`, `sge`) and the arithmetic shift `sar`: native
//! `Bits<N>`, the IR and Icarus HDL against an `i128` oracle, plus values
//! wider than 64 bits up to the 4096-bit limit.
mod support;
use bitlogic::{Bits, LogicValue, bits, logic};
use support::{Rng, check_hdl};

#[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
struct Narrow {
    /// A DSP48E1's operand widths.
    a: Bits<25>,
    b: Bits<18>,
    c: Bits<16>,
    d: Bits<16>,
    e: Bits<64>,
    f: Bits<64>,
    one: Bits<1>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
struct NarrowOut {
    umul: Bits<43>,
    smul: Bits<43>,
    wrap: Bits<16>,
    wrap64: Bits<64>,
    smul64: Bits<128>,
    umul64: Bits<128>,
    /// A constant operand, and a zero one (folded away).
    times5: Bits<16>,
    times0: Bits<16>,
    /// 1-bit operands: unsigned 0/1, signed 0/-1.
    one_u: Bits<2>,
    one_s: Bits<2>,
    lt: bool,
    le: bool,
    gt: bool,
    ge: bool,
    sar0: Bits<16>,
    sar3: Bits<16>,
    sar15: Bits<16>,
    sar16: Bits<16>,
    sar40: Bits<16>,
}

#[logic]
fn narrow(x: Narrow) -> NarrowOut {
    NarrowOut {
        umul: x.a.mul_wide::<18, 43>(x.b),
        smul: x.a.smul_wide::<18, 43>(x.b),
        wrap: x.c * x.d,
        wrap64: x.e * x.f,
        smul64: x.e.smul_wide::<64, 128>(x.f),
        umul64: x.e.mul_wide::<64, 128>(x.f),
        times5: x.c * bits::<16>(5),
        times0: x.c * bits::<16>(0),
        one_u: x.one.mul_wide::<1, 2>(x.one),
        one_s: x.one.smul_wide::<1, 2>(x.one),
        lt: x.c.slt(x.d),
        le: x.c.sle(x.d),
        gt: x.c.sgt(x.d),
        ge: x.c.sge(x.d),
        sar0: x.c.sar(0),
        sar3: x.c.sar(3),
        sar15: x.c.sar(15),
        sar16: x.c.sar(16),
        sar40: x.c.sar(40),
    }
}

fn mask(width: u32) -> u128 {
    if width == 128 {
        u128::MAX
    } else {
        (1 << width) - 1
    }
}
/// `v`, a `width`-bit two's complement value, as an `i128`.
fn signed(v: u128, width: u32) -> i128 {
    ((v << (128 - width)) as i128) >> (128 - width)
}
/// `v` as `width` bits of two's complement.
fn bits_of(v: i128, width: u32) -> u128 {
    v as u128 & mask(width)
}

/// The oracle, field for field, in `NarrowOut` order.
fn oracle(a: u128, b: u128, c: u128, d: u128, e: u128, f: u128, one: u128) -> Vec<u128> {
    let (sc, sd) = (signed(c, 16), signed(d, 16));
    let sar = |k: u32| bits_of(sc >> k.min(127), 16);
    vec![
        a * b,
        bits_of(signed(a, 25) * signed(b, 18), 43),
        (c * d) & mask(16),
        e.wrapping_mul(f) & mask(64),
        bits_of(signed(e, 64) * signed(f, 64), 128),
        e * f,
        (c * 5) & mask(16),
        0,
        one * one,
        bits_of(signed(one, 1) * signed(one, 1), 2),
        u128::from(sc < sd),
        u128::from(sc <= sd),
        u128::from(sc > sd),
        u128::from(sc >= sd),
        sar(0),
        sar(3),
        sar(15),
        sar(16),
        sar(40),
    ]
}

/// A `width`-bit sample biased toward the edges: 0, 1, -1, the most
/// negative and most positive values.
fn sample(rng: &mut Rng, width: u32) -> u128 {
    let v = (u128::from(rng.next()) << 64 | u128::from(rng.next())) & mask(width);
    match rng.next() % 8 {
        0 => 0,
        1 => 1,
        2 => mask(width),
        3 => 1 << (width - 1),
        4 => mask(width) >> 1,
        _ => v,
    }
}

fn values(out: &NarrowOut) -> Vec<u128> {
    out.flatten()
        .iter()
        .map(|v| {
            let w = v.words();
            u128::from(*w.first().unwrap_or(&0)) | (u128::from(*w.get(1).unwrap_or(&0)) << 64)
        })
        .collect()
}

#[test]
fn narrow_products_compares_and_shifts_match_oracle_ir_and_hdl() {
    let ir = narrow::ir().unwrap();
    let mut rng = Rng(0x6d75_6c74_6970_6c79);
    let mut cases = Vec::new();
    for _ in 0..800 {
        let (a, b, c, d) = (
            sample(&mut rng, 25),
            sample(&mut rng, 18),
            sample(&mut rng, 16),
            sample(&mut rng, 16),
        );
        let (e, f, one) = (
            sample(&mut rng, 64),
            sample(&mut rng, 64),
            rng.next() as u128 & 1,
        );
        let x = Narrow {
            a: bits(a as u64),
            b: bits(b as u64),
            c: bits(c as u64),
            d: bits(d as u64),
            e: bits(e as u64),
            f: bits(f as u64),
            one: bits(one as u64),
        };
        let native = narrow(x);
        assert_eq!(
            values(&native),
            oracle(a, b, c, d, e, f, one),
            "a={a:x} b={b:x} c={c:x} d={d:x} e={e:x} f={f:x} one={one}"
        );
        assert_eq!(ir.evaluate_typed::<_, NarrowOut>(x).unwrap(), native);
        cases.push((x.flatten(), native.flatten()));
    }
    check_hdl(&ir, "narrow", &cases);
}

#[test]
fn products_emit_verilog_multiplies() {
    let verilog = narrow::ir().unwrap().to_verilog("narrow").unwrap();
    assert!(verilog.contains("$signed(n"), "{verilog}");
    // umul, smul, wrap, wrap64, smul64, umul64, times5, one_u, one_s; times0
    // folds to a constant.
    assert_eq!(verilog.matches(" * ").count(), 9, "{verilog}");
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
struct Wide {
    a: Bits<100>,
    b: Bits<100>,
    c: Bits<64>,
    d: Bits<60>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
struct WideOut {
    wrap: Bits<100>,
    umul: Bits<124>,
    smul: Bits<124>,
    /// The full 200-bit products: checked against the IR and HDL, and by
    /// their low 100 bits against `wrap`.
    full: Bits<200>,
    sfull: Bits<200>,
    lt: bool,
    ge: bool,
    sar: Bits<100>,
}

#[logic]
fn wide(x: Wide) -> WideOut {
    WideOut {
        wrap: x.a * x.b,
        umul: x.c.mul_wide::<60, 124>(x.d),
        smul: x.c.smul_wide::<60, 124>(x.d),
        full: x.a.mul_wide::<100, 200>(x.b),
        sfull: x.a.smul_wide::<100, 200>(x.b),
        lt: x.a.slt(x.b),
        ge: x.a.sge(x.b),
        sar: x.a.sar(70),
    }
}

fn b100(v: u128) -> Bits<100> {
    Bits::from_words(&[v as u64, (v >> 64) as u64])
}
fn u128_of(words: &[u64]) -> u128 {
    u128::from(words[0]) | (u128::from(*words.get(1).unwrap_or(&0)) << 64)
}

#[test]
fn wide_products_match_oracle_ir_and_hdl() {
    let ir = wide::ir().unwrap();
    let mut rng = Rng(0x0077_6964_6570_726f);
    let mut cases = Vec::new();
    for _ in 0..400 {
        let (a, b) = (sample(&mut rng, 100), sample(&mut rng, 100));
        let (c, d) = (sample(&mut rng, 64), sample(&mut rng, 60));
        let x = Wide {
            a: b100(a),
            b: b100(b),
            c: bits(c as u64),
            d: bits(d as u64),
        };
        let out = wide(x);
        let (sa, sb) = (signed(a, 100), signed(b, 100));
        assert_eq!(u128_of(out.wrap.words()), a.wrapping_mul(b) & mask(100));
        assert_eq!(u128_of(out.umul.words()), c * d);
        assert_eq!(
            u128_of(out.smul.words()),
            bits_of(signed(c, 64) * signed(d, 60), 124)
        );
        assert_eq!(out.full.slice::<100>(0), out.wrap);
        assert_eq!(out.sfull.slice::<100>(0), out.wrap);
        // The signed product's sign: negative when exactly one operand is.
        let neg = |v: i128| u128::from(v < 0);
        assert_eq!(
            out.sfull.bit(199),
            (neg(sa) ^ neg(sb)) == 1 && a != 0 && b != 0
        );
        assert_eq!(out.lt, sa < sb);
        assert_eq!(out.ge, sa >= sb);
        assert_eq!(u128_of(out.sar.words()), bits_of(sa >> 70, 100));
        assert_eq!(ir.evaluate_typed::<_, WideOut>(x).unwrap(), out);
        cases.push((x.flatten(), out.flatten()));
    }
    check_hdl(&ir, "wide_mul", &cases);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
struct Huge {
    a: Bits<2048>,
    b: Bits<2048>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
struct HugeOut {
    full: Bits<4096>,
    sfull: Bits<4096>,
}

#[logic]
fn huge(x: Huge) -> HugeOut {
    HugeOut {
        full: x.a.mul_wide::<2048, 4096>(x.b),
        sfull: x.a.smul_wide::<2048, 4096>(x.b),
    }
}

#[test]
fn products_reach_4096_bits() {
    let ones = Bits::<2048>::from_words(&[u64::MAX; 32]);
    let one = bits::<2048>(1);
    let ir = huge::ir().unwrap();
    // (2^2048 - 1)^2 = 2^4096 - 2^2049 + 1; signed, (-1)(-1) = 1.
    let out = huge(Huge { a: ones, b: ones });
    let mut want = vec![0u64; 64];
    want[0] = 1;
    want[32] = u64::MAX - 1;
    want[33..].fill(u64::MAX);
    assert_eq!(out.full.words(), want.as_slice());
    assert_eq!(out.sfull, bits::<4096>(1));
    // x · 2^k is x shifted; signed, -1 · 2^k is -2^k.
    let mut rng = Rng(0x4096);
    let words: Vec<u64> = (0..32).map(|_| rng.next()).collect();
    let x = Bits::<2048>::from_words(&words);
    for k in [0, 1, 63, 64, 1000, 2047] {
        let p = one << k;
        let x_out = huge(Huge { a: x, b: p });
        assert_eq!(
            x_out.full,
            bits::<2048>(0).concat::<2048, 4096>(x) << k,
            "x << {k}"
        );
        let neg = huge(Huge { a: ones, b: p });
        let want = if k == 2047 {
            // 2^2047 is the most negative 2048-bit value: -1 · -2^2047 = 2^2047.
            bits::<4096>(1) << 2047
        } else {
            !bits::<4096>(0) << k
        };
        assert_eq!(neg.sfull, want, "-1 * 2^{k}");
    }
    for case in [
        Huge { a: ones, b: ones },
        Huge {
            a: x,
            b: one << 1000,
        },
    ] {
        assert_eq!(ir.evaluate_typed::<_, HugeOut>(case).unwrap(), huge(case));
    }
}
