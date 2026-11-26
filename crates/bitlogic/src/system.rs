//! Clocked composition for [`system!`](crate::system): units, memories and
//! boundary registers declared once, then simulated in Rust and emitted as
//! Verilog.
//!
//! The `system!` macro generates a typed cycle model that uses the types here
//! ([`Evaluate`], [`Memory`], [`Responder`]) and a [`Design`] that
//! [`Design::verilog`] turns into the core module, one registered wrapper per
//! unit, the ROMs and the shared simple dual-port RAM.
//!
//! A `port` connects the system to something outside it with variable
//! latency, such as a DRAM controller behind clock-domain-crossing FIFOs: a
//! request channel out and a response channel in, each with valid/ready
//! handshakes that transfer on an edge where both are high. In the cycle
//! model a [`Responder`] plays the other side; [`InOrder`] is a queue with
//! configurable depth and latency.
use crate::value::Schema;
use crate::{BitVec, Error, Logic, LogicValue};
use std::any::TypeId;
use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::fmt::Write;

fn fail<T>(message: impl Into<String>) -> Result<T, Error> {
    Err(Error(message.into()))
}

/// A single-input `#[logic]` function: `#[logic]` implements this for the
/// marker type it generates alongside the function.
pub trait LogicFn: 'static {
    type Input: LogicValue;
    type Output: LogicValue;
    /// Native Rust evaluation.
    fn call(input: Self::Input) -> Self::Output;
    /// The captured expression graph.
    fn graph() -> Result<Logic, Error>;
}

/// How a cycle model evaluates its units' step functions.
pub trait Evaluate {
    fn eval<F: LogicFn>(&self, input: F::Input) -> F::Output;
}

/// Evaluate step functions as native Rust.
#[derive(Clone, Copy, Debug, Default)]
pub struct Native;

impl Evaluate for Native {
    fn eval<F: LogicFn>(&self, input: F::Input) -> F::Output {
        F::call(input)
    }
}

/// Evaluate step functions through their captured IR graphs. Each graph is
/// built on first use; a graph that fails to build or evaluate panics.
#[derive(Debug, Default)]
pub struct Interpreted {
    graphs: RefCell<HashMap<TypeId, Logic>>,
}

impl Interpreted {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Evaluate for Interpreted {
    fn eval<F: LogicFn>(&self, input: F::Input) -> F::Output {
        let mut graphs = self.graphs.borrow_mut();
        let logic = graphs
            .entry(TypeId::of::<F>())
            .or_insert_with(|| F::graph().expect("logic graph builds"));
        logic.evaluate_typed(input).expect("logic graph evaluates")
    }
}

/// A RAM or ROM in a cycle model: `2^addr_bits` cells and the registered read
/// output. Reads have one cycle of latency and return the old contents when
/// the same address is written on the same edge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Memory<T> {
    pub cells: Vec<T>,
    /// The registered read data: the cell addressed on the previous edge.
    pub rdata: T,
}

fn index<A: LogicValue>(address: A) -> usize {
    address
        .as_index()
        .and_then(|a| usize::try_from(a).ok())
        .expect("memory addresses are a single leaf that fits usize")
}

impl<T: LogicValue + Copy> Memory<T> {
    /// All cells zero.
    pub fn new(addr_bits: u32) -> Self {
        Self::rom(addr_bits, [])
    }
    /// `contents` from address 0; the remaining cells are zero. Panics when
    /// `contents` is longer than the memory.
    pub fn rom(addr_bits: u32, contents: impl IntoIterator<Item = T>) -> Self {
        let len = 1usize << addr_bits;
        let mut cells: Vec<T> = contents.into_iter().collect();
        assert!(
            cells.len() <= len,
            "memory contents exceed 2^{addr_bits} cells"
        );
        cells.resize(len, T::zeroed());
        // Before the first edge the read port holds the reset address, 0.
        let rdata = cells[0];
        Self { cells, rdata }
    }
    pub fn read<A: LogicValue>(&self, address: A) -> T {
        self.cells[index(address)]
    }
    pub fn write<A: LogicValue>(&mut self, address: A, data: T) {
        let i = index(address);
        self.cells[i] = data;
    }
}

/// What a `cross` carries between clock domains, and how.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CrossKind {
    /// A counter that moves by at most one (up or down, wrapping) per source
    /// edge, Gray-coded across: the destination sees either the value
    /// before a move or the one after it, never a mixture.
    Gray,
    /// Bits synchronized one by one (a level, a flag, a quasi-static value):
    /// while a value changes, each bit may arrive a destination edge apart.
    Level,
}

/// Synchronizer noise for a cycle model's crossings: with probability
/// `percent` / 100, a value that changed since the destination's last edge
/// is sampled, bit by bit at random, as it was before the change, as a
/// metastable first flip-flop may resolve. Off by default.
#[derive(Clone, Debug, Default)]
pub struct CdcNoise {
    state: u64,
    percent: u8,
}

impl CdcNoise {
    pub fn new(seed: u64, percent: u8) -> Self {
        Self {
            state: seed | 1,
            percent: percent.min(100),
        }
    }
    fn next(&mut self) -> u64 {
        // xorshift64*
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn fires(&mut self) -> bool {
        self.percent != 0 && self.next() % 100 < u64::from(self.percent)
    }
}

/// One `cross` in a cycle model, register for register as the generated
/// Verilog: a source register (Gray-coded for [`CrossKind::Gray`]) clocked
/// in the source domain, then two synchronizer flip-flops in the
/// destination domain; the destination sees the second.
#[derive(Clone, Debug)]
pub struct Crossing<T> {
    name: &'static str,
    kind: CrossKind,
    /// Source register (Gray code for `Gray`) and its value before the last
    /// source edge that changed it.
    src: Vec<BitVec>,
    prev: Vec<BitVec>,
    /// The source register changed since the destination's last edge.
    fresh: bool,
    s1: Vec<BitVec>,
    s2: Vec<BitVec>,
    _value: std::marker::PhantomData<T>,
}

fn gray_of(v: &BitVec) -> BitVec {
    let w = v.words();
    let shifted: Vec<u64> = (0..w.len())
        .map(|i| (w[i] >> 1) | (w.get(i + 1).copied().unwrap_or(0) << 63))
        .collect();
    BitVec::from_words(
        &w.iter()
            .zip(&shifted)
            .map(|(a, b)| a ^ b)
            .collect::<Vec<_>>(),
    )
}

fn binary_of(g: &BitVec, width: u16) -> BitVec {
    // Bit i of the binary value is the XOR of Gray bits i and above.
    let mut words = vec![0u64; crate::bitvec::words_for(usize::from(width))];
    let mut acc = false;
    for i in (0..usize::from(width)).rev() {
        acc ^= g.bit(i);
        if acc {
            words[i / 64] |= 1 << (i % 64);
        }
    }
    BitVec::from_words(&words)
}

impl<T: LogicValue + Copy> Crossing<T> {
    /// Panics if a [`CrossKind::Gray`] crossing does not carry a single
    /// `Bits` value of at most 64 bits.
    pub fn new(name: &'static str, kind: CrossKind) -> Self {
        let zero = T::zeroed().flatten();
        if kind == CrossKind::Gray {
            let width = T::schema().width().unwrap_or(0);
            assert!(
                zero.len() == 1 && (1..=64).contains(&width),
                "cross `{name}`: a Gray crossing carries one Bits value of 1 to 64 bits"
            );
        }
        Self {
            name,
            kind,
            src: zero.clone(),
            prev: zero.clone(),
            fresh: false,
            s1: zero.clone(),
            s2: zero,
            _value: std::marker::PhantomData,
        }
    }

    /// A source-domain edge: the source register takes `next` (zero with
    /// `rst`). Panics when a Gray crossing's value moves by more than one.
    pub fn source_edge(&mut self, next: T, rst: bool) {
        let mut leaves = next.flatten();
        if rst {
            leaves = T::zeroed().flatten();
        } else if self.kind == CrossKind::Gray {
            let width = T::schema().width().expect("checked in new");
            let old = binary_of(&self.src[0], width)
                .to_u64()
                .expect("at most 64 bits");
            let new = leaves[0].to_u64().expect("at most 64 bits");
            let mask = if width == 64 {
                u64::MAX
            } else {
                (1 << width) - 1
            };
            let step = new.wrapping_sub(old) & mask;
            assert!(
                step == 0 || step == 1 || step == mask,
                "cross `{}`: a Gray crossing's value moved from {old} to {new} in one \
                 source edge (at most one step at a time)",
                self.name
            );
        }
        if self.kind == CrossKind::Gray {
            leaves = leaves.iter().map(gray_of).collect();
        }
        if leaves != self.src {
            self.prev = std::mem::replace(&mut self.src, leaves);
            self.fresh = true;
        }
    }

