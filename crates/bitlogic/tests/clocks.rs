//! `system!` with two clock domains: a producer in `a` writes a fixed
//! sequence into a RAM read in `b`, Gray-coded pointers cross both ways, a
//! flag crosses as a level, and a consumer in `b` sums and hashes what it
//! reads (scaled through a ROM in `b`). The clocks are 10 ns and 13.7 ns,
//! so their edges never coincide.
//!
//! Checks: the consumed stream equals the produced sequence in order (an
//! independent integer reference), with and without synchronizer noise;
//! the cycle model equals the IR interpreter edge by edge; the generated
//! Verilog under Icarus, with two real clocks, equals the cycle model on
//! every edge; a Gray-crossed pointer always arrives as a value it had,
//! while a level-crossed one tears under noise; a Gray
//! crossing of a value that jumps stops the model; the generated files,
//! ports and constraints.
mod support;
use bitlogic::system::{CdcNoise, Schedule};
use bitlogic::{Bits, Interpreted, LogicValue, Native, bits, logic};
use std::{fmt::Write, process::Command};
use support::Rng;

#[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
pub struct Ctl {
    go: bool,
    flag: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
pub struct ProdState {
    wr: Bits<4>,
    written: Bits<8>,
    flag: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
pub struct ProdIn {
    state: ProdState,
    ctl: Ctl,
    rd: Bits<4>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
pub struct ProdOut {
    state: ProdState,
    we: bool,
    waddr: Bits<4>,
    wdata: Bits<8>,
}

/// The `k`th value written: `37 k + 11` (mod 256).
fn value(k: u64) -> u64 {
    (37 * k + 11) % 256
}

/// While `go` and the ring has room, write the next value of the sequence.
#[logic]
fn produce(i: ProdIn) -> ProdOut {
    let s = i.state;
    let full = s.wr + bits::<4>(1) == i.rd;
    let we = i.ctl.go && !full;
    ProdOut {
        state: ProdState {
            wr: if we { s.wr + bits::<4>(1) } else { s.wr },
            written: if we {
                s.written + bits::<8>(1)
            } else {
                s.written
            },
            flag: i.ctl.flag,
        },
        we,
        waddr: s.wr,
        // 37 written + 11.
        wdata: (s.written << 5) + (s.written << 2) + s.written + bits::<8>(11),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
pub enum Phase {
    Idle,
    Wait,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
pub struct ConsState {
    rd: Bits<4>,
    phase: Phase,
    count: Bits<16>,
    sum: Bits<16>,
    hash: Bits<16>,
    flags: Bits<16>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
pub struct ConsIn {
    state: ConsState,
    wr: Bits<4>,
    data: Bits<8>,
    scale: Bits<4>,
    flag: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
pub struct ConsOut {
    state: ConsState,
    raddr: Bits<4>,
    rom_addr: Bits<2>,
}

/// Address a filled slot (Idle), then fold it in (Wait): its sum, an
/// order-sensitive hash with the ROM's scale, and how often the crossed
/// flag was set.
#[logic]
fn consume(i: ConsIn) -> ConsOut {
    let s = i.state;
    let take = s.phase == Phase::Wait;
    let data = bits::<8>(0).concat::<8, 16>(i.data);
    let scale = bits::<12>(0).concat::<4, 16>(i.scale);
    ConsOut {
        state: ConsState {
            rd: if take { s.rd + bits::<4>(1) } else { s.rd },
            phase: if !take && i.wr != s.rd {
                Phase::Wait
            } else {
                Phase::Idle
            },
            count: if take {
                s.count + bits::<16>(1)
            } else {
                s.count
            },
            sum: if take { s.sum + data } else { s.sum },
            // 31 hash + data + 256 scale.
            hash: if take {
                (s.hash << 5) - s.hash + data + (scale << 8)
            } else {
                s.hash
            },
            flags: if i.flag {
                s.flags + bits::<16>(1)
            } else {
                s.flags
            },
        },
        raddr: s.rd,
        rom_addr: s.rd.slice::<2>(0),
    }
}

const SCALES: [u64; 4] = [3, 5, 7, 9];

bitlogic::system! {
    /// A FIFO between two clock domains.
    pub mod fifo {
        clock a, period = 10.0;
        clock b, period = 13.7;
        input ctl: Ctl in a, registered;
        ram ring: Bits<8>, addr_bits = 4;
        rom scale: Bits<4>, addr_bits = 2, contents = SCALES.map(bits::<4>);
        unit prod: ProdState in a = produce(ProdIn { state, ctl, rd: rd_a });
        unit cons: ConsState in b = consume(ConsIn {
            state,
            wr: wr_b,
            data: ring.rdata,
            scale: scale.rdata,
            flag: flag_b,
        });
        cross wr_b: Bits<4> = gray(prod.state.wr) into b;
        cross rd_a: Bits<4> = gray(cons.state.rd) into a;
        cross flag_b: bool = level(prod.state.flag) into b;
        write ring(prod.we, prod.waddr, prod.wdata) in a;
        read ring(cons.raddr) in b;
        read scale(cons.rom_addr) in b;
        output written: Bits<8> in a = prod.state.written, registered;
        output count: Bits<16> in b = cons.state.count;
        output sum: Bits<16> in b = cons.state.sum;
        output hash: Bits<16> in b = cons.state.hash, registered;
    }
}

// The same FIFO with its write pointer crossed bit by bit, as a level: a
// pointer that moves while it crosses can be seen half updated.
bitlogic::system! {
    mod torn {
        clock a, period = 10.0;
        clock b, period = 13.7;
        input ctl: Ctl in a, registered;
        ram ring: Bits<8>, addr_bits = 4;
        rom scale: Bits<4>, addr_bits = 2, contents = SCALES.map(bits::<4>);
        unit prod: ProdState in a = produce(ProdIn { state, ctl, rd: rd_a });
        unit cons: ConsState in b = consume(ConsIn {
            state,
            wr: wr_b,
            data: ring.rdata,
            scale: scale.rdata,
            flag: flag_b,
        });
        cross wr_b: Bits<4> = level(prod.state.wr) into b;
        cross rd_a: Bits<4> = gray(cons.state.rd) into a;
        cross flag_b: bool = level(prod.state.flag) into b;
        write ring(prod.we, prod.waddr, prod.wdata) in a;
        read ring(cons.raddr) in b;
        read scale(cons.rom_addr) in b;
        output count: Bits<16> in b = cons.state.count;
        output hash: Bits<16> in b = cons.state.hash, registered;
    }
}

// A Gray crossing of a counter that moves by two.
#[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
pub struct TwoIn {
    state: Bits<4>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
pub struct TwoOut {
    state: Bits<4>,
}

#[logic]
fn by_two(i: TwoIn) -> TwoOut {
    TwoOut {
        state: i.state + bits::<4>(2),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
pub struct SeeIn {
    state: Bits<4>,
    v: Bits<4>,
}

#[logic]
fn see(i: SeeIn) -> TwoOut {
    TwoOut { state: i.v }
}

bitlogic::system! {
    mod jumpy {
        clock a, period = 10.0;
        clock b, period = 13.7;
        unit c: Bits<4> in a = by_two(TwoIn { state });
        unit s: Bits<4> in b = see(SeeIn { state, v: v_b });
        cross v_b: Bits<4> = gray(c.state) into b;
        output v: Bits<4> in b = s.state;
    }
}

/// The edges after `Core::new`'s two reset edges per clock, phases 5 ns and
/// 6.85 ns (as in the testbench), each with its clock.
fn edges(n: usize) -> Vec<(u64, fifo::Clock)> {
    let mut sched = Schedule::new(&[(10_000_000, 5_000_000), (13_700_000, 6_850_000)]);
    let mut seen = [0usize; 2];
    let mut out = Vec::new();
    while out.len() < n {
        let (t, i) = sched.next().unwrap();
        seen[i] += 1;
        if seen[i] > 2 {
            out.push((t, fifo::Clock::ALL[i]));
        }
    }
    out
}

fn stimulus(rng: &mut Rng) -> fifo::Inputs {
    fifo::Inputs {
        ctl: Ctl {
            // Bursts, so the ring both fills and drains.
            go: rng.next() % 8 < 5,
            flag: rng.next().is_multiple_of(4),
        },
    }
}

/// The consumer's sum and hash after taking the first `n` values.
fn reference(n: u64) -> (u16, u16) {
    let (mut sum, mut hash) = (0u16, 0u16);
    for k in 0..n {
        let v = value(k) as u16;
        sum = sum.wrapping_add(v);
        hash = hash
            .wrapping_mul(31)
            .wrapping_add(v)
            .wrapping_add((SCALES[(k % 4) as usize] as u16) << 8);
    }
    (sum, hash)
}

fn run_fifo(noise: Option<CdcNoise>, n: usize) -> fifo::Core {
    let mut core = fifo::Core::new(Native);
    if let Some(noise) = noise {
        core.cdc_noise = noise;
    }
    let mut rng = Rng(0x00c1_0c4d);
    let mut inputs = stimulus(&mut rng);
    for (_, clock) in edges(n) {
        if clock == fifo::Clock::A {
            inputs = stimulus(&mut rng);
        }
        core.tick(clock, inputs);
    }
    core
}

#[test]
fn consumed_stream_is_the_produced_sequence() {
    for noise in [
        None,
        Some(CdcNoise::new(7, 50)),
        Some(CdcNoise::new(99, 100)),
    ] {
        let core = run_fifo(noise.clone(), 20_000);
        let out = core.outputs();
        let n = u64::from(out.count.get() as u16);
        assert!(n > 2_000, "{n} values crossed ({noise:?})");
        let (sum, _) = reference(n);
        assert_eq!(out.sum.get() as u16, sum, "{noise:?}");
        // The registered hash lags a `b` edge: compare the state.
        assert_eq!(core.cons.hash.get() as u16, reference(n).1, "{noise:?}");
        // Everything written but at most a ring's worth was consumed.
        let written = u64::from(core.prod.written.get() as u8);
        assert!((written + 256 - (n % 256)) % 256 <= 15, "{written} {n}");
        // The flag crossed: set about a quarter of the time.
        assert!(core.cons.flags.get() > 1_000, "{noise:?}");
    }
}

/// Whether every write pointer the consumer's domain saw was a value the
/// producer's pointer had held in its last eight edges (the crossing
/// takes about four, noise adds a `b` edge).
macro_rules! pointers_whole {
    ($sys:ident, $noise:expr) => {{
        let mut core = $sys::Core::new(Native);
        if let Some(noise) = $noise {
            core.cdc_noise = noise;
        }
        let mut rng = Rng(0x00c1_0c4d);
        let mut inputs = $sys::Inputs {
            ctl: stimulus(&mut rng).ctl,
        };
        let mut recent = std::collections::VecDeque::from([0u64; 8]);
        let mut whole = true;
        for (_, clock) in edges(20_000) {
            if clock == fifo::Clock::A {
                inputs = $sys::Inputs {
                    ctl: stimulus(&mut rng).ctl,
                };
                core.tick($sys::Clock::A, inputs);
                recent.pop_front();
                recent.push_back(core.prod.wr.get());
            } else {
                core.tick($sys::Clock::B, inputs);
                whole &= recent.contains(&core.__cross_wr_b.value().get());
            }
        }
        whole
    }};
}

#[test]
fn a_gray_crossed_pointer_arrives_whole_a_level_crossed_one_tears() {
    // Gray: always a value the pointer had, noise or not.
    assert!(pointers_whole!(fifo, None::<CdcNoise>));
    assert!(pointers_whole!(fifo, Some(CdcNoise::new(3, 100))));
    // Bit by bit: whole without noise, torn with it.
    assert!(pointers_whole!(torn, None::<CdcNoise>));
    assert!(!pointers_whole!(torn, Some(CdcNoise::new(3, 100))));
}

#[test]
#[should_panic(expected = "cross `v_b`: a Gray crossing's value moved from 0 to 2")]
fn a_gray_crossing_that_jumps_stops_the_model() {
    let mut core = jumpy::Core::new(Native);
    core.tick(jumpy::Clock::A, jumpy::Inputs {});
    core.tick(jumpy::Clock::A, jumpy::Inputs {});
}

#[test]
fn cycle_model_matches_the_ir_edge_by_edge() {
    let mut native = fifo::Core::new(Native);
    let mut ir = fifo::Core::new(Interpreted::new());
    native.cdc_noise = CdcNoise::new(5, 30);
    ir.cdc_noise = CdcNoise::new(5, 30);
    let mut rng = Rng(0x1d0c);
    let mut inputs = stimulus(&mut rng);
    for (_, clock) in edges(4_000) {
        if clock == fifo::Clock::A {
            inputs = stimulus(&mut rng);
        }
        assert_eq!(native.tick(clock, inputs), ir.tick(clock, inputs));
    }
}

#[test]
fn schedule_orders_edges_and_applies_ppm() {
    let mut s = Schedule::new(&[(10, 0), (15, 5)]);
    let got: Vec<_> = (0..6).map(|_| s.next().unwrap()).collect();
    assert_eq!(got, [(0, 0), (5, 1), (10, 0), (20, 0), (20, 1), (30, 0)]);
    assert_eq!(Schedule::period_fs(40.0, 0.0), 40_000_000);
    assert_eq!(Schedule::period_fs(40.0, 100.0), 40_004_000);
    assert_eq!(fifo::Clock::A.period_ns(), Some(10.0));
    assert_eq!(fifo::Clock::B.name(), "b");
}

#[test]
fn generated_files_ports_and_constraints() {
    let files = fifo::verilog().unwrap();
    let names: Vec<_> = files.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        [
            "fifo.v",
            "prod_unit.v",
            "cons_unit.v",
            "scale_rom.v",
            "bitlogic_dc_ram.v",
            "bitlogic_cdc.v",
            "fifo_cdc.tcl",
        ]
    );
    let core = &files[0].1;
    for want in [
        "input wire clk_a",
        "input wire rst_a",
        "input wire clk_b",
        "input wire rst_b",
        "always @(posedge clk_a)",
        "prod_unit prod_i (\n        .clk(clk_a), .rst(rst_a),",
        "cons_unit cons_i (\n        .clk(clk_b), .rst(rst_b),",
        "bitlogic_dc_ram #(.ADDR_W(4), .DATA_W(8)) ring_i (\n        .wclk(clk_a), .rclk(clk_b),",
        "scale_rom scale_i (.clk(clk_b)",
        "bitlogic_cdc_gray #(.W(4)) wr_b_cross_i (",
        ".src_clk(clk_a), .src_rst(rst_a), .src_in(prod_s_wr)",
        ".dest_clk(clk_b), .dest_rst(rst_b), .dest_out(wr_b_sync)",
        "bitlogic_cdc_level #(.W(1)) flag_b_cross_i (",
    ] {
        assert!(core.contains(want), "missing {want:?} in\n{core}");
    }
    assert!(!core.contains("input wire clk,"));
    let tcl = &files[6].1;
    for want in [
        "bitlogic_cdc {*wr_b_cross_i/src_q_reg*} {*wr_b_cross_i/s1_reg*} 1",
        "bitlogic_cdc {*rd_a_cross_i/src_q_reg*} {*rd_a_cross_i/s1_reg*} 1",
        "bitlogic_cdc {*flag_b_cross_i/src_q_reg*} {*flag_b_cross_i/s1_reg*} 0",
        "bitlogic_cdc_ram {*ring_i/*rdata_reg*}",
        "set_max_delay -datapath_only",
        "set_bus_skew",
    ] {
        assert!(tcl.contains(want), "missing {want:?} in\n{tcl}");
    }
}

#[test]
fn generated_verilog_matches_the_cycle_model_with_two_clocks() {
    let mut core = fifo::Core::new(Native);
    let mut rng = Rng(0xfeed);
    let mut inputs = stimulus(&mut rng);
    let mut tb = String::from(
        "`timescale 1ns / 1ps\nmodule tb;\n    \
         reg clk_a = 0, clk_b = 0, rst_a = 1, rst_b = 1;\n    \
         always #5 clk_a = ~clk_a;\n    always #6.85 clk_b = ~clk_b;\n    \
         reg [0:0] go = 0, flag = 0;\n    \
         wire [7:0] written;\n    wire [15:0] count, sum, hash;\n    \
         fifo dut(.clk_a(clk_a), .rst_a(rst_a), .clk_b(clk_b), .rst_b(rst_b),\n        \
         .ctl_go(go), .ctl_flag(flag), .written(written), .count(count), .sum(sum), .hash(hash));\n    \
         initial begin\n        @(posedge clk_a); @(posedge clk_b); @(posedge clk_a); @(posedge clk_b);\n",
    );
    // `Core::new`'s reset edges are each clock's first two: a at 5 and 15,
    // b at 6.85 and 20.55 ns; both resets are released just after 20.55.
    let set = |tb: &mut String, i: &fifo::Inputs| {
        writeln!(
            tb,
            "        go = {}; flag = {};",
            u8::from(i.ctl.go),
            u8::from(i.ctl.flag)
        )
        .unwrap();
    };
    tb.push_str("        #0.01 rst_a = 0; rst_b = 0;\n");
    set(&mut tb, &inputs);
    for (k, (_, clock)) in edges(3_000).into_iter().enumerate() {
        let out = core.tick(clock, inputs);
        let clk = if clock == fifo::Clock::A {
            "clk_a"
        } else {
            "clk_b"
        };
        writeln!(
            tb,
            "        @(posedge {clk}); #0.01;\n        \
             if (written !== 8'h{:x} || count !== 16'h{:x} || sum !== 16'h{:x} || hash !== 16'h{:x}) begin\n            \
             $display(\"edge {k} ({clk}): written=%h count=%h sum=%h hash=%h\", written, count, sum, hash); $fatal(1);\n        end",
            out.written.get(),
            out.count.get(),
            out.sum.get(),
            out.hash.get(),
        )
        .unwrap();
        if clock == fifo::Clock::A {
            inputs = stimulus(&mut rng);
            set(&mut tb, &inputs);
        }
    }
    tb.push_str("        $display(\"HDL PASS\"); $finish;\n    end\nendmodule\n");

    let dir = tempfile::tempdir().unwrap();
    let mut paths = vec![dir.path().join("tb.v")];
    std::fs::write(&paths[0], tb).unwrap();
    for (name, text) in fifo::verilog().unwrap() {
        if name.ends_with(".v") {
            paths.push(dir.path().join(&name));
            std::fs::write(paths.last().unwrap(), text).unwrap();
        }
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
