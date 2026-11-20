//! Mapping a [`Net`] onto 7-series primitives: LUT1..LUT6 for AND/XOR logic
//! and CARRY4 for carry chains.
//!
//! Logic is covered with k-feasible cuts (k = 6) chosen in three passes, as
//! in priority-cut mappers: depth-optimal, then area flow, then exact area,
//! the last two without increasing depth. Carry and sum nodes are cut
//! boundaries; each chain stage's propagate input must come from a LUT (the
//! slice's S pin), as on the device.
use crate::{Lit, Net, Node};
use std::collections::HashMap;
use std::fmt::Write;

const K: usize = 6;
/// Cuts kept per node.
const CUTS: usize = 12;
/// Delay units: a LUT level, and one carry-chain stage (a CARRY4 is about
/// one LUT level).
const LUT_DELAY: u32 = 4;
const STAGE_DELAY: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Cut {
    leaves: [u32; K],
    len: u8,
}

impl Cut {
    fn leaves(&self) -> &[u32] {
        &self.leaves[..self.len as usize]
    }
    fn unit(node: u32) -> Self {
        let mut leaves = [0; K];
        leaves[0] = node;
        Cut { leaves, len: 1 }
    }
    const EMPTY: Cut = Cut {
        leaves: [0; K],
        len: 0,
    };
    fn merge(a: &Cut, b: &Cut) -> Option<Cut> {
        let (x, y) = (a.leaves(), b.leaves());
        let mut out = [0u32; K];
        let (mut i, mut j, mut n) = (0, 0, 0);
        while i < x.len() || j < y.len() {
            let v = match (x.get(i), y.get(j)) {
                (Some(&p), Some(&q)) if p == q => {
                    i += 1;
                    j += 1;
                    p
                }
                (Some(&p), Some(&q)) if p < q => {
                    i += 1;
                    p
                }
                (Some(&p), None) => {
                    i += 1;
                    p
                }
                (_, Some(&q)) => {
                    j += 1;
                    q
                }
                (None, None) => unreachable!(),
            };
            if n == K {
                return None;
            }
            out[n] = v;
            n += 1;
        }
        Some(Cut {
            leaves: out,
            len: n as u8,
        })
    }
}

/// Where a value comes from in the mapped netlist.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Src {
    Const(bool),
    Sig(u32),
}

#[derive(Clone, Debug)]
enum Cell {
    Lut {
        out: u32,
        ins: Vec<Src>,
        init: u64,
    },
    /// A chain stage's carry out: `s ? ci : di`.
    Co {
        out: u32,
        s: Src,
        di: Src,
        ci: Src,
    },
    /// A chain stage's sum: `s ^ ci`.
    O {
        out: u32,
        s: Src,
        ci: Src,
    },
}

/// One carry-chain stage, for emission.
#[derive(Clone, Debug)]
struct Stage {
    co: Option<u32>,
    o: Option<u32>,
    s: Src,
    di: Src,
}

/// One CARRY4 chain: its stages (indices into the stage list) in order, and
/// the chain's carry in.
#[derive(Clone, Debug)]
struct Chain {
    stages: Vec<usize>,
    cin: Src,
}