    /// A destination-domain edge: the synchronizer shifts (zero with `rst`).
    pub fn dest_edge(&mut self, rst: bool, noise: &mut CdcNoise) {
        if rst {
            self.s1 = T::zeroed().flatten();
            self.s2 = self.s1.clone();
            return;
        }
        let mut sample = self.src.clone();
        if self.fresh && noise.fires() {
            for (bits, old) in sample.iter_mut().zip(&self.prev) {
                let words: Vec<u64> = bits
                    .words()
                    .iter()
                    .enumerate()
                    .map(|(i, &new)| {
                        let keep = noise.next();
                        let old = old.words().get(i).copied().unwrap_or(0);
                        (new & !keep) | (old & keep)
                    })
                    .collect();
                *bits = BitVec::from_words(&words);
            }
        }
        self.fresh = false;
        self.s2 = std::mem::replace(&mut self.s1, sample);
    }

    /// What the destination sees: the second synchronizer flip-flop.
    pub fn value(&self) -> T {
        let leaves = if self.kind == CrossKind::Gray {
            let width = T::schema().width().expect("checked in new");
            vec![binary_of(&self.s2[0], width)]
        } else {
            self.s2.clone()
        };
        T::read(&mut leaves.into_iter()).expect("same schema")
    }
}

/// Rising edges of several free-running clocks in time order, for driving a
/// multi-clock cycle model: each clock has a period and a phase (the time
/// of its first edge), in femtoseconds. Edges at the same time come in
/// clock order.
#[derive(Clone, Debug)]
pub struct Schedule {
    periods: Vec<u64>,
    next: Vec<u64>,
}

impl Schedule {
    /// `clocks`: `(period, phase)` in femtoseconds.
    pub fn new(clocks: &[(u64, u64)]) -> Self {
        assert!(
            clocks.iter().all(|&(p, _)| p > 0),
            "periods must be positive"
        );
        Self {
            periods: clocks.iter().map(|c| c.0).collect(),
            next: clocks.iter().map(|c| c.1).collect(),
        }
    }

    /// A period in femtoseconds from one in nanoseconds, off by `ppm` parts
    /// per million (positive: slower).
    pub fn period_fs(ns: f64, ppm: f64) -> u64 {
        (ns * 1e6 * (1.0 + ppm * 1e-6)).round() as u64
    }
}

impl Iterator for Schedule {
    /// `(time in femtoseconds, clock index)`.
    type Item = (u64, usize);
    fn next(&mut self) -> Option<(u64, usize)> {
        let (i, &t) = self.next.iter().enumerate().min_by_key(|&(i, &t)| (t, i))?;
        self.next[i] = t + self.periods[i];
        Some((t, i))
    }
}

/// Where a connection's value comes from. Paths select struct fields.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    Param {
        name: &'static str,
        path: Vec<&'static str>,
    },
    /// A system input, after its boundary register when it has one.
    Input {
        name: &'static str,
        path: Vec<&'static str>,
    },
    /// A unit's registered (current) state.
    State {
        unit: &'static str,
        path: Vec<&'static str>,
    },
    /// A unit's combinational output field (never `state`).
    Output {
        unit: &'static str,
        path: Vec<&'static str>,
    },
    /// A memory's registered read data.
    Rdata { memory: &'static str },
    /// A port's signal from outside the system this cycle (unregistered).
    Port {
        name: &'static str,
        signal: PortSignal,
        path: Vec<&'static str>,
    },
    /// A `cross`'s value in its destination domain.
    Cross { name: &'static str },
}

/// The signals a port brings into a system.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PortSignal {
    /// The other side accepts a request on this edge if one is valid.
    ReqReady,
    /// A response is offered on this edge.
    RspValid,
    /// The response offered (meaningful while `RspValid`).
    Rsp,
}

/// A `port`: request and response types and what drives its outgoing
/// signals (`request port(valid, data)` and `respond port(ready)`).
pub struct PortSpec {
    pub name: &'static str,
    /// The port's clock domain (`None` in a system with one clock).
    pub clock: Option<&'static str>,
    pub request: Schema,
    pub response: Schema,
    pub req_valid: Source,
    pub req: Source,
    pub rsp_ready: Source,
}

/// The other side of a `port` in a cycle model. Before each edge the model
/// asks what the responder offers, evaluates the units (whose outgoing port
/// signals may depend on it), then reports what transferred on the edge.
/// What a responder offers must not depend on the system's signals in the
/// same cycle, which is what makes the handshake free of combinational
/// loops in hardware as well.
pub trait Responder<Req, Rsp> {
    /// A request valid this cycle is accepted on the edge.
    fn req_ready(&self) -> bool;
    /// The response offered this cycle, if any.
    fn rsp(&self) -> Option<Rsp>;
    /// The edge: the request that transferred (valid and ready), and whether
    /// the offered response was consumed (valid and ready).
    fn edge(&mut self, req: Option<Req>, rsp_taken: bool);
}

/// A responder that never accepts a request and never responds: every port
/// of a new cycle model starts with one.
#[derive(Clone, Copy, Debug, Default)]
pub struct Unconnected;

impl<Req, Rsp> Responder<Req, Rsp> for Unconnected {
    fn req_ready(&self) -> bool {
        false
    }
    fn rsp(&self) -> Option<Rsp> {
        None
    }
    fn edge(&mut self, _: Option<Req>, _: bool) {}
}

/// A port's signals during one cycle, as both sides saw them before the edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PortCycle<Req, Rsp> {
    pub req_valid: bool,
    pub req: Req,
    pub req_ready: bool,
    pub rsp_valid: bool,
    pub rsp: Rsp,
    pub rsp_ready: bool,
}

impl<Req: LogicValue + Copy, Rsp: LogicValue + Copy> PortCycle<Req, Rsp> {
    /// Every signal low or zero.
    pub fn idle() -> Self {
        Self {
            req_valid: false,
            req: Req::zeroed(),
            req_ready: false,
            rsp_valid: false,
            rsp: Rsp::zeroed(),
            rsp_ready: false,
        }
    }
    /// The request transferred on the edge.
    pub fn request(&self) -> Option<Req> {
        (self.req_valid && self.req_ready).then_some(self.req)
    }
    /// The response consumed on the edge.
    pub fn response(&self) -> Option<Rsp> {
        (self.rsp_valid && self.rsp_ready).then_some(self.rsp)
    }
}

/// Per-request latency of an [`InOrder`] responder: called once per accepted
/// request.
pub type Latency = Box<dyn FnMut() -> u64>;

/// An in-order responder: up to `depth` requests in flight; each accepted
/// request is answered by `serve` when it is accepted (so a memory model
/// sees requests in order) and its response, if any, is offered `latency()`
/// edges later (at least one), in acceptance order. Requests without a
/// response (writes) still hold their place in the queue until their time,
/// as an in-order controller would.
pub struct InOrder<Req, Rsp> {
    depth: usize,
    latency: Latency,
    serve: Box<dyn FnMut(Req) -> Option<Rsp>>,
    /// `(edge at which it is due, response)` in acceptance order.
    queue: VecDeque<(u64, Option<Rsp>)>,
    edges: u64,
    /// Requests accepted and responses consumed so far.
    pub accepted: u64,
    pub consumed: u64,
}

impl<Req, Rsp: Copy> InOrder<Req, Rsp> {
    /// # Panics
    /// When `depth` is zero.
    pub fn new(
        depth: usize,
        latency: impl FnMut() -> u64 + 'static,
        serve: impl FnMut(Req) -> Option<Rsp> + 'static,
    ) -> Self {
        assert!(
            depth > 0,
            "an in-order responder holds at least one request"
        );
        Self {
            depth,
            latency: Box::new(latency),
            serve: Box::new(serve),
            queue: VecDeque::new(),
            edges: 0,
            accepted: 0,
            consumed: 0,
        }
    }

    /// Requests accepted whose response has not been consumed yet.
    pub fn in_flight(&self) -> usize {
        self.queue.len()
    }

    /// Retire due requests that have no response.
    fn retire(&mut self) {
        while let Some(&(due, None)) = self.queue.front() {
            if due > self.edges {
                break;
            }
            self.queue.pop_front();
        }
    }
}

