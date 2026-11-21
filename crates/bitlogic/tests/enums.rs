//! Fieldless enums: derived encodings (implicit and explicit discriminants),
//! variant constants, `match` with alternatives and `_`, `const` patterns on
//! `Bits` and `bool`, and named encodings in the generated Verilog. Every
//! state of a small machine is checked across native Rust, the IR, Icarus HDL
//! and an oracle written with plain integers.
mod support;
use bitlogic::{Bits, LogicValue, Schema, bits, logic};
use support::check_hdl;

#[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
enum Phase {
    Idle,
    Parse,
    Prep,
    Copy,
}

/// Explicit, sparse discriminants: 4 bits wide, and the zero value is `Low`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
enum Mode {
    Low = 1,
    Mid = 4,
    High = 9,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
struct Machine {
    phase: Phase,
    count: Bits<4>,
    mode: Mode,
    flags: [bool; 2],
}

const LIMIT: Bits<4> = bits(5);
const ZERO: Bits<4> = bits(0);

#[logic]
fn advance(phase: Phase) -> Phase {
    match phase {
        Phase::Idle => Phase::Parse,
        Phase::Parse => Phase::Prep,
        Phase::Prep => Phase::Copy,
        Phase::Copy => Phase::Idle,
    }
}

#[logic]
fn step(m: Machine) -> Machine {
    let phase = match m.phase {
        Phase::Idle => {
            if m.flags[0] {
                Phase::Parse
            } else {
                Phase::Idle
            }
        }
        Phase::Parse | Phase::Prep => {
            if m.count == LIMIT {
                advance(m.phase)
            } else {
                m.phase
            }
        }
        _ => Phase::Idle,
    };
    let count = match m.count {
        LIMIT => ZERO,
        ZERO => match m.flags[1] {
            true => bits::<4>(9),
            false => bits::<4>(1),
        },
        _ => m.count + bits::<4>(1),
    };
    Machine {
        phase,
        count,
        mode: match m.mode {
            Mode::Low => Mode::Mid,
            Mode::Mid => Mode::High,
            Mode::High => Mode::Low,
        },
        flags: [m.flags[1], phase == Phase::Copy || m.phase == Phase::Copy],
    }
}

/// Independent model over raw encodings.
fn oracle(phase: u64, count: u64, mode: u64, f0: bool, f1: bool) -> Vec<u64> {
    let next_phase = match phase {
        0 => u64::from(f0),
        1 | 2 if count == 5 => phase + 1,
        1 | 2 => phase,
        _ => 0,
    };
    let next_count = match count {
        5 => 0,
        0 if f1 => 9,
        0 => 1,
        c => (c + 1) % 16,
    };
    let next_mode = match mode {
        1 => 4,
        4 => 9,
        _ => 1,
    };
    vec![
        next_phase,
        next_count,
        next_mode,
        u64::from(f1),
        u64::from(next_phase == 3 || phase == 3),
    ]
}

#[test]
fn every_state_matches_native_ir_hdl_and_oracle() {
    let ir = step::ir().unwrap();
    let mut cases = Vec::new();
    for phase in [Phase::Idle, Phase::Parse, Phase::Prep, Phase::Copy] {
        for count in 0..16 {
            for mode in [Mode::Low, Mode::Mid, Mode::High] {
                for flags in [[false, false], [false, true], [true, false], [true, true]] {
                    let m = Machine {
                        phase,
                        count: bits(count),
                        mode,
                        flags,
                    };
                    let raw = support::raw(&m.flatten());
                    let want = oracle(raw[0], raw[1], raw[2], flags[0], flags[1]);
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
    }
    assert_eq!(cases.len(), 768);
    check_hdl(&ir, "step", &cases);
}

#[test]
fn encodings_widths_and_zero_values() {
    let Schema::Enum {
        width, variants, ..
    } = Phase::schema()
    else {
        panic!("Phase is an enum");
    };
    assert_eq!(width, 2);
    assert_eq!(variants[3], ("Copy".to_string(), 3));
    let Schema::Enum { width, .. } = Mode::schema() else {
        panic!("Mode is an enum");
    };
    assert_eq!(width, 4);
    assert_eq!(Mode::High.flatten(), [9]);
    assert_eq!(Mode::zeroed(), Mode::Low);
    assert!(Mode::from_values(&[2]).is_err(), "2 is not a Mode");
    // Distinct enums with identical variants are distinct types.
    #[derive(Clone, Copy, LogicValue)]
    enum Other {
        Idle,
        Parse,
        Prep,
        Copy,
    }
    assert_ne!(Other::schema(), Phase::schema());
    let _ = [Other::Idle, Other::Parse, Other::Prep, Other::Copy];
}

#[test]
fn verilog_names_enum_encodings() {
    let verilog = step::ir().unwrap().to_verilog("step").unwrap();
    assert!(
        verilog.contains("// i_phase: enums::Phase { Idle = 0, Parse = 1, Prep = 2, Copy = 3 }"),
        "{verilog}"
    );
    assert!(verilog.contains("// o_mode: enums::Mode { Low = 1, Mid = 4, High = 9 }"));
    assert!(verilog.contains("input wire [3:0] i_mode"));
}

const ALSO_LIMIT: Bits<4> = bits(5);

/// Overlapping and unreachable arms: the first matching arm wins, as in Rust.
#[allow(unreachable_patterns)]
#[logic]
fn priority(n: Bits<4>) -> Bits<2> {
    match n {
        LIMIT => bits::<2>(1),
        ALSO_LIMIT => bits::<2>(2),
        ZERO | LIMIT => bits::<2>(3),
        _ => bits::<2>(0),
        ZERO => bits::<2>(2),
    }
}

#[test]
fn first_matching_arm_wins() {
    let ir = priority::ir().unwrap();
    for n in 0..16 {
        let want = match n {
            5 => 1,
            0 => 3,
            _ => 0,
        };
        assert_eq!(priority(bits(n)).get(), want, "native {n}");
        assert_eq!(
            ir.evaluate_typed::<_, Bits<2>>(bits::<4>(n)).unwrap().get(),
            want,
            "ir {n}"
        );
    }
}
