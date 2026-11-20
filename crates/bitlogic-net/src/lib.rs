//! A bit-level logic network for optimizing bitlogic graphs and mapping them
//! onto Xilinx 7-series LUT6 and CARRY4 primitives.
//!
//! [`Net`] has three kinds of logic node, all on single bits:
//!
//! - `And(a, b)` and `Xor(a, b)`, the general logic;
//! - `Carry { p, g, c }` = `p ? c : g` and `Sum { p, c }` = `p ^ c`, one stage
//!   of a carry chain (propagate `p`, generate `g`, carry in `c`), kept apart
//!   from general logic so that adders, subtractors and magnitude comparisons
//!   can map onto CARRY4.
//!
//! Every edge is a [`Lit`]: a node plus an inversion flag, so NOT is free.
//! Nodes are structurally hashed on creation, so identical nodes are never
//! built twice, and constants simplify as nodes are created. Node indices are
//! a topological order.
mod map;

pub use map::{Mapped, MappingStats, map};

use bitlogic::{Binary, Error, Logic, Operation};
use std::collections::HashMap;

/// A reference to a node's output, possibly inverted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Lit(u32);

impl Lit {
    pub const FALSE: Lit = Lit(0);
    pub const TRUE: Lit = Lit(1);

    fn new(node: u32, inverted: bool) -> Self {
        Lit(node << 1 | u32::from(inverted))
    }
    pub fn node(self) -> u32 {
        self.0 >> 1
    }
    pub fn inverted(self) -> bool {
        self.0 & 1 == 1
    }
    pub fn positive(self) -> Self {
        Lit(self.0 & !1)
    }
    pub fn is_const(self) -> bool {
        self.node() == 0
    }
}

impl std::ops::Not for Lit {
    type Output = Lit;
    fn not(self) -> Lit {
        Lit(self.0 ^ 1)
    }
}

impl std::ops::BitXor<bool> for Lit {
    type Output = Lit;
    fn bitxor(self, invert: bool) -> Lit {
        Lit(self.0 ^ u32::from(invert))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Node {
    /// Node 0 only: constant false.
    Const,
    /// One bit of the inputs, numbered across all ports in order.
    Input(u32),
    And(Lit, Lit),
    Xor(Lit, Lit),
    /// One carry-chain stage's carry out: `p ? c : g`.
    Carry {
        p: Lit,
        g: Lit,
        c: Lit,
    },
    /// One carry-chain stage's sum: `p ^ c`.
    Sum {
        p: Lit,
        c: Lit,
    },
}

/// A named group of bits, least significant first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Port {
    pub name: String,
    pub bits: Vec<Lit>,
}

#[derive(Clone, Debug, Default)]
pub struct Net {
    nodes: Vec<Node>,
    table: HashMap<Node, u32>,
    inputs: Vec<Port>,
    outputs: Vec<Port>,
}

/// Node counts by kind.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NetStats {
    pub inputs: usize,
    pub ands: usize,
    pub xors: usize,
    pub carries: usize,
    pub sums: usize,
    /// Longest path in logic nodes (each node counts 1).
    pub depth: usize,
}

impl Net {
    pub fn new() -> Self {
        Net {
            nodes: vec![Node::Const],
            ..Default::default()
        }
    }
    pub fn nodes(&self) -> &[Node] {
        &self.nodes
    }
    pub fn inputs(&self) -> &[Port] {
        &self.inputs
    }
    pub fn outputs(&self) -> &[Port] {
        &self.outputs
    }
    pub fn node(&self, lit: Lit) -> Node {
        self.nodes[lit.node() as usize]
    }

    fn intern(&mut self, node: Node) -> Lit {
        if let Some(&id) = self.table.get(&node) {
            return Lit::new(id, false);
        }
        let id = u32::try_from(self.nodes.len()).expect("fewer than 2^31 nodes");
        self.nodes.push(node);
        self.table.insert(node, id);
        Lit::new(id, false)
    }