/// A netlist of LUTs and carry-chain stages.
#[derive(Clone, Debug)]
pub struct Mapped {
    inputs: Vec<(String, Vec<u32>)>,
    outputs: Vec<(String, Vec<Src>)>,
    signals: u32,
    /// Cells in evaluation order.
    cells: Vec<Cell>,
    stages: Vec<Stage>,
    chains: Vec<Chain>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MappingStats {
    /// LUTs by input count, 1..=6 at index 1..=6.
    pub luts_by_size: [usize; 7],
    pub luts: usize,
    /// LUT1 inverters for non-LUT signals (carry outputs, inputs).
    pub route_luts: usize,
    pub carry4: usize,
    /// Longest path in LUT levels (carry chains count zero).
    pub depth: usize,
}

const VARS: [u64; 6] = [
    0xAAAA_AAAA_AAAA_AAAA,
    0xCCCC_CCCC_CCCC_CCCC,
    0xF0F0_F0F0_F0F0_F0F0,
    0xFF00_FF00_FF00_FF00,
    0xFFFF_0000_FFFF_0000,
    0xFFFF_FFFF_0000_0000,
];

fn mask(k: usize) -> u64 {
    if k == 6 { !0 } else { (1u64 << (1 << k)) - 1 }
}

struct Mapper<'a> {
    net: &'a Net,
    logic: Vec<bool>,
    cuts: Vec<Vec<Cut>>,
    best: Vec<Cut>,
    arrival: Vec<u32>,
    required: Vec<u32>,
    flow: Vec<f32>,
    refs: Vec<u32>,
    /// Hard uses: outputs, chain pins.
    roots: Vec<Lit>,
}

impl<'a> Mapper<'a> {
    fn new(net: &'a Net) -> Self {
        let n = net.nodes().len();
        let logic = net
            .nodes()
            .iter()
            .map(|x| matches!(x, Node::And(..) | Node::Xor(..)))
            .collect();
        let mut roots: Vec<Lit> = net.outputs().iter().flat_map(|p| p.bits.clone()).collect();
        for node in net.nodes() {
            match *node {
                Node::Carry { p, g, c } => roots.extend([p, g, c]),
                Node::Sum { p, c } => roots.extend([p, c]),
                _ => {}
            }
        }
        Mapper {
            net,
            logic,
            cuts: vec![Vec::new(); n],
            best: vec![Cut::EMPTY; n],
            arrival: vec![0; n],
            required: vec![u32::MAX; n],
            flow: vec![0.0; n],
            refs: vec![0; n],
            roots,
        }
    }

    fn children(&self, k: usize) -> (Lit, Lit) {
        match self.net.nodes()[k] {
            Node::And(a, b) | Node::Xor(a, b) => (a, b),
            _ => unreachable!("only logic nodes have cuts"),
        }
    }

    /// Arrival of a node's value as a signal.
    fn arrival_of(&self, node: u32) -> u32 {
        self.arrival[node as usize]
    }

    fn cut_arrival(&self, cut: &Cut) -> u32 {
        LUT_DELAY
            + cut
                .leaves()
                .iter()
                .map(|&l| self.arrival_of(l))
                .max()
                .unwrap_or(0)
    }

    fn cut_flow(&self, cut: &Cut, fanout: &[f32]) -> f32 {
        1.0 + cut
            .leaves()
            .iter()
            .map(|&l| {
                if self.logic[l as usize] {
                    self.flow[l as usize] / fanout[l as usize].max(1.0)
                } else {
                    0.0
                }
            })
            .sum::<f32>()
    }

    /// The cuts a parent may combine from child `l`: its stored cuts plus the
    /// cut that stops at it.
    fn child_cuts(&self, l: Lit) -> Vec<Cut> {
        let id = l.node();
        if l.is_const() {
            return vec![Cut::EMPTY];
        }
        let mut cuts = vec![Cut::unit(id)];
        if self.logic[id as usize] {
            cuts.extend(self.cuts[id as usize].iter().copied());
        }
        cuts
    }

