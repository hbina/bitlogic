//! `unit ..., pipeline`: rewrite rules put registers around a step
//! function's multiplies, and the unit holds its state while they fill, only
//! in the phase that uses the products. The same step function, with and
//! without `pipeline`, computes the same values; the pipelined one's
//! Verilog matches its cycle model under Icarus, cycle by cycle.
mod support;
use bitlogic::{Bits, Interpreted, LogicValue, Native, bits, logic};
use std::{fmt::Write, process::Command};
use support::Rng;

#[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
pub enum Phase {
    Idle,
    Mult,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
pub struct MacState {
    phase: Phase,
    a: Bits<25>,
    b: Bits<18>,
    acc: Bits<48>,
    square: Bits<36>,
    done: Bits<16>,
    /// Edges the unit did not hold on.
    steps: Bits<16>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
pub struct MacIn {
    state: MacState,
    k: Bits<48>,
    go: bool,
    a: Bits<25>,
    b: Bits<18>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
pub struct MacOut {
    state: MacState,
    busy: bool,
}

/// Latch `a` and `b` on `go`; next cycle, `acc += a b + k` and `square = b²`
/// (signed).
#[logic]
fn mac(i: MacIn) -> MacOut {
    let s = i.state;
    let mult = s.phase == Phase::Mult;
    let load = !mult && i.go;
    let product = bits::<5>(0).concat::<43, 48>(s.a.mul_wide::<18, 43>(s.b));
    MacOut {
        state: MacState {
            phase: if load { Phase::Mult } else { Phase::Idle },
            a: if load { i.a } else { s.a },
            b: if load { i.b } else { s.b },
            acc: if mult { s.acc + product + i.k } else { s.acc },
            square: if mult {
                s.b.smul_wide::<18, 36>(s.b)
            } else {
                s.square
            },
            done: if mult { s.done + bits::<16>(1) } else { s.done },
            steps: s.steps + bits::<16>(1),
        },
        busy: mult,
    }
}

bitlogic::system! {
    mod plain {
        param k: Bits<48> = bits(7);
        input go: bool;
        input a: Bits<25>;
        input b: Bits<18>;
        unit mac: MacState = mac(MacIn { state, k, go, a, b });
        output acc: Bits<48> = mac.state.acc;
        output square: Bits<36> = mac.state.square;
        output done: Bits<16> = mac.state.done;
        output steps: Bits<16> = mac.state.steps;
        output busy: bool = mac.busy, registered;
    }
}

bitlogic::system! {
    mod piped {
        param k: Bits<48> = bits(7);
        input go: bool;
        input a: Bits<25>;
        input b: Bits<18>;
        unit mac: MacState = mac(MacIn { state, k, go, a, b }), pipeline;
        output acc: Bits<48> = mac.state.acc;
        output square: Bits<36> = mac.state.square;
        output done: Bits<16> = mac.state.done;
        output steps: Bits<16> = mac.state.steps;
        output busy: bool = mac.busy, registered;
    }
}

/// Operands chosen by how many products are done, so both systems see the
/// same ones whatever their timing.
fn operands(done: u64) -> (u64, u64) {
    let mut rng = Rng(0x9e37_79b9 ^ done);
    (rng.next() % (1 << 25), rng.next() % (1 << 18))
}

#[test]
fn report_names_the_phase_and_latency() {
    let reports = piped::design().unwrap().pipeline_reports().unwrap();
    assert_eq!(reports.len(), 1);
    let (unit, report) = &reports[0];
    assert_eq!(*unit, "mac");
    assert!(report.contains("2 multiplies"), "{report}");
    assert!(report.contains("6 registers"), "{report}");
    assert!(report.contains("latency 2"), "{report}");
    assert!(report.contains("state.phase"), "{report}");
    assert!(
        plain::design()
            .unwrap()
            .pipeline_reports()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn pipelined_unit_computes_the_same_values_holding_two_cycles() {
    let mut plain = plain::Core::new(Native, bits(7));
    let mut piped = piped::Core::new(Native, bits(7));
    let mut piped_ir = piped::Core::new(Interpreted::new(), bits(7));
    let mut rng = Rng(0x0070_6970_6564);
    let (mut seen_plain, mut seen_piped) = (Vec::new(), Vec::new());
    let (mut run_plain, mut run_piped) = (0, 0);
    let mut runs = Vec::new();
    for _ in 0..4_000 {
        let go = !rng.next().is_multiple_of(5);
        let (a, b) = operands(plain.mac.done.get());
        let out = plain.clock(plain::Inputs {
            go,
            a: bits(a),
            b: bits(b),
        });
        if out.done.get() as usize > seen_plain.len() {
            seen_plain.push((out.acc, out.square));
        }
        run_plain = if plain.mac.phase == Phase::Mult {
            run_plain + 1
        } else {
            0
        };
        assert!(run_plain <= 1);

        let (a, b) = operands(piped.mac.done.get());
        let inputs = piped::Inputs {
            go,
            a: bits(a),
            b: bits(b),
        };
        let out = piped.clock(inputs);
        assert_eq!(piped_ir.clock(inputs), out, "IR interpreter");
        if out.done.get() as usize > seen_piped.len() {
            seen_piped.push((out.acc, out.square));
        }
        if piped.mac.phase == Phase::Mult {
            run_piped += 1;
        } else if run_piped > 0 {
            runs.push(run_piped);
            run_piped = 0;
        }
    }
    let n = seen_piped.len();
    assert!(n > 500, "{n} products");
    assert_eq!(seen_plain[..n], seen_piped[..]);
    // `Mult` lasts three cycles: two held, then the one that takes the products.
    assert!(runs.iter().all(|&r| r == 3), "{runs:?}");
    // Only `Mult` holds: `steps` counts every other edge.
    assert!(piped.mac.steps.get() < 4_000);
}

#[test]
fn generated_verilog_matches_cycle_model() {
    let files = piped::verilog().unwrap();
    let unit = &files.iter().find(|(n, _)| n == "mac_unit.v").unwrap().1;
    // A register on each operand and on each product.
    assert_eq!(unit.matches("always @(posedge clk) n").count(), 6, "{unit}");
    assert!(
        unit.contains("wire stall = stall_when && waited != 2'd2;"),
        "{unit}"
    );
    // Without `pipeline`, no registers in the step function.
    let plain_files = plain::verilog().unwrap();
    let plain_unit = &plain_files
        .iter()
        .find(|(n, _)| n == "mac_unit.v")
        .unwrap()
        .1;
    assert!(!plain_unit.contains("always @(posedge clk) n"));

    let mut core = piped::Core::new(Native, bits(7));
    let mut rng = Rng(0x0068_646c_7069_7065);
    let mut tb = String::from(
        "`timescale 1ns / 1ps\nmodule tb;\n    reg clk = 0, rst = 1;\n    always #5 clk = ~clk;\n    \
         reg [0:0] go = 0;\n    reg [24:0] a = 0;\n    reg [17:0] b = 0;\n    \
         wire [47:0] acc;\n    wire [35:0] square;\n    wire [15:0] done, steps;\n    wire [0:0] busy;\n    \
         piped dut(.clk(clk), .rst(rst), .go(go), .a(a), .b(b),\n        \
         .acc(acc), .square(square), .done(done), .steps(steps), .busy(busy));\n    initial begin\n        \
         @(posedge clk); @(posedge clk); #1 rst = 0;\n",
    );
    for k in 0..2_000 {
        let rst = k % 700 == 650;
        let go = !rng.next().is_multiple_of(4);
        let (a, b) = (rng.next() % (1 << 25), rng.next() % (1 << 18));
        let inputs = piped::Inputs {
            go,
            a: bits(a),
            b: bits(b),
        };
        let out = if rst {
            core.reset(inputs)
        } else {
            core.clock(inputs)
        };
        writeln!(
            tb,
            "        rst = {}; go = {}; a = {a}; b = {b}; @(posedge clk); #1;\n        \
             if (acc !== 48'h{:x} || square !== 36'h{:x} || done !== 16'h{:x} || steps !== 16'h{:x} || busy !== {}) begin\n            \
             $display(\"cycle {k}: acc=%h square=%h done=%h steps=%h busy=%b\", acc, square, done, steps, busy); $fatal(1);\n        end",
            u8::from(rst),
            u8::from(go),
            out.acc.get(),
            out.square.get(),
            out.done.get(),
            out.steps.get(),
            u8::from(out.busy),
        )
        .unwrap();
    }
    tb.push_str("        $display(\"HDL PASS\"); $finish;\n    end\nendmodule\n");

    let dir = tempfile::tempdir().unwrap();
    let mut paths = vec![dir.path().join("tb.v")];
    std::fs::write(&paths[0], tb).unwrap();
    for (name, text) in &files {
        paths.push(dir.path().join(name));
        std::fs::write(paths.last().unwrap(), text).unwrap();
    }
    let exe = dir.path().join("tb.vvp");
    let compile = Command::new("iverilog")
        .args(["-g2012", "-s", "tb", "-o"])
        .arg(&exe)
        .args(&paths)
        .output()
        .expect("Icarus Verilog is required");
    assert!(
        compile.status.success(),
        "{}",
        String::from_utf8_lossy(&compile.stderr)
    );
    let run = Command::new("vvp").arg(exe).output().unwrap();
    let stdout = String::from_utf8_lossy(&run.stdout);
    assert!(
        run.status.success() && stdout.contains("HDL PASS"),
        "{stdout}"
    );
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
pub struct BadOut {
    state: MacState,
    product: Bits<43>,
}

/// Multiplies an input, which can change while the unit holds.
#[logic]
fn from_input(i: MacIn) -> MacOut {
    let s = i.state;
    MacOut {
        state: MacState {
            phase: s.phase,
            a: s.a,
            b: s.b,
            acc: bits::<5>(0).concat::<43, 48>(i.a.mul_wide::<18, 43>(s.b)),
            square: s.square,
            done: s.done,
            steps: s.steps,
        },
        busy: false,
    }
}

/// A product on an output other than the state.
#[logic]
fn to_output(i: MacIn) -> BadOut {
    let s = i.state;
    BadOut {
        state: s,
        product: s.a.mul_wide::<18, 43>(s.b),
    }
}

/// No multiply: nothing to pipeline.
#[logic]
fn no_mul(i: MacIn) -> MacOut {
    let s = i.state;
    MacOut {
        state: MacState {
            phase: s.phase,
            a: s.a,
            b: s.b,
            acc: s.acc,
            square: s.square,
            done: s.done,
            steps: s.steps + bits::<16>(1),
        },
        busy: i.go,
    }
}

bitlogic::system! {
    mod bad_input {
        param k: Bits<48> = bits(7);
        input go: bool;
        input a: Bits<25>;
        input b: Bits<18>;
        unit mac: MacState = from_input(MacIn { state, k, go, a, b }), pipeline;
        output acc: Bits<48> = mac.state.acc;
    }
}

bitlogic::system! {
    mod bad_output {
        param k: Bits<48> = bits(7);
        input go: bool;
        input a: Bits<25>;
        input b: Bits<18>;
        unit mac: MacState = to_output(MacIn { state, k, go, a, b }), pipeline;
        output product: Bits<43> = mac.product, registered;
    }
}

bitlogic::system! {
    mod nothing {
        param k: Bits<48> = bits(7);
        input go: bool;
        input a: Bits<25>;
        input b: Bits<18>;
        unit mac: MacState = no_mul(MacIn { state, k, go, a, b }), pipeline;
        output steps: Bits<16> = mac.state.steps;
    }
}

#[test]
fn rules_refuse_what_holding_cannot_make_correct() {
    let err = bad_input::verilog().unwrap_err().0;
    assert!(
        err.contains("unit `mac`") && err.contains("depends on `a`"),
        "{err}"
    );
    let err = bad_output::verilog().unwrap_err().0;
    assert!(err.contains("output `product`"), "{err}");
}

#[test]
fn a_unit_without_multiplies_is_unchanged() {
    let files = nothing::verilog().unwrap();
    let unit = &files.iter().find(|(n, _)| n == "mac_unit.v").unwrap().1;
    let design = nothing::design().unwrap();
    let logic = &design.units[0].logic;
    assert_eq!(*unit, logic.to_unit_verilog("mac_unit").unwrap());
    let (_, report) = &design.pipeline_reports().unwrap()[0];
    assert!(report.contains("no registers"), "{report}");
    let mut core = nothing::Core::new(Native, bits(7));
    for _ in 0..10 {
        core.clock(nothing::Inputs {
            go: true,
            a: bits(0),
            b: bits(0),
        });
    }
    assert_eq!(core.mac.steps.get(), 10);
}
