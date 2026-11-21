//! Clocked composition for [`system!`](crate::system): units, memories and
//! boundary registers declared once, then simulated in Rust and emitted as
//! Verilog.
//!
//! The `system!` macro generates a typed cycle model that uses the types here
//! ([`Evaluate`], [`Memory`]) and a [`Design`] that [`Design::verilog`] turns
//! into the core module, one registered wrapper per unit, the ROMs and the
//! shared simple dual-port RAM.
use crate::value::Schema;
use crate::{BitVec, Error, Logic, LogicValue};
use std::any::TypeId;
use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
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
}

pub struct ParamSpec {
    pub name: &'static str,
    pub schema: Schema,
    pub default: Vec<BitVec>,
}

pub struct InputSpec {
    pub name: &'static str,
    pub schema: Schema,
    pub registered: bool,
}

pub struct OutputSpec {
    pub name: &'static str,
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
}

pub struct UnitSpec {
    pub name: &'static str,
    pub logic: Logic,
    pub state: Schema,
    /// Input struct fields other than `state`, and what drives them.
    pub connections: Vec<(&'static str, Source)>,
}

pub struct WriteSpec {
    pub memory: &'static str,
    pub enable: Source,
    pub address: Source,
    pub data: Source,
}

pub struct ReadSpec {
    pub memory: &'static str,
    pub address: Source,
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
        };
        base.select(path, what)
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
        if self.memories.iter().any(|m| m.contents.is_none()) {
            files.push(("bitlogic_sdp_ram.v".into(), SDP_RAM.into()));
        }
        Ok(files)
    }

    fn check_names(&self) -> Result<(), Error> {
        let mut seen = BTreeSet::new();
        let names = self.params.iter().map(|p| p.name);
        let names = names.chain(self.inputs.iter().map(|i| i.name));
        let names = names.chain(self.outputs.iter().map(|o| o.name));
        let names = names.chain(self.memories.iter().map(|m| m.name));
        let names = names.chain(self.units.iter().map(|u| u.name));
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
        let mut ports = vec!["input wire clk".to_string(), "input wire rst".to_string()];
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
            registers(&mut text, &reg, &raw.signals)?;
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
        for u in &self.units {
            self.instance(&mut text, u)?;
        }
        for m in &self.memories {
            self.memory_instance(&mut text, m)?;
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
                registers(&mut text, &reg, &source.signals)?;
                reg.signals
            } else {
                source.signals
            };
            for (port, value) in ports.signals.iter().zip(drive) {
                writeln!(text, "    assign {port} = {value};").unwrap();
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
                "unit `{}`: state enums (also inside variant fields) must list a variant with discriminant 0 first",
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
        writeln!(
            text,
            "    {name}_unit {name}_i (\n        .clk(clk), .rst(rst),\n        {}\n    );",
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
        if m.contents.is_some() {
            writeln!(
                text,
                "    {name}_rom {name}_i (.clk(clk), .addr({raddr}), .data({name}_rdata));",
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
        writeln!(
            text,
            "    bitlogic_sdp_ram #(.ADDR_W({addr}), .DATA_W({width})) {name}_i (\n        \
             .clk(clk), .we({}), .waddr({}), .wdata({}),\n        \
             .raddr({raddr}), .rdata({name}_rdata)\n    );",
            we.scalar(1, &what)?,
            waddr.scalar(addr, &what)?,
            wdata.scalar(width, &what)?,
            name = m.name
        )
        .unwrap();
        Ok(())
    }
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
fn registers(text: &mut String, reg: &Value, next: &[String]) -> Result<(), Error> {
    for (name, w) in reg.signals.iter().zip(reg.widths()?) {
        writeln!(
            text,
            "    reg [{}:0] {name} = {};",
            w - 1,
            hex(w, &BitVec::zero())
        )
        .unwrap();
    }
    text.push_str("    always @(posedge clk) begin\n        if (rst) begin\n");
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
// zero. Vivado infers block RAM for large instances and LUT RAM for small ones.
module bitlogic_sdp_ram #(
    parameter ADDR_W = 11,
    parameter DATA_W = 8
) (
    input  wire              clk,
    input  wire              we,
    input  wire [ADDR_W-1:0] waddr,
    input  wire [DATA_W-1:0] wdata,
    input  wire [ADDR_W-1:0] raddr,
    output reg  [DATA_W-1:0] rdata
);
    reg [DATA_W-1:0] mem [0:(1 << ADDR_W) - 1];

    integer i;
    initial begin
        for (i = 0; i < (1 << ADDR_W); i = i + 1)
            mem[i] = {DATA_W{1'b0}};
        rdata = {DATA_W{1'b0}};
    end

    always @(posedge clk) begin
        if (we)
            mem[waddr] <= wdata;
        rdata <= mem[raddr];
    end
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