    pub fn input(&mut self, name: &str, width: usize) -> Vec<Lit> {
        let first: usize = self.inputs.iter().map(|p| p.bits.len()).sum();
        let bits: Vec<Lit> = (0..width)
            .map(|k| {
                let id = u32::try_from(self.nodes.len()).expect("node count");
                self.nodes
                    .push(Node::Input(u32::try_from(first + k).expect("bits")));
                Lit::new(id, false)
            })
            .collect();
        self.inputs.push(Port {
            name: name.into(),
            bits: bits.clone(),
        });
        bits
    }

    pub fn output(&mut self, name: &str, bits: Vec<Lit>) {
        self.outputs.push(Port {
            name: name.into(),
            bits,
        });
    }

    pub fn and(&mut self, a: Lit, b: Lit) -> Lit {
        let (a, b) = if a <= b { (a, b) } else { (b, a) };
        if a == Lit::FALSE || a == !b {
            return Lit::FALSE;
        }
        if a == Lit::TRUE || a == b {
            return b;
        }
        self.intern(Node::And(a, b))
    }

    pub fn or(&mut self, a: Lit, b: Lit) -> Lit {
        !self.and(!a, !b)
    }

    pub fn xor(&mut self, a: Lit, b: Lit) -> Lit {
        // Inversions move to the output so equal functions share a node.
        let invert = a.inverted() ^ b.inverted();
        let (a, b) = (a.positive(), b.positive());
        let (a, b) = if a <= b { (a, b) } else { (b, a) };
        let lit = if a == b {
            Lit::FALSE
        } else if a == Lit::FALSE {
            b
        } else {
            self.intern(Node::Xor(a, b))
        };
        lit ^ invert
    }

    pub fn mux(&mut self, s: Lit, yes: Lit, no: Lit) -> Lit {
        if yes == no {
            return yes;
        }
        let a = self.and(s, yes);
        let b = self.and(!s, no);
        self.or(a, b)
    }

    /// Carry out of a chain stage: `p ? c : g`. A constant carry in starts a
    /// chain and stays (CARRY4 takes it as CYINIT); an inverted carry in is
    /// normalized away so chains link without inverters.
    pub fn carry(&mut self, p: Lit, g: Lit, c: Lit) -> Lit {
        if p == Lit::FALSE || g == c {
            return g;
        }
        if p == Lit::TRUE {
            return c;
        }
        if c.inverted() && !c.is_const() {
            return !self.carry(p, !g, !c);
        }
        self.intern(Node::Carry { p, g, c })
    }

    /// Sum of a chain stage: `p ^ c`, with the stage's own `p` and `c`.
    pub fn sum(&mut self, p: Lit, c: Lit) -> Lit {
        if c.is_const() || p.is_const() {
            return self.xor(p, c);
        }
        if c.inverted() {
            return !self.sum(p, !c);
        }
        self.intern(Node::Sum { p, c })
    }

    /// `a + b + cin`: the sum bits and the carry out.
    pub fn add(&mut self, a: &[Lit], b: &[Lit], cin: Lit) -> (Vec<Lit>, Lit) {
        let mut c = cin;
        let mut out = Vec::with_capacity(a.len());
        for (&x, &y) in a.iter().zip(b) {
            let p = self.xor(x, y);
            out.push(self.sum(p, c));
            // Where p is 0 the operands are equal, so either generates; the
            // smaller literal makes a + b and b + a share nodes.
            c = self.carry(p, x.min(y), c);
        }
        (out, c)
    }

    /// Carry out of `a - b`, i.e. `a >= b` (unsigned).
    fn ge(&mut self, a: &[Lit], b: &[Lit]) -> Lit {
        let nb: Vec<Lit> = b.iter().map(|&x| !x).collect();
        self.add(a, &nb, Lit::TRUE).1
    }

    /// AND of all `bits` as a balanced tree.
    pub fn and_all(&mut self, bits: &[Lit]) -> Lit {
        match bits {
            [] => Lit::TRUE,
            [x] => *x,
            _ => {
                let (l, r) = bits.split_at(bits.len() / 2);
                let (l, r) = (self.and_all(l), self.and_all(r));
                self.and(l, r)
            }
        }
    }

