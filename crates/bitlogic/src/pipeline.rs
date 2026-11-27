//! Pipelining a unit's step function by rewrite rules, without changing its
//! Rust code.
//!
//! The rules are structural, with no timing model: [`Rule::RegisterMul`]
//! puts registers on a multiply's operands and product, where a DSP block
//! has its own. A unit is a loop (its next state is computed from its
//! state), so registers inside the step function are only safe if the unit
//! waits for them: with `L` layers of registers on the deepest path, it
//! holds its state for `L` cycles before taking the next state from a path
//! through them.
//!
//! It holds only when it has to. [`pipeline`] follows each path from a
//! register to the next state, collecting the conditions of the muxes it
//! passes through: the **stall condition** is when some such path is
//! selected. A unit that uses a product only in one phase holds only in
//! that phase. The condition is evaluated on the current state and inputs;
//! the unit holds while it is true, for at most `L` cycles, then takes the
//! next state.
//!
//! This is sound when, while the unit holds, nothing a register depends on
//! changes: the registers' inputs may only depend on the unit's own `state`
//! and on params ([`pipeline`] checks). The unit's other outputs (memory
//! addresses, write enables) must not depend on a register, and while the
//! unit holds they repeat the held state's values each cycle, in the cycle
//! model and the hardware alike.
use crate::ir::{Binary, Node, NodeId, Operation, Port, operands};
use crate::{BitVec, Error, Logic};
use std::collections::HashMap;
use std::fmt::Write;

/// A rewrite rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Rule {
    /// `before` registers on each operand of every multiply (constants
    /// excepted) and `after` on its product. On Xilinx 7-series, one before
    /// and one after are a DSP48E1's input (A/B) and output (P) registers;
    /// a second after is its multiplier (M) register.
    RegisterMul { before: u8, after: u8 },
}

/// The rules a `unit ..., pipeline` gets.
pub const DEFAULT_RULES: &[Rule] = &[Rule::RegisterMul {
    before: 1,
    after: 1,
}];

/// The name of the stall condition's output in [`Pipelined::logic`].
pub const STALL_PORT: &str = "bitlogic_stall";
/// The name of the hold input of [`Pipelined::logic`]: set, the next state
/// is the current one.
pub const HOLD_PORT: &str = "bitlogic_hold";

/// A step function after [`pipeline`].
#[derive(Clone, Debug)]
pub struct Pipelined {
    /// The step function with its registers. When `latency > 0`, its last
    /// output is [`STALL_PORT`], the stall condition, its last input
    /// [`HOLD_PORT`], which holds the next state at the current one, and it
    /// has no schemas (the other ports are the original's).
    pub logic: Logic,
    /// Layers of registers on the deepest path to the next state: how many
    /// cycles the unit holds.
    pub latency: u32,
    /// The stall condition alone, as a pure graph with one 1-bit output,
    /// reading only the inputs it needs (`None` when `latency == 0`).
    pub stall: Option<Logic>,
    /// The step function's input leaves that `stall`'s inputs are, in order.
    pub stall_inputs: Vec<usize>,
    /// Whether the stall condition reads only the unit's `state`, so it
    /// changes only when the state does.
    pub stall_reads_state_only: bool,
    /// What the rules did, for the build log.
    pub report: String,
}

/// A value's stall dependence: never, always, or when a 1-bit node is set.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Dep {
    Never,
    Always,
    When(NodeId),
}

struct Graph {
    nodes: Vec<Node>,
    /// The stall condition's nodes, shared when identical.
    interned: HashMap<Operation, NodeId>,
}

