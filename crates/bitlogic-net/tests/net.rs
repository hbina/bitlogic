//! Random word-level graphs using every IR operation, checked through each
//! stage: the IR interpreter (reference), the bit-level `Net`, every pass,
//! and the mapped LUT/CARRY4 netlist (Rust model and Icarus).
use bitlogic::{Binary, Builder, Logic, NodeId};
use bitlogic_net::{Net, map};
use std::{fmt::Write, process::Command};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

const OPS: [Binary; 11] = [
    Binary::And,
    Binary::Or,
    Binary::Xor,
    Binary::Add,
    Binary::Sub,
    Binary::Eq,
    Binary::Ne,
    Binary::Lt,
    Binary::Le,
    Binary::Gt,
    Binary::Ge,
];

/// A random graph of `size` operations over a few inputs.
fn random_logic(rng: &mut Rng, size: usize) -> Logic {
    let mut b = Builder::default();
    let mut values: Vec<(NodeId, u8)> = Vec::new();
    for k in 0..(2 + rng.below(3)) {
        let w = 1 + rng.below(12) as u8;
        values.push((b.input(&format!("x{k}"), w).unwrap(), w));
    }
    // Width-adjust a value: slice down or zero-extend with a concat.
    let fit = |b: &mut Builder, (id, w): (NodeId, u8), want: u8| -> NodeId {
        if w == want {
            id
        } else if w > want {
            b.slice(id, 0, want).unwrap()
        } else {
            let z = b.constant(want - w, 0).unwrap();
            b.concat(z, id).unwrap()
        }
    };
    for _ in 0..size {
        let pick = |rng: &mut Rng, values: &Vec<(NodeId, u8)>| {
            values[rng.below(values.len() as u64) as usize]
        };
        let a = pick(rng, &values);
        let r = match rng.below(9) {
            0..=3 => {
                let op = OPS[rng.below(OPS.len() as u64) as usize];
                let other = if rng.below(4) == 0 {
                    // A constant operand, as in counters and compares.
                    let c = rng.next() & (u64::MAX >> (64 - a.1));
                    b.constant(a.1, c).unwrap()
                } else {
                    let o = pick(rng, &values);
                    fit(&mut b, o, a.1)
                };
                let w = match op {
                    Binary::And | Binary::Or | Binary::Xor | Binary::Add | Binary::Sub => a.1,
                    _ => 1,
                };
                (b.binary(op, a.0, other).unwrap(), w)
            }
            4 => (b.not(a.0).unwrap(), a.1),
            5 => {
                let c = pick(rng, &values);
                let c = b.slice(c.0, rng.below(c.1 as u64) as u8, 1).unwrap();
                let o = pick(rng, &values);
                let o = fit(&mut b, o, a.1);
                (b.mux(c, a.0, o).unwrap(), a.1)
            }
            6 => {
                let w = 1 + rng.below(a.1 as u64) as u8;
                let off = rng.below((a.1 - w + 1) as u64) as u8;
                (b.slice(a.0, off, w).unwrap(), w)
            }
            7 if a.1 < 40 => {
                let o = pick(rng, &values);
                let o = if o.1 > 20 {
                    (b.slice(o.0, 0, 20).unwrap(), 20)
                } else {
                    o
                };
                (b.concat(a.0, o.0).unwrap(), a.1 + o.1)
            }
            _ => (
                b.shift(a.0, rng.below(a.1 as u64 + 2) as u32, rng.below(2) == 0)
                    .unwrap(),
                a.1,
            ),
        };
        values.push(r);
    }
    let outs = 1 + rng.below(4) as usize;
    for k in 0..outs {
        let step = (values.len() / (outs + 1)).max(1);
        let v = values[(values.len() - 1).saturating_sub(k * step)];
        b.output(&format!("y{k}"), v.0).unwrap();
    }
    b.finish().unwrap()
}

fn random_inputs(rng: &mut Rng, logic: &Logic) -> Vec<Vec<u64>> {
    (0..64)
        .map(|_| {
            logic
                .inputs()
                .iter()
                .map(|p| rng.next() & (u64::MAX >> (64 - p.width)))
                .collect()
        })
        .collect()
}

