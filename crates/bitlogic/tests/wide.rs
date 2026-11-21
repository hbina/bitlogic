//! Values wider than 64 bits: native `Bits<N>` arithmetic, the IR and Icarus
//! HDL against a `u128` oracle, plus wide leaves in structs, enums with fields
//! and a `system!` register.
mod support;
use bitlogic::{BitVec, Bits, LogicValue, Native, bits, logic};
use support::{Rng, check_hdl};

const MASK100: u128 = (1 << 100) - 1;

fn b100(v: u128) -> Bits<100> {
    Bits::from_words(&[v as u64, (v >> 64) as u64])
}
fn b96(v: u128) -> Bits<96> {
    Bits::from_words(&[v as u64, (v >> 64) as u64])
}
fn u128_of(words: &[u64]) -> u128 {
    assert!(words[2..].iter().all(|&w| w == 0), "fits 128 bits");
    u128::from(words[0]) | (u128::from(*words.get(1).unwrap_or(&0)) << 64)
}
fn bv(v: u128) -> BitVec {
    BitVec::from_words(&[v as u64, (v >> 64) as u64])
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
struct Pair {
    a: Bits<100>,
    b: Bits<100>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
struct Wide {
    sum: Bits<100>,
    diff: Bits<100>,
    mixed: Bits<100>,
    left: Bits<100>,
    right: Bits<100>,
    lt: bool,
    eq: bool,
    /// `{a, b}`: 200 bits, four words.
    joined: Bits<200>,
    /// Bits 90..130 of `joined`: across the `b`/`a` boundary and a word edge.
    middle: Bits<40>,
    top_bit: bool,
}

#[logic]
fn wide(p: Pair) -> Wide {
    let joined = p.a.concat::<100, 200>(p.b);
    Wide {
        sum: p.a + p.b,
        diff: p.a - p.b,
        mixed: (p.a & p.b) | (!p.a ^ p.b),
        left: p.a << 70,
        right: p.a >> 65,
        lt: p.a < p.b,
        eq: p.a == p.b + bits::<100>(1),
        joined,
        middle: joined.slice::<40>(90),
        top_bit: joined.bit(199),
    }
}

/// The oracle, written with u128 arithmetic; `joined` as (high, low) halves.
fn oracle(a: u128, b: u128) -> (Vec<u128>, (u128, u128)) {
    let joined_low = b | ((a & ((1 << 28) - 1)) << 100);
    let joined_high = a >> 28;
    let middle = ((b >> 90) | (a << 10)) & ((1 << 40) - 1);
    let fields = vec![
        a.wrapping_add(b) & MASK100,
        a.wrapping_sub(b) & MASK100,
        ((a & b) | (!a ^ b)) & MASK100,
        (a << 70) & MASK100,
        a >> 65,
        u128::from(a < b),
        u128::from(a == (b + 1) & MASK100),
    ];
    let mut fields = fields;
    fields.push(middle);
    fields.push(a >> 99);
    (fields, (joined_high, joined_low))
}

fn sample(rng: &mut Rng) -> u128 {
    let v = (u128::from(rng.next()) << 64 | u128::from(rng.next())) & MASK100;
    // Bias toward carries, borrows and equal operands.
    match rng.next() % 6 {
        0 => MASK100,
        1 => 0,
        2 => v & 0xffff,
        _ => v,
    }
}

#[test]
fn wide_arithmetic_matches_native_ir_hdl_and_u128() {
    let ir = wide::ir().unwrap();
    let mut rng = Rng(0x5eed_1234_abcd_0001);
    let mut cases = Vec::new();
    for i in 0..600 {
        let a = sample(&mut rng);
        let b = if i % 7 == 0 {
            (a + MASK100) & MASK100
        } else {
            sample(&mut rng)
        };
        let p = Pair {
            a: b100(a),
            b: b100(b),
        };
        let native = wide(p);
        let (fields, (high, low)) = oracle(a, b);
        let got = [
            u128_of(native.sum.words()),
            u128_of(native.diff.words()),
            u128_of(native.mixed.words()),
            u128_of(native.left.words()),
            u128_of(native.right.words()),
            u128::from(native.lt),
            u128::from(native.eq),
            u128::from(native.middle.get()),
            u128::from(native.top_bit),
        ];
        assert_eq!(got.to_vec(), fields, "a={a:x} b={b:x}");
        let j = native.joined.words();
        assert_eq!(u128_of(&[j[0], j[1], 0]), low, "joined low");
        assert_eq!(u128_of(&[j[2], j[3], 0]), high, "joined high");
        assert_eq!(
            ir.evaluate_typed::<_, Wide>(p).unwrap(),
            native,
            "ir a={a:x} b={b:x}"
        );
        cases.push((p.flatten(), native.flatten()));
    }
    check_hdl(&ir, "wide", &cases);
}

#[test]
fn bits_words_ordering_and_get() {
    let big = Bits::<130>::from_words(&[1, 2, 3]);
    assert_eq!(big.words(), [1, 2, 3]);
    assert_eq!(
        Bits::<130>::from_words(&[1, 2, 7]).words(),
        [1, 2, 3],
        "cut to 130 bits"
    );
    assert!(Bits::<130>::from_words(&[0, 0, 1]) > Bits::<130>::from_words(&[u64::MAX, u64::MAX]));
    assert_eq!(big.to_u64(), None);
    assert_eq!(bits::<130>(9).to_u64(), Some(9));
    assert_eq!(bits::<64>(u64::MAX).get(), u64::MAX);
    assert_eq!(
        format!("{:?}", Bits::<70>::from_words(&[0, 1])),
        "Bits(0x10000000000000000)"
    );
    // Wrapping at a width that is not a multiple of 64.
    let max = !bits::<70>(0);
    assert_eq!((max + bits(1)).to_u64(), Some(0));
    assert_eq!((bits::<70>(0) - bits(1)), max);
    const WIDE: Bits<128> = bits(0x1234);
    assert_eq!(WIDE.to_u64(), Some(0x1234));
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
enum Frame {
    Empty,
    Header { tag: Bits<4>, body: Bits<96> },
    Raw(Bits<160>),
}

#[logic]
fn widen(f: Frame) -> Frame {
    match f {
        Frame::Empty => Frame::Raw(bits::<160>(1)),
        Frame::Header { tag, body } => {
            Frame::Raw(tag.concat::<96, 100>(body).concat::<60, 160>(bits::<60>(0)))
        }
        Frame::Raw(r) => Frame::Header {
            tag: r.slice::<4>(156),
            body: r.slice::<96>(0),
        },
    }
}

#[test]
fn data_enums_carry_wide_fields() {
    let ir = widen::ir().unwrap();
    let body = Bits::<96>::from_words(&[0xdead_beef_0123_4567, 0x89ab_cdef]);
    let raw = Bits::<160>::from_words(&[0x1111, 0x2222, 0xf000_0000]);
    let frames = [
        Frame::Empty,
        Frame::Header {
            tag: bits(0xa),
            body,
        },
        Frame::Raw(raw),
    ];
    let mut cases = Vec::new();
    for f in frames {
        let native = widen(f);
        assert_eq!(ir.evaluate_typed::<_, Frame>(f).unwrap(), native, "{f:?}");
        assert_eq!(Frame::from_values(&f.flatten()).unwrap(), f);
        cases.push((f.flatten(), native.flatten()));
    }
    let Frame::Raw(r) = widen(Frame::Header {
        tag: bits(0xa),
        body,
    }) else {
        panic!("Header widens to Raw");
    };
    assert_eq!(r.slice::<4>(156), bits::<4>(0xa));
    assert_eq!(r.slice::<96>(60), body);
    assert_eq!(
        widen(Frame::Raw(raw)),
        Frame::Header {
            tag: bits(0xf),
            body: raw.slice::<96>(0)
        }
    );
    check_hdl(&ir, "widen", &cases);
    // The payload is one 160-bit leaf (Raw, the widest variant); one bit more
    // is rejected.
    let widths: Vec<_> = Frame::schema()
        .leaves()
        .iter()
        .map(|(_, leaf)| leaf.width().unwrap())
        .collect();
    assert_eq!(widths, [2, 160]);
    assert!(Frame::from_values(&[bv(2), BitVec::from_words(&[0, 0, 1 << 32])]).is_err());
    assert!(Frame::from_values(&[bv(2), BitVec::from_words(&[0, 0, 1 << 31])]).is_ok());
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
struct AccIn {
    state: Bits<96>,
    step: Bits<96>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
struct AccOut {
    state: Bits<96>,
}

#[logic]
fn accumulate(i: AccIn) -> AccOut {
    AccOut {
        state: i.state + i.step,
    }
}

bitlogic::system! {
    mod acc {
        param step: Bits<96> = Bits::from_words(&[u64::MAX, 1]);
        unit a: Bits<96> = accumulate(AccIn { state, step });
        output total: Bits<96> = a.state;
    }
}

#[test]
fn wide_registers_and_params_in_a_system() {
    let step = (1u128 << 65) - 1;
    let mut native = acc::Core::new(Native, b96(step));
    // The same through the IR interpreter.
    let mut ir = acc::Core::new(bitlogic::Interpreted::new(), b96(step));
    for _ in 0..3 {
        native.clock(acc::Inputs {});
        ir.clock(acc::Inputs {});
    }
    let total = native.outputs().total;
    assert_eq!(u128_of(&[total.words()[0], total.words()[1], 0]), 3 * step);
    assert_eq!(ir.outputs().total, total);
    let files = acc::verilog().unwrap();
    let core_v = &files[0].1;
    assert!(core_v.contains("96'h1ffffffffffffffff"), "{core_v}");
    // Icarus elaborates and runs the generated core: after reset and three
    // edges the total is 3 * step.
    let dir = tempfile::tempdir().unwrap();
    let mut sources = Vec::new();
    for (name, text) in &files {
        let path = dir.path().join(name);
        std::fs::write(&path, text).unwrap();
        sources.push(path);
    }
    let tb = dir.path().join("tb.v");
    std::fs::write(
        &tb,
        format!(
            "module tb;\nreg clk = 0, rst = 1;\nwire [95:0] total;\n\
             acc dut(.clk(clk), .rst(rst), .total(total));\n\
             initial begin\n#1 clk = 1; #1 clk = 0; rst = 0;\n\
             repeat (3) begin #1 clk = 1; #1 clk = 0; end\n\
             if (total !== 96'h{:x}) begin $display(\"total=%h\", total); $fatal(1); end\n\
             $display(\"HDL PASS\"); $finish;\nend\nendmodule\n",
            3 * step
        ),
    )
    .unwrap();
    let exe = dir.path().join("tb.vvp");
    let compile = std::process::Command::new("iverilog")
        .args(["-g2012", "-s", "tb", "-o"])
        .arg(&exe)
        .arg(&tb)
        .args(&sources)
        .output()
        .expect("Icarus Verilog is required");
    assert!(
        compile.status.success(),
        "{}",
        String::from_utf8_lossy(&compile.stderr)
    );
    let run = std::process::Command::new("vvp")
        .arg(&exe)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&run.stdout);
    assert!(
        run.status.success() && stdout.contains("HDL PASS"),
        "{stdout}"
    );
}