impl<Req, Rsp: Copy> Responder<Req, Rsp> for InOrder<Req, Rsp> {
    fn req_ready(&self) -> bool {
        self.queue.len() < self.depth
    }
    fn rsp(&self) -> Option<Rsp> {
        match self.queue.front() {
            Some(&(due, Some(rsp))) if due <= self.edges => Some(rsp),
            _ => None,
        }
    }
    fn edge(&mut self, req: Option<Req>, rsp_taken: bool) {
        if rsp_taken {
            let taken = self.queue.pop_front();
            assert!(
                matches!(taken, Some((due, Some(_))) if due <= self.edges),
                "a response was consumed that was not offered"
            );
            self.consumed += 1;
        }
        self.edges += 1;
        if let Some(req) = req {
            assert!(
                self.queue.len() < self.depth,
                "a request was accepted while not ready"
            );
            let due = self.edges + (self.latency)().max(1) - 1;
            let rsp = (self.serve)(req);
            self.queue.push_back((due, rsp));
            self.accepted += 1;
        }
        self.retire();
    }
}

pub struct ParamSpec {
    pub name: &'static str,
    pub schema: Schema,
    pub default: Vec<BitVec>,
}

pub struct InputSpec {
    pub name: &'static str,
    pub clock: Option<&'static str>,
    pub schema: Schema,
    pub registered: bool,
}

pub struct OutputSpec {
    pub name: &'static str,
    pub clock: Option<&'static str>,
    pub schema: Schema,
    pub source: Source,
    pub registered: bool,
}

pub struct MemorySpec {
    pub name: &'static str,
    pub element: Schema,
    pub addr_bits: u32,
    /// `Some` for a ROM: one leaf value per cell.
    pub contents: Option<Vec<BitVec>>,
    /// The RAM's implementation (`ram ..., style = ...`); every style
    /// behaves the same: one cycle of read latency, read-first.
    pub style: RamStyle,
}

/// How a RAM is built on an FPGA: Vivado's `ram_style`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RamStyle {
    /// The synthesis tool chooses (block RAM for large RAMs).
    Auto,
    /// LUT RAM with its read register in the fabric: the read data leaves a
    /// flip-flop, much sooner after the clock than from a block RAM, for
    /// small RAMs on critical paths.
    Distributed,
    Block,
}

impl RamStyle {
    fn verilog(self) -> &'static str {
        match self {
            RamStyle::Auto => "auto",
            RamStyle::Distributed => "distributed",
            RamStyle::Block => "block",
        }
    }
}

pub struct UnitSpec {
    pub name: &'static str,
    pub clock: Option<&'static str>,
    pub logic: Logic,
    pub state: Schema,
    /// Input struct fields other than `state`, and what drives them.
    pub connections: Vec<(&'static str, Source)>,
}

pub struct WriteSpec {
    pub memory: &'static str,
    pub clock: Option<&'static str>,
    pub enable: Source,
    pub address: Source,
    pub data: Source,
}

pub struct ReadSpec {
    pub memory: &'static str,
    pub clock: Option<&'static str>,
    pub address: Source,
}

/// A `clock` domain: its name and nominal period (for the cycle model's
/// default schedule; the constraints take periods from the clocks Vivado
/// knows).
pub struct ClockSpec {
    pub name: &'static str,
    pub period_ns: Option<f64>,
}

/// A `cross`: `source`, in domain `from`, synchronized into domain `to`.
pub struct CrossSpec {
    pub name: &'static str,
    pub kind: CrossKind,
    pub schema: Schema,
    pub source: Source,
    pub from: &'static str,
    pub to: &'static str,
}

/// A whole `system!` declaration, as data for the Verilog backend.
pub struct Design {
    pub name: &'static str,
    pub params: Vec<ParamSpec>,
    pub inputs: Vec<InputSpec>,
    pub outputs: Vec<OutputSpec>,
    pub memories: Vec<MemorySpec>,
    pub units: Vec<UnitSpec>,
    pub writes: Vec<WriteSpec>,
    pub reads: Vec<ReadSpec>,
    pub ports: Vec<PortSpec>,
    /// Empty for a system with one clock (ports `clk` and `rst`); else
    /// ports `clk_<name>` and `rst_<name>` per domain.
    pub clocks: Vec<ClockSpec>,
    pub crossings: Vec<CrossSpec>,
}

/// A structured value in the generated core: its schema and one Verilog
/// expression per leaf.
#[derive(Clone)]
struct Value {
    schema: Schema,
    signals: Vec<String>,
}

impl Value {
    fn named(schema: Schema, name: impl Fn(&str) -> String) -> Self {
        let signals = schema.leaves().iter().map(|(path, _)| name(path)).collect();
        Self { schema, signals }
    }
    fn select(&self, path: &[&str], what: &str) -> Result<Value, Error> {
        let mut schema = &self.schema;
        let mut offset = 0;
        for name in path {
            let Schema::Struct { fields, .. } = schema else {
                return fail(format!("{what}: `{name}` selects from a non-struct"));
            };
            let mut found = None;
            for (field, child) in fields {
                if field == name {
                    found = Some(child);
                    break;
                }
                offset += child.leaves().len();
            }
            schema = found.ok_or_else(|| Error(format!("{what}: no field `{name}`")))?;
        }
        let n = schema.leaves().len();
        Ok(Value {
            schema: schema.clone(),
            signals: self.signals[offset..offset + n].to_vec(),
        })
    }
    fn widths(&self) -> Result<Vec<u16>, Error> {
        self.schema
            .leaves()
            .iter()
            .map(|(_, leaf)| leaf.width())
            .collect()
    }
    /// The single leaf of a scalar value, checked against `width`.
    fn scalar(&self, width: u16, what: &str) -> Result<&str, Error> {
        match (self.widths()?.as_slice(), self.signals.as_slice()) {
            ([w], [s]) if *w == width => Ok(s),
            _ => fail(format!("{what}: expected a {width}-bit scalar")),
        }
    }
}

/// `root` for a scalar, else `root_<path with dots as underscores>`.
fn leaf_name(root: &str, path: &str) -> String {
    if path.is_empty() {
        root.to_owned()
    } else {
        format!("{root}_{}", path.replace('.', "_"))
    }
}

/// Verilog keywords that a Rust identifier could collide with.
const VERILOG_KEYWORDS: &[&str] = &[
    "always",
    "and",
    "assign",
    "begin",
    "buf",
    "bufif0",
    "bufif1",
    "case",
    "casex",
    "casez",
    "cmos",
    "deassign",
    "default",
    "defparam",
    "disable",
    "edge",
    "else",
    "end",
    "endcase",
    "endfunction",
    "endmodule",
    "endprimitive",
    "endspecify",
    "endtable",
    "endtask",
    "event",
    "for",
    "force",
    "forever",
    "fork",
    "function",
    "highz0",
    "highz1",
    "if",
    "ifnone",
    "initial",
    "inout",
    "input",
    "integer",
    "join",
    "large",
    "localparam",
    "macromodule",
    "medium",
    "module",
    "nand",
    "negedge",
    "nmos",
    "nor",
    "not",
    "notif0",
    "notif1",
    "or",
    "output",
    "parameter",
    "pmos",
    "posedge",
    "primitive",
    "pull0",
    "pull1",
    "pulldown",
    "pullup",
    "rcmos",
    "real",
    "realtime",
    "reg",
    "release",
    "repeat",
    "rnmos",
    "rpmos",
    "rtran",
    "rtranif0",
    "rtranif1",
    "scalared",
    "signed",
    "small",
    "specify",
    "specparam",
    "strong0",
    "strong1",
    "supply0",
    "supply1",
    "table",
    "task",
    "time",
    "tran",
    "tranif0",
    "tranif1",
    "tri",
    "tri0",
    "tri1",
    "triand",
    "trior",
    "trireg",
    "unsigned",
    "vectored",
    "wait",
    "wand",
    "weak0",
    "weak1",
    "while",
    "wire",
    "wor",
    "xnor",
    "xor",
    "logic",
    "bit",
    "byte",
    "int",
    "clk",
    "rst",
];

fn hex(width: u16, value: &BitVec) -> String {
    value.to_verilog(width)
}

const STATE: &str = "state";

/// The unit wrapper's output for a state input port: `s` for a scalar state
/// (port `state`), `s_<field>` for `state_<field>`; `None` for other ports.
fn state_output(port: &str) -> Option<String> {
    if port == STATE {
        Some("s".into())
    } else {
        port.strip_prefix("state_")
            .map(|field| format!("s_{field}"))
    }
}

impl Design {
    /// The value a source refers to, in the core module's signal names.
    fn source(&self, source: &Source, what: &str) -> Result<Value, Error> {
        let (base, path) = match source {
            Source::Param { name, path } => {
                let p = self.params.iter().find(|p| p.name == *name);
                let p = p.ok_or_else(|| Error(format!("{what}: no param `{name}`")))?;
                (param_value(p), path.as_slice())
            }
            Source::Input { name, path } => {
                let i = self.inputs.iter().find(|i| i.name == *name);
                let i = i.ok_or_else(|| Error(format!("{what}: no input `{name}`")))?;
                (input_value(i), path.as_slice())
            }
            Source::State { unit, path } => (self.unit(unit, what)?.0, path.as_slice()),
            Source::Output { unit, path } => {
                if path.first() == Some(&STATE) {
                    return fail(format!(
                        "{what}: use the registered state, not `state` outputs"
                    ));
                }
                (self.unit(unit, what)?.1, path.as_slice())
            }
            Source::Rdata { memory } => {
                let m = self.memory(memory, what)?;
                let value = Value {
                    schema: m.element.clone(),
                    signals: vec![format!("{memory}_rdata")],
                };
                (value, &[][..])
            }
            Source::Port { name, signal, path } => {
                let p = self.ports.iter().find(|p| p.name == *name);
                let p = p.ok_or_else(|| Error(format!("{what}: no port `{name}`")))?;
                let value = match signal {
                    PortSignal::ReqReady => Value {
                        schema: Schema::Bool,
                        signals: vec![format!("{name}_req_ready")],
                    },
                    PortSignal::RspValid => Value {
                        schema: Schema::Bool,
                        signals: vec![format!("{name}_rsp_valid")],
                    },
                    PortSignal::Rsp => port_value(p, "rsp", &p.response),
                };
                (value, path.as_slice())
            }
            Source::Cross { name } => {
                let c = self.crossing(name, what)?;
                let value = Value {
                    schema: c.schema.clone(),
                    signals: vec![format!("{name}_sync")],
                };
                (value, &[][..])
            }
        };
        base.select(path, what)
    }

