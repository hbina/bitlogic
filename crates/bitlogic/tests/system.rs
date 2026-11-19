//! `system!`: a producer and a consumer joined through a RAM, plus a ROM,
//! a param, registered and unregistered inputs and every kind of output.
//! The cycle model is checked against an independent integer model, the IR
//! interpreter and the generated Verilog under Icarus, cycle by cycle.
mod support;
use bitlogic::{Bits, Interpreted, LogicValue, Native, bits, logic};
use std::{fmt::Write, process::Command};
use support::Rng;

#[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
pub struct Cfg {
    step: Bits<8>,
    enable: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
pub struct Ctl {
    go: bool,
    bump: Bits<8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
pub struct ProdState {
    wr: Bits<4>,
    count: Bits<8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
pub struct ProdIn {
    state: ProdState,
    cfg: Cfg,
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

/// Write `count` into the ring while `go` and there is room.
#[logic]
fn produce(i: ProdIn) -> ProdOut {
    let s = i.state;
    let full = s.wr + bits::<4>(1) == i.rd;
    let we = i.cfg.enable && i.ctl.go && !full;
    ProdOut {
        state: ProdState {
            wr: if we { s.wr + bits::<4>(1) } else { s.wr },
            count: if we {
                s.count + i.cfg.step + i.ctl.bump
            } else {
                s.count
            },
        },
        we,
        waddr: s.wr,
        wdata: s.count,
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
    sum: Bits<16>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
pub struct ConsIn {
    state: ConsState,
    wr: Bits<4>,
    data: Bits<8>,
    scale: Bits<6>,
    poke: Bits<8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
pub struct ConsOut {
    state: ConsState,
    raddr: Bits<4>,
    rom_addr: Bits<3>,
    odd: bool,
}

/// Address a filled slot (Idle), then add it, its ROM scale and `poke` (Wait).
#[logic]
fn consume(i: ConsIn) -> ConsOut {
    let s = i.state;
    ConsOut {
        state: match s.phase {
            Phase::Idle => ConsState {
                rd: s.rd,
                phase: if i.wr != s.rd {
                    Phase::Wait
                } else {
                    Phase::Idle
                },
                sum: s.sum,
            },
            Phase::Wait => ConsState {
                rd: s.rd + bits::<4>(1),
                phase: Phase::Idle,
                sum: s.sum
                    + bits::<8>(0).concat::<8, 16>(i.data)
                    + bits::<10>(0).concat::<6, 16>(i.scale)
                    + bits::<8>(0).concat::<8, 16>(i.poke),
            },
        },
        raddr: s.rd,
        rom_addr: s.rd.slice::<3>(0),
        odd: s.sum.bit(0),
    }
}

const SCALES: [u64; 8] = [3, 1, 4, 1, 5, 9, 2, 6];

bitlogic::system! {
    /// Producer and consumer joined through a RAM.
    pub mod pipe {
        param cfg: Cfg = Cfg { step: bits(3), enable: true };
        input ctl: Ctl, registered;
        input poke: Bits<8>;
        ram ring: Bits<8>, addr_bits = 4;
        rom scale: Bits<6>, addr_bits = 3, contents = SCALES.map(bits::<6>);
        unit prod: ProdState = produce(ProdIn { state, cfg, ctl, rd: cons.state.rd });
        unit cons: ConsState = consume(ConsIn {
            state,
            wr: prod.state.wr,
            data: ring.rdata,
            scale: scale.rdata,
            poke,
        });
        write ring(prod.we, prod.waddr, prod.wdata);
        read ring(cons.raddr);
        read scale(cons.rom_addr);
        output sum: Bits<16> = cons.state.sum;
        output odd: bool = cons.odd, registered;
        output going: bool = ctl.go;
        output step: Bits<8> = cfg.step;
        /// Shows read-first behaviour when both ports hit one address.
        output peek: Bits<8> = ring.rdata;
    }
}

/// The same system written directly with integers.
#[derive(Default)]
struct Model {
    go: bool,
    bump: u64,
    wr: u64,
    count: u64,
    rd: u64,
    wait: bool,
    sum: u64,
    odd: bool,
    mem: [u64; 16],
    rdata: u64,
    rom_rdata: u64,
    reads: usize,
    collided: bool,
}

impl Model {
    fn new() -> Self {
        Self {
            rom_rdata: SCALES[0],
            ..Self::default()
        }
    }
    /// One edge, with the synchronous reset when `rst`; returns (sum, odd,
    /// going, step) after it.
    fn clock(
        &mut self,
        rst: bool,
        step: u64,
        go: bool,
        bump: u64,
        poke: u64,
    ) -> (u64, bool, bool, u64) {
        // Everything below reads the registers as they stood before the edge.
        let we = self.go && (self.wr + 1) % 16 != self.rd;
        let odd = self.sum & 1 == 1;
        let (rd, rdata, rom_rdata) = (self.rd, self.rdata, self.rom_rdata);
        if self.wait {
            self.sum = (self.sum + rdata + rom_rdata + poke) % 65536;
            self.rd = (rd + 1) % 16;
            self.wait = false;
            self.reads += 1;
        } else {
            self.wait = self.wr != rd;
        }
        // Read-first: sample the old contents, then write.
        self.rdata = self.mem[rd as usize];
        self.rom_rdata = SCALES[(rd % 8) as usize];
        self.collided = we && self.wr == rd;
        if we {
            self.mem[self.wr as usize] = self.count;
            self.wr = (self.wr + 1) % 16;
            self.count = (self.count + step + self.bump) % 256;
        }
        (self.go, self.bump, self.odd) = (go, bump, odd);
        if rst {
            // Registers clear; the memories and their read data do not.
            *self = Self {
                mem: self.mem,
                rdata: self.rdata,
                rom_rdata: self.rom_rdata,
                reads: self.reads,
                collided: self.collided,
                ..Self::default()
            };
        }
        (self.sum, self.odd, self.go, step)
    }
}

#[derive(Clone, Copy)]
struct Stim {
    rst: bool,
    go: bool,
    bump: u64,
    poke: u64,
}

fn stimulus(cycles: usize) -> Vec<Stim> {
    let mut rng = Rng(0x5157_e3d0_0d1e);
    (0..cycles)
        .map(|k| Stim {
            // Two reset edges partway through each run.
            rst: (k % 1000) == 700 || (k % 1000) == 701,
            // Bursts of `go`, so the ring both fills and drains.
            go: (k / 40) % 3 != 2 && !rng.next().is_multiple_of(4),
            bump: rng.next() % 4,
            poke: rng.next() % 256,
        })
        .collect()
}

fn inputs(s: Stim) -> pipe::Inputs {
    pipe::Inputs {
        ctl: Ctl {
            go: s.go,
            bump: bits(s.bump),
        },
        poke: bits(s.poke),
    }
}

fn edge<E: bitlogic::Evaluate>(core: &mut pipe::Core<E>, s: Stim) -> pipe::Outputs {
    if s.rst {
        core.reset(inputs(s))
    } else {
        core.clock(inputs(s))
    }
}

fn config() -> Cfg {
    Cfg {
        step: bits(3),
        enable: true,
    }
}

#[test]
fn cycle_model_matches_integer_model_and_ir() {
    let stim = stimulus(3_000);
    let mut core = pipe::Core::new(Native, config());
    let mut ir = pipe::Core::new(Interpreted::new(), config());
    let mut model = Model::new();
    let (mut full, mut collisions) = (false, 0);
    for (k, &s) in stim.iter().enumerate() {
        let out = edge(&mut core, s);
        assert_eq!(edge(&mut ir, s), out, "cycle {k}");
        let (sum, odd, going, step) = model.clock(s.rst, 3, s.go, s.bump, s.poke);
        full |= (model.wr + 1) % 16 == model.rd;
        let want = pipe::Outputs {
            sum: bits(sum),
            odd,
            going,
            step: bits(step),
            peek: bits(model.rdata),
        };
        assert_eq!(out, want, "cycle {k}");
        collisions += usize::from(model.collided);
        assert_eq!(core.prod.wr.get(), model.wr, "cycle {k}");
        assert_eq!(core.cons.rd.get(), model.rd, "cycle {k}");
        assert_eq!(core.ring.cells[3].get(), model.mem[3], "cycle {k}");
    }
    let reads = model.reads;
    assert!(
        reads > 500 && full,
        "the ring fills and drains ({reads} reads)"
    );
    assert!(
        collisions > 10,
        "reads and writes share an address ({collisions})"
    );
}

#[test]
fn generated_verilog_matches_cycle_model() {
    let stim = stimulus(1_500);
    let mut core = pipe::Core::new(Native, config());
    let files = pipe::verilog().unwrap();
    let names: Vec<_> = files.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        [
            "pipe.v",
            "prod_unit.v",
            "cons_unit.v",
            "scale_rom.v",
            "bitlogic_sdp_ram.v"
        ]
    );
    let core_v = &files[0].1;
    assert!(core_v.contains("parameter [7:0] CFG_STEP = 8'h3"));
    assert!(core_v.contains("bitlogic_sdp_ram #(.ADDR_W(4), .DATA_W(8)) ring_i"));

    let mut tb = String::from(
        "`timescale 1ns / 1ps\nmodule tb;\n    reg clk = 0, rst = 1;\n    always #5 clk = ~clk;\n    \
         reg [0:0] go = 0;\n    reg [7:0] bump = 0, poke = 0;\n    wire [15:0] sum;\n    \
         wire [0:0] odd, going;\n    wire [7:0] step, peek;\n    \
         pipe dut(.clk(clk), .rst(rst), .ctl_go(go), .ctl_bump(bump), .poke(poke),\n        \
         .sum(sum), .odd(odd), .going(going), .step(step), .peek(peek));\n    initial begin\n        \
         @(posedge clk); @(posedge clk); #1 rst = 0;\n",
    );
    for (k, &s) in stim.iter().enumerate() {
        let out = edge(&mut core, s);
        writeln!(
            tb,
            "        rst = {}; go = {}; bump = {}; poke = {}; @(posedge clk); #1;\n        \
             if (sum !== 16'h{:x} || odd !== {} || going !== {} || step !== 8'h{:x} || peek !== 8'h{:x}) begin\n            \
             $display(\"cycle {k}: sum=%h odd=%b going=%b step=%h\", sum, odd, going, step); $fatal(1);\n        end",
            u8::from(s.rst),
            u8::from(s.go),
            s.bump,
            s.poke,
            out.sum.get(),
            u8::from(out.odd),
            u8::from(out.going),
            out.step.get(),
            out.peek.get(),
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

// A RAM declared wider than the addresses that drive it.
bitlogic::system! {
    mod wide {
        param cfg: Cfg;
        input ctl: Ctl;
        ram ring: Bits<8>, addr_bits = 5;
        unit prod: ProdState = produce(ProdIn { state, cfg, ctl, rd: prod.state.wr });
        write ring(prod.we, prod.waddr, prod.wdata);
        read ring(prod.waddr);
        output count: Bits<8> = prod.state.count;
    }
}

#[test]
fn design_checks_widths_and_reset_values() {
    let err = wide::verilog().unwrap_err();
    assert!(err.0.contains("5-bit scalar"), "{err}");
    // The defaults reach the Verilog parameters; an absent default is zero.
    let design = wide::design().unwrap();
    assert_eq!(design.params[0].default, [0, 0]);
    assert_eq!(pipe::design().unwrap().params[0].default, [3, 1]);
}
