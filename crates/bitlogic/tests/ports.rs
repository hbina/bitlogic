//! `system!` ports: a unit that runs read-modify-write passes over a memory
//! outside the system, through a request/response port with valid/ready
//! handshakes. Its results cannot depend on when the other side accepts or
//! answers, so the cycle model (native and IR) is checked against a plain
//! sequential loop under random latency, queue depth and stalls, and the
//! generated Verilog is checked cycle by cycle against the cycle model under
//! Icarus, with the other side's signals replayed.
mod support;
use bitlogic::system::{InOrder, PortCycle, Responder};
use bitlogic::{Bits, Interpreted, LogicValue, Native, bits, logic};
use std::cell::RefCell;
use std::rc::Rc;
use std::{fmt::Write, process::Command};
use support::Rng;

#[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
pub struct MemReq {
    write: bool,
    addr: Bits<8>,
    data: Bits<16>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
pub struct MemRsp {
    data: Bits<16>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
pub enum Phase {
    Read,
    Wait,
    Write { value: Bits<16> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
pub struct ClientState {
    phase: Phase,
    /// Passes completed; the next adds `k + 1`.
    k: Bits<16>,
    /// 8-bit Galois LFSR: the address of the current pass (never 0).
    addr: Bits<8>,
    /// Sum of every value read.
    sum: Bits<32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
pub struct ClientIn {
    state: ClientState,
    req_ready: bool,
    rsp_valid: bool,
    rsp: MemRsp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
pub struct ClientOut {
    state: ClientState,
    req_valid: bool,
    req: MemReq,
    rsp_ready: bool,
}

#[logic]
fn lfsr(a: Bits<8>) -> Bits<8> {
    let shifted = a >> 1;
    if a.bit(0) {
        shifted ^ bits::<8>(0xb8)
    } else {
        shifted
    }
}

/// Read `mem[addr]`, wait for it, write back `value + k + 1`, move on.
#[logic]
fn client(i: ClientIn) -> ClientOut {
    let s = i.state;
    // Reset state is all zeros; address 0 would lock the LFSR.
    let addr = if s.addr == bits::<8>(0) {
        bits::<8>(1)
    } else {
        s.addr
    };
    let read = s.phase == Phase::Read;
    let wait = s.phase == Phase::Wait;
    let value = match s.phase {
        Phase::Write { value } => value,
        _ => bits::<16>(0),
    };
    let writing = matches!(s.phase, Phase::Write { .. });
    let got = wait && i.rsp_valid;
    let written = writing && i.req_ready;
    let next_phase = match s.phase {
        Phase::Read => {
            if i.req_ready {
                Phase::Wait
            } else {
                Phase::Read
            }
        }
        Phase::Wait => {
            if i.rsp_valid {
                Phase::Write { value: i.rsp.data }
            } else {
                Phase::Wait
            }
        }
        Phase::Write { value } => {
            if i.req_ready {
                Phase::Read
            } else {
                Phase::Write { value }
            }
        }
    };
    ClientOut {
        state: ClientState {
            phase: next_phase,
            k: if written { s.k + bits::<16>(1) } else { s.k },
            addr: if written { lfsr(addr) } else { addr },
            sum: if got {
                s.sum + bits::<16>(0).concat::<16, 32>(i.rsp.data)
            } else {
                s.sum
            },
        },
        req_valid: read || writing,
        req: MemReq {
            write: writing,
            addr,
            data: value + s.k + bits::<16>(1),
        },
        rsp_ready: wait,
    }
}

bitlogic::system! {
    mod rmw {
        /// The external memory.
        port mem: MemReq => MemRsp;
        unit client: ClientState = client(ClientIn {
            state,
            req_ready: mem.req_ready,
            rsp_valid: mem.rsp_valid,
            rsp: mem.rsp,
        });
        request mem(client.req_valid, client.req);
        respond mem(client.rsp_ready);
        output passes: Bits<16> = client.state.k;
        output sum: Bits<32> = client.state.sum;
        /// A port signal, registered.
        output seen: bool = mem.rsp_valid, registered;
    }
}

type Cells = Rc<RefCell<Vec<u16>>>;

/// A memory behind an in-order queue: reads answer, writes do not.
fn memory(
    cells: &Cells,
    depth: usize,
    latency: impl FnMut() -> u64 + 'static,
) -> InOrder<MemReq, MemRsp> {
    let cells = cells.clone();
    InOrder::new(depth, latency, move |r: MemReq| {
        let a = r.addr.get() as usize;
        let mut cells = cells.borrow_mut();
        if r.write {
            cells[a] = r.data.get() as u16;
            None
        } else {
            Some(MemRsp {
                data: bits(cells[a].into()),
            })
        }
    })
}

/// The other side, with extra stalls: on a random half of the cycles it
/// neither accepts nor offers, whatever its queue holds.
struct Stalling {
    inner: InOrder<MemReq, MemRsp>,
    rng: Rng,
    open: bool,
}

impl Responder<MemReq, MemRsp> for Stalling {
    fn req_ready(&self) -> bool {
        self.open && self.inner.req_ready()
    }
    fn rsp(&self) -> Option<MemRsp> {
        self.open.then(|| self.inner.rsp()).flatten()
    }
    fn edge(&mut self, req: Option<MemReq>, rsp_taken: bool) {
        self.inner.edge(req, rsp_taken);
        self.open = self.rng.next().is_multiple_of(2);
    }
}

/// The sequential reference: `passes` read-modify-write passes.
fn reference(passes: u32) -> (Vec<u16>, u32) {
    let mut cells = vec![0u16; 256];
    let (mut addr, mut sum) = (1u8, 0u32);
    for k in 0..passes {
        let v = cells[usize::from(addr)];
        sum = sum.wrapping_add(u32::from(v));
        cells[usize::from(addr)] = v.wrapping_add(k as u16 + 1);
        addr = (addr >> 1) ^ if addr & 1 == 1 { 0xb8 } else { 0 };
    }
    (cells, sum)
}

fn run<E: bitlogic::Evaluate>(
    eval: E,
    responder: Box<dyn Responder<MemReq, MemRsp>>,
    cycles: usize,
) -> (rmw::Core<E>, Vec<rmw::Outputs>) {
    let mut core = rmw::Core::new(eval);
    core.mem = responder;
    let trace = (0..cycles).map(|_| core.clock(rmw::Inputs {})).collect();
    (core, trace)
}

#[test]
fn results_do_not_depend_on_latency_or_stalls() {
    let mut rng = Rng(0x9047);
    let mut seen_depths = [false; 5];
    for trial in 0..24 {
        let depth = 1 + (rng.next() % 4) as usize;
        let max_latency = 1 + rng.next() % 24;
        let stall = trial % 3 == 0;
        let cells: Cells = Rc::new(RefCell::new(vec![0; 256]));
        let mut lat = Rng(rng.next());
        let inner = memory(&cells, depth, move || 1 + lat.next() % max_latency);
        let responder: Box<dyn Responder<MemReq, MemRsp>> = if stall {
            Box::new(Stalling {
                inner,
                rng: Rng(rng.next()),
                open: true,
            })
        } else {
            Box::new(inner)
        };
        let (core, trace) = run(Native, responder, 6_000);
        let passes = core.client.k.get() as u32;
        assert!(passes > 50, "trial {trial}: only {passes} passes");
        // A pass in `Write` has read its value (it is in `sum`) but not yet
        // written it back.
        let (want_cells, _) = reference(passes);
        let read_ahead = matches!(core.client.phase, Phase::Write { .. });
        let (_, want_sum) = reference(passes + u32::from(read_ahead));
        assert_eq!(core.client.sum.get() as u32, want_sum, "trial {trial}: sum");
        assert_eq!(*cells.borrow(), want_cells, "trial {trial}: memory");
        assert_eq!(trace.last().unwrap().passes.get() as u32, passes);
        // Handshakes transfer only when both sides are high.
        for c in &trace {
            let m = &c.mem;
            assert_eq!(m.request().is_some(), m.req_valid && m.req_ready);
        }
        seen_depths[depth] = true;
    }
    assert!(
        seen_depths[1..].iter().all(|&s| s),
        "every queue depth tried"
    );
}

#[test]
fn interpreted_model_matches_native() {
    let make = || {
        let cells: Cells = Rc::new(RefCell::new(vec![0; 256]));
        let mut lat = Rng(77);
        let inner = memory(&cells, 3, move || 1 + lat.next() % 9);
        Box::new(Stalling {
            inner,
            rng: Rng(5),
            open: true,
        }) as Box<dyn Responder<MemReq, MemRsp>>
    };
    let (_, a) = run(Native, make(), 3_000);
    let (_, b) = run(Interpreted::new(), make(), 3_000);
    assert_eq!(a, b);
}

#[test]
fn unconnected_port_never_transfers() {
    let mut core = rmw::Core::new(Native);
    for _ in 0..50 {
        let out = core.clock(rmw::Inputs {});
        assert!(out.mem.req_valid && !out.mem.req_ready && out.mem.request().is_none());
        assert_eq!(out.passes.get(), 0);
    }
}

#[test]
fn generated_verilog_matches_cycle_model() {
    let files = rmw::verilog().unwrap();
    let core_v = &files[0].1;
    for port in [
        "output wire [0:0] mem_req_valid",
        "output wire [0:0] mem_req_write",
        "output wire [7:0] mem_req_addr",
        "output wire [15:0] mem_req_data",
        "input wire [0:0] mem_req_ready",
        "input wire [0:0] mem_rsp_valid",
        "input wire [15:0] mem_rsp_data",
        "output wire [0:0] mem_rsp_ready",
    ] {
        assert!(core_v.contains(port), "{port} in\n{core_v}");
    }

    let cells: Cells = Rc::new(RefCell::new(vec![0; 256]));
    let mut lat = Rng(31);
    let inner = memory(&cells, 2, move || 1 + lat.next() % 7);
    let (_, trace) = run(
        Native,
        Box::new(Stalling {
            inner,
            rng: Rng(9),
            open: true,
        }),
        2_000,
    );

    // Replay the other side's signals; compare everything the core drives.
    let mut tb = String::from(
        "`timescale 1ns / 1ps\nmodule tb;\n    reg clk = 0, rst = 1;\n    always #5 clk = ~clk;\n    \
         reg [0:0] req_ready = 0, rsp_valid = 0;\n    reg [15:0] rsp_data = 0;\n    \
         wire [0:0] req_valid, req_write, rsp_ready, seen;\n    wire [7:0] req_addr;\n    \
         wire [15:0] req_data, passes;\n    wire [31:0] sum;\n    \
         rmw dut(.clk(clk), .rst(rst), .mem_req_valid(req_valid), .mem_req_write(req_write),\n        \
         .mem_req_addr(req_addr), .mem_req_data(req_data), .mem_req_ready(req_ready),\n        \
         .mem_rsp_valid(rsp_valid), .mem_rsp_data(rsp_data), .mem_rsp_ready(rsp_ready),\n        \
         .passes(passes), .sum(sum), .seen(seen));\n    initial begin\n        \
         @(posedge clk); @(posedge clk); #1 rst = 0;\n",
    );
    let mut transfers = 0;
    for (k, out) in trace.iter().enumerate() {
        let m: &PortCycle<MemReq, MemRsp> = &out.mem;
        transfers += usize::from(m.request().is_some()) + usize::from(m.response().is_some());
        // Before the edge: drive the other side, check what the core drives.
        writeln!(
            tb,
            "        req_ready = {}; rsp_valid = {}; rsp_data = 16'h{:x}; #1;\n        \
             if (req_valid !== {} || {{req_write, req_addr, req_data}} !== {{1'b{}, 8'h{:x}, 16'h{:x}}} && {} || rsp_ready !== {}) begin\n            \
             $display(\"cycle {k}: port %b %b %h %h %b\", req_valid, req_write, req_addr, req_data, rsp_ready); $fatal(1);\n        end\n        \
             @(posedge clk); #1;\n        \
             if (passes !== 16'h{:x} || sum !== 32'h{:x} || seen !== {}) begin\n            \
             $display(\"cycle {k}: passes=%h sum=%h seen=%b\", passes, sum, seen); $fatal(1);\n        end",
            u8::from(m.req_ready),
            u8::from(m.rsp_valid),
            m.rsp.data.get(),
            u8::from(m.req_valid),
            u8::from(m.req.write),
            m.req.addr.get(),
            m.req.data.get(),
            // Request fields matter only while valid.
            u8::from(m.req_valid),
            u8::from(m.rsp_ready),
            out.passes.get(),
            out.sum.get(),
            u8::from(out.seen),
        )
        .unwrap();
    }
    assert!(transfers > 300, "{transfers} transfers");
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