    fn crossing(&self, name: &str, what: &str) -> Result<&CrossSpec, Error> {
        self.crossings
            .iter()
            .find(|c| c.name == name)
            .ok_or_else(|| Error(format!("{what}: no crossing `{name}`")))
    }

    /// The clock domain a source belongs to: `Ok(None)` for a param (any
    /// domain); in a system with one clock every other source is in
    /// `Some(None)`.
    fn domain(&self, source: &Source, what: &str) -> Result<Option<Option<&'static str>>, Error> {
        Ok(Some(match source {
            Source::Param { .. } => return Ok(None),
            Source::Input { name, .. } => {
                let i = self.inputs.iter().find(|i| i.name == *name);
                i.ok_or_else(|| Error(format!("{what}: no input `{name}`")))?
                    .clock
            }
            Source::State { unit, .. } | Source::Output { unit, .. } => {
                let u = self.units.iter().find(|u| u.name == *unit);
                u.ok_or_else(|| Error(format!("{what}: no unit `{unit}`")))?
                    .clock
            }
            Source::Rdata { memory } => {
                let r = self.reads.iter().find(|r| r.memory == *memory);
                r.ok_or_else(|| Error(format!("{what}: no read port of `{memory}`")))?
                    .clock
            }
            Source::Port { name, .. } => {
                let p = self.ports.iter().find(|p| p.name == *name);
                p.ok_or_else(|| Error(format!("{what}: no port `{name}`")))?
                    .clock
            }
            Source::Cross { name } => Some(self.crossing(name, what)?.to),
        }))
    }

    /// `source` must be in `clock` (or be a param).
    fn same_domain(
        &self,
        source: &Source,
        clock: Option<&'static str>,
        what: &str,
    ) -> Result<(), Error> {
        match self.domain(source, what)? {
            Some(d) if d != clock => fail(format!(
                "{what}: its source is in clock domain `{}`, not `{}` (cross it with a \
                 `cross`)",
                d.unwrap_or("-"),
                clock.unwrap_or("-")
            )),
            _ => Ok(()),
        }
    }

    /// The clock and reset signals of a domain.
    fn clock_signals(&self, clock: Option<&str>) -> (String, String) {
        match clock {
            Some(d) => (format!("clk_{d}"), format!("rst_{d}")),
            None => ("clk".into(), "rst".into()),
        }
    }

    fn memory(&self, name: &str, what: &str) -> Result<&MemorySpec, Error> {
        self.memories
            .iter()
            .find(|m| m.name == name)
            .ok_or_else(|| Error(format!("{what}: no memory `{name}`")))
    }

    /// A unit's registered state and its combinational outputs.
    fn unit(&self, name: &str, what: &str) -> Result<(Value, Value), Error> {
        let u = self.units.iter().find(|u| u.name == name);
        let u = u.ok_or_else(|| Error(format!("{what}: no unit `{name}`")))?;
        let (input, output) = unit_values(u)?;
        let state = input.select(&[STATE], name)?;
        let state = Value {
            signals: state
                .signals
                .iter()
                .map(|port| {
                    state_output(port)
                        .map(|s| format!("{name}_{s}"))
                        .ok_or_else(|| Error(format!("{what}: state port `{port}`")))
                })
                .collect::<Result<_, _>>()?,
            ..state
        };
        let output = Value {
            signals: output
                .signals
                .iter()
                .map(|port| format!("{name}_o_{port}"))
                .collect(),
            ..output
        };
        Ok((state, output))
    }

    /// Every generated file as `(file name, contents)`: the core module, one
    /// wrapper per unit, one module per ROM and the shared RAM.
    pub fn verilog(&self) -> Result<Vec<(String, String)>, Error> {
        self.check_names()?;
        let mut files = vec![(format!("{}.v", self.name), self.core()?)];
        for u in &self.units {
            let module = format!("{}_unit", u.name);
            files.push((format!("{module}.v"), u.logic.to_unit_verilog(&module)?));
        }
        for m in &self.memories {
            if let Some(contents) = &m.contents {
                let module = format!("{}_rom", m.name);
                files.push((format!("{module}.v"), rom_verilog(&module, m, contents)?));
            }
        }
        if self
            .memories
            .iter()
            .any(|m| m.contents.is_none() && !self.two_clocks(m))
        {
            files.push(("bitlogic_sdp_ram.v".into(), SDP_RAM.into()));
        }
        if self.memories.iter().any(|m| self.two_clocks(m)) {
            files.push(("bitlogic_dc_ram.v".into(), DC_RAM.into()));
        }
        if !self.crossings.is_empty() {
            files.push(("bitlogic_cdc.v".into(), CDC.into()));
        }
        if !self.clocks.is_empty() {
            files.push((format!("{}_cdc.tcl", self.name), self.cdc_constraints()?));
        }
        Ok(files)
    }

    /// A RAM whose write and read ports are in different clock domains.
    fn two_clocks(&self, m: &MemorySpec) -> bool {
        let w = self.writes.iter().find(|w| w.memory == m.name);
        let r = self.reads.iter().find(|r| r.memory == m.name);
        matches!((w, r), (Some(w), Some(r)) if w.clock != r.clock)
    }