    /// Recompute every logic node's cuts, best cut and arrival. With
    /// `area`, prefer area flow among cuts meeting the required time.
    fn enumerate(&mut self, area: bool, fanout: &[f32]) {
        let n = self.net.nodes().len();
        for k in 0..n {
            match self.net.nodes()[k] {
                Node::And(..) | Node::Xor(..) => {}
                Node::Carry { p, g, c } => {
                    let pin = |s: &Self, l: Lit, lut: bool| {
                        let a = s.arrival_of(l.node());
                        if lut && !s.logic[l.node() as usize] && !l.is_const() {
                            a + LUT_DELAY
                        } else {
                            a
                        }
                    };
                    self.arrival[k] = STAGE_DELAY
                        + pin(self, p, true)
                            .max(pin(self, g, false))
                            .max(pin(self, c, false));
                    continue;
                }
                Node::Sum { p, c } => {
                    let a = self.arrival_of(p.node());
                    let pa = if self.logic[p.node() as usize] || p.is_const() {
                        a
                    } else {
                        a + LUT_DELAY
                    };
                    self.arrival[k] = STAGE_DELAY + pa.max(self.arrival_of(c.node()));
                    continue;
                }
                _ => continue,
            }
            let (a, b) = self.children(k);
            let (ca, cb) = (self.child_cuts(a), self.child_cuts(b));
            let mut cands: Vec<Cut> = Vec::new();
            for x in &ca {
                for y in &cb {
                    if let Some(c) = Cut::merge(x, y)
                        && !cands.contains(&c)
                    {
                        cands.push(c);
                    }
                }
            }
            let req = self.required[k];
            let mut scored: Vec<(u32, f32, u8, Cut)> = cands
                .into_iter()
                .filter(|c| c.len > 0)
                .map(|c| (self.cut_arrival(&c), self.cut_flow(&c, fanout), c.len, c))
                .collect();
            if area {
                scored.sort_by(|x, y| {
                    let ok = |s: &(u32, f32, u8, Cut)| s.0 > req;
                    (ok(x), x.1, x.0, x.2)
                        .partial_cmp(&(ok(y), y.1, y.0, y.2))
                        .unwrap()
                });
            } else {
                scored.sort_by(|x, y| (x.0, x.1, x.2).partial_cmp(&(y.0, y.1, y.2)).unwrap());
            }
            scored.truncate(CUTS);
            if scored.is_empty() {
                // Both children constant cannot happen after simplification;
                // fall back to the children themselves.
                let c = Cut::merge(&Cut::unit(a.node()), &Cut::unit(b.node())).unwrap();
                scored.push((self.cut_arrival(&c), 1.0, c.len, c));
            }
            self.best[k] = scored[0].3;
            self.arrival[k] = scored[0].0;
            self.flow[k] = scored[0].1;
            self.cuts[k] = scored.into_iter().map(|s| s.3).collect();
        }
    }

    fn root_nodes(&self) -> Vec<u32> {
        self.roots
            .iter()
            .map(|l| l.node())
            .filter(|&n| self.logic[n as usize])
            .collect()
    }

    /// Reference counts of the current cover, from the roots.
    fn count_refs(&mut self) {
        self.refs.iter_mut().for_each(|r| *r = 0);
        for n in self.root_nodes() {
            self.refs[n as usize] += 1;
        }
        for k in (0..self.refs.len()).rev() {
            if self.logic[k] && self.refs[k] > 0 {
                for &l in self.best[k].leaves() {
                    if self.logic[l as usize] {
                        self.refs[l as usize] += 1;
                    }
                }
            }
        }
    }

    /// Required times of the current cover, all roots at the latest arrival.
    fn compute_required(&mut self) {
        let target = self
            .roots
            .iter()
            .map(|l| self.arrival_of(l.node()))
            .max()
            .unwrap_or(0);
        self.required.iter_mut().for_each(|r| *r = u32::MAX);
        for n in self.root_nodes() {
            self.required[n as usize] = target;
        }
        // Chain pins: a LUT feeding a chain must arrive by the chain's own
        // requirement; approximate with the global target.
        for k in (0..self.required.len()).rev() {
            if self.logic[k] && self.refs[k] > 0 && self.required[k] != u32::MAX {
                let r = self.required[k].saturating_sub(LUT_DELAY);
                for &l in self.best[k].leaves() {
                    let l = l as usize;
                    self.required[l] = self.required[l].min(r);
                }
            }
        }
    }

    fn fanout_estimate(&self) -> Vec<f32> {
        self.refs.iter().map(|&r| r.max(1) as f32).collect()
    }

