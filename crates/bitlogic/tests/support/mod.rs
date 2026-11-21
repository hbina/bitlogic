//! Icarus Verilog check for generated modules: apply each case's input leaf
#![allow(dead_code)]
//! values to the ports and compare every output port's valid bits.
use bitlogic::{BitVec, Logic, Schema};
use std::{fmt::Write, process::Command};

/// Leaf values as `u64`s, for integer oracles (each leaf at most 64 bits).
pub fn raw(values: &[BitVec]) -> Vec<u64> {
    values
        .iter()
        .map(|v| v.to_u64().expect("leaf fits a u64"))
        .collect()
}

/// `a & b`.
pub fn and(a: &BitVec, b: &BitVec) -> BitVec {
    let words: Vec<u64> = a
        .words()
        .iter()
        .zip(b.words())
        .map(|(x, y)| x & y)
        .collect();
    BitVec::from_words(&words)
}

/// `leaves` of a `schema` value with the don't-care payload bits of every
/// data enum filled from `rng`: an equal value with a different encoding.
pub fn with_padding(schema: &Schema, leaves: &[BitVec], rng: &mut Rng) -> Vec<BitVec> {
    let masks = schema.valid_bits(leaves).unwrap();
    schema
        .leaves()
        .iter()
        .zip(leaves.iter().zip(&masks))
        .map(|((_, leaf), (value, mask))| {
            let width = leaf.width().unwrap();
            let words: Vec<u64> = (0..usize::from(width).div_ceil(64))
                .map(|i| {
                    let v = value.words().get(i).copied().unwrap_or(0);
                    let m = mask.words().get(i).copied().unwrap_or(0);
                    v | (rng.next() & !m)
                })
                .collect();
            BitVec::from_words(&words).masked(width)
        })
        .collect()
}

/// Cases are `(inputs, outputs)` port values, as `u64`s or `BitVec`s. With
/// the IR's output schema, only valid bits are compared: data enum payload
/// bits above the active variant are don't-care.
pub fn check_hdl<A, B>(ir: &Logic, module: &str, cases: &[(Vec<A>, Vec<B>)])
where
    A: Clone + Into<BitVec>,
    B: Clone + Into<BitVec>,
{
    let cases: Vec<(Vec<BitVec>, Vec<BitVec>)> = cases
        .iter()
        .map(|(i, o)| {
            (
                i.iter().cloned().map(Into::into).collect(),
                o.iter().cloned().map(Into::into).collect(),
            )
        })
        .collect();
    let mut source = ir.to_verilog(module).unwrap();
    source.push_str("module tb;\n");
    for p in ir.inputs() {
        writeln!(source, "reg [{}:0] i_{};", p.width - 1, p.name).unwrap();
    }
    for p in ir.outputs() {
        writeln!(source, "wire [{}:0] o_{};", p.width - 1, p.name).unwrap();
    }
    let connections: Vec<_> = ir
        .inputs()
        .iter()
        .map(|p| format!(".i_{0}(i_{0})", p.name))
        .chain(
            ir.outputs()
                .iter()
                .map(|p| format!(".o_{0}(o_{0})", p.name)),
        )
        .collect();
    writeln!(
        source,
        "{module} dut({});\ninitial begin",
        connections.join(",")
    )
    .unwrap();
    for (case, (inputs, outputs)) in cases.iter().enumerate() {
        assert_eq!(inputs.len(), ir.inputs().len());
        assert_eq!(outputs.len(), ir.outputs().len());
        for (p, v) in ir.inputs().iter().zip(inputs) {
            write!(source, "i_{}={}'h{v:x};", p.name, p.width).unwrap();
        }
        source.push_str("#1;\n");
        let masks = match ir.schemas() {
            Some((_, schema)) => schema.valid_bits(outputs).unwrap(),
            None => ir.outputs().iter().map(|p| BitVec::ones(p.width)).collect(),
        };
        for ((p, v), m) in ir.outputs().iter().zip(outputs).zip(&masks) {
            let v = and(v, m);
            writeln!(
                source,
                "if((o_{0}&{1}'h{m:x})!=={1}'h{v:x}) begin $display(\"case {case} o_{0}=%h want {v:x} mask {m:x}\",o_{0}); $fatal(1); end",
                p.name, p.width
            )
            .unwrap();
        }
    }
    source.push_str("$display(\"HDL PASS\");$finish;end\nendmodule\n");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.v");
    let executable = dir.path().join("test.vvp");
    std::fs::write(&path, source).unwrap();
    let compile = Command::new("iverilog")
        .args(["-g2012", "-s", "tb", "-o"])
        .arg(&executable)
        .arg(path)
        .output()
        .expect("Icarus Verilog is required");
    assert!(
        compile.status.success(),
        "{}",
        String::from_utf8_lossy(&compile.stderr)
    );
    let run = Command::new("vvp").arg(executable).output().unwrap();
    let stdout = String::from_utf8_lossy(&run.stdout);
    assert!(
        run.status.success() && stdout.contains("HDL PASS"),
        "{stdout}"
    );
}

/// Deterministic xorshift generator for reproducible random cases.
pub struct Rng(pub u64);
impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}