impl Graph {
    fn push(&mut self, width: u16, operation: Operation) -> NodeId {
        self.nodes.push(Node { width, operation });
        NodeId(self.nodes.len() - 1)
    }
    fn shared(&mut self, operation: Operation) -> NodeId {
        if let Some(&id) = self.interned.get(&operation) {
            return id;
        }
        let id = self.push(1, operation.clone());
        self.interned.insert(operation, id);
        id
    }
    fn regs(&mut self, mut id: NodeId, count: u8) -> NodeId {
        for _ in 0..count {
            let width = self.nodes[id.0].width;
            id = self.push(width, Operation::Reg(id));
        }
        id
    }
    fn or(&mut self, a: Dep, b: Dep) -> Dep {
        match (a, b) {
            (Dep::Never, x) | (x, Dep::Never) => x,
            (Dep::Always, _) | (_, Dep::Always) => Dep::Always,
            (Dep::When(x), Dep::When(y)) if x == y => Dep::When(x),
            // `x || (x && z)` is `x`.
            (Dep::When(x), Dep::When(y)) if self.conjunct(y, x) => Dep::When(x),
            (Dep::When(x), Dep::When(y)) if self.conjunct(x, y) => Dep::When(y),
            (Dep::When(x), Dep::When(y)) => {
                let (x, y) = (x.min(y), x.max(y));
                Dep::When(self.shared(Operation::Binary(Binary::Or, x, y)))
            }
        }
    }
    /// Whether `x` is `term && ...` (or `term` itself).
    fn conjunct(&self, x: NodeId, term: NodeId) -> bool {
        x == term
            || matches!(self.nodes[x.0].operation,
                Operation::Binary(Binary::And, a, b) if self.conjunct(a, term) || self.conjunct(b, term))
    }
    fn and(&mut self, a: Dep, b: Dep) -> Dep {
        match (a, b) {
            (Dep::Never, _) | (_, Dep::Never) => Dep::Never,
            (Dep::Always, x) | (x, Dep::Always) => x,
            (Dep::When(x), Dep::When(y)) if x == y => Dep::When(x),
            (Dep::When(x), Dep::When(y)) => {
                let (x, y) = (x.min(y), x.max(y));
                Dep::When(self.shared(Operation::Binary(Binary::And, x, y)))
            }
        }
    }
}

/// The first path segment of each port's schema leaf (`state`, a field).
fn roots(logic: &Logic, input: bool) -> Result<Vec<String>, Error> {
    let Some((i, o)) = logic.schemas() else {
        return Err(Error("pipeline: the step function has no schemas".into()));
    };
    let leaves = if input { i.leaves() } else { o.leaves() };
    Ok(leaves
        .into_iter()
        .map(|(path, _)| path.split('.').next().unwrap_or("").to_owned())
        .collect())
}

fn paths(logic: &Logic, input: bool) -> Vec<String> {
    let schema = logic.schemas().map(|(i, o)| if input { i } else { o });
    let ports = if input {
        logic.inputs()
    } else {
        logic.outputs()
    };
    match schema {
        Some(s) => s.leaves().into_iter().map(|(p, _)| p).collect(),
        None => ports.iter().map(|p| p.name.clone()).collect(),
    }
}