    /// LUTs added by referencing node `k`'s current cut (and recursively
    /// newly referenced leaves).
    fn reference(&mut self, k: usize) -> u32 {
        let mut area = 1;
        let leaves: Vec<u32> = self.best[k].leaves().to_vec();
        for l in leaves {
            let l = l as usize;
            if self.logic[l] {
                self.refs[l] += 1;
                if self.refs[l] == 1 {
                    area += self.reference(l);
                }
            }
        }
        area
    }

    fn dereference(&mut self, k: usize) -> u32 {
        let mut area = 1;
        let leaves: Vec<u32> = self.best[k].leaves().to_vec();
        for l in leaves {
            let l = l as usize;
            if self.logic[l] {
                self.refs[l] -= 1;
                if self.refs[l] == 0 {
                    area += self.dereference(l);
                }
            }
        }
        area
    }

    /// Exact-area recovery: for each used node, pick the cut that adds the
    /// fewest LUTs given everything else, without breaking required times.
    fn exact_area(&mut self) {
        for k in 0..self.refs.len() {
            if !self.logic[k] || self.refs[k] == 0 {
                continue;
            }
            self.dereference(k);
            let req = self.required[k];
            let mut choice = (u32::MAX, u32::MAX, self.best[k]);
            for c in self.cuts[k].clone() {
                let arr = self.cut_arrival(&c);
                if arr > req {
                    continue;
                }
                self.best[k] = c;
                let area = self.reference(k);
                self.dereference(k);
                if (area, arr) < (choice.0, choice.1) {
                    choice = (area, arr, c);
                }
            }
            self.best[k] = choice.2;
            self.arrival[k] = self.cut_arrival(&choice.2);
            self.reference(k);
        }
    }

    /// Truth table of node `k` over `leaves`.
    fn truth(&self, k: u32, leaves: &[u32]) -> u64 {
        let mut memo: HashMap<u32, u64> = HashMap::new();
        for (j, &l) in leaves.iter().enumerate() {
            memo.insert(l, VARS[j]);
        }
        fn eval(net: &Net, memo: &mut HashMap<u32, u64>, k: u32) -> u64 {
            if k == 0 {
                return 0;
            }
            if let Some(&v) = memo.get(&k) {
                return v;
            }
            let lit = |memo: &mut HashMap<u32, u64>, l: Lit| {
                eval(net, memo, l.node()) ^ if l.inverted() { !0 } else { 0 }
            };
            let v = match net.nodes()[k as usize] {
                Node::And(a, b) => lit(memo, a) & lit(memo, b),
                Node::Xor(a, b) => lit(memo, a) ^ lit(memo, b),
                other => unreachable!("cut cone reaches {other:?}"),
            };
            memo.insert(k, v);
            v
        }
        eval(self.net, &mut memo, k)
    }
}

/// Drop LUT inputs the function does not depend on.
fn shrink(init: u64, ins: Vec<Src>) -> (u64, Vec<Src>) {
    let mut tt = init;
    let mut ins = ins;
    let mut j = 0;
    while j < ins.len() {
        let k = ins.len();
        let var = VARS[j] & mask(k);
        let hi = tt & var;
        let lo = tt & !var & mask(k);
        let shift = 1 << j;
        if hi >> shift == lo {
            // Independent of input j: remove it by compacting the table.
            let mut out = 0u64;
            let mut bit = 0;
            for idx in 0..(1usize << k) {
                if idx & shift == 0 {
                    out |= (tt >> idx & 1) << bit;
                    bit += 1;
                }
            }
            tt = out;
            ins.remove(j);
        } else {
            j += 1;
        }
    }
    (tt, ins)
}

/// Map `net` onto LUTs and carry-chain stages.
pub fn map(net: &Net) -> Mapped {
    let mut m = Mapper::new(net);
    let unit = vec![1.0f32; net.nodes().len()];
    m.enumerate(false, &unit);
    m.count_refs();
    m.compute_required();
    for _ in 0..2 {
        let fanout = m.fanout_estimate();
        m.enumerate(true, &fanout);
        m.count_refs();
        m.compute_required();
    }
    m.exact_area();
    m.count_refs();
    build(&m)
}

