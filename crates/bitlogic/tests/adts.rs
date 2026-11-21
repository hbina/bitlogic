//! Enums whose variants carry fields: derived layouts (a tag leaf, then every
//! variant's fields, inactive fields at their zero value), brace and tuple
//! construction, `match` with field bindings, `==`, run-time array reads, and
//! a register in a `system!`. Every state of a small machine is checked across
//! native Rust, the IR, Icarus HDL and an oracle written with plain integers.
mod support;
use bitlogic::{Bits, LogicValue, Native, Schema, bits, logic};
use support::check_hdl;

/// Its zero value (`Low`) is encoded as 1, so inactive `Store` fields hold 1.
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

/// Raw `Cmd` leaves: tag, Load.addr, Load.wide, Store.0, Store.1, Nest pair.
fn nop() -> Vec<u64> {
    vec![0, 0, 0, 0, 1, 0, 0]
}
fn load(addr: u64, wide: u64) -> Vec<u64> {
    vec![1, addr, wide, 0, 1, 0, 0]
}
fn store(addr: u64, mode: u64) -> Vec<u64> {
    vec![2, 0, 0, addr, mode, 0, 0]
}
fn nest(p0: u64, p1: u64) -> Vec<u64> {
    vec![3, 0, 0, 0, 1, p0, p1]
}

/// Independent model over raw leaves.
fn oracle(raw: &[u64]) -> Vec<u64> {
    let [tag, la, lw, sa, sm, n0, _, count, go] = raw.try_into().unwrap();
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
    let next_go = u64::from(next[..] == raw[..7] || next[0] == 3);
    let mut out = next;
    out.extend([next_count, next_go]);
    out
}

#[test]
fn every_state_matches_native_ir_hdl_and_oracle() {
    let ir = step::ir().unwrap();
    let mut cases = Vec::new();
    for cmd in all_cmds() {
        for count in 0..8 {
            for go in [false, true] {
                let m = Machine {
                    cmd,
                    count: bits(count),
                    go,
                };
                let raw = m.flatten();
                let want = oracle(&raw);
                let native = step(m);
                assert_eq!(native.flatten(), want, "native {m:?}");
                assert_eq!(
                    ir.evaluate_typed::<_, Machine>(m).unwrap(),
                    native,
                    "ir {m:?}"
                );
                cases.push((raw, want));
            }
        }
    }
    assert_eq!(cases.len(), 592);
    check_hdl(&ir, "step", &cases);
}

#[test]
fn layout_zero_value_and_canonical_reads() {
    let leaves: Vec<_> = Cmd::schema().leaves().into_iter().map(|(p, _)| p).collect();
    assert_eq!(
        leaves,
        [
            "tag",
            "Load.addr",
            "Load.wide",
            "Store.0",
            "Store.1",
            "Nest.0.pair.0",
            "Nest.0.pair.1"
        ]
    );
    let Schema::Tagged { tag, .. } = Cmd::schema() else {
        panic!("Cmd has fields");
    };
    assert_eq!(tag.width().unwrap(), 2);
    assert_eq!(Cmd::zeroed(), Cmd::Nop);
    assert_eq!(Cmd::Nop.flatten(), nop());
    assert_eq!(Cmd::Store(bits(5), Mode::High).flatten(), store(5, 2));
    for cmd in all_cmds() {
        assert_eq!(Cmd::from_values(&cmd.flatten()).unwrap(), cmd);
    }
    // Inactive fields must hold their zero values (Mode's is 1).
    let err = Cmd::from_values(&[0, 3, 0, 0, 1, 0, 0]).unwrap_err();
    assert!(err.0.contains("non-canonical"), "{err}");
    assert!(Cmd::from_values(&[0, 0, 0, 0, 0, 0, 0]).is_err());
    assert!(Cmd::from_values(&nop()[..6]).is_err());
}

#[test]
fn verilog_names_tag_encoding_and_variant_ports() {
    let verilog = step::ir().unwrap().to_verilog("step").unwrap();
    assert!(
        verilog.contains("// i_cmd_tag: adts::Cmd { Nop = 0, Load = 1, Store = 2, Nest = 3 }"),
        "{verilog}"
    );
    assert!(verilog.contains("// o_cmd_Store_1: adts::Mode { Low = 1, High = 2 }"));
    assert!(verilog.contains("input wire [2:0] i_cmd_Load_addr"));
    assert!(verilog.contains("output wire [0:0] o_cmd_Nest_0_pair_1"));
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
    let mut seen = Vec::new();
    for go in [true, false, false, true, false] {
        core.clock(seq::Inputs { go });
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
    assert!(files.iter().any(|(_, text)| text.contains("Run_left")));
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
struct CmdIn {
    state: Cmd,
}

#[logic]
fn hold(i: CmdIn) -> CmdIn {
    i
}

bitlogic::system! {
    mod nonzero {
        unit h: Cmd = hold(CmdIn { state });
        output now: Cmd = h.state;
    }
}

#[test]
fn registers_reject_nonzero_inactive_fields() {
    let err = nonzero::verilog().unwrap_err();
    assert!(err.0.contains("also inside variant fields"), "{err}");
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
    // Tag comparison only: inputs, one constant, one Eq and the AND seed.
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
    // Tag: four distinct constants, one is the base (3); each of the six
    // field leaves differs from its zero value in one arm only (6).
    assert_eq!(muxes, 9, "{:?}", ir.nodes());
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
