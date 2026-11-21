//! Enums whose variants carry fields: the tagged-union layout (a tag leaf and
//! one payload leaf holding the active variant's fields from bit 0, the bits
//! above them don't-care), brace and tuple construction, `match` with field
//! bindings, tag-aware `==`, run-time array reads, and registers in a
//! `system!`. Every state of a small machine is checked across native Rust,
//! the IR, Icarus HDL and an oracle written with plain integers, with random
//! bits in the don't-care payload bits of the inputs.
mod support;
use bitlogic::{BitVec, Bits, LogicValue, Native, Schema, bits, logic};
use support::{Rng, check_hdl, with_padding};

/// Its zero value (`Low`) is encoded as 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
enum Mode {
    Low = 1,
    High = 2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
struct Inner {
    pair: [bool; 2],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
enum Cmd {
    Nop,
    Load { addr: Bits<3>, wide: bool },
    Store(Bits<3>, Mode),
    Nest(Inner),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
struct Machine {
    cmd: Cmd,
    count: Bits<3>,
    go: bool,
}

#[logic]
fn classify(c: Cmd) -> Bits<2> {
    match c {
        Cmd::Nop => bits::<2>(0),
        Cmd::Load { .. } => bits::<2>(1),
        Cmd::Store(..) => bits::<2>(2),
        Cmd::Nest(_) => bits::<2>(3),
    }
}

#[logic]
fn step(m: Machine) -> Machine {
    let cmd = match m.cmd {
        Cmd::Nop => {
            if m.go {
                Cmd::Load {
                    addr: m.count,
                    wide: false,
                }
            } else {
                Cmd::Nop
            }
        }
        Cmd::Load { addr, wide: w } => {
            if w {
                Cmd::Store(addr, Mode::High)
            } else {
                Cmd::Load {
                    wide: true,
                    addr: addr + bits::<3>(1),
                }
            }
        }
        Cmd::Store(a, _) => Cmd::Nest(Inner {
            pair: [a.bit(0), m.go],
        }),
        Cmd::Nest(inner) => {
            if inner.pair[0] {
                Cmd::Nop
            } else {
                Cmd::Store(m.count, Mode::Low)
            }
        }
    };
    let count = match m.cmd {
        Cmd::Load { .. } | Cmd::Nest(..) => m.count + bits::<3>(1),
        Cmd::Store(_, mode) => match mode {
            Mode::Low => bits::<3>(0),
            Mode::High => m.count,
        },
        _ => m.count,
    };
    Machine {
        cmd,
        count,
        go: cmd == m.cmd || classify(cmd) == bits::<2>(3),
    }
}

/// Every `Cmd` value: 1 + 16 + 16 + 4.
fn all_cmds() -> Vec<Cmd> {
    let mut all = vec![Cmd::Nop];
    for a in 0..8 {
        for wide in [false, true] {
            all.push(Cmd::Load {
                addr: bits(a),
                wide,
            });
        }
        for mode in [Mode::Low, Mode::High] {
            all.push(Cmd::Store(bits(a), mode));
        }
    }
    for p0 in [false, true] {
        for p1 in [false, true] {
            all.push(Cmd::Nest(Inner { pair: [p0, p1] }));
        }
    }
    all
}

/// Raw `Cmd` leaves, `[tag, payload]` with zero padding. Payload bits:
/// Load addr [2:0] wide [3]; Store addr [2:0] mode [4:3]; Nest pair [1:0].
fn nop() -> Vec<u64> {
    vec![0, 0]
}
fn load(addr: u64, wide: u64) -> Vec<u64> {
    vec![1, addr | wide << 3]
}
fn store(addr: u64, mode: u64) -> Vec<u64> {
    vec![2, addr | mode << 3]
}
fn nest(p0: u64, p1: u64) -> Vec<u64> {
    vec![3, p0 | p1 << 1]
}

/// Independent model over raw leaves (any padding).
fn oracle(raw: &[u64]) -> Vec<u64> {
    let [tag, payload, count, go] = raw.try_into().unwrap();
    let (la, lw) = (payload & 7, payload >> 3 & 1);
    let (sa, sm) = (payload & 7, payload >> 3 & 3);
    let n0 = payload & 1;
    // The input as a canonical encoding, for `cmd == m.cmd`.
    let current = match tag {
        0 => nop(),
        1 => load(la, lw),
        2 => store(sa, sm),
        _ => nest(n0, payload >> 1 & 1),
    };
    let next = match tag {
        0 if go == 1 => load(count, 0),
        0 => nop(),
        1 if lw == 1 => store(la, 2),
        1 => load((la + 1) % 8, 1),
        2 => nest(sa & 1, go),
        _ if n0 == 1 => nop(),
        _ => store(count, 1),
    };
    let next_count = match tag {
        1 | 3 => (count + 1) % 8,
        2 if sm == 1 => 0,
        _ => count,
    };
    let next_go = u64::from(next == current || next[0] == 3);
    let mut out = next;
    out.extend([next_count, next_go]);
    out
}

#[test]
fn every_state_matches_native_ir_hdl_and_oracle() {
    let ir = step::ir().unwrap();
    let mut rng = Rng(0xadd5_0001);
    let mut cases = Vec::new();
    let mut padded_inputs = 0;
    for cmd in all_cmds() {
        for count in 0..8 {
            for go in [false, true] {
                let m = Machine {
                    cmd,
                    count: bits(count),
                    go,
                };
                let native = step(m);
                assert_eq!(
                    native.flatten(),
                    oracle(&support::raw(&m.flatten())),
                    "native {m:?}"
                );
                assert_eq!(
                    ir.evaluate_typed::<_, Machine>(m).unwrap(),
                    native,
                    "ir {m:?}"
                );
                // The same value with random don't-care bits.
                let padded = with_padding(&Machine::schema(), &m.flatten(), &mut rng);
                padded_inputs += usize::from(padded != m.flatten());
                let raw = support::raw(&padded);
                let want = oracle(&raw);
                assert_eq!(Machine::from_values(&want).unwrap(), native, "oracle {m:?}");
                let out = ir.evaluate(&padded).unwrap();
                assert_eq!(
                    Machine::from_values(&out).unwrap(),
                    native,
                    "padded ir {m:?}"
                );
                cases.push((raw, want));
            }
        }
    }
    assert_eq!(cases.len(), 592);
    assert!(padded_inputs > 150, "{padded_inputs} inputs had padding");
    check_hdl(&ir, "step", &cases);
}

fn layout(schema: &Schema, variant: &str) -> Vec<(String, u16, u16)> {
    schema.variant_layout(variant).unwrap()
}
fn slot(path: &str, offset: u16, width: u16) -> (String, u16, u16) {
    (path.into(), offset, width)
}

#[test]
fn layout_zero_value_and_reads() {
    let schema = Cmd::schema();
    let leaves: Vec<_> = schema
        .leaves()
        .into_iter()
        .map(|(p, leaf)| (p, leaf.width().unwrap()))
        .collect();
    // The payload is as wide as the widest variant (Store: 3 + 2 bits).
    assert_eq!(leaves, [("tag".to_string(), 2), ("payload".to_string(), 5)]);
    assert_eq!(layout(&schema, "Nop"), []);
    assert_eq!(
        layout(&schema, "Load"),
        [slot("addr", 0, 3), slot("wide", 3, 1)]
    );
    assert_eq!(layout(&schema, "Store"), [slot("0", 0, 3), slot("1", 3, 2)]);
    assert_eq!(
        layout(&schema, "Nest"),
        [slot("0.pair.0", 0, 1), slot("0.pair.1", 1, 1)]
    );
    assert_eq!(Cmd::zeroed(), Cmd::Nop);
    assert_eq!(Cmd::Nop.flatten(), nop());
    assert_eq!(Cmd::Store(bits(5), Mode::High).flatten(), store(5, 2));
    for cmd in all_cmds() {
        assert_eq!(Cmd::from_values(&cmd.flatten()).unwrap(), cmd);
    }
    // Payload bits above the active variant are ignored...
    assert_eq!(Cmd::from_values(&[0u64, 31]).unwrap(), Cmd::Nop);
    assert_eq!(
        Cmd::from_values(&[1u64, 0b1_0101]).unwrap(),
        Cmd::Load {
            addr: bits(5),
            wide: false
        }
    );
    assert_eq!(
        Cmd::from_values(&[3u64, 0b1_1110]).unwrap(),
        Cmd::Nest(Inner {
            pair: [false, true]
        })
    );
    // ...but the active variant's fields are checked (Mode has no 0), and
    // so are the tag and the payload width.
    assert!(Cmd::from_values(&[2u64, 0b00_101]).is_err());
    assert!(Seq::from_values(&[3u64, 0]).is_err());
    assert!(Cmd::from_values(&[0u64, 32]).is_err());
    assert!(Cmd::from_values(&[0u64]).is_err());
    // The encoding helpers.
    let bv = BitVec::from;
    assert_eq!(
        schema.encode_variant("Store", &[bv(5), bv(2)]).unwrap(),
        store(5, 2)
    );
    assert_eq!(
        schema.decode_variant(&[bv(1), bv(0b1_0101)]).unwrap(),
        ("Load".to_string(), vec![bv(5), bv(0)])
    );
    assert_eq!(
        schema
            .valid_bits(&load(5, 0).iter().map(|&v| bv(v)).collect::<Vec<_>>())
            .unwrap(),
        [3, 0b1111]
    );
    assert_eq!(schema.valid_bits(&[bv(0), bv(0)]).unwrap(), [3, 0]);
    assert_eq!(schema.valid_bits(&[bv(2), bv(0)]).unwrap(), [3, 0b11111]);
}

#[test]
fn verilog_names_tag_encoding_and_payload_layout() {
    let verilog = step::ir().unwrap().to_verilog("step").unwrap();
    assert!(
        verilog.contains("// i_cmd_tag: adts::Cmd { Nop = 0, Load = 1, Store = 2, Nest = 3 }"),
        "{verilog}"
    );
    assert!(
        verilog.contains(
            "// o_cmd_payload: adts::Cmd payload { Nop, Load { addr [2:0], wide [3:3] }, \
             Store { 0 [2:0], 1 [4:3] }, Nest { 0.pair.0 [0:0], 0.pair.1 [1:1] } }"
        ),
        "{verilog}"
    );
    assert!(verilog.contains("input wire [4:0] i_cmd_payload"));
    assert!(verilog.contains("output wire [4:0] o_cmd_payload"));
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
struct Pick {
    cmds: [Cmd; 3],
    index: Bits<2>,
}

/// A run-time read; index 3 is out of range and yields `Cmd::zeroed()`.
#[logic]
fn pick(p: Pick) -> Cmd {
    p.cmds.at(p.index)
}

#[test]
fn arrays_of_data_enums_select_whole_values() {
    let ir = pick::ir().unwrap();
    let some: Vec<_> = all_cmds().into_iter().step_by(5).collect();
    let mut cases = Vec::new();
    for (i, &a) in some.iter().enumerate() {
        let b = some[(i + 3) % some.len()];
        let c = some[(i + 5) % some.len()];
        for index in 0..4 {
            let p = Pick {
                cmds: [a, b, c],
                index: bits(index),
            };
            let want = [a, b, c].get(index as usize).copied().unwrap_or(Cmd::Nop);
            assert_eq!(pick(p), want);
            assert_eq!(ir.evaluate_typed::<_, Cmd>(p).unwrap(), want);
            cases.push((p.flatten(), want.flatten()));
        }
    }
    check_hdl(&ir, "pick", &cases);
}

/// A register state type: its zero value is all-zero bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
enum Seq {
    Idle,
    Run { left: Bits<2> },
    Done(bool),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
struct SeqIn {
    state: Seq,
    go: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
struct SeqOut {
    state: Seq,
}

#[logic]
fn sequence(i: SeqIn) -> SeqOut {
    SeqOut {
        state: match i.state {
            Seq::Idle => {
                if i.go {
                    Seq::Run { left: bits::<2>(2) }
                } else {
                    Seq::Idle
                }
            }
            Seq::Run { left } => {
                if left == bits::<2>(0) {
                    Seq::Done(i.go)
                } else {
                    Seq::Run {
                        left: left - bits::<2>(1),
                    }
                }
            }
            Seq::Done(_) => Seq::Idle,
        },
    }
}

bitlogic::system! {
    mod seq {
        input go: bool;
        unit s: Seq = sequence(SeqIn { state, go });
        output now: Seq = s.state;
    }
}

#[test]
fn data_enum_registers_in_a_system() {
    let mut core = seq::Core::new(Native);
    let mut interpreted = seq::Core::new(bitlogic::Interpreted::new());
    let mut seen = Vec::new();
    for go in [true, false, false, true, false] {
        core.clock(seq::Inputs { go });
        interpreted.clock(seq::Inputs { go });
        assert_eq!(interpreted.outputs().now, core.outputs().now);
        seen.push(core.outputs().now);
    }
    assert_eq!(
        seen,
        [
            Seq::Run { left: bits(2) },
            Seq::Run { left: bits(1) },
            Seq::Run { left: bits(0) },
            Seq::Done(true),
            Seq::Idle,
        ]
    );
    let files = seq::verilog().unwrap();
    assert!(files.iter().any(|(_, text)| text.contains("state_payload")));
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
struct CmdIn {
    state: Cmd,
}

#[logic]
fn hold(i: CmdIn) -> CmdIn {
    i
}

/// Its zero value is `First(Mode::Low)`, whose payload is 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
enum Bad {
    First(Mode),
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
struct BadIn {
    state: Bad,
}

#[logic]
fn hold_bad(i: BadIn) -> BadIn {
    i
}

bitlogic::system! {
    mod cmd_reg {
        unit h: Cmd = hold(CmdIn { state });
        output now: Cmd = h.state;
    }
}

bitlogic::system! {
    mod bad_reg {
        unit h: Bad = hold_bad(BadIn { state });
        output now: Bad = h.state;
    }
}

/// A register resets to zero bits: `Cmd`'s zero value (`Nop`) is, `Bad`'s
/// (`First(Mode::Low)`, payload 1) is not.
#[test]
fn registers_need_an_all_zero_zero_value() {
    assert!(cmd_reg::verilog().is_ok());
    let err = bad_reg::verilog().unwrap_err();
    assert!(err.0.contains("first variant's fields"), "{err}");
    let _ = Bad::Other;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, LogicValue)]
enum Ordered {
    A(bool),
    B,
}

#[logic]
fn less(pair: [Ordered; 2]) -> bool {
    pair[0] < pair[1]
}

#[test]
fn data_enums_support_only_equality() {
    let err = less::ir().unwrap_err();
    assert!(err.0.contains("only == and !="), "{err}");
    let _ = [Ordered::A(true), Ordered::B];
}

#[logic]
fn known(c: Cmd) -> bool {
    c == Cmd::Nop || Cmd::Store(bits::<3>(5), Mode::High) == c
}

#[logic]
fn known_nop(c: Cmd) -> bool {
    c == Cmd::Nop
}

/// Against a constant, `==` compares the tag and that variant's fields only.
#[test]
fn equality_with_a_known_variant_reads_only_its_leaves() {
    let ir = known::ir().unwrap();
    let mut cases = Vec::new();
    for cmd in all_cmds() {
        let want = matches!(cmd, Cmd::Nop) || cmd == Cmd::Store(bits(5), Mode::High);
        assert_eq!(known(cmd), want);
        assert_eq!(ir.evaluate_typed::<_, bool>(cmd).unwrap(), want, "{cmd:?}");
        cases.push((cmd.flatten(), vec![u64::from(want)]));
    }
    check_hdl(&ir, "known", &cases);
    // `Nop` has no fields: one comparison, of the tag.
    let nop = known_nop::ir().unwrap();
    let compares = nop
        .nodes()
        .iter()
        .filter(|n| {
            matches!(
                n.operation,
                bitlogic::Operation::Binary(bitlogic::Binary::Eq, ..)
            )
        })
        .count();
    assert_eq!(compares, 1, "{:?}", nop.nodes());
}

/// Rebuilds its input variant by variant.
#[logic]
fn rebuild(c: Cmd) -> Cmd {
    match c {
        Cmd::Nop => Cmd::Nop,
        Cmd::Load { addr, wide } => Cmd::Load { addr, wide },
        Cmd::Store(a, m) => Cmd::Store(a, m),
        Cmd::Nest(i) => Cmd::Nest(i),
    }
}

/// A `match` whose arms test distinct variants, with a tested last arm, is
/// one-hot: each leaf starts from its most common constant and muxes in only
/// the arms that differ, instead of one mux per arm.
#[test]
fn one_hot_matches_mux_only_differing_arms() {
    let ir = rebuild::ir().unwrap();
    let mut cases = Vec::new();
    for cmd in all_cmds() {
        assert_eq!(ir.evaluate_typed::<_, Cmd>(cmd).unwrap(), cmd);
        cases.push((cmd.flatten(), cmd.flatten()));
    }
    check_hdl(&ir, "rebuild", &cases);
    let muxes = ir
        .nodes()
        .iter()
        .filter(|n| matches!(n.operation, bitlogic::Operation::Mux { .. }))
        .count();
    // Tag: four distinct constants, one is the base (3). The payload needs
    // none: each arm rebuilds a prefix of the input payload and leaves the
    // rest undefined, so every split mux folds to the input.
    assert_eq!(muxes, 3, "{:?}", ir.nodes());
    let payload = ir.outputs()[1].value;
    assert!(matches!(
        ir.nodes()[payload.index()].operation,
        bitlogic::Operation::Input(1)
    ));
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
enum Outer {
    Empty,
    Wrap(Seq, Bits<2>),
}

fn all_outers() -> Vec<Outer> {
    let seqs = [
        Seq::Idle,
        Seq::Run { left: bits(0) },
        Seq::Run { left: bits(3) },
        Seq::Done(false),
        Seq::Done(true),
    ];
    let mut all = vec![Outer::Empty];
    for s in seqs {
        for b in 0..4 {
            all.push(Outer::Wrap(s, bits(b)));
        }
    }
    all
}

#[test]
fn nested_data_enums_pack_recursively() {
    let schema = Outer::schema();
    // Seq: a 2-bit tag and a 2-bit payload (Run's left), then the Bits<2>.
    assert_eq!(
        layout(&schema, "Wrap"),
        [
            slot("0.tag", 0, 2),
            slot("0.payload", 2, 2),
            slot("1", 4, 2)
        ]
    );
    let value = Outer::Wrap(Seq::Done(true), bits(2));
    let leaves = value.flatten();
    assert_eq!(leaves, [1, 0b10_01_10]);
    // Done uses one bit of Seq's payload: bit 3 is don't-care too.
    assert_eq!(schema.valid_bits(&leaves).unwrap(), [1, 0b11_01_11]);
    for o in all_outers() {
        assert_eq!(Outer::from_values(&o.flatten()).unwrap(), o);
    }
}

#[logic]
fn same(pair: [Cmd; 2]) -> bool {
    pair[0] == pair[1]
}

#[logic]
fn differ(pair: [Outer; 2]) -> bool {
    pair[0] != pair[1]
}

/// `==` between two run-time values compares tags and the active variant's
/// fields only, as Rust's derived `PartialEq`, whatever the padding holds.
#[test]
fn equality_of_unknown_values_ignores_padding() {
    let mut rng = Rng(0xe9a1_0002);
    let ir = same::ir().unwrap();
    let mut cases = Vec::new();
    let cmds = all_cmds();
    for &a in &cmds {
        for &b in &cmds {
            let want = a == b;
            assert_eq!(same([a, b]), want);
            let padded = with_padding(&<[Cmd; 2]>::schema(), &[a, b].flatten(), &mut rng);
            assert_eq!(
                ir.evaluate(&padded).unwrap(),
                [u64::from(want)],
                "{a:?} {b:?}"
            );
            cases.push((padded, vec![u64::from(want)]));
        }
    }
    check_hdl(&ir, "same", &cases);
    let ir = differ::ir().unwrap();
    let mut cases = Vec::new();
    let outers = all_outers();
    for &a in &outers {
        for &b in &outers {
            let want = a != b;
            assert_eq!(differ([a, b]), want);
            let padded = with_padding(&<[Outer; 2]>::schema(), &[a, b].flatten(), &mut rng);
            assert_eq!(
                ir.evaluate(&padded).unwrap(),
                [u64::from(want)],
                "{a:?} {b:?}"
            );
            cases.push((padded, vec![u64::from(want)]));
        }
    }
    check_hdl(&ir, "differ", &cases);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
enum Quote {
    Ask { bid: Bits<8> },
    Emit { bid: Bits<8>, ask: Bits<8> },
}

/// `bid` is the first field of both variants, so it sits at the same payload
/// bits: moving it between them is wires, not muxes.
#[logic]
fn advance(q: Quote) -> Quote {
    match q {
        Quote::Ask { bid } => Quote::Emit {
            bid,
            ask: bits::<8>(1),
        },
        Quote::Emit { bid, .. } => Quote::Ask { bid },
    }
}

#[test]
fn shared_prefix_fields_move_without_muxes() {
    let ir = advance::ir().unwrap();
    let mut rng = Rng(0x9007_e003);
    let mut cases = Vec::new();
    for bid in [0, 1, 0x5a, 0xff] {
        for q in [
            Quote::Ask { bid: bits(bid) },
            Quote::Emit {
                bid: bits(bid),
                ask: bits(0xc3),
            },
        ] {
            let native = advance(q);
            let padded = with_padding(&Quote::schema(), &q.flatten(), &mut rng);
            let out = ir.evaluate(&padded).unwrap();
            assert_eq!(Quote::from_values(&out).unwrap(), native, "{q:?}");
            cases.push((padded, native.flatten()));
        }
    }
    check_hdl(&ir, "advance", &cases);
    // Only the 1-bit tag is muxed; the payload is {8'h01, payload[7:0]}.
    let muxes: Vec<_> = ir
        .nodes()
        .iter()
        .filter(|n| matches!(n.operation, bitlogic::Operation::Mux { .. }))
        .map(|n| n.width)
        .collect();
    assert!(muxes.iter().all(|&w| w == 1), "{:?}", ir.nodes());
}

#[logic]
fn moving(c: Cmd) -> bool {
    matches!(c, Cmd::Load { .. } | Cmd::Store(..)) && !matches!(c, Cmd::Load { wide: _, addr: _ },)
}

#[test]
fn matches_macro_tests_variants() {
    let ir = moving::ir().unwrap();
    for cmd in all_cmds() {
        let want = matches!(cmd, Cmd::Store(..));
        assert_eq!(moving(cmd), want);
        assert_eq!(ir.evaluate_typed::<_, bool>(cmd).unwrap(), want, "{cmd:?}");
    }
}