    /// Timing constraints for every crossing and every RAM written in one
    /// clock domain and read in another, as Tcl for `read_xdc -unmanaged`:
    /// a datapath-only maximum delay (and, for a Gray crossing, a bus skew)
    /// of the faster clock's period, from the source registers to the first
    /// synchronizer flip-flops; the periods come from the clocks Vivado
    /// knows at that point.
    fn cdc_constraints(&self) -> Result<String, Error> {
        let mut text = format!(
            "# Generated by bitlogic from `system!` `{}`; do not edit.\n\
             # Read with `read_xdc -unmanaged` after the clocks are created. The\n\
             # system's clock domains must not be in asynchronous clock groups\n\
             # (they would override these constraints).\n\
             proc bitlogic_cdc {{from_pattern to_pattern skew}} {{\n    \
             set leaf {{IS_PRIMITIVE && PRIMITIVE_LEVEL != MACRO}}\n    \
             set from [get_cells -quiet -hierarchical -filter \"NAME =~ $from_pattern && $leaf\"]\n    \
             set to [get_cells -quiet -hierarchical -filter \"NAME =~ $to_pattern && $leaf\"]\n    \
             if {{[llength $from] == 0 || [llength $to] == 0}} {{ return }}\n    \
             set pins [get_pins -quiet -of_objects [concat $from $to] -filter {{IS_CLOCK}}]\n    \
             set periods [lsort -real [get_property PERIOD [get_clocks -quiet -of_objects $pins]]]\n    \
             if {{[llength $periods] == 0}} {{ return }}\n    \
             set period [lindex $periods 0]\n    \
             set_max_delay -datapath_only -from $from -to $to $period\n    \
             if {{$skew}} {{ set_bus_skew -from $from -to $to $period }}\n\
             }}\n\
             # A dual-clock RAM: its read registers and, from whatever cells drive\n\
             # them, the memory cells (the tool may pack several RAMs' bits into\n\
             # one primitive, so the cells are found from the registers, not by name).\n\
             proc bitlogic_cdc_ram {{to_pattern}} {{\n    \
             set to [get_cells -quiet -hierarchical -filter \"NAME =~ $to_pattern && IS_PRIMITIVE\"]\n    \
             if {{[llength $to] == 0}} {{ return }}\n    \
             set d [get_pins -quiet -of_objects $to -filter {{REF_PIN_NAME == D}}]\n    \
             set from [filter [all_fanin -quiet -flat -startpoints_only -only_cells $d] {{PRIMITIVE_GROUP == DMEM}}]\n    \
             if {{[llength $from] == 0}} {{ return }}\n    \
             set pins [get_pins -quiet -of_objects [concat $from $to] -filter {{IS_CLOCK}}]\n    \
             set periods [lsort -real [get_property PERIOD [get_clocks -quiet -of_objects $pins]]]\n    \
             if {{[llength $periods] == 0}} {{ return }}\n    \
             set_max_delay -datapath_only -from $from -to $to [lindex $periods 0]\n\
             }}\n",
            self.name
        );
        for c in &self.crossings {
            let skew = u8::from(c.kind == CrossKind::Gray && c.schema.width()? > 1);
            writeln!(
                text,
                "bitlogic_cdc {{*{n}_cross_i/src_q_reg*}} {{*{n}_cross_i/s1_reg*}} {skew}",
                n = c.name
            )
            .unwrap();
        }
        for m in self.memories.iter().filter(|m| self.two_clocks(m)) {
            writeln!(text, "bitlogic_cdc_ram {{*{n}_i/*rdata_reg*}}", n = m.name).unwrap();
        }
        Ok(text)
    }

    fn check_names(&self) -> Result<(), Error> {
        let mut seen = BTreeSet::new();
        let names = self.params.iter().map(|p| p.name);
        let names = names.chain(self.inputs.iter().map(|i| i.name));
        let names = names.chain(self.outputs.iter().map(|o| o.name));
        let names = names.chain(self.memories.iter().map(|m| m.name));
        let names = names.chain(self.units.iter().map(|u| u.name));
        let names = names.chain(self.ports.iter().map(|p| p.name));
        let names = names.chain(self.crossings.iter().map(|c| c.name));
        // Clock names have their own namespace (a unit may share its
        // domain's name): they become `clk_<name>` and `rst_<name>`.
        let mut clocks = BTreeSet::new();
        for c in &self.clocks {
            if !crate::ir::identifier(c.name) || !clocks.insert(c.name) {
                return fail(format!("invalid or duplicate clock `{}`", c.name));
            }
        }
        for name in names.chain([self.name]) {
            if !crate::ir::identifier(name)
                || VERILOG_KEYWORDS.contains(&name)
                || !seen.insert(name)
            {
                return fail(format!("invalid or duplicate name `{name}`"));
            }
        }
        for m in &self.memories {
            if !(1..=24).contains(&m.addr_bits) {
                return fail(format!("{}: address width must be 1..=24 bits", m.name));
            }
            m.element.width()?;
            let writes = self.writes.iter().filter(|w| w.memory == m.name).count();
            let reads = self.reads.iter().filter(|r| r.memory == m.name).count();
            let want = usize::from(m.contents.is_none());
            if writes != want || reads != 1 {
                let kind = if want == 1 { "a RAM" } else { "a ROM" };
                return fail(format!(
                    "{}: {kind} needs {want} write and 1 read port, found {writes} and {reads}",
                    m.name
                ));
            }
        }
        self.check_domains()
    }

    /// Every clocked item names a declared domain (or none, with one
    /// clock), every connection stays in its consumer's domain, and every
    /// crossing goes from one domain to another.
    fn check_domains(&self) -> Result<(), Error> {
        let declared = |clock: Option<&str>, what: &str| -> Result<(), Error> {
            let ok = match clock {
                None => self.clocks.is_empty(),
                Some(d) => self.clocks.iter().any(|c| c.name == d),
            };
            if ok {
                Ok(())
            } else {
                fail(format!("{what}: needs one of the system's clock domains"))
            }
        };
        for u in &self.units {
            let what = format!("unit `{}`", u.name);
            declared(u.clock, &what)?;
            for (field, source) in &u.connections {
                self.same_domain(source, u.clock, &format!("{what} field `{field}`"))?;
            }
        }
        for i in &self.inputs {
            declared(i.clock, &format!("input `{}`", i.name))?;
        }
        for o in &self.outputs {
            let what = format!("output `{}`", o.name);
            declared(o.clock, &what)?;
            self.same_domain(&o.source, o.clock, &what)?;
        }
        for w in &self.writes {
            let what = format!("memory `{}` write port", w.memory);
            declared(w.clock, &what)?;
            for s in [&w.enable, &w.address, &w.data] {
                self.same_domain(s, w.clock, &what)?;
            }
        }
        for r in &self.reads {
            let what = format!("memory `{}` read port", r.memory);
            declared(r.clock, &what)?;
            self.same_domain(&r.address, r.clock, &what)?;
        }
        for p in &self.ports {
            let what = format!("port `{}`", p.name);
            declared(p.clock, &what)?;
            for s in [&p.req_valid, &p.req, &p.rsp_ready] {
                self.same_domain(s, p.clock, &what)?;
            }
        }
        for c in &self.crossings {
            let what = format!("crossing `{}`", c.name);
            if self.clocks.is_empty() {
                return fail(format!(
                    "{what}: a system with one clock has nothing to cross"
                ));
            }
            declared(Some(c.from), &what)?;
            declared(Some(c.to), &what)?;
            if c.from == c.to {
                return fail(format!("{what}: crosses from `{}` into itself", c.to));
            }
            self.same_domain(&c.source, Some(c.from), &what)?;
            if !matches!(c.source, Source::State { .. } | Source::Input { .. }) {
                return fail(format!("{what}: crosses a unit's state or an input"));
            }
            let width = c.schema.width()?;
            let leaves = c.schema.leaves().len();
            if leaves != 1 || (c.kind == CrossKind::Gray && width > 64) {
                return fail(format!(
                    "{what}: crosses one Bits or bool value (a Gray one of at most 64 bits)"
                ));
            }
            if self.source(&c.source, &what)?.schema != c.schema {
                return fail(format!("{what}: source type differs"));
            }
        }
        Ok(())
    }