/// A stage being assembled: carry out, sum out, p, g, c, latest node.
type StageDraft = (Option<u32>, Option<u32>, Lit, Option<Lit>, Lit, u32);

#[allow(clippy::needless_range_loop)]
fn build(m: &Mapper) -> Mapped {
    let net = m.net;
    let n = net.nodes().len();
    let mut signals = 0u32;
    let mut fresh = || {
        signals += 1;
        signals - 1
    };
    // Positive value of each node as a signal.
    let mut pos: Vec<Option<Src>> = vec![None; n];
    pos[0] = Some(Src::Const(false));
    let mut inputs = Vec::new();
    for port in net.inputs() {
        let sigs: Vec<u32> = port
            .bits
            .iter()
            .map(|l| {
                let s = fresh();
                pos[l.node() as usize] = Some(Src::Sig(s));
                s
            })
            .collect();
        inputs.push((port.name.clone(), sigs));
    }
    // Which polarities hard pins need.
    let mut need_neg = vec![false; n];
    let mut need_pos_hard = vec![false; n];
    for l in &m.roots {
        if l.inverted() && !l.is_const() {
            need_neg[l.node() as usize] = true;
        } else {
            need_pos_hard[l.node() as usize] = true;
        }
    }
    // Positive uses of each logic node as a LUT leaf.
    let mut leaf_use = vec![false; n];
    for k in 0..n {
        if m.logic[k] && m.refs[k] > 0 {
            for &l in m.best[k].leaves() {
                leaf_use[l as usize] = true;
            }
        }
    }

    let mut cells: Vec<(u32, u8, Cell)> = Vec::new();
    let mut neg: Vec<Option<Src>> = vec![None; n];
    let mut stage_of: HashMap<(Lit, Lit), Vec<usize>> = HashMap::new();
    let mut stages: Vec<StageDraft> = Vec::new();

    // One stage per carry node; sums join a stage with the same p and c.
    for k in 0..n {
        if let Node::Carry { p, g, c } = net.nodes()[k] {
            let co = fresh();
            stages.push((Some(co), None, p, Some(g), c, k as u32));
            stage_of.entry((p, c)).or_default().push(stages.len() - 1);
            pos[k] = Some(Src::Sig(co));
        }
    }
    for k in 0..n {
        match net.nodes()[k] {
            Node::And(..) | Node::Xor(..) if m.refs[k] > 0 => {
                let leaves = m.best[k].leaves();
                let tt = m.truth(k as u32, leaves) & mask(leaves.len());
                let ins: Vec<Src> = leaves
                    .iter()
                    .map(|&l| pos[l as usize].expect("leaf before root"))
                    .collect();
                let positive = leaf_use[k] || need_pos_hard[k];
                if positive {
                    let (init, ins) = shrink(tt, ins.clone());
                    let out = fresh();
                    cells.push((k as u32, 0, Cell::Lut { out, ins, init }));
                    pos[k] = Some(Src::Sig(out));
                }
                if need_neg[k] {
                    let (init, ins) = shrink(!tt & mask(leaves.len()), ins);
                    let out = fresh();
                    cells.push((k as u32, 1, Cell::Lut { out, ins, init }));
                    neg[k] = Some(Src::Sig(out));
                }
            }
            Node::Carry { .. } => {}
            Node::Sum { p, c } => {
                let free = stage_of
                    .get(&(p, c))
                    .and_then(|ids| ids.iter().copied().find(|&i| stages[i].1.is_none()));
                let id = free.unwrap_or_else(|| {
                    stages.push((None, None, p, None, c, k as u32));
                    stages.len() - 1
                });
                let o = fresh();
                stages[id].1 = Some(o);
                stages[id].5 = stages[id].5.max(k as u32);
                pos[k] = Some(Src::Sig(o));
            }
            _ => {}
        }
        // Buffers and inverters for non-LUT signals at hard pins.
        if !m.logic[k] && k != 0 && need_neg[k] {
            let out = fresh();
            let src = pos[k].expect("signal exists");
            cells.push((
                k as u32,
                1,
                Cell::Lut {
                    out,
                    ins: vec![src],
                    init: 0b01,
                },
            ));
            neg[k] = Some(Src::Sig(out));
        }
    }
    let resolve = |l: Lit| -> Src {
        if l.is_const() {
            return Src::Const(l.inverted());
        }
        let k = l.node() as usize;
        if l.inverted() { neg[k] } else { pos[k] }.expect("literal resolved")
    };
    // S pins take any signal: Vivado routes a non-LUT signal through the
    // slice's LUT site without using a LUT.
    let s_of = resolve;

    // Chains: link stages through carry outputs.
    let carry_sig: HashMap<u32, usize> = stages
        .iter()
        .enumerate()
        .filter_map(|(i, s)| s.0.map(|co| (co, i)))
        .collect();
    let mut next: Vec<Vec<usize>> = vec![Vec::new(); stages.len()];
    for (i, s) in stages.iter().enumerate() {
        if let Src::Sig(sig) = resolve(s.4)
            && let Some(&prev) = carry_sig.get(&sig)
        {
            next[prev].push(i);
        }
    }
    let mut order: Vec<usize> = (0..stages.len()).collect();
    order.sort_by_key(|&i| stages[i].5);
    let mut taken = vec![false; stages.len()];
    let mut chains = Vec::new();
    for &start in &order {
        if taken[start] {
            continue;
        }
        let mut chain = vec![start];
        taken[start] = true;
        let mut cur = start;
        while let Some(&nx) = next[cur].iter().find(|&&j| !taken[j]) {
            taken[nx] = true;
            chain.push(nx);
            cur = nx;
        }
        chains.push(Chain {
            cin: resolve(stages[start].4),
            stages: chain,
        });
    }
    let mut out_stages = Vec::with_capacity(stages.len());
    for st in &stages {
        let (sp, di, ci) = (
            s_of(st.2),
            st.3.map_or(Src::Const(false), resolve),
            resolve(st.4),
        );
        if let Some(co) = st.0 {
            cells.push((
                st.5,
                3,
                Cell::Co {
                    out: co,
                    s: sp,
                    di,
                    ci,
                },
            ));
        }
        if let Some(o) = st.1 {
            cells.push((st.5, 3, Cell::O { out: o, s: sp, ci }));
        }
        out_stages.push(Stage {
            co: st.0,
            o: st.1,
            s: sp,
            di,
        });
    }
    let outputs = net
        .outputs()
        .iter()
        .map(|p| (p.name.clone(), p.bits.iter().map(|&l| resolve(l)).collect()))
        .collect();
    Mapped {
        inputs,
        outputs,
        signals,
        cells: topological(cells.into_iter().map(|c| c.2).collect(), signals),
        stages: out_stages,
        chains,
    }
}

