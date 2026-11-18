//! Icarus Verilog check for generated modules: apply each case's input leaf
#![allow(dead_code)]
//! values to the ports and compare every output port.
use bitlogic::Logic;
use std::{fmt::Write, process::Command};

pub fn check_hdl(ir: &Logic, module: &str, cases: &[(Vec<u64>, Vec<u64>)]) {
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
        for (p, v) in ir.outputs().iter().zip(outputs) {
            writeln!(
                source,
                "if(o_{0}!=={1}'h{v:x}) begin $display(\"case {case} o_{0}=%h want {v:x}\",o_{0}); $fatal(1); end",
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