    fn core(&self) -> Result<String, Error> {
        let mut params = Vec::new();
        for p in &self.params {
            let value = param_value(p);
            for ((name, w), v) in value.signals.iter().zip(value.widths()?).zip(&p.default) {
                params.push(format!("parameter [{}:0] {name} = {}", w - 1, hex(w, v)));
            }
        }
        let mut ports = Vec::new();
        if self.clocks.is_empty() {
            ports.push("input wire clk".to_string());
            ports.push("input wire rst".to_string());
        }
        for c in &self.clocks {
            let (clk, rst) = self.clock_signals(Some(c.name));
            ports.push(format!("input wire {clk}"));
            ports.push(format!("input wire {rst}"));
        }
        for i in &self.inputs {
            let value = Value::named(i.schema.clone(), |path| leaf_name(i.name, path));
            for (name, w) in value.signals.iter().zip(value.widths()?) {
                ports.push(format!("input wire [{}:0] {name}", w - 1));
            }
        }
        for o in &self.outputs {
            let value = Value::named(o.schema.clone(), |path| leaf_name(o.name, path));
            for (name, w) in value.signals.iter().zip(value.widths()?) {
                ports.push(format!("output wire [{}:0] {name}", w - 1));
            }
        }
        for p in &self.ports {
            let mut leaves = |dir: &str, value: Value| -> Result<(), Error> {
                for (name, w) in value.signals.iter().zip(value.widths()?) {
                    ports.push(format!("{dir} wire [{}:0] {name}", w - 1));
                }
                Ok(())
            };
            let flag = |name: String| Value {
                schema: Schema::Bool,
                signals: vec![name],
            };
            leaves("output", flag(format!("{}_req_valid", p.name)))?;
            leaves("output", port_value(p, "req", &p.request))?;
            leaves("input", flag(format!("{}_req_ready", p.name)))?;
            leaves("input", flag(format!("{}_rsp_valid", p.name)))?;
            leaves("input", port_value(p, "rsp", &p.response))?;
            leaves("output", flag(format!("{}_rsp_ready", p.name)))?;
        }

        let mut text = format!(
            "// Generated by bitlogic from `system!` `{name}`; do not edit.\n\
             `timescale 1ns / 1ps\nmodule {name}",
            name = self.name
        );
        if !params.is_empty() {
            write!(text, " #(\n    {}\n)", params.join(",\n    ")).unwrap();
        }
        writeln!(text, " (\n    {}\n);", ports.join(",\n    ")).unwrap();

        // Boundary registers on inputs.
        for i in self.inputs.iter().filter(|i| i.registered) {
            let raw = Value::named(i.schema.clone(), |path| leaf_name(i.name, path));
            let reg = input_value(i);
            let (clk, rst) = self.clock_signals(i.clock);
            registers(&mut text, &reg, &raw.signals, &clk, &rst)?;
        }

        // Units: registered state and combinational outputs, then instances.
        for u in &self.units {
            let (state, output) = self.unit(u.name, u.name)?;
            for value in [&state, &output] {
                for (name, w) in value.signals.iter().zip(value.widths()?) {
                    writeln!(text, "    wire [{}:0] {name};", w - 1).unwrap();
                }
            }
        }
        for m in &self.memories {
            writeln!(
                text,
                "    wire [{}:0] {}_rdata;",
                m.element.width()? - 1,
                m.name
            )
            .unwrap();
        }
        for c in &self.crossings {
            writeln!(
                text,
                "    wire [{}:0] {}_sync;",
                c.schema.width()? - 1,
                c.name
            )
            .unwrap();
        }
        for u in &self.units {
            self.instance(&mut text, u)?;
        }
        for m in &self.memories {
            self.memory_instance(&mut text, m)?;
        }
        for c in &self.crossings {
            let what = format!("crossing `{}`", c.name);
            let source = self.source(&c.source, &what)?;
            let (sclk, srst) = self.clock_signals(Some(c.from));
            let (dclk, drst) = self.clock_signals(Some(c.to));
            let module = match c.kind {
                CrossKind::Gray => "bitlogic_cdc_gray",
                CrossKind::Level => "bitlogic_cdc_level",
            };
            writeln!(
                text,
                "    {module} #(.W({w})) {n}_cross_i (\n        \
                 .src_clk({sclk}), .src_rst({srst}), .src_in({src}),\n        \
                 .dest_clk({dclk}), .dest_rst({drst}), .dest_out({n}_sync)\n    );",
                w = c.schema.width()?,
                n = c.name,
                src = source.scalar(c.schema.width()?, &what)?,
            )
            .unwrap();
        }

        // Outputs, registered or not.
        for o in &self.outputs {
            let what = format!("output `{}`", o.name);
            let source = self.source(&o.source, &what)?;
            if source.schema != o.schema {
                return fail(format!("{what}: source type differs"));
            }
            if !o.registered {
                match &o.source {
                    Source::Output { .. } => {
                        return fail(format!("{what}: a unit output must be `registered`"));
                    }
                    Source::Input { name, .. }
                        if !self.inputs.iter().any(|i| i.name == *name && i.registered) =>
                    {
                        return fail(format!("{what}: an unregistered input cannot drive it"));
                    }
                    _ => {}
                }
            }
            let ports = Value::named(o.schema.clone(), |path| leaf_name(o.name, path));
            let drive = if o.registered {
                let reg = Value {
                    signals: ports.signals.iter().map(|s| format!("{s}_q")).collect(),
                    ..ports.clone()
                };
                let (clk, rst) = self.clock_signals(o.clock);
                registers(&mut text, &reg, &source.signals, &clk, &rst)?;
                reg.signals
            } else {
                source.signals
            };
            for (port, value) in ports.signals.iter().zip(drive) {
                writeln!(text, "    assign {port} = {value};").unwrap();
            }
        }

        // Ports: the outgoing handshake signals, combinational.
        for p in &self.ports {
            let what = format!("port `{}`", p.name);
            let drives = [
                (
                    &p.req_valid,
                    Schema::Bool,
                    vec![format!("{}_req_valid", p.name)],
                    "request valid",
                ),
                (
                    &p.req,
                    p.request.clone(),
                    port_value(p, "req", &p.request).signals,
                    "request",
                ),
                (
                    &p.rsp_ready,
                    Schema::Bool,
                    vec![format!("{}_rsp_ready", p.name)],
                    "response ready",
                ),
            ];
            for (source, schema, signals, role) in drives {
                let what = format!("{what} {role}");
                if matches!(source, Source::Port { .. }) {
                    return fail(format!("{what}: a port cannot drive a port"));
                }
                let value = self.source(source, &what)?;
                if value.schema != schema {
                    return fail(format!("{what}: source type differs"));
                }
                for (port, v) in signals.iter().zip(&value.signals) {
                    writeln!(text, "    assign {port} = {v};").unwrap();
                }
            }
        }
        text.push_str("endmodule\n");
        Ok(text)
    }

    fn instance(&self, text: &mut String, u: &UnitSpec) -> Result<(), Error> {
        let (input, _) = unit_values(u)?;
        let (_, output_schema) = u.logic.schemas().expect("checked by unit_values");
        let declared = input.select(&[STATE], u.name)?.schema;
        let next = Value::named(output_schema.clone(), |_| String::new());
        if declared != u.state || next.select(&[STATE], u.name)?.schema != u.state {
            return fail(format!(
                "unit `{}`: `state` input/output type differs",
                u.name
            ));
        }
        // The register resets to all zeros, so the zero value must be that.
        if u.state.zero_values().iter().any(|v| !v.is_zero()) {
            return fail(format!(
                "unit `{}`: state enums (also the first variant's fields of a data enum) must list a variant with discriminant 0 first",
                u.name
            ));
        }
        let Schema::Struct { fields, .. } = &input.schema else {
            return fail(format!("unit `{}`: input must be a struct", u.name));
        };
        let mut bindings = Vec::new();
        for (field, _) in fields.iter().filter(|(f, _)| f != STATE) {
            let what = format!("unit `{}` field `{field}`", u.name);
            let mut sources = u.connections.iter().filter(|(f, _)| f == field);
            let (Some((_, source)), None) = (sources.next(), sources.next()) else {
                return fail(format!("{what}: needs exactly one connection"));
            };
            if matches!(source, Source::Output { .. }) {
                return fail(format!(
                    "{what}: a unit's combinational outputs may drive only memory ports and outputs"
                ));
            }
            let port = input.select(&[field], &what)?;
            let value = self.source(source, &what)?;
            if value.schema != port.schema {
                return fail(format!("{what}: source type differs"));
            }
            for (p, v) in port.signals.iter().zip(&value.signals) {
                bindings.push(format!(".i_{p}({v})"));
            }
        }
        if let Some((f, _)) = u
            .connections
            .iter()
            .find(|(f, _)| !fields.iter().any(|(name, _)| name == f) || *f == STATE)
        {
            return fail(format!("unit `{}`: unexpected connection `{f}`", u.name));
        }
        let (state, output) = self.unit(u.name, u.name)?;
        let raw = self.unit_ports(u)?;
        for (port, wire) in raw.1.iter().zip(&output.signals) {
            bindings.push(format!(".o_{port}({wire})"));
        }
        for (port, wire) in raw.0.iter().zip(&state.signals) {
            let s = state_output(port).expect("a state port");
            bindings.push(format!(".{s}({wire})"));
        }
        let (clk, rst) = self.clock_signals(u.clock);
        writeln!(
            text,
            "    {name}_unit {name}_i (\n        .clk({clk}), .rst({rst}),\n        {}\n    );",
            bindings.join(",\n        "),
            name = u.name
        )
        .unwrap();
        Ok(())
    }

    /// Port names of a unit's state leaves and non-state outputs.
    fn unit_ports(&self, u: &UnitSpec) -> Result<(Vec<String>, Vec<String>), Error> {
        let (input, output) = unit_values(u)?;
        Ok((input.select(&[STATE], u.name)?.signals, output.signals))
    }