fn cell_io(cell: &Cell) -> (u32, Vec<Src>) {
    match cell {
        Cell::Lut { out, ins, .. } => (*out, ins.clone()),
        Cell::Co { out, s, di, ci } => (*out, vec![*s, *di, *ci]),
        Cell::O { out, s, ci } => (*out, vec![*s, *ci]),
    }
}

/// Order cells so every cell follows the cells driving its inputs.
fn topological(cells: Vec<Cell>, signals: u32) -> Vec<Cell> {
    let mut driver = vec![usize::MAX; signals as usize];
    for (i, c) in cells.iter().enumerate() {
        driver[cell_io(c).0 as usize] = i;
    }
    let mut pending = vec![0usize; cells.len()];
    let mut users: Vec<Vec<usize>> = vec![Vec::new(); cells.len()];
    for (i, c) in cells.iter().enumerate() {
        for s in cell_io(c).1 {
            if let Src::Sig(x) = s
                && driver[x as usize] != usize::MAX
            {
                pending[i] += 1;
                users[driver[x as usize]].push(i);
            }
        }
    }
    let mut ready: Vec<usize> = (0..cells.len())
        .filter(|&i| pending[i] == 0)
        .rev()
        .collect();
    let mut order = Vec::with_capacity(cells.len());
    while let Some(i) = ready.pop() {
        order.push(i);
        for &u in &users[i] {
            pending[u] -= 1;
            if pending[u] == 0 {
                ready.push(u);
            }
        }
    }
    assert_eq!(order.len(), cells.len(), "mapped netlist has a cycle");
    let mut slots: Vec<Option<Cell>> = cells.into_iter().map(Some).collect();
    order
        .into_iter()
        .map(|i| slots[i].take().unwrap())
        .collect()
}

