//! Fixed-length arrays: literal and run-time indexing (`at`), functional
//! update (`set`), array literals and repeats, aggregate equality, and arrays
//! nested in structs. Native Rust, the IR and Icarus-simulated HDL are checked
//! against an oracle written with plain integers and vectors.
mod support;
use bitlogic::{Bits, LogicValue, Operation, bits, logic};
use support::{Rng, check_hdl};

#[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
struct Entry {
    valid: bool,
    key: Bits<8>,
    value: Bits<12>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
struct Request {
    /// Five entries: not a power of two, so some indexes are out of range.
    table: [Entry; 5],
    read_at: Bits<4>,
    narrow_at: Bits<1>,
    write_at: Bits<2>,
    entry: Entry,
    key: Bits<8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
struct Lookup {
    hit: bool,
    index: Bits<3>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
struct Response {
    read: Entry,
    narrow: Entry,
    written: [Entry; 5],
    lookup: Lookup,
    same: bool,
    differs: bool,
    nibbles: [Bits<4>; 3],
    filled: [Bits<2>; 2],
    last: Bits<12>,
}

#[logic]
fn find(table: [Entry; 5], key: Bits<8>) -> Lookup {
    Lookup {
        hit: false,
        index: bits::<3>(0),
    }
    .fold::<5>(|acc, i| {
        // A constant index inside fold selects without a mux.
        let e = table.at(i);
        let found = !acc.hit && e.valid && e.key == key;
        Lookup {
            hit: acc.hit || found,
            index: if found { i.slice::<3>(0) } else { acc.index },
        }
    })
}

#[logic]
fn respond(r: Request) -> Response {
    let written = r.table.set(r.write_at, r.entry);
    Response {
        read: r.table.at(r.read_at),
        narrow: r.table.at(r.narrow_at),
        written,
        lookup: find(r.table, r.key),
        same: r.table[0] == r.entry,
        differs: written != r.table,
        nibbles: [
            r.read_at,
            bits::<3>(0).concat::<1, 4>(r.narrow_at),
            bits::<4>(0),
        ],
        filled: [r.write_at; 2],
        last: r.table[4].value,
    }
}

/// Independent model: entries as `(valid, key, value)` integer tuples.
fn oracle(r: &Request) -> Vec<u64> {
    let table: Vec<(u64, u64, u64)> = r
        .table
        .iter()
        .map(|e| (u64::from(e.valid), e.key.get(), e.value.get()))
        .collect();
    let entry = (
        u64::from(r.entry.valid),
        r.entry.key.get(),
        r.entry.value.get(),
    );
    let read = |i: u64| table.get(i as usize).copied().unwrap_or((0, 0, 0));
    let mut written = table.clone();
    if let Some(slot) = written.get_mut(r.write_at.get() as usize) {
        *slot = entry;
    }
    let hit = table
        .iter()
        .position(|&(v, k, _)| v == 1 && k == r.key.get());
    let mut out = Vec::new();
    for (v, k, x) in [read(r.read_at.get()), read(r.narrow_at.get())] {
        out.extend([v, k, x]);
    }
    for (v, k, x) in &written {
        out.extend([*v, *k, *x]);
    }
    out.push(u64::from(hit.is_some()));
    out.push(hit.unwrap_or(0) as u64);
    out.push(u64::from(table[0] == entry));
    out.push(u64::from(written != table));
    out.extend([r.read_at.get(), r.narrow_at.get(), 0]);
    out.extend([r.write_at.get(); 2]);
    out.push(table[4].2);
    out
}

fn random_entry(rng: &mut Rng) -> Entry {
    Entry {
        valid: !rng.next().is_multiple_of(3),
        // Few distinct keys, so lookups hit and entries repeat.
        key: bits(rng.next() % 4),
        value: bits(rng.next()),
    }
}

#[test]
fn arrays_match_native_ir_and_hdl() {
    let ir = respond::ir().unwrap();
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut cases = Vec::new();
    for case in 0..2_000u64 {
        let table = [(); 5].map(|_| random_entry(&mut rng));
        let request = Request {
            table,
            // Every read index, including the out-of-range 5..=15.
            read_at: bits(case % 16),
            narrow_at: bits(rng.next()),
            write_at: bits(rng.next()),
            entry: if case % 7 == 0 {
                table[0]
            } else {
                random_entry(&mut rng)
            },
            key: bits(rng.next() % 5),
        };
        let want = oracle(&request);
        let native = respond(request);
        assert_eq!(native.flatten(), want, "native {request:?}");
        assert_eq!(
            ir.evaluate_typed::<_, Response>(request).unwrap(),
            native,
            "ir {request:?}"
        );
        cases.push((request.flatten(), want));
    }
    check_hdl(&ir, "respond", &cases);
}

#[logic]
fn third(t: [Bits<8>; 4]) -> [Bits<8>; 2] {
    [t.at(bits::<2>(2)), t[3]]
}

#[test]
fn constant_indexes_need_no_logic_and_ports_are_named_by_index() {
    let ir = third::ir().unwrap();
    assert!(
        !ir.nodes()
            .iter()
            .any(|n| matches!(n.operation, Operation::Mux { .. })),
        "a constant index must not build a mux tree"
    );
    let names = |ports: &[bitlogic::Port]| -> Vec<String> {
        ports.iter().map(|p| p.name.clone()).collect()
    };
    assert_eq!(names(ir.inputs()), ["t_0", "t_1", "t_2", "t_3"]);
    assert_eq!(names(ir.outputs()), ["result_0", "result_1"]);
    let t = [bits(10), bits(11), bits(12), bits(13)];
    assert_eq!(
        ir.evaluate_typed::<_, [Bits<8>; 2]>(t).unwrap(),
        [bits(12), bits(13)]
    );
    assert_eq!(third(t), [bits(12), bits(13)]);
}

#[logic]
fn wide_index(t: [Bits<8>; 3]) -> Bits<8> {
    // A 64-bit constant index far past the end reads the zero value.
    t.at(bits::<64>(0xffff_ffff_ffff_fff0))
}

#[test]
fn out_of_range_reads_are_zero() {
    let t = [bits(1), bits(2), bits(3)];
    assert_eq!(wide_index(t), bits(0));
    assert_eq!(
        wide_index::ir()
            .unwrap()
            .evaluate_typed::<_, Bits<8>>(t)
            .unwrap(),
        bits(0)
    );
    assert_eq!(
        <[Entry; 2]>::zeroed(),
        [Entry {
            valid: false,
            key: bits(0),
            value: bits(0)
        }; 2]
    );
}

#[logic]
fn rotate(t: [Bits<8>; 4]) -> [Bits<8>; 4] {
    // Constant-index writes, directly and inside fold.
    let shifted = t.set(bits::<2>(0), t[3]);
    t.fold::<3>(|acc, i| acc.set(i + bits::<8>(1), t.at(i)))
        .set(bits::<2>(0), shifted[0])
}

#[test]
fn constant_index_writes() {
    let t = [bits(1), bits(2), bits(3), bits(4)];
    let want = [bits(4), bits(1), bits(2), bits(3)];
    assert_eq!(rotate(t), want);
    let ir = rotate::ir().unwrap();
    assert_eq!(ir.evaluate_typed::<_, [Bits<8>; 4]>(t).unwrap(), want);
    assert!(
        !ir.nodes()
            .iter()
            .any(|n| matches!(n.operation, Operation::Mux { .. })),
        "constant writes must not build muxes"
    );
}