/// Apply `rules` to a unit's step function. `stable` names the input
/// fields, besides `state`, that cannot change while the unit holds (its
/// params). Output fields other than `state` must not depend on a register.
pub fn pipeline(logic: &Logic, rules: &[Rule], stable: &[&str]) -> Result<Pipelined, Error> {
    let input_roots = roots(logic, true)?;
    let output_roots = roots(logic, false)?;
    let input_paths = paths(logic, true);
    let output_paths = paths(logic, false);
    let is_stable =
        |i: usize| input_roots[i] == "state" || stable.contains(&input_roots[i].as_str());

    let mul_rule = rules
        .iter()
        .map(|r| match *r {
            Rule::RegisterMul { before, after } => (before, after),
        })
        .next();

    // Rewrite.
    let old = logic.nodes();
    let mut g = Graph {
        nodes: Vec::with_capacity(old.len()),
        interned: HashMap::new(),
    };
    let mut map = Vec::with_capacity(old.len());
    let mut muls = 0;
    for node in old {
        let operation = node.operation.remap(|id| map[id.0]);
        let id = match (operation, mul_rule) {
            (Operation::Mul { a, b, signed }, Some((before, after))) if before + after > 0 => {
                muls += 1;
                let mut operand = |id: NodeId| {
                    if matches!(g.nodes[id.0].operation, Operation::Constant(_)) {
                        id
                    } else {
                        g.regs(id, before)
                    }
                };
                let (a, b) = (operand(a), operand(b));
                let product = g.push(node.width, Operation::Mul { a, b, signed });
                g.regs(product, after)
            }
            (operation, _) => g.push(node.width, operation),
        };
        map.push(id);
    }
    let outputs: Vec<Port> = logic
        .outputs()
        .iter()
        .map(|p| Port {
            value: map[p.value.0],
            ..p.clone()
        })
        .collect();
    let inputs = logic.inputs().to_vec();

    // Register depth, and the first unstable input each node depends on.
    let n = g.nodes.len();
    let mut depth = vec![0u32; n];
    let mut unstable: Vec<Option<usize>> = vec![None; n];
    for (i, node) in g.nodes.iter().enumerate() {
        let ops = operands(&node.operation);
        depth[i] = ops.iter().map(|id| depth[id.0]).max().unwrap_or(0)
            + u32::from(matches!(node.operation, Operation::Reg(_)));
        unstable[i] = match node.operation {
            Operation::Input(index) if !is_stable(index) => Some(index),
            _ => ops.iter().find_map(|id| unstable[id.0]),
        };
        if let Operation::Reg(a) = node.operation
            && let Some(index) = unstable[a.0]
        {
            return Err(Error(format!(
                "pipeline: a register depends on `{}`, which can change while the unit \
                 holds (only `state` and params can feed one)",
                input_paths[index]
            )));
        }
    }
    let regs = g
        .nodes
        .iter()
        .filter(|n| matches!(n.operation, Operation::Reg(_)))
        .count();
    let mut latency = 0;
    for (k, port) in outputs.iter().enumerate() {
        let d = depth[port.value.0];
        if output_roots[k] == "state" {
            latency = latency.max(d);
        } else if d > 0 {
            return Err(Error(format!(
                "pipeline: output `{}` depends on a register; only the next state may",
                output_paths[k]
            )));
        }
    }
    if latency == 0 {
        return Ok(Pipelined {
            logic: logic.clone(),
            latency: 0,
            stall: None,
            stall_inputs: Vec::new(),
            stall_reads_state_only: true,
            report: format!("{muls} multiplies, no registers added"),
        });
    }

    // The stall condition: when the next state depends on a register.
    let mut dep = vec![Dep::Never; n];
    for i in 0..n {
        let operation = g.nodes[i].operation.clone();
        dep[i] = match operation {
            Operation::Input(_) | Operation::Constant(_) => Dep::Never,
            Operation::Reg(_) => Dep::Always,
            Operation::Mux { condition, yes, no } => {
                let on_yes = g.and(Dep::When(condition), dep[yes.0]);
                let on_no = if dep[no.0] == Dep::Never {
                    Dep::Never
                } else {
                    let not = g.shared(Operation::Not(condition));
                    g.and(Dep::When(not), dep[no.0])
                };
                let either = g.or(on_yes, on_no);
                // A condition that itself passes a register: always.
                g.or(dep[condition.0], either)
            }
            ref other => {
                let mut d = Dep::Never;
                for id in operands(other) {
                    d = g.or(d, dep[id.0]);
                }
                d
            }
        };
    }
    let mut condition = Dep::Never;
    for (k, port) in outputs.iter().enumerate() {
        if output_roots[k] == "state" {
            condition = g.or(condition, dep[port.value.0]);
        }
    }
    let stall_node = match condition {
        Dep::When(id) => id,
        Dep::Always => g.push(1, Operation::Constant(BitVec::from(1u64))),
        Dep::Never => g.push(1, Operation::Constant(BitVec::zero())),
    };
    let (stall, stall_inputs) = prune(&g.nodes, &inputs, stall_node)?;
    let stall_paths: Vec<String> = stall_inputs
        .iter()
        .map(|&k| input_paths[k].clone())
        .collect();
    let report = format!(
        "{muls} multiplies, {regs} registers, latency {latency}; holds up to {latency} \
         cycles while {}",
        describe(&stall, &stall_paths)
    );
    // The hold, folded into each state leaf's own keep-the-value mux
    // (`c ? new : old` becomes `(c && !hold) ? new : old`), so synthesis
    // gives each leaf one clock enable, as without the hold, instead of a
    // shared enable for the hold and the leaf's mux in a LUT.
    let hold = g.push(1, Operation::Input(inputs.len()));
    let not_hold = g.push(1, Operation::Not(hold));
    let mut inputs = inputs;
    inputs.push(Port {
        name: HOLD_PORT.into(),
        width: 1,
        value: hold,
    });
    let mut ports = outputs;
    for (k, port) in ports.iter_mut().enumerate() {
        if output_roots[k] != "state" {
            continue;
        }
        let Some(old) = inputs.iter().find(|p| p.name == port.name).map(|p| p.value) else {
            return Err(Error(format!(
                "pipeline: no state input for output `{}`",
                output_paths[k]
            )));
        };
        let next = port.value;
        let width = g.nodes[next.0].width;
        port.value = match g.nodes[next.0].operation {
            _ if next == old => next,
            Operation::Mux { condition, yes, no } if no == old => {
                let c = g.push(1, Operation::Binary(Binary::And, condition, not_hold));
                g.push(
                    width,
                    Operation::Mux {
                        condition: c,
                        yes,
                        no,
                    },
                )
            }
            Operation::Mux { condition, yes, no } if yes == old => {
                let c = g.push(1, Operation::Binary(Binary::Or, condition, hold));
                g.push(
                    width,
                    Operation::Mux {
                        condition: c,
                        yes,
                        no,
                    },
                )
            }
            _ => g.push(
                width,
                Operation::Mux {
                    condition: hold,
                    yes: old,
                    no: next,
                },
            ),
        };
    }
    ports.push(Port {
        name: STALL_PORT.into(),
        width: 1,
        value: stall_node,
    });
    Ok(Pipelined {
        logic: Logic::from_parts(g.nodes, inputs, ports, None)?,
        latency,
        stall: Some(stall),
        stall_reads_state_only: stall_inputs.iter().all(|&k| input_roots[k] == "state"),
        stall_inputs,
        report,
    })
}