impl Mapped {
    /// Bit-parallel evaluation: one word per input bit, 64 vectors each.
    /// Returns, per output port, one word per bit.
    pub fn simulate(&self, inputs: &[u64]) -> Vec<Vec<u64>> {
        let mut v = vec![0u64; self.signals as usize];
        let mut bit = 0;
        for (_, sigs) in &self.inputs {
            for &s in sigs {
                v[s as usize] = inputs[bit];
                bit += 1;
            }
        }
        let get = |v: &[u64], s: Src| match s {
            Src::Const(b) => {
                if b {
                    !0
                } else {
                    0
                }
            }
            Src::Sig(i) => v[i as usize],
        };
        for cell in &self.cells {
            match cell {
                Cell::Lut { out, ins, init } => {
                    let words: Vec<u64> = ins.iter().map(|&s| get(&v, s)).collect();
                    let mut r = 0u64;
                    for lane in 0..64 {
                        let idx = words
                            .iter()
                            .enumerate()
                            .fold(0usize, |a, (j, w)| a | ((w >> lane & 1) as usize) << j);
                        r |= (init >> idx & 1) << lane;
                    }
                    v[*out as usize] = r;
                }
                Cell::Co { out, s, di, ci } => {
                    let (s, di, ci) = (get(&v, *s), get(&v, *di), get(&v, *ci));
                    v[*out as usize] = (s & ci) | (!s & di);
                }
                Cell::O { out, s, ci } => {
                    v[*out as usize] = get(&v, *s) ^ get(&v, *ci);
                }
            }
        }
        self.outputs
            .iter()
            .map(|(_, bits)| bits.iter().map(|&s| get(&v, s)).collect())
            .collect()
    }

    pub fn stats(&self) -> MappingStats {
        let mut st = MappingStats::default();
        let mut level = vec![0usize; self.signals as usize];
        let lv = |level: &[usize], s: &Src| match s {
            Src::Const(_) => 0,
            Src::Sig(i) => level[*i as usize],
        };
        for cell in &self.cells {
            match cell {
                Cell::Lut { out, ins, init } => {
                    st.luts += 1;
                    st.luts_by_size[ins.len().max(1)] += 1;
                    if ins.len() == 1 && (*init == 0b01 || *init == 0b10) {
                        st.route_luts += 1;
                    }
                    level[*out as usize] = 1 + ins.iter().map(|s| lv(&level, s)).max().unwrap_or(0);
                }
                Cell::Co { out, s, di, ci } => {
                    level[*out as usize] = lv(&level, s).max(lv(&level, di)).max(lv(&level, ci));
                }
                Cell::O { out, s, ci } => {
                    level[*out as usize] = lv(&level, s).max(lv(&level, ci));
                }
            }
        }
        st.carry4 = self.chains.iter().map(|c| c.stages.len().div_ceil(4)).sum();
        st.depth = self
            .outputs
            .iter()
            .flat_map(|(_, b)| b)
            .map(|s| lv(&level, s))
            .max()
            .unwrap_or(0);
        st
    }

