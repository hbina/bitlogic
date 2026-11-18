//! Calls between `#[logic]` functions: helpers with zero, one or several
//! arguments, struct arguments and results, module paths, calls inside
//! conditionals and folds, and recursion rejection. Native Rust, the IR and
//! Icarus-simulated HDL are checked against an independent oracle.
use bitlogic::{Bits, LogicValue, bits, logic};
use std::{fmt::Write, process::Command};

mod digits {
    use bitlogic::{Bits, bits, logic};
    /// ASCII uppercase hex digit of a nibble.
    #[logic]
    pub fn hex(n: Bits<4>) -> Bits<8> {
        let wide = bits::<4>(0).concat::<4, 8>(n);
        if n < bits::<4>(10) {
            wide + bits::<8>(0x30)
        } else {
            wide + bits::<8>(0x37)
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
struct Pair {
    high: Bits<8>,
    low: Bits<8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
struct Encoded {
    text: Pair,
    swapped: Pair,
    parity: bool,
    sum: Bits<8>,
}

#[logic]
fn offset() -> Bits<8> {
    bits::<8>(0x5A)
}

#[logic]
fn xor2(a: bool, b: bool) -> bool {
    a ^ b
}

#[logic]
fn add3(a: Bits<8>, b: Bits<8>, c: Bits<8>) -> Bits<8> {
    a + b + c
}

#[logic]
fn swap(p: Pair) -> Pair {
    Pair {
        high: p.low,
        low: p.high,
    }
}

#[logic]
fn hex_pair(byte: Bits<8>) -> Pair {
    Pair {
        high: digits::hex(byte.slice::<4>(4)),
        low: crate::digits::hex(byte.slice::<4>(0)),
    }
}

#[logic]
fn encode(byte: Bits<8>) -> Encoded {
    let text = hex_pair(byte);
    // A call inside a fold body: one inlined copy per iteration.
    let parity = false.fold::<8>(|acc, i| {
        let shifted = byte;
        xor2(
            acc,
            if i == bits::<8>(0) {
                shifted.bit(0)
            } else if i == bits::<8>(1) {
                shifted.bit(1)
            } else if i == bits::<8>(2) {
                shifted.bit(2)
            } else if i == bits::<8>(3) {
                shifted.bit(3)
            } else if i == bits::<8>(4) {
                shifted.bit(4)
            } else if i == bits::<8>(5) {
                shifted.bit(5)
            } else if i == bits::<8>(6) {
                shifted.bit(6)
            } else {
                shifted.bit(7)
            },
        )
    });
    Encoded {
        text,
        swapped: if parity { swap(text) } else { text },
        parity,
        sum: add3(text.high, text.low, offset()),
    }
}

fn expected(byte: u8) -> Encoded {
    let text = format!("{byte:02X}").into_bytes();
    let text = Pair {
        high: bits(text[0].into()),
        low: bits(text[1].into()),
    };
    let parity = byte.count_ones() % 2 == 1;
    Encoded {
        text,
        swapped: if parity {
            Pair {
                high: text.low,
                low: text.high,
            }
        } else {
            text
        },
        parity,
        sum: bits(text.high.get() + text.low.get() + 0x5A),
    }
}

#[test]
fn calls_match_native_ir_and_hdl_for_every_byte() {
    let ir = encode::ir().unwrap();
    let mut source = ir.to_verilog("encode").unwrap();
    source.push_str(
        "module tb;reg [7:0] byte_in;wire [7:0] th,tl,sh,sl,sum;wire parity;\n\
         encode dut(.i_byte(byte_in),.o_text_high(th),.o_text_low(tl),.o_swapped_high(sh),\
         .o_swapped_low(sl),.o_parity(parity),.o_sum(sum));initial begin\n",
    );
    for byte in 0..=255u8 {
        let want = expected(byte);
        let input: Bits<8> = bits(byte.into());
        assert_eq!(encode(input), want, "native {byte:#x}");
        assert_eq!(
            ir.evaluate_typed::<_, Encoded>(input).unwrap(),
            want,
            "ir {byte:#x}"
        );
        writeln!(
            source,
            "byte_in=8'd{byte};#1;if(th!==8'd{}||tl!==8'd{}||sh!==8'd{}||sl!==8'd{}||parity!==1'd{}||sum!==8'd{}) $fatal(1,\"byte {byte}\");",
            want.text.high.get(),
            want.text.low.get(),
            want.swapped.high.get(),
            want.swapped.low.get(),
            u8::from(want.parity),
            want.sum.get()
        )
        .unwrap();
    }
    source.push_str("$display(\"CALLS PASS\");$finish;end endmodule\n");
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
    assert!(
        run.status.success() && String::from_utf8_lossy(&run.stdout).contains("CALLS PASS"),
        "{}",
        String::from_utf8_lossy(&run.stdout)
    );
}

#[test]
fn callee_graphs_are_inlined_without_extra_ports() {
    // Inlining adds no ports: only the caller's input and output leaves.
    let ir = encode::ir().unwrap();
    let inputs: Vec<_> = ir.inputs().iter().map(|p| p.name.as_str()).collect();
    assert_eq!(inputs, ["byte"]);
    assert_eq!(ir.outputs().len(), 6);
    // A single-input helper is also a complete circuit on its own.
    let hex = digits::hex::ir().unwrap();
    for n in 0..16 {
        let want = u64::from(format!("{n:X}").as_bytes()[0]);
        assert_eq!(
            hex.evaluate_typed::<_, Bits<8>>(bits::<4>(n))
                .unwrap()
                .get(),
            want
        );
    }
}

#[logic]
fn recursive(a: bool) -> bool {
    if a { recursive(!a) } else { false }
}

#[test]
fn recursive_calls_fail_during_capture() {
    // Native evaluation terminates; capture would unroll forever and must stop.
    assert!(!recursive(true));
    let error = recursive::ir().unwrap_err();
    assert!(error.0.contains("call depth"), "{error}");
}