    /// Convert a word-level graph, one bit at a time.
    pub fn from_logic(logic: &Logic) -> Result<Net, Error> {
        let mut net = Net::new();
        let input_bits: Vec<Vec<Lit>> = logic
            .inputs()
            .iter()
            .map(|p| net.input(&p.name, usize::from(p.width)))
            .collect();
        let mut values: Vec<Vec<Lit>> = Vec::with_capacity(logic.nodes().len());
        for node in logic.nodes() {
            let w = usize::from(node.width);
            let v = |id: bitlogic::NodeId| -> &Vec<Lit> { &values[id.index()] };
            let bits = match &node.operation {
                Operation::Input(k) => input_bits[*k].clone(),
                Operation::Constant(c) => (0..w)
                    .map(|k| {
                        if c >> k & 1 == 1 {
                            Lit::TRUE
                        } else {
                            Lit::FALSE
                        }
                    })
                    .collect(),
                Operation::Not(a) => v(*a).iter().map(|&x| !x).collect(),
                Operation::Binary(op, a, b) => {
                    let (a, b) = (v(*a).clone(), v(*b).clone());
                    let pairs = || a.iter().copied().zip(b.iter().copied());
                    match op {
                        Binary::And => pairs().map(|(x, y)| net.and(x, y)).collect(),
                        Binary::Or => pairs().map(|(x, y)| net.or(x, y)).collect(),
                        Binary::Xor => pairs().map(|(x, y)| net.xor(x, y)).collect(),
                        Binary::Add => net.add(&a, &b, Lit::FALSE).0,
                        Binary::Sub => {
                            let nb: Vec<Lit> = b.iter().map(|&x| !x).collect();
                            net.add(&a, &nb, Lit::TRUE).0
                        }
                        Binary::Eq | Binary::Ne => {
                            let same: Vec<Lit> = pairs().map(|(x, y)| !net.xor(x, y)).collect();
                            let eq = net.and_all(&same);
                            vec![if *op == Binary::Eq { eq } else { !eq }]
                        }
                        Binary::Ge => vec![net.ge(&a, &b)],
                        Binary::Lt => vec![!net.ge(&a, &b)],
                        Binary::Le => vec![net.ge(&b, &a)],
                        Binary::Gt => vec![!net.ge(&b, &a)],
                    }
                }
                Operation::Mux { condition, yes, no } => {
                    let s = v(*condition)[0];
                    let (yes, no) = (v(*yes).clone(), v(*no).clone());
                    yes.iter()
                        .zip(&no)
                        .map(|(&y, &n)| net.mux(s, y, n))
                        .collect()
                }
                Operation::Slice { value, offset } => {
                    let o = usize::from(*offset);
                    v(*value)[o..o + w].to_vec()
                }
                Operation::Concat(hi, lo) => {
                    let mut bits = v(*lo).clone();
                    bits.extend(v(*hi));
                    bits
                }
                Operation::Shift {
                    value,
                    amount,
                    left,
                } => {
                    let src = v(*value);
                    let n = usize::try_from(*amount).unwrap_or(usize::MAX);
                    (0..w)
                        .map(|k| {
                            let from = if *left {
                                k.checked_sub(n)
                            } else {
                                k.checked_add(n).filter(|&j| j < w)
                            };
                            from.map_or(Lit::FALSE, |j| src[j])
                        })
                        .collect()
                }
            };
            values.push(bits);
        }
        for port in logic.outputs() {
            net.output(&port.name, values[port.value.index()].clone());
        }
        Ok(net)
    }