/// Bit words (one per input bit, lanes = vectors) for `vectors`.
fn pack(logic: &Logic, vectors: &[Vec<u64>]) -> Vec<u64> {
    let mut words = Vec::new();
    for (k, p) in logic.inputs().iter().enumerate() {
        for bit in 0..p.width {
            let mut w = 0u64;
            for (lane, v) in vectors.iter().enumerate() {
                w |= (v[k] >> bit & 1) << lane;
            }
            words.push(w);
        }
    }
    words
}

/// Output words expected from the IR interpreter.
fn expected(logic: &Logic, vectors: &[Vec<u64>]) -> Vec<Vec<u64>> {
    let results: Vec<Vec<u64>> = vectors.iter().map(|v| logic.evaluate(v).unwrap()).collect();
    logic
        .outputs()
        .iter()
        .enumerate()
        .map(|(k, p)| {
            (0..p.width)
                .map(|bit| {
                    results
                        .iter()
                        .enumerate()
                        .fold(0u64, |w, (lane, r)| w | (r[k] >> bit & 1) << lane)
                })
                .collect()
        })
        .collect()
}

fn check_net(net: &Net, words: &[u64], want: &[Vec<u64>], what: &str) {
    assert_eq!(net.output_words(&net.simulate(words)), want, "{what}");
}

#[test]
fn every_stage_matches_the_interpreter() {
    let mut rng = Rng(0x5eed_1234_abcd);
    let mut totals = [0usize; 4];
    for case in 0..400 {
        let logic = random_logic(&mut rng, 6 + case % 40);
        let net = Net::from_logic(&logic).unwrap();
        let passes = [
            ("from_logic", net.clone()),
            ("cleanup", net.cleanup()),
            ("balance", net.balance()),
            ("lower", net.lower_short_chains(5)),
            ("lower+balance", net.lower_short_chains(3).balance()),
        ];
        for _ in 0..4 {
            let vectors = random_inputs(&mut rng, &logic);
            let words = pack(&logic, &vectors);
            let want = expected(&logic, &vectors);
            for (name, n) in &passes {
                check_net(n, &words, &want, &format!("case {case} {name}"));
                let mapped = map(n);
                assert_eq!(mapped.simulate(&words), want, "case {case} {name} mapped");
            }
        }
        let st = map(&passes[1].1).stats();
        totals[0] += st.luts;
        totals[1] += st.carry4;
        totals[2] += passes[1].1.stats().carries;
        totals[3] += st.depth;
    }
    eprintln!(
        "400 graphs: {} LUTs, {} CARRY4, {} carry nodes, depth sum {}",
        totals[0], totals[1], totals[2], totals[3]
    );
    assert!(totals[1] > 50, "carry chains are exercised");
}

#[test]
fn exhaustive_small_operators() {
    // Every operator at width 4 against every input pair.
    for op in OPS {
        let mut b = Builder::default();
        let x = b.input("a", 4).unwrap();
        let y = b.input("b", 4).unwrap();
        let r = b.binary(op, x, y).unwrap();
        b.output("r", r).unwrap();
        let logic = b.finish().unwrap();
        let vectors: Vec<Vec<u64>> = (0..256).map(|i| vec![i & 15, i >> 4]).collect();
        let net = Net::from_logic(&logic).unwrap();
        for chunk in vectors.chunks(64) {
            let words = pack(&logic, chunk);
            let want = expected(&logic, chunk);
            check_net(&net, &words, &want, &format!("{op:?}"));
            assert_eq!(map(&net).simulate(&words), want, "{op:?} mapped");
        }
    }
}