    /// Structural Verilog of LUT1..LUT6 and CARRY4 instances with the same
    /// ports as [`bitlogic::Logic::to_verilog`]: `i_<input>`, `o_<output>`.
    pub fn to_verilog(&self, module: &str) -> String {
        let mut t = String::new();
        let mut ports = Vec::new();
        for (name, sigs) in &self.inputs {
            ports.push(format!("input wire [{}:0] i_{name}", sigs.len() - 1));
        }
        for (name, bits) in &self.outputs {
            ports.push(format!("output wire [{}:0] o_{name}", bits.len() - 1));
        }
        writeln!(
            t,
            "// Generated by bitlogic-net: LUT/CARRY4 netlist; do not edit."
        )
        .unwrap();
        writeln!(t, "module {module}(\n    {}\n);", ports.join(",\n    ")).unwrap();
        writeln!(t, "    wire [{}:0] s;", self.signals.max(1) - 1).unwrap();
        let src = |x: &Src| match x {
            Src::Const(b) => format!("1'b{}", u8::from(*b)),
            Src::Sig(i) => format!("s[{i}]"),
        };
        for (name, sigs) in &self.inputs {
            for (j, s) in sigs.iter().enumerate() {
                writeln!(t, "    assign s[{s}] = i_{name}[{j}];").unwrap();
            }
        }
        for (ci, cell) in self.cells.iter().enumerate() {
            if let Cell::Lut { out, ins, init } = cell {
                let k = ins.len().max(1);
                let pins: Vec<String> = if ins.is_empty() {
                    vec![".I0(1'b0)".into()]
                } else {
                    ins.iter()
                        .enumerate()
                        .map(|(j, s)| format!(".I{j}({})", src(s)))
                        .collect()
                };
                let init = init & mask(k);
                writeln!(
                    t,
                    "    LUT{k} #(.INIT({}'h{:x})) l{ci} (.O(s[{out}]), {});",
                    1u32 << k,
                    init,
                    pins.join(", ")
                )
                .unwrap();
            }
        }
        for (chain_id, chain) in self.chains.iter().enumerate() {
            for (b, block) in chain.stages.chunks(4).enumerate() {
                let mut s_pins = vec!["1'b0".to_string(); 4];
                let mut di_pins = vec!["1'b0".to_string(); 4];
                let mut co = vec![String::new(); 4];
                let mut o = vec![String::new(); 4];
                for (j, &stage) in block.iter().enumerate() {
                    let Stage { co: c, o: x, s, di } = &self.stages[stage];
                    s_pins[j] = src(s);
                    di_pins[j] = src(di);
                    if let Some(c) = c {
                        co[j] = format!("s[{c}]");
                    }
                    if let Some(x) = x {
                        o[j] = format!("s[{x}]");
                    }
                }
                let name = format!("c{chain_id}_{b}");
                writeln!(t, "    wire [3:0] {name}_co, {name}_o;").unwrap();
                let (ci, cyinit) = if b == 0 {
                    ("1'b0".to_string(), src(&chain.cin))
                } else {
                    (format!("c{chain_id}_{}_co[3]", b - 1), "1'b0".to_string())
                };
                writeln!(
                    t,
                    "    CARRY4 {name} (.CO({name}_co), .O({name}_o), .CI({ci}), .CYINIT({cyinit}), \
                     .DI({{{}}}), .S({{{}}}));",
                    di_pins.iter().rev().cloned().collect::<Vec<_>>().join(", "),
                    s_pins.iter().rev().cloned().collect::<Vec<_>>().join(", "),
                )
                .unwrap();
                for j in 0..4 {
                    if !co[j].is_empty() {
                        writeln!(t, "    assign {} = {name}_co[{j}];", co[j]).unwrap();
                    }
                    if !o[j].is_empty() {
                        writeln!(t, "    assign {} = {name}_o[{j}];", o[j]).unwrap();
                    }
                }
            }
        }
        for (name, bits) in &self.outputs {
            let parts: Vec<String> = bits.iter().rev().map(src).collect();
            writeln!(t, "    assign o_{name} = {{{}}};", parts.join(", ")).unwrap();
        }
        t.push_str("endmodule\n");
        t
    }
}