    /// Bit-parallel simulation: each input bit carries 64 independent
    /// vectors. Returns one word per node.
    pub fn simulate(&self, inputs: &[u64]) -> Vec<u64> {
        let mut v = vec![0u64; self.nodes.len()];
        let lit = |v: &[u64], l: Lit| v[l.node() as usize] ^ if l.inverted() { !0 } else { 0 };
        for (k, node) in self.nodes.iter().enumerate() {
            v[k] = match *node {
                Node::Const => 0,
                Node::Input(i) => inputs[i as usize],
                Node::And(a, b) => lit(&v, a) & lit(&v, b),
                Node::Xor(a, b) => lit(&v, a) ^ lit(&v, b),
                Node::Carry { p, g, c } => {
                    let p = lit(&v, p);
                    (p & lit(&v, c)) | (!p & lit(&v, g))
                }
                Node::Sum { p, c } => lit(&v, p) ^ lit(&v, c),
            };
        }
        v
    }

    /// Output bit words from a [`Net::simulate`] result.
    pub fn output_words(&self, values: &[u64]) -> Vec<Vec<u64>> {
        self.outputs
            .iter()
            .map(|p| {
                p.bits
                    .iter()
                    .map(|&l| values[l.node() as usize] ^ if l.inverted() { !0 } else { 0 })
                    .collect()
            })
            .collect()
    }

    pub fn stats(&self) -> NetStats {
        let mut s = NetStats::default();
        let mut depth = vec![0usize; self.nodes.len()];
        for (k, node) in self.nodes.iter().enumerate() {
            let d = |l: &Lit| depth[l.node() as usize];
            depth[k] = match node {
                Node::Const => 0,
                Node::Input(_) => {
                    s.inputs += 1;
                    0
                }
                Node::And(a, b) => {
                    s.ands += 1;
                    1 + d(a).max(d(b))
                }
                Node::Xor(a, b) => {
                    s.xors += 1;
                    1 + d(a).max(d(b))
                }
                Node::Carry { p, g, c } => {
                    s.carries += 1;
                    1 + d(p).max(d(g)).max(d(c))
                }
                Node::Sum { p, c } => {
                    s.sums += 1;
                    1 + d(p).max(d(c))
                }
            };
        }
        s.depth = self
            .outputs
            .iter()
            .flat_map(|p| &p.bits)
            .map(|l| depth[l.node() as usize])
            .max()
            .unwrap_or(0);
        s
    }

    /// Rebuild keeping only nodes that reach an output; also re-applies the
    /// constructors' simplifications. `lower` decides, for each chain stage,
    /// whether to turn it into plain AND/XOR logic.
    fn rebuild(&self, lower: impl Fn(u32) -> bool) -> Net {
        let mut live = vec![false; self.nodes.len()];
        for l in self.outputs.iter().flat_map(|p| &p.bits) {
            live[l.node() as usize] = true;
        }
        for k in (1..self.nodes.len()).rev() {
            if !live[k] {
                continue;
            }
            let mut mark = |l: Lit| live[l.node() as usize] = true;
            match self.nodes[k] {
                Node::And(a, b) | Node::Xor(a, b) | Node::Sum { p: a, c: b } => {
                    mark(a);
                    mark(b);
                }
                Node::Carry { p, g, c } => {
                    mark(p);
                    mark(g);
                    mark(c);
                }
                Node::Const | Node::Input(_) => {}
            }
        }
        let mut net = Net::new();
        let mut map = vec![Lit::FALSE; self.nodes.len()];
        for port in &self.inputs {
            let bits = net.input(&port.name, port.bits.len());
            for (old, new) in port.bits.iter().zip(bits) {
                map[old.node() as usize] = new;
            }
        }
        for (k, node) in self.nodes.iter().enumerate() {
            if !live[k] {
                continue;
            }
            let m = |l: Lit| map[l.node() as usize] ^ l.inverted();
            map[k] = match *node {
                Node::Const | Node::Input(_) => map[k],
                Node::And(a, b) => net.and(m(a), m(b)),
                Node::Xor(a, b) => net.xor(m(a), m(b)),
                Node::Carry { p, g, c } if lower(k as u32) => {
                    let (p, g, c) = (m(p), m(g), m(c));
                    net.mux(p, c, g)
                }
                Node::Carry { p, g, c } => net.carry(m(p), m(g), m(c)),
                Node::Sum { p, c } if lower(k as u32) => net.xor(m(p), m(c)),
                Node::Sum { p, c } => net.sum(m(p), m(c)),
            };
        }
        for port in &self.outputs {
            let bits = port
                .bits
                .iter()
                .map(|l| map[l.node() as usize] ^ l.inverted())
                .collect();
            net.output(&port.name, bits);
        }
        net
    }