/// Behavioral models of the primitives, written from the UG953 truth
/// tables, for Icarus.
const PRIMITIVES: &str = r#"
module LUT1 #(parameter [1:0] INIT = 0) (output O, input I0); assign O = INIT[I0]; endmodule
module LUT2 #(parameter [3:0] INIT = 0) (output O, input I0, I1); assign O = INIT[{I1, I0}]; endmodule
module LUT3 #(parameter [7:0] INIT = 0) (output O, input I0, I1, I2); assign O = INIT[{I2, I1, I0}]; endmodule
module LUT4 #(parameter [15:0] INIT = 0) (output O, input I0, I1, I2, I3); assign O = INIT[{I3, I2, I1, I0}]; endmodule
module LUT5 #(parameter [31:0] INIT = 0) (output O, input I0, I1, I2, I3, I4); assign O = INIT[{I4, I3, I2, I1, I0}]; endmodule
module LUT6 #(parameter [63:0] INIT = 0) (output O, input I0, I1, I2, I3, I4, I5); assign O = INIT[{I5, I4, I3, I2, I1, I0}]; endmodule
module CARRY4 (output [3:0] CO, output [3:0] O, input CI, input CYINIT, input [3:0] DI, input [3:0] S);
    wire c0 = CI | CYINIT;
    assign CO[0] = S[0] ? c0 : DI[0];
    assign CO[1] = S[1] ? CO[0] : DI[1];
    assign CO[2] = S[2] ? CO[1] : DI[2];
    assign CO[3] = S[3] ? CO[2] : DI[3];
    assign O = S ^ {CO[2:0], c0};
endmodule
"#;

#[test]
fn emitted_netlist_matches_under_icarus() {
    let mut rng = Rng(0xfeed_0001);
    let mut source = String::from(PRIMITIVES);
    let mut tb = String::from("module tb;\ninitial begin\n");
    let mut decls = String::new();
    for case in 0..40 {
        let logic = random_logic(&mut rng, 10 + case);
        let net = Net::from_logic(&logic)
            .unwrap()
            .lower_short_chains(2)
            .balance();
        let mapped = map(&net);
        let module = format!("m{case}");
        source.push_str(&mapped.to_verilog(&module));
        let vectors = random_inputs(&mut rng, &logic);
        let mut conns = Vec::new();
        for p in logic.inputs() {
            writeln!(decls, "reg [{}:0] {module}_i_{};", p.width - 1, p.name).unwrap();
            conns.push(format!(".i_{0}({module}_i_{0})", p.name));
        }
        for p in logic.outputs() {
            writeln!(decls, "wire [{}:0] {module}_o_{};", p.width - 1, p.name).unwrap();
            conns.push(format!(".o_{0}({module}_o_{0})", p.name));
        }
        writeln!(decls, "{module} u{case}({});", conns.join(", ")).unwrap();
        for v in &vectors {
            let r = logic.evaluate(v).unwrap();
            for (p, x) in logic.inputs().iter().zip(v) {
                write!(tb, "{module}_i_{} = {}'h{x:x}; ", p.name, p.width).unwrap();
            }
            tb.push_str("#1;\n");
            for (p, x) in logic.outputs().iter().zip(&r) {
                writeln!(
                    tb,
                    "if ({module}_o_{0} !== {1}'h{x:x}) begin $display(\"{module} {0} %h want {x:x}\", {module}_o_{0}); $fatal(1); end",
                    p.name, p.width
                )
                .unwrap();
            }
        }
    }
    tb.push_str("$display(\"HDL PASS\"); $finish;\nend\nendmodule\n");
    let text = source + &tb.replacen("module tb;\n", &format!("module tb;\n{decls}"), 1);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.v");
    std::fs::write(&path, text).unwrap();
    let exe = dir.path().join("t.vvp");
    let c = Command::new("iverilog")
        .args(["-g2012", "-s", "tb", "-o"])
        .arg(&exe)
        .arg(&path)
        .output()
        .expect("Icarus Verilog is required");
    assert!(c.status.success(), "{}", String::from_utf8_lossy(&c.stderr));
    let r = Command::new("vvp").arg(exe).output().unwrap();
    let out = String::from_utf8_lossy(&r.stdout);
    assert!(r.status.success() && out.contains("HDL PASS"), "{out}");
}
