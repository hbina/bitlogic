use crate::bitvec::{self, BitVec, MAX_WIDTH, words_for};
use crate::{LogicValue, Schema};
use std::{
    error,
    fmt::{self, Write},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error(pub String);
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl error::Error for Error {}
pub(crate) fn fail<T>(message: &str) -> Result<T, Error> {
    Err(Error(message.into()))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(pub(crate) usize);
impl NodeId {
    pub fn index(self) -> usize {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Binary {
    And,
    Or,
    Xor,
    Add,
    Sub,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Operation {
    Input(usize),
    Constant(BitVec),
    Not(NodeId),
    Binary(Binary, NodeId, NodeId),
    Mux {
        condition: NodeId,
        yes: NodeId,
        no: NodeId,
    },
    Slice {
        value: NodeId,
        offset: u16,
    },
    Concat(NodeId, NodeId),
    Shift {
        value: NodeId,
        amount: u32,
        left: bool,
    },
    /// `a × b`, cut to the node's width. The operands may have any widths:
    /// each is extended to the node's width first, with copies of its top
    /// bit when `signed` (two's complement), else with zeros.
    Mul {
        a: NodeId,
        b: NodeId,
        signed: bool,
    },
    /// The operand one clock cycle ago: a pipeline register, added by
    /// [`crate::pipeline`]'s rewrite rules (never by `#[logic]`). Pure
    /// evaluation, which has no clock, passes the operand through.
    Reg(NodeId),
}

impl Operation {
    /// This operation with each operand `id` replaced by `map(id)`.
    pub fn remap(&self, mut map: impl FnMut(NodeId) -> NodeId) -> Operation {
        match *self {
            Operation::Input(index) => Operation::Input(index),
            Operation::Constant(ref v) => Operation::Constant(v.clone()),
            Operation::Not(a) => Operation::Not(map(a)),
            Operation::Binary(op, a, b) => Operation::Binary(op, map(a), map(b)),
            Operation::Mux { condition, yes, no } => Operation::Mux {
                condition: map(condition),
                yes: map(yes),
                no: map(no),
            },
            Operation::Slice { value, offset } => Operation::Slice {
                value: map(value),
                offset,
            },
            Operation::Concat(a, b) => Operation::Concat(map(a), map(b)),
            Operation::Shift {
                value,
                amount,
                left,
            } => Operation::Shift {
                value: map(value),
                amount,
                left,
            },
            Operation::Mul { a, b, signed } => Operation::Mul {
                a: map(a),
                b: map(b),
                signed,
            },
            Operation::Reg(a) => Operation::Reg(map(a)),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    pub width: u16,
    pub operation: Operation,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Port {
    pub name: String,
    pub width: u16,
    pub value: NodeId,
}

/// A DAG of total, deterministic, unsigned bit-vector expressions. No clocks or storage.
#[derive(Clone, Debug)]
pub struct Logic {
    pub(crate) nodes: Vec<Node>,
    pub(crate) inputs: Vec<Port>,
    pub(crate) outputs: Vec<Port>,
    pub(crate) schemas: Option<(Schema, Schema)>,
    /// The interpreter's word layout, computed on first evaluation.
    layout: std::sync::OnceLock<Layout>,
    /// The `(input, output)` types last checked against `schemas` by
    /// `evaluate_typed`, so later calls skip building the schemas.
    typed: std::sync::OnceLock<(std::any::TypeId, std::any::TypeId)>,
}

/// Where each node's value starts in the interpreter's word array (and the
/// total at the end), and whether a node and its operands all fit one word.
#[derive(Clone, Debug)]
struct Layout {
    offsets: Vec<usize>,
    small: Vec<bool>,
}

impl Logic {
    pub fn schemas(&self) -> Option<(&Schema, &Schema)> {
        self.schemas.as_ref().map(|(input, output)| (input, output))
    }

    pub fn with_schemas(mut self, input: Schema, output: Schema) -> Result<Self, Error> {
        for (schema, ports) in [(&input, &self.inputs), (&output, &self.outputs)] {
            let leaves = schema.leaves();
            if leaves.len() != ports.len() {
                return fail("schema port count mismatch");
            }
            for ((_, leaf), port) in leaves.iter().zip(ports) {
                if leaf.width()? != port.width {
                    return fail("schema port width mismatch");
                }
            }
        }
        self.schemas = Some((input, output));
        self.typed = std::sync::OnceLock::new();
        Ok(self)
    }

    pub fn evaluate_typed<I: LogicValue, O: LogicValue>(&self, input: I) -> Result<O, Error> {
        let types = (std::any::TypeId::of::<I>(), std::any::TypeId::of::<O>());
        if self.typed.get() != Some(&types) {
            if self.schemas.as_ref() != Some(&(I::schema(), O::schema())) {
                return fail("typed evaluation schema mismatch");
            }
            // Remember the first matching pair; others are checked each time.
            let _ = self.typed.set(types);
        }
        O::from_values(&self.evaluate(&input.flatten())?)
    }

    /// Change external HDL names without changing field schemas or port order.
    pub fn rename_ports(mut self, inputs: &[&str], outputs: &[&str]) -> Result<Self, Error> {
        for (ports, names) in [(&mut self.inputs, inputs), (&mut self.outputs, outputs)] {
            if ports.len() != names.len() {
                return fail("port rename count mismatch");
            }
            let mut seen = std::collections::BTreeSet::new();
            for (port, &name) in ports.iter_mut().zip(names) {
                if !identifier(name) || !seen.insert(name) {
                    return fail("invalid or duplicate port name");
                }
                port.name = name.into();
            }
        }
        Ok(self)
    }

    /// Structural input dependencies for each output, in input port order.
    pub fn dependencies(&self) -> Vec<(String, Vec<String>)> {
        let mut dependencies: Vec<std::collections::BTreeSet<usize>> = Vec::new();
        for node in &self.nodes {
            let mut set = std::collections::BTreeSet::new();
            if let Operation::Input(index) = node.operation {
                set.insert(index);
            }
            for source in operands(&node.operation) {
                set.extend(&dependencies[source.0]);
            }
            dependencies.push(set);
        }
        let paths = |schema: Option<&Schema>, ports: &[Port]| -> Vec<String> {
            match schema {
                Some(schema) => schema
                    .leaves()
                    .iter()
                    .zip(ports)
                    .map(|((path, _), port)| {
                        if path.is_empty() {
                            port.name.clone()
                        } else {
                            path.clone()
                        }
                    })
                    .collect(),
                None => ports.iter().map(|p| p.name.clone()).collect(),
            }
        };
        let inputs = paths(self.schemas().map(|s| s.0), &self.inputs);
        let outputs = paths(self.schemas().map(|s| s.1), &self.outputs);
        self.outputs
            .iter()
            .zip(outputs)
            .map(|(port, name)| {
                (
                    name,
                    dependencies[port.value.0]
                        .iter()
                        .map(|&i| inputs[i].clone())
                        .collect(),
                )
            })
            .collect()
    }

    pub fn nodes(&self) -> &[Node] {
        &self.nodes
    }
    /// A graph from parts, checked: operands precede their nodes, ports
    /// name valid nodes, widths are in range.
    pub(crate) fn from_parts(
        nodes: Vec<Node>,
        inputs: Vec<Port>,
        outputs: Vec<Port>,
        schemas: Option<(Schema, Schema)>,
    ) -> Result<Self, Error> {
        for (i, node) in nodes.iter().enumerate() {
            if !(1..=MAX_WIDTH).contains(&node.width)
                || operands(&node.operation).iter().any(|id| id.0 >= i)
            {
                return fail("malformed logic graph");
            }
        }
        if inputs
            .iter()
            .chain(&outputs)
            .any(|p| p.value.0 >= nodes.len())
        {
            return fail("malformed logic graph");
        }
        Ok(Logic {
            nodes,
            inputs,
            outputs,
            schemas,
            layout: std::sync::OnceLock::new(),
            typed: std::sync::OnceLock::new(),
        })
    }
    /// Whether the graph has pipeline registers (and so a clock).
    pub fn is_clocked(&self) -> bool {
        self.nodes
            .iter()
            .any(|n| matches!(n.operation, Operation::Reg(_)))
    }
    pub fn inputs(&self) -> &[Port] {
        &self.inputs
    }
    pub fn outputs(&self) -> &[Port] {
        &self.outputs
    }

    /// Port values (`u64`s or [`BitVec`]s) use schema leaf order (low-level
    /// graphs use insertion order). Out-of-range inputs are errors.
    pub fn evaluate<V: Clone + Into<BitVec>>(&self, inputs: &[V]) -> Result<Vec<BitVec>, Error> {
        if inputs.len() != self.inputs.len() {
            return fail("input port count mismatch");
        }
        let inputs: Vec<BitVec> = inputs.iter().cloned().map(Into::into).collect();
        for (port, value) in self.inputs.iter().zip(&inputs) {
            if !value.fits(port.width) {
                return fail("input value exceeds port width");
            }
        }
        // Every node's value, in one array: node i at words offsets[i]..
        let layout = self.layout.get_or_init(|| {
            let mut offsets = Vec::with_capacity(self.nodes.len() + 1);
            let mut total = 0;
            for node in &self.nodes {
                offsets.push(total);
                total += words_for(usize::from(node.width));
            }
            offsets.push(total);
            let small = self
                .nodes
                .iter()
                .map(|node| {
                    node.width <= 64
                        && operands(&node.operation)
                            .iter()
                            .all(|id| self.nodes[id.0].width <= 64)
                })
                .collect();
            Layout { offsets, small }
        });
        let offsets = &layout.offsets;
        let mut words = vec![0u64; offsets[self.nodes.len()]];
        for (i, node) in self.nodes.iter().enumerate() {
            // Operands precede the node, so they are all in `done`.
            let (done, rest) = words.split_at_mut(offsets[i]);
            let out = &mut rest[..offsets[i + 1] - offsets[i]];
            match node.operation {
                Operation::Input(index) => bitvec::copy(out, inputs[index].words()),
                ref operation if layout.small[i] => {
                    let value = |id: NodeId| done[offsets[id.0]];
                    out[0] = compute_small(operation, value, |id| self.nodes[id.0].width)
                        & (u64::MAX >> (64 - node.width));
                }
                ref operation => compute(
                    operation,
                    |id| &done[offsets[id.0]..offsets[id.0 + 1]],
                    |id| self.nodes[id.0].width,
                    out,
                    node.width,
                ),
            }
        }
        Ok(self
            .outputs
            .iter()
            .map(|p| BitVec::from_words(&words[offsets[p.value.0]..offsets[p.value.0 + 1]]))
            .collect())
    }

    /// Compose structured interfaces by nominal type and schema; low-level graphs by widths.
    pub fn then(&self, next: &Self) -> Result<Self, Error> {
        match (&self.schemas, &next.schemas) {
            (Some((_, output)), Some((input, _))) if output == input => {}
            (None, None) => {}
            _ => return fail("composition logical schema mismatch"),
        }
        if self.outputs.len() != next.inputs.len()
            || self
                .outputs
                .iter()
                .zip(&next.inputs)
                .any(|(a, b)| a.width != b.width)
        {
            return fail("composition port widths/count mismatch");
        }
        let mut result = self.clone();
        result.layout = std::sync::OnceLock::new();
        result.typed = std::sync::OnceLock::new();
        let mut mapped = Vec::new();
        for node in &next.nodes {
            let id = if let Operation::Input(index) = node.operation {
                self.outputs[index].value
            } else {
                let operation = node.operation.remap(|id| mapped[id.0]);
                let id = NodeId(result.nodes.len());
                result.nodes.push(Node {
                    width: node.width,
                    operation,
                });
                id
            };
            mapped.push(id);
        }
        result.outputs = next
            .outputs
            .iter()
            .map(|p| Port {
                value: mapped[p.value.0],
                ..p.clone()
            })
            .collect();
        result.schemas = self
            .schemas
            .as_ref()
            .zip(next.schemas.as_ref())
            .map(|((input, _), (_, output))| (input.clone(), output.clone()));
        Ok(result)
    }

    /// Emit portable combinational Verilog. Ports are prefixed with i_ / o_.
    pub fn to_verilog(&self, module: &str) -> Result<String, Error> {
        if !identifier(module) {
            return fail("invalid Verilog module identifier");
        }
        // Prefixing avoids Verilog keywords in Rust parameter names.
        let clock = self.is_clocked().then(|| "input wire clk".to_string());
        let ports: Vec<_> = clock
            .into_iter()
            .chain(
                self.inputs
                    .iter()
                    .map(|p| format!("input wire [{}:0] i_{}", p.width - 1, p.name)),
            )
            .chain(
                self.outputs
                    .iter()
                    .map(|p| format!("output wire [{}:0] o_{}", p.width - 1, p.name)),
            )
            .collect();
        let mut text = format!("module {module}(\n    {}\n);\n", ports.join(",\n    "));
        // Name the encodings of enum-typed ports and the layouts of data
        // enum payloads.
        if let Some((input, output)) = &self.schemas {
            for (prefix, schema, ports) in
                [("i", input, &self.inputs), ("o", output, &self.outputs)]
            {
                for (note, port) in schema.leaf_notes().iter().zip(ports) {
                    if let Some(note) = note {
                        writeln!(text, "// {prefix}_{}: {note}", port.name).unwrap();
                    }
                }
            }
        }
        for (index, node) in self.nodes.iter().enumerate() {
            let expr = match node.operation {
                // No reset: the stall that pipelines a unit refills every
                // register before its value is used.
                Operation::Reg(a) => {
                    writeln!(
                        text,
                        "reg [{w}:0] n{index} = {w1}'d0;\nalways @(posedge clk) n{index} <= n{a};",
                        w = node.width - 1,
                        w1 = node.width,
                        a = a.0
                    )
                    .unwrap();
                    continue;
                }
                Operation::Input(p) => format!("i_{}", self.inputs[p].name),
                Operation::Constant(ref v) => v.to_verilog(node.width),
                Operation::Not(a) => format!("~n{}", a.0),
                Operation::Binary(op, a, b) => {
                    let op = match op {
                        Binary::And => "&",
                        Binary::Or => "|",
                        Binary::Xor => "^",
                        Binary::Add => "+",
                        Binary::Sub => "-",
                        Binary::Eq => "==",
                        Binary::Ne => "!=",
                        Binary::Lt => "<",
                        Binary::Le => "<=",
                        Binary::Gt => ">",
                        Binary::Ge => ">=",
                    };
                    format!("n{} {op} n{}", a.0, b.0)
                }
                Operation::Mux { condition, yes, no } => {
                    format!("n{} ? n{} : n{}", condition.0, yes.0, no.0)
                }
                Operation::Slice { value, offset } => {
                    format!("n{}[{}:{}]", value.0, offset + node.width - 1, offset)
                }
                Operation::Concat(a, b) => format!("{{n{}, n{}}}", a.0, b.0),
                Operation::Shift {
                    value,
                    amount,
                    left,
                } => format!("n{} {} {amount}", value.0, if left { "<<" } else { ">>" }),
                // The assignment's width (the node's, at least the operands')
                // sizes the product; `$signed` operands are sign-extended to it.
                Operation::Mul { a, b, signed } => {
                    if signed {
                        format!("$signed(n{}) * $signed(n{})", a.0, b.0)
                    } else {
                        format!("n{} * n{}", a.0, b.0)
                    }
                }
            };
            writeln!(
                text,
                "wire [{}:0] n{index};\nassign n{index} = {expr};",
                node.width - 1
            )
            .unwrap();
        }
        for port in &self.outputs {
            writeln!(text, "assign o_{} = n{};", port.name, port.value.0).unwrap();
        }
        text.push_str("endmodule\n");
        Ok(text)
    }
}

/// The value of a non-input operation whose value and operands are at most
/// 64 bits wide (unmasked): the interpreter's fast path.
fn compute_small(
    operation: &Operation,
    value: impl Fn(NodeId) -> u64,
    width: impl Fn(NodeId) -> u16,
) -> u64 {
    match *operation {
        Operation::Input(_) => unreachable!("inputs have no operands"),
        Operation::Constant(ref v) => v.to_u64().unwrap_or(0),
        Operation::Reg(a) => value(a),
        Operation::Not(a) => !value(a),
        Operation::Binary(op, a, b) => {
            let (a, b) = (value(a), value(b));
            match op {
                Binary::And => a & b,
                Binary::Or => a | b,
                Binary::Xor => a ^ b,
                Binary::Add => a.wrapping_add(b),
                Binary::Sub => a.wrapping_sub(b),
                Binary::Eq => u64::from(a == b),
                Binary::Ne => u64::from(a != b),
                Binary::Lt => u64::from(a < b),
                Binary::Le => u64::from(a <= b),
                Binary::Gt => u64::from(a > b),
                Binary::Ge => u64::from(a >= b),
            }
        }
        Operation::Mux { condition, yes, no } => {
            if value(condition) != 0 {
                value(yes)
            } else {
                value(no)
            }
        }
        Operation::Slice { value: a, offset } => value(a) >> offset,
        Operation::Concat(a, b) => (value(a) << width(b)) | value(b),
        Operation::Shift {
            value: a,
            amount,
            left,
        } => if left {
            value(a).checked_shl(amount)
        } else {
            value(a).checked_shr(amount)
        }
        .unwrap_or(0),
        Operation::Mul { a, b, signed } => {
            if signed {
                let x = bitvec::sign_extend(value(a), u32::from(width(a)));
                let y = bitvec::sign_extend(value(b), u32::from(width(b)));
                x.wrapping_mul(y) as u64
            } else {
                value(a).wrapping_mul(value(b))
            }
        }
    }
}

/// Write the value of a non-input operation, `width` bits wide, into `out`
/// from its operands' words.
fn compute<'a>(
    operation: &'a Operation,
    value: impl Fn(NodeId) -> &'a [u64],
    width: impl Fn(NodeId) -> u16,
    out: &mut [u64],
    out_width: u16,
) {
    let w = usize::from(out_width);
    match *operation {
        Operation::Input(_) => unreachable!("inputs have no operands"),
        Operation::Constant(ref v) => bitvec::copy(out, v.words()),
        Operation::Reg(a) => bitvec::copy(out, value(a)),
        Operation::Not(a) => bitvec::not(out, value(a), w),
        Operation::Binary(op, a, b) => {
            let (a, b) = (value(a), value(b));
            match op {
                Binary::And => bitvec::bitwise(out, a, b, |x, y| x & y),
                Binary::Or => bitvec::bitwise(out, a, b, |x, y| x | y),
                Binary::Xor => bitvec::bitwise(out, a, b, |x, y| x ^ y),
                Binary::Add => bitvec::add(out, a, b, w, false),
                Binary::Sub => bitvec::add(out, a, b, w, true),
                _ => {
                    let order = bitvec::compare(a, b);
                    let result = match op {
                        Binary::Eq => order.is_eq(),
                        Binary::Ne => order.is_ne(),
                        Binary::Lt => order.is_lt(),
                        Binary::Le => order.is_le(),
                        Binary::Gt => order.is_gt(),
                        _ => order.is_ge(),
                    };
                    bitvec::copy(out, &[u64::from(result)]);
                }
            }
        }
        Operation::Mux { condition, yes, no } => {
            let chosen = if bitvec::is_zero(value(condition)) {
                no
            } else {
                yes
            };
            bitvec::copy(out, value(chosen));
        }
        Operation::Slice { value: a, offset } => bitvec::shr(out, value(a), usize::from(offset), w),
        Operation::Concat(a, b) => {
            bitvec::concat(out, value(a), value(b), usize::from(width(b)), w);
        }
        Operation::Shift {
            value: a,
            amount,
            left,
        } => {
            let amount = usize::try_from(amount).unwrap_or(usize::MAX).min(w);
            if left {
                bitvec::shl(out, value(a), amount, w);
            } else {
                bitvec::shr(out, value(a), amount, w);
            }
        }
        Operation::Mul { a, b, signed } => bitvec::mul(
            out,
            (value(a), usize::from(width(a))),
            (value(b), usize::from(width(b))),
            w,
            signed,
        ),
    }
}

pub(crate) fn operands(operation: &Operation) -> Vec<NodeId> {
    match *operation {
        Operation::Input(_) | Operation::Constant(_) => vec![],
        Operation::Not(a)
        | Operation::Reg(a)
        | Operation::Slice { value: a, .. }
        | Operation::Shift { value: a, .. } => {
            vec![a]
        }
        Operation::Binary(_, a, b) | Operation::Concat(a, b) | Operation::Mul { a, b, .. } => {
            vec![a, b]
        }
        Operation::Mux { condition, yes, no } => vec![condition, yes, no],
    }
}

pub(crate) fn identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Checked scalar construction API. Prefer `#[logic]` for structured pure functions.
#[derive(Default)]
pub struct Builder {
    nodes: Vec<Node>,
    inputs: Vec<Port>,
    outputs: Vec<Port>,
    /// Ids of undefined (don't-care) nodes: zero constants that folds may
    /// replace with any value. Only the builder knows; a finished [`Logic`]
    /// sees zeros.
    undefined: std::collections::BTreeSet<usize>,
    /// Each pushed `(width, operation)` (inputs excepted): an identical
    /// operation is the same node.
    interned: std::collections::HashMap<(u16, Operation), NodeId>,
}
impl Builder {
    fn push(&mut self, width: u16, operation: Operation) -> Result<NodeId, Error> {
        if !(1..=MAX_WIDTH).contains(&width) {
            return Err(Error(format!("bit widths must be in 1..={MAX_WIDTH}")));
        }
        // Constant folding: an operation on constants is a constant.
        let sources = operands(&operation);
        if !sources.is_empty() && sources.iter().all(|&id| self.constant_value(id).is_some()) {
            let mut out = vec![0; words_for(usize::from(width))];
            compute(
                &operation,
                |id| self.constant_value(id).map_or(&[], BitVec::words),
                |id| self.nodes[id.0].width,
                &mut out,
                width,
            );
            return self.push_node(width, Operation::Constant(BitVec::from_words(&out)));
        }
        self.push_node(width, operation)
    }
    /// A node for `operation`, shared with an identical earlier one.
    fn push_node(&mut self, width: u16, operation: Operation) -> Result<NodeId, Error> {
        if let Some(&id) = self.interned.get(&(width, operation.clone())) {
            return Ok(id);
        }
        let shared = !matches!(operation, Operation::Input(_));
        let id = self.push_fresh(width, operation.clone())?;
        if shared {
            self.interned.insert((width, operation), id);
        }
        Ok(id)
    }
    /// A new node, never shared.
    fn push_fresh(&mut self, width: u16, operation: Operation) -> Result<NodeId, Error> {
        if self.nodes.len() >= 65_536 {
            return fail("logic exceeds 65536 expression nodes");
        }
        let id = NodeId(self.nodes.len());
        self.nodes.push(Node { width, operation });
        Ok(id)
    }
    fn width(&self, id: NodeId) -> Result<u16, Error> {
        self.nodes
            .get(id.0)
            .map(|n| n.width)
            .ok_or_else(|| Error("invalid expression reference".into()))
    }
    pub fn input(&mut self, name: &str, width: u16) -> Result<NodeId, Error> {
        if !identifier(name) || self.inputs.iter().any(|p| p.name == name) {
            return fail("invalid or duplicate input name");
        }
        let value = self.push(width, Operation::Input(self.inputs.len()))?;
        self.inputs.push(Port {
            name: name.into(),
            width,
            value,
        });
        Ok(value)
    }
    /// A constant, cut to `width` bits.
    pub fn constant(&mut self, width: u16, value: impl Into<BitVec>) -> Result<NodeId, Error> {
        let value = value.into().masked(width);
        self.push(width, Operation::Constant(value))
    }
    /// The value of `id` if it is a constant node (undefined nodes are not).
    pub fn constant_value(&self, id: NodeId) -> Option<&BitVec> {
        if self.is_undefined(id) {
            return None;
        }
        match &self.nodes.get(id.0)?.operation {
            Operation::Constant(value) => Some(value),
            _ => None,
        }
    }
    /// `width` don't-care bits: folds may give them any value; if they
    /// survive, they are zero.
    pub fn undefined(&mut self, width: u16) -> Result<NodeId, Error> {
        if !(1..=MAX_WIDTH).contains(&width) {
            return Err(Error(format!("bit widths must be in 1..={MAX_WIDTH}")));
        }
        // Not shared: a real zero constant must stay a zero.
        let id = self.push_fresh(width, Operation::Constant(BitVec::zero()))?;
        self.undefined.insert(id.0);
        Ok(id)
    }
    /// Whether `id` is an undefined node.
    pub fn is_undefined(&self, id: NodeId) -> bool {
        self.undefined.contains(&id.0)
    }
    pub(crate) fn node_width(&self, id: NodeId) -> Result<u16, Error> {
        self.width(id)
    }
    pub fn not(&mut self, a: NodeId) -> Result<NodeId, Error> {
        self.push(self.width(a)?, Operation::Not(a))
    }
    pub fn binary(&mut self, op: Binary, a: NodeId, b: NodeId) -> Result<NodeId, Error> {
        let width = self.width(a)?;
        if width != self.width(b)? {
            return fail("operand width mismatch");
        }
        let result = if matches!(
            op,
            Binary::And | Binary::Or | Binary::Xor | Binary::Add | Binary::Sub
        ) {
            width
        } else {
            1
        };
        self.push(result, Operation::Binary(op, a, b))
    }
    /// A mux, folded: a constant condition selects an operand; undefined
    /// operands and muxes between equal operands (one node, or equal
    /// constants) are the other operand; and a mux of a concatenation is
    /// split at its low part, so constant, undefined and equal segments fold
    /// separately.
    pub fn mux(&mut self, condition: NodeId, yes: NodeId, no: NodeId) -> Result<NodeId, Error> {
        let width = self.width(yes)?;
        if self.width(condition)? != 1 || width != self.width(no)? {
            return fail("mux type mismatch");
        }
        if let Some(c) = self.constant_value(condition) {
            return Ok(if c.is_zero() { no } else { yes });
        }
        if self.is_undefined(yes) {
            return Ok(no);
        }
        if self.is_undefined(no)
            || yes == no
            || self
                .constant_value(yes)
                .is_some_and(|y| self.constant_value(no) == Some(y))
        {
            return Ok(yes);
        }
        let split = [yes, no]
            .into_iter()
            .find_map(|id| match self.nodes[id.0].operation {
                Operation::Concat(_, low) => Some(self.nodes[low.0].width),
                _ => None,
            });
        if let Some(low) = split {
            let high = width - low;
            let (yes_high, no_high) = (self.slice(yes, low, high)?, self.slice(no, low, high)?);
            let (yes_low, no_low) = (self.slice(yes, 0, low)?, self.slice(no, 0, low)?);
            let high = self.mux(condition, yes_high, no_high)?;
            let low = self.mux(condition, yes_low, no_low)?;
            return self.concat(high, low);
        }
        self.push(width, Operation::Mux { condition, yes, no })
    }
    /// Bits `offset..offset + width` of `value`, folded through slices and
    /// concatenations: a slice inside one part of a concatenation is a slice
    /// of that part.
    pub fn slice(&mut self, value: NodeId, offset: u16, width: u16) -> Result<NodeId, Error> {
        let full = self.width(value)?;
        if width == 0 || u32::from(offset) + u32::from(width) > u32::from(full) {
            return fail("slice out of bounds");
        }
        if offset == 0 && width == full {
            return Ok(value);
        }
        if self.is_undefined(value) {
            return self.undefined(width);
        }
        match self.nodes[value.0].operation {
            Operation::Slice {
                value: inner,
                offset: base,
            } => {
                return self.slice(inner, base + offset, width);
            }
            Operation::Concat(high, low) => {
                let split = self.nodes[low.0].width;
                if offset + width <= split {
                    return self.slice(low, offset, width);
                }
                if offset >= split {
                    return self.slice(high, offset - split, width);
                }
                let high = self.slice(high, 0, offset + width - split)?;
                let low = self.slice(low, offset, split - offset)?;
                return self.concat(high, low);
            }
            _ => {}
        }
        self.push(width, Operation::Slice { value, offset })
    }
    /// `{a, b}`, folded: undefined parts give undefined bits, and adjacent
    /// slices of one value are one slice.
    pub fn concat(&mut self, a: NodeId, b: NodeId) -> Result<NodeId, Error> {
        let (high, low) = (self.width(a)?, self.width(b)?);
        let width = u16::try_from(u32::from(high) + u32::from(low)).unwrap_or(u16::MAX);
        if self.is_undefined(a) && self.is_undefined(b) {
            return self.undefined(width);
        }
        if let (
            Operation::Slice {
                value: x,
                offset: ox,
            },
            Operation::Slice {
                value: y,
                offset: oy,
            },
        ) = (&self.nodes[a.0].operation, &self.nodes[b.0].operation)
            && x == y
            && *ox == oy + low
        {
            let (value, offset) = (*x, *oy);
            return self.slice(value, offset, width);
        }
        self.push(width, Operation::Concat(a, b))
    }
    pub fn shift(&mut self, value: NodeId, amount: u32, left: bool) -> Result<NodeId, Error> {
        self.push(
            self.width(value)?,
            Operation::Shift {
                value,
                amount,
                left,
            },
        )
    }
    /// `a × b` (operands of any widths), `width` bits wide: the product of
    /// the operands extended to `width` bits (sign-extended when `signed`),
    /// modulo `2^width`. A zero operand folds to zero.
    pub fn mul(&mut self, a: NodeId, b: NodeId, width: u16, signed: bool) -> Result<NodeId, Error> {
        self.width(a)?;
        self.width(b)?;
        if [a, b]
            .iter()
            .any(|&id| self.constant_value(id).is_some_and(BitVec::is_zero))
        {
            return self.constant(width, 0);
        }
        self.push(width, Operation::Mul { a, b, signed })
    }
    pub fn output(&mut self, name: &str, value: NodeId) -> Result<(), Error> {
        if !identifier(name) || self.outputs.iter().any(|p| p.name == name) {
            return fail("invalid or duplicate output name");
        }
        self.outputs.push(Port {
            name: name.into(),
            width: self.width(value)?,
            value,
        });
        Ok(())
    }
    pub fn finish(self) -> Result<Logic, Error> {
        if self.outputs.is_empty() {
            return fail("logic requires at least one output");
        }
        Ok(Logic {
            nodes: self.nodes,
            inputs: self.inputs,
            outputs: self.outputs,
            schemas: None,
            layout: std::sync::OnceLock::new(),
            typed: std::sync::OnceLock::new(),
        })
    }
}