/// The pure graph of `output` alone, with only the inputs it reads, and
/// the indices of those inputs among `inputs`.
fn prune(nodes: &[Node], inputs: &[Port], output: NodeId) -> Result<(Logic, Vec<usize>), Error> {
    let mut live = vec![false; nodes.len()];
    live[output.0] = true;
    for i in (0..nodes.len()).rev() {
        if live[i] {
            for id in operands(&nodes[i].operation) {
                live[id.0] = true;
            }
        }
    }
    let used: Vec<usize> = (0..inputs.len())
        .filter(|&k| live[inputs[k].value.0])
        .collect();
    let mut map = vec![NodeId(0); nodes.len()];
    let mut kept = Vec::new();
    for (i, node) in nodes.iter().enumerate() {
        if live[i] {
            map[i] = NodeId(kept.len());
            let operation = match node.operation {
                Operation::Input(k) => {
                    Operation::Input(used.iter().position(|&u| u == k).expect("a live input"))
                }
                ref other => other.remap(|id| map[id.0]),
            };
            kept.push(Node {
                width: node.width,
                operation,
            });
        }
    }
    let ports = used
        .iter()
        .map(|&k| Port {
            value: map[inputs[k].value.0],
            ..inputs[k].clone()
        })
        .collect();
    let outputs = vec![Port {
        name: STALL_PORT.into(),
        width: 1,
        value: map[output.0],
    }];
    Ok((Logic::from_parts(kept, ports, outputs, None)?, used))
}

/// The stall condition as text, in input field paths.
fn describe(stall: &Logic, input_paths: &[String]) -> String {
    fn expr(logic: &Logic, paths: &[String], id: NodeId, budget: &mut u32, out: &mut String) {
        if *budget == 0 {
            out.push('…');
            return;
        }
        *budget -= 1;
        let node = &logic.nodes()[id.0];
        match node.operation {
            Operation::Input(i) => out.push_str(&paths[i]),
            Operation::Constant(ref v) => write!(out, "{v}").unwrap(),
            Operation::Not(a) => {
                out.push('!');
                expr(logic, paths, a, budget, out);
            }
            Operation::Binary(op, a, b) => {
                let sym = match op {
                    Binary::And => "&&",
                    Binary::Or => "||",
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
                out.push('(');
                expr(logic, paths, a, budget, out);
                write!(out, " {sym} ").unwrap();
                expr(logic, paths, b, budget, out);
                out.push(')');
            }
            Operation::Slice { value, offset } => {
                expr(logic, paths, value, budget, out);
                write!(out, "[{}:{offset}]", offset + node.width - 1).unwrap();
            }
            _ => out.push('…'),
        }
    }
    let mut out = String::new();
    let mut budget = 24;
    expr(
        stall,
        input_paths,
        stall.outputs()[0].value,
        &mut budget,
        &mut out,
    );
    out
}