    fn memory_instance(&self, text: &mut String, m: &MemorySpec) -> Result<(), Error> {
        let width = m.element.width()?;
        let addr = u16::try_from(m.addr_bits).expect("checked");
        let what = format!("memory `{}`", m.name);
        let read = self
            .reads
            .iter()
            .find(|r| r.memory == m.name)
            .expect("checked");
        let raddr = self.source(&read.address, &format!("{what} read address"))?;
        let raddr = raddr.scalar(addr, &what)?;
        let (rclk, _) = self.clock_signals(read.clock);
        if m.contents.is_some() {
            writeln!(
                text,
                "    {name}_rom {name}_i (.clk({rclk}), .addr({raddr}), .data({name}_rdata));",
                name = m.name
            )
            .unwrap();
            return Ok(());
        }
        let write = self
            .writes
            .iter()
            .find(|w| w.memory == m.name)
            .expect("checked");
        let we = self.source(&write.enable, &format!("{what} write enable"))?;
        let waddr = self.source(&write.address, &format!("{what} write address"))?;
        let wdata = self.source(&write.data, &format!("{what} write data"))?;
        if wdata.schema != m.element {
            return fail(format!("{what}: write data type differs"));
        }
        let (wclk, _) = self.clock_signals(write.clock);
        let (module, clocks) = if self.two_clocks(m) {
            ("bitlogic_dc_ram", format!(".wclk({wclk}), .rclk({rclk})"))
        } else {
            ("bitlogic_sdp_ram", format!(".clk({rclk})"))
        };
        writeln!(
            text,
            "    {module} #(.ADDR_W({addr}), .DATA_W({width}){style}) {name}_i (\n        \
             {clocks}, .we({}), .waddr({}), .wdata({}),\n        \
             .raddr({raddr}), .rdata({name}_rdata)\n    );",
            we.scalar(1, &what)?,
            waddr.scalar(addr, &what)?,
            wdata.scalar(width, &what)?,
            name = m.name,
            style = match m.style {
                RamStyle::Auto => String::new(),
                s => format!(", .STYLE(\"{}\")", s.verilog()),
            }
        )
        .unwrap();
        Ok(())
    }
}

/// A port's request (`req`) or response (`rsp`) leaves: `<port>_<dir>` for a
/// scalar, else `<port>_<dir>_<path>`.
fn port_value(p: &PortSpec, dir: &str, schema: &Schema) -> Value {
    Value::named(schema.clone(), |path| {
        leaf_name(&format!("{}_{dir}", p.name), path)
    })
}

fn param_value(p: &ParamSpec) -> Value {
    Value::named(p.schema.clone(), |path| {
        leaf_name(p.name, path).to_uppercase()
    })
}

fn input_value(i: &InputSpec) -> Value {
    Value::named(i.schema.clone(), |path| {
        let port = leaf_name(i.name, path);
        if i.registered {
            format!("{port}_q")
        } else {
            port
        }
    })
}

/// A unit's input and non-state output values, named by the graph's ports.
fn unit_values(u: &UnitSpec) -> Result<(Value, Value), Error> {
    let Some((input, output)) = u.logic.schemas() else {
        return fail(format!("unit `{}`: graph has no schemas", u.name));
    };
    let input = Value {
        schema: input.clone(),
        signals: u.logic.inputs().iter().map(|p| p.name.clone()).collect(),
    };
    let all = Value {
        schema: output.clone(),
        signals: u.logic.outputs().iter().map(|p| p.name.clone()).collect(),
    };
    let Schema::Struct { fields, .. } = output else {
        return fail(format!("unit `{}`: output must be a struct", u.name));
    };
    let mut signals = Vec::new();
    for (field, _) in fields.iter().filter(|(f, _)| f != STATE) {
        signals.extend(all.select(&[field], u.name)?.signals);
    }
    // The output schema keeps `state`; only the signal list drops it.
    let outputs = Value {
        schema: Schema::Struct {
            identity: TypeId::of::<()>(),
            name: "outputs",
            fields: fields.iter().filter(|(f, _)| f != STATE).cloned().collect(),
        },
        signals,
    };
    Ok((input, outputs))
}

/// Registers `reg` <= `next`, reset to zero.
fn registers(
    text: &mut String,
    reg: &Value,
    next: &[String],
    clk: &str,
    rst: &str,
) -> Result<(), Error> {
    for (name, w) in reg.signals.iter().zip(reg.widths()?) {
        writeln!(
            text,
            "    reg [{}:0] {name} = {};",
            w - 1,
            hex(w, &BitVec::zero())
        )
        .unwrap();
    }
    writeln!(
        text,
        "    always @(posedge {clk}) begin\n        if ({rst}) begin"
    )
    .unwrap();
    for (name, w) in reg.signals.iter().zip(reg.widths()?) {
        writeln!(text, "            {name} <= {};", hex(w, &BitVec::zero())).unwrap();
    }
    text.push_str("        end else begin\n");
    for (name, value) in reg.signals.iter().zip(next) {
        writeln!(text, "            {name} <= {value};").unwrap();
    }
    text.push_str("        end\n    end\n");
    Ok(())
}

/// Synchronous ROM with one cycle of read latency, like the RAM.
fn rom_verilog(module: &str, m: &MemorySpec, contents: &[BitVec]) -> Result<String, Error> {
    let (w, a) = (m.element.width()?, m.addr_bits);
    let first = contents.first().cloned().unwrap_or_default();
    let mut text = format!(
        "// Generated by bitlogic; do not edit.\n`timescale 1ns / 1ps\n\
         module {module}(\n    input wire clk,\n    input wire [{}:0] addr,\n    \
         output reg [{}:0] data\n);\n    initial data = {};\n    \
         always @(posedge clk) begin\n        case (addr)\n",
        a - 1,
        w - 1,
        hex(w, &first),
    );
    for (address, value) in contents.iter().enumerate() {
        if !value.is_zero() {
            writeln!(
                text,
                "            {a}'d{address}: data <= {};",
                hex(w, value)
            )
            .unwrap();
        }
    }
    writeln!(
        text,
        "            default: data <= {};\n        endcase\n    end\nendmodule",
        hex(w, &BitVec::zero())
    )
    .unwrap();
    Ok(text)
}

const SDP_RAM: &str = "// Generated by bitlogic; do not edit.
`timescale 1ns / 1ps

// Simple dual-port RAM: one write port, one always-enabled read port, one clock.
// Read data is registered (one cycle of latency) and read-first: a read of an
// address written on the same edge returns the old contents. Contents start at
// zero. STYLE is Vivado's ram_style: \"auto\" (block RAM for large instances,
// LUT RAM for small ones), \"distributed\" (LUT RAM, the read register in the
// fabric) or \"block\".
module bitlogic_sdp_ram #(
    parameter ADDR_W = 11,
    parameter DATA_W = 8,
    parameter STYLE = \"auto\"
) (
    input  wire              clk,
    input  wire              we,
    input  wire [ADDR_W-1:0] waddr,
    input  wire [DATA_W-1:0] wdata,
    input  wire [ADDR_W-1:0] raddr,
    output reg  [DATA_W-1:0] rdata
);
    initial rdata = {DATA_W{1'b0}};
    generate
        if (STYLE == \"distributed\") begin : g
            (* ram_style = \"distributed\" *) reg [DATA_W-1:0] mem [0:(1 << ADDR_W) - 1];
            integer i;
            initial
                for (i = 0; i < (1 << ADDR_W); i = i + 1)
                    mem[i] = {DATA_W{1'b0}};
            always @(posedge clk) begin
                if (we)
                    mem[waddr] <= wdata;
                rdata <= mem[raddr];
            end
        end else if (STYLE == \"block\") begin : g
            (* ram_style = \"block\" *) reg [DATA_W-1:0] mem [0:(1 << ADDR_W) - 1];
            integer i;
            initial
                for (i = 0; i < (1 << ADDR_W); i = i + 1)
                    mem[i] = {DATA_W{1'b0}};
            always @(posedge clk) begin
                if (we)
                    mem[waddr] <= wdata;
                rdata <= mem[raddr];
            end
        end else begin : g
            reg [DATA_W-1:0] mem [0:(1 << ADDR_W) - 1];
            integer i;
            initial
                for (i = 0; i < (1 << ADDR_W); i = i + 1)
                    mem[i] = {DATA_W{1'b0}};
            always @(posedge clk) begin
                if (we)
                    mem[waddr] <= wdata;
                rdata <= mem[raddr];
            end
        end
    endgenerate
endmodule
";

const DC_RAM: &str = "// Generated by bitlogic; do not edit.
`timescale 1ns / 1ps