    /// Drop nodes that reach no output.
    pub fn cleanup(&self) -> Net {
        self.rebuild(|_| false)
    }

    /// Length of the carry chain through each chain node (0 elsewhere): the
    /// number of stages from the chain's start to its last stage.
    #[allow(clippy::needless_range_loop)]
    pub fn chain_lengths(&self) -> Vec<usize> {
        let n = self.nodes.len();
        // Stages before and including each carry node.
        let mut up = vec![0usize; n];
        for (k, node) in self.nodes.iter().enumerate() {
            if let Node::Carry { c, .. } = node {
                let prev = c.node() as usize;
                up[k] = 1 + if matches!(self.nodes[prev], Node::Carry { .. }) {
                    up[prev]
                } else {
                    0
                };
            }
        }
        // Stages after each carry node.
        let mut down = vec![0usize; n];
        for k in (0..n).rev() {
            match self.nodes[k] {
                Node::Carry { c, .. } => {
                    let prev = c.node() as usize;
                    if matches!(self.nodes[prev], Node::Carry { .. }) {
                        down[prev] = down[prev].max(1 + down[k]);
                    }
                }
                Node::Sum { c, .. } => {
                    let prev = c.node() as usize;
                    if matches!(self.nodes[prev], Node::Carry { .. }) {
                        down[prev] = down[prev].max(1);
                    }
                }
                _ => {}
            }
        }
        let mut len = vec![0usize; n];
        for k in 0..n {
            if let Node::Carry { .. } = self.nodes[k] {
                len[k] = up[k] + down[k];
            }
        }
        // Propagate each chain's full length to all its stages.
        let mut total = len.clone();
        for k in (0..n).rev() {
            if let Node::Carry { c, .. } = self.nodes[k] {
                let prev = c.node() as usize;
                if matches!(self.nodes[prev], Node::Carry { .. }) {
                    total[prev] = total[prev].max(total[k]);
                }
            }
        }
        for k in 0..n {
            if let Node::Carry { c, .. } = self.nodes[k] {
                let prev = c.node() as usize;
                if matches!(self.nodes[prev], Node::Carry { .. }) {
                    total[k] = total[k].max(total[prev]);
                }
            }
        }
        // A sum takes the length of the carry it follows or shares a stage with.
        let mut stage: HashMap<(Lit, Lit), usize> = HashMap::new();
        for k in 0..n {
            if let Node::Carry { p, c, .. } = self.nodes[k] {
                stage.insert((p, c), total[k]);
            }
        }
        for k in 0..n {
            if let Node::Sum { p, c } = self.nodes[k] {
                let after = match self.nodes[c.node() as usize] {
                    Node::Carry { .. } => total[c.node() as usize],
                    _ => 1,
                };
                total[k] = stage.get(&(p, c)).copied().unwrap_or(after);
            }
        }
        total
    }

    /// Turn carry chains shorter than `min_stages` into plain logic.
    pub fn lower_short_chains(&self, min_stages: usize) -> Net {
        let len = self.chain_lengths();
        self.rebuild(|k| len[k as usize] < min_stages)
    }

    /// Rebuild with AND and XOR trees rebalanced to reduce depth: maximal
    /// single-fanout trees of one operator are collected and rebuilt pairing
    /// the shallowest operands first.
    pub fn balance(&self) -> Net {
        let n = self.nodes.len();
        let mut fanout = vec![0u32; n];
        for node in &self.nodes {
            let mut f = |l: Lit| fanout[l.node() as usize] += 1;
            match *node {
                Node::And(a, b) | Node::Xor(a, b) | Node::Sum { p: a, c: b } => {
                    f(a);
                    f(b);
                }
                Node::Carry { p, g, c } => {
                    f(p);
                    f(g);
                    f(c);
                }
                _ => {}
            }
        }
        for l in self.outputs.iter().flat_map(|p| &p.bits) {
            fanout[l.node() as usize] += 1;
        }
        let mut net = Net::new();
        let mut map = vec![Lit::FALSE; n];
        let mut depth: HashMap<u32, usize> = HashMap::new();
        for port in &self.inputs {
            let bits = net.input(&port.name, port.bits.len());
            for (old, new) in port.bits.iter().zip(bits) {
                map[old.node() as usize] = new;
            }
        }
        let is_xor = |node: &Node| matches!(node, Node::Xor(..));
        // Single-fanout operands of a same-operator parent are rebuilt as part
        // of the parent's tree, not on their own.
        let mut absorbed = vec![false; n];
        for node in &self.nodes {
            let (a, b, xor) = match *node {
                Node::And(a, b) => (a, b, false),
                Node::Xor(a, b) => (a, b, true),
                _ => continue,
            };
            for l in [a, b] {
                let id = l.node() as usize;
                let same = match self.nodes[id] {
                    Node::And(..) => !xor && !l.inverted(),
                    Node::Xor(..) => xor,
                    _ => false,
                };
                if same && fanout[id] == 1 {
                    absorbed[id] = true;
                }
            }
        }
        for k in 1..n {
            if absorbed[k] {
                continue;
            }
            let node = self.nodes[k];
            let m = |l: Lit| map[l.node() as usize] ^ l.inverted();
            map[k] = match node {
                Node::Input(_) => map[k],
                Node::And(..) | Node::Xor(..) => {
                    // Gather the operands of this operator's tree.
                    let xor = is_xor(&node);
                    let mut leaves = Vec::new();
                    let mut stack = vec![Lit::new(k as u32, false)];
                    while let Some(l) = stack.pop() {
                        let id = l.node() as usize;
                        let inner = &self.nodes[id];
                        let same = match inner {
                            Node::And(..) => !xor && !l.inverted(),
                            Node::Xor(..) => xor,
                            _ => false,
                        };
                        let expand = same && (id == k || fanout[id] == 1);
                        if expand {
                            let (a, b) = match *inner {
                                Node::And(a, b) | Node::Xor(a, b) => (a, b),
                                _ => unreachable!(),
                            };
                            // XOR inversions are collected separately.
                            if xor && l.inverted() {
                                leaves.push(Lit::TRUE);
                            }
                            stack.push(a);
                            stack.push(b);
                        } else {
                            leaves.push(m(l));
                        }
                    }
                    let level = |net: &Net, depth: &HashMap<u32, usize>, l: Lit| {
                        if matches!(net.node(l), Node::And(..) | Node::Xor(..)) {
                            depth.get(&l.node()).copied().unwrap_or(0)
                        } else {
                            0
                        }
                    };
                    let mut items: Vec<(usize, Lit)> = leaves
                        .iter()
                        .map(|&l| (level(&net, &depth, l), l))
                        .collect();
                    while items.len() > 1 {
                        items.sort_by_key(|&(d, l)| std::cmp::Reverse((d, l)));
                        let (da, a) = items.pop().unwrap();
                        let (db, b) = items.pop().unwrap();
                        let r = if xor { net.xor(a, b) } else { net.and(a, b) };
                        let d = if matches!(net.node(r), Node::And(..) | Node::Xor(..)) {
                            *depth.entry(r.node()).or_insert(da.max(db) + 1)
                        } else {
                            0
                        };
                        items.push((d, r));
                    }
                    items.pop().map_or(Lit::TRUE, |(_, l)| l)
                }
                Node::Carry { p, g, c } => net.carry(m(p), m(g), m(c)),
                Node::Sum { p, c } => net.sum(m(p), m(c)),
                Node::Const => Lit::FALSE,
            };
        }
        for port in &self.outputs {
            let bits = port
                .bits
                .iter()
                .map(|l| map[l.node() as usize] ^ l.inverted())
                .collect();
            net.output(&port.name, bits);
        }
        net.cleanup()
    }
}