// Simple dual-port RAM with a clock per port: the write port on `wclk`, the
// always-enabled read port on `rclk` (a RAM written in one clock domain and
// read in another).
// Read data is registered (one `rclk` cycle of latency); a read of an address
// being written in the other domain is undefined (the design must not). Contents start at
// zero. STYLE is Vivado's ram_style: \"auto\" (block RAM for large instances,
// LUT RAM for small ones), \"distributed\" (LUT RAM, the read register in the
// fabric) or \"block\".
module bitlogic_dc_ram #(
    parameter ADDR_W = 11,
    parameter DATA_W = 8,
    parameter STYLE = \"auto\"
) (
    input  wire              wclk,
    input  wire              rclk,
    input  wire              we,
    input  wire [ADDR_W-1:0] waddr,
    input  wire [DATA_W-1:0] wdata,
    input  wire [ADDR_W-1:0] raddr,
    output reg  [DATA_W-1:0] rdata
);
    initial rdata = {DATA_W{1'b0}};
    generate
        if (STYLE == \"distributed\") begin : g
            (* ram_style = \"distributed\" *) reg [DATA_W-1:0] mem [0:(1 << ADDR_W) - 1];
            integer i;
            initial
                for (i = 0; i < (1 << ADDR_W); i = i + 1)
                    mem[i] = {DATA_W{1'b0}};
            always @(posedge wclk)
                if (we)
                    mem[waddr] <= wdata;
            always @(posedge rclk)
                rdata <= mem[raddr];
        end else if (STYLE == \"block\") begin : g
            (* ram_style = \"block\" *) reg [DATA_W-1:0] mem [0:(1 << ADDR_W) - 1];
            integer i;
            initial
                for (i = 0; i < (1 << ADDR_W); i = i + 1)
                    mem[i] = {DATA_W{1'b0}};
            always @(posedge wclk)
                if (we)
                    mem[waddr] <= wdata;
            always @(posedge rclk)
                rdata <= mem[raddr];
        end else begin : g
            reg [DATA_W-1:0] mem [0:(1 << ADDR_W) - 1];
            integer i;
            initial
                for (i = 0; i < (1 << ADDR_W); i = i + 1)
                    mem[i] = {DATA_W{1'b0}};
            always @(posedge wclk)
                if (we)
                    mem[waddr] <= wdata;
            always @(posedge rclk)
                rdata <= mem[raddr];
        end
    endgenerate
endmodule
";

const CDC: &str = "// Generated by bitlogic; do not edit.
`timescale 1ns / 1ps

// Clock-domain crossings of `system!` `cross` items: a source register in the
// source domain, then two synchronizer flip-flops (ASYNC_REG) in the
// destination domain, which sees the second. The generated `<system>_cdc.tcl`
// constrains each source-to-first-flip-flop path.

// A counter that moves by at most one per source edge, crossed Gray-coded.
module bitlogic_cdc_gray #(
    parameter W = 4
) (
    input  wire         src_clk,
    input  wire         src_rst,
    input  wire [W-1:0] src_in,
    input  wire         dest_clk,
    input  wire         dest_rst,
    output wire [W-1:0] dest_out
);
    // Its own register per crossing (never merged with an equivalent one,
    // which the constraints would not name).
    (* DONT_TOUCH = \"TRUE\" *) reg [W-1:0] src_q = {W{1'b0}};
    (* ASYNC_REG = \"TRUE\" *) reg [W-1:0] s1_reg = {W{1'b0}};
    (* ASYNC_REG = \"TRUE\" *) reg [W-1:0] s2_reg = {W{1'b0}};
    always @(posedge src_clk)
        src_q <= src_rst ? {W{1'b0}} : (src_in ^ (src_in >> 1));
    always @(posedge dest_clk)
        if (dest_rst) begin
            s1_reg <= {W{1'b0}};
            s2_reg <= {W{1'b0}};
        end else begin
            s1_reg <= src_q;
            s2_reg <= s1_reg;
        end
    genvar i;
    generate
        for (i = 0; i < W; i = i + 1) begin : bin
            assign dest_out[i] = ^s2_reg[W-1:i];
        end
    endgenerate
endmodule

// Bits synchronized one by one.
module bitlogic_cdc_level #(
    parameter W = 1
) (
    input  wire         src_clk,
    input  wire         src_rst,
    input  wire [W-1:0] src_in,
    input  wire         dest_clk,
    input  wire         dest_rst,
    output wire [W-1:0] dest_out
);
    // Its own register per crossing (never merged with an equivalent one,
    // which the constraints would not name).
    (* DONT_TOUCH = \"TRUE\" *) reg [W-1:0] src_q = {W{1'b0}};
    (* ASYNC_REG = \"TRUE\" *) reg [W-1:0] s1_reg = {W{1'b0}};
    (* ASYNC_REG = \"TRUE\" *) reg [W-1:0] s2_reg = {W{1'b0}};
    always @(posedge src_clk)
        src_q <= src_rst ? {W{1'b0}} : src_in;
    always @(posedge dest_clk)
        if (dest_rst) begin
            s1_reg <= {W{1'b0}};
            s2_reg <= {W{1'b0}};
        end else begin
            s1_reg <= src_q;
            s2_reg <= s1_reg;
        end
    assign dest_out = s2_reg;
endmodule
";

impl Logic {
    /// Wrap this step function (emitted as `<module>_logic`) in a registered
    /// unit `<module>`. The input and output structs must both have a field
    /// named `state` of the same type: it becomes one state register with
    /// synchronous reset to zero, exposed as `s_<leaf>` outputs. The other
    /// input fields are `i_<leaf>` ports and the other outputs `o_<leaf>`.
    pub fn to_unit_verilog(&self, module: &str) -> Result<String, Error> {
        let inner = format!("{module}_logic");
        let mut text =
            String::from("// Generated by bitlogic; do not edit.\n`timescale 1ns / 1ps\n");
        text.push_str(&self.to_verilog(&inner)?);

        let split = |ports: &[crate::Port]| -> (Vec<crate::Port>, Vec<crate::Port>) {
            ports
                .iter()
                .cloned()
                .partition(|p| state_output(&p.name).is_some())
        };
        let (state_in, other_in) = split(self.inputs());
        let (state_out, other_out) = split(self.outputs());
        let shape = |ports: &[crate::Port]| -> Vec<(String, u16)> {
            ports.iter().map(|p| (p.name.clone(), p.width)).collect()
        };
        if state_in.is_empty() || shape(&state_in) != shape(&state_out) {
            return fail(format!("{module}: state input/output leaves differ"));
        }
        let width: usize = state_in.iter().map(|p| usize::from(p.width)).sum();

        let mut ports = vec!["input wire clk".to_string(), "input wire rst".to_string()];
        for p in &other_in {
            ports.push(format!("input wire [{}:0] i_{}", p.width - 1, p.name));
        }
        for p in &other_out {
            ports.push(format!("output wire [{}:0] o_{}", p.width - 1, p.name));
        }
        for p in &state_in {
            let s = state_output(&p.name).expect("a state port");
            ports.push(format!("output wire [{}:0] {s}", p.width - 1));
        }
        writeln!(
            text,
            "\nmodule {module}(\n    {}\n);",
            ports.join(",\n    ")
        )
        .unwrap();
        writeln!(text, "    reg [{}:0] state = {width}'d0;", width - 1).unwrap();
        writeln!(text, "    wire [{}:0] next_state;", width - 1).unwrap();

        let mut connections = Vec::new();
        let mut offset = 0;
        for p in &state_in {
            let s = state_output(&p.name).expect("a state port");
            let (lo, hi) = (offset, offset + usize::from(p.width) - 1);
            writeln!(text, "    assign {s} = state[{hi}:{lo}];").unwrap();
            connections.push(format!(".i_{}(state[{hi}:{lo}])", p.name));
            connections.push(format!(".o_{}(next_state[{hi}:{lo}])", p.name));
            offset += usize::from(p.width);
        }
        for p in other_in {
            connections.push(format!(".i_{0}(i_{0})", p.name));
        }
        for p in other_out {
            connections.push(format!(".o_{0}(o_{0})", p.name));
        }
        writeln!(
            text,
            "    {inner} step(\n        {}\n    );",
            connections.join(",\n        ")
        )
        .unwrap();
        writeln!(
            text,
            "    always @(posedge clk) begin\n        if (rst) state <= {width}'d0;\n        \
             else state <= next_state;\n    end\nendmodule"
        )
        .unwrap();
        Ok(text)
    }
}
