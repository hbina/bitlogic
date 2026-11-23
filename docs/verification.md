# Bitlogic verification

Validated locally on 2026-11-08 with rustc 1.99.0
`b940084d7 2026-09-28`, Cargo 1.99.0 `5f94df478 2026-08-27`,
and Icarus Verilog 12.0. Cargo.lock fixes the Rust dependency versions.

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
make emit example
```

The active workspace has 80 tests and 27 doctests (21 compile-fail).

The 2026-11-08 audit also reran the 16 historical processor CLI commands,
the generic mapped and routed processor regressions, Ruff and the physical
report checker. All passed. Freshly emitted processor RTL matches both flow
inputs byte for byte, and all 15 recorded physical-evidence hashes match.

## Native and graph checks

Five `bitlogic/tests/logic.rs` integration tests include independent comparisons
of native Rust and graph evaluation. Cases cover every input of a three-bit
conditional transform, a minimum/flag function, bit slicing/concatenation and
bounded immutable fold expansion. The graph interpreter exhausts all 64 input pairs for
each of eleven binary operations at three bits. Tests also check unsigned wrap
behavior, 64-bit boundaries, oversized shifts, composition identity, mismatched
port shapes, out-of-range input values, and invalid widths/module names.

## Emitted combinational HDL

One Icarus test checks all 128 combinations of a conditional transform against an
independent Verilog oracle. A separate operator test checks 2,725 cases across
one-, three-, and 64-bit arithmetic/bitwise/comparison operations, slices,
concatenation, and shift boundaries. The tests execute generated HDL using
`iverilog -g2012` and `vvp`, require successful exit, and check success markers.
They do not infer clock behavior or physical timing.

## Bounded set example

Two tests in `crates/bitlogic/examples/set.rs` independently check every one of
256 membership bitmaps, eight keys, and four operations (8,192 cases). A Rust
`BTreeSet` oracle supplies the expected next set and pre-operation membership.
One test checks native Rust and IR evaluation; the other checks generated HDL
with Icarus against the oracle's expected values. This includes duplicate inserts,
absent removals, empty/full sets, both boundary keys, and clear for every key.
The example's `test = true` Cargo setting includes these in the workspace tests.

The removed set demo historically passed its native/IR
assertions across a sequence of operations. The `verilog` mode emits `set_step`,
which compiles with `iverilog -g2012 -s set_step`. No state registers are emitted.

## Protocol processor example

Twenty-five tests in `examples/protocol_processor/tests.rs` and its child modules participate in the workspace
suite through Cargo's `test = true` setting. All pass on 2026-11-08. The reference
machine uses ordinary integers and a separate decoder, not the DSL or host decoder.
Protocol waveforms and peer responses are specified independently of instruction
execution, then checked against native Rust, IR and generated HDL.

| Check | Exact local evidence |
|---|---|
| ISA and image validation | Every nonzero 16-bit WAIT encoding; all 65,536 SET_PINS mask/value combinations; expected encoding constants, invalid operand constructors, malformed/reserved bits, capacity, final HALT, and branch-target rejection |
| Transition equivalence | 327,190 reference/native/IR/combinational-Icarus cases, including every WAIT issue/countdown value, masks, PC boundaries, halted/wait/fault priority, scratch aliases/truth tables, input sampling, runtime control flow, pin configuration, atomic sample/output updates and word parity; includes 65,536 configuration mask/enable combinations |
| UART TX | 6,144 one-byte programs (256 bytes × 8 pins × periods 1/2/8), each with zero and nonzero initial output values in native/IR; independent serialized-frame oracle; clocked HDL uses reset state |
| UART TX clocked coverage | 326,345 edges including all 6,144 programs, empty/multi-byte cases and a full period-65536 empty transmission |
| Core clocked boundaries | 801 edges for reset during WAIT/HALT/fault, restart, unused ROM, HALT at PC=255 and increment faults there |
| Runtime polling | 8,836 clocked edges with matching native/IR; input transitions swept across polling/timeout boundaries for pins 0/7, attempts 1/2/5 and gaps 1/2/8; infinite-loop runner limit checked |
| SPI | 2,112 independent peer/waveform programs: all bytes × four modes × two orders at half-period 2, plus selected bytes at 3/8; 84,064 clocked edges; received-byte reconstruction and pin/clock/select scheduling |
| UART RX | 797 independent frame/error scenarios; all bytes and polling phases at period 16, selected bytes at 8/32, adjacent next-frame start, false start, bad stop, no-start timeout; 135,340 clocked edges |
| I²C | 684 independent resolved-bus scenarios: all 256 write bytes and 256 read responses, all 112 ordinary addresses, two pin mappings, stretching each of 19 clock releases, address/data NACK, busy clock/data, and timeout edges; 119,794 clocked edges |
| Pad reset | 19 clocked edges verify reset while the controller drives SCL and subsequent released pads; atomic configuration expected values, reserved bits and label/image errors checked separately |
| JTAG | 672 TAP-model scenarios: IR/DR scans of widths 1/4/8/16/32 from all 16 states, all byte values at width 8, captured data and final Idle; 83,808 clocked edges after shared falling/setup edge compilation |
| PS/2 | 272 independent resolved-bus scenarios: all 256 bytes, alternate pins/hold and initial inhibit, inhibit after each of the first ten bits, host request, timeout, odd parity and released exits; 32,482 clocked edges |
| SWD | 549 independent push-pull target scenarios: DP/AP request fields, all ACKs, read/write, parity failure, WAIT retry/exhaustion and alternate pins/holds; 119,250 clocked edges with ownership checked before/after every edge |
| SWD reset/atomic instruction | 298 clocked edges check 56-high/two-low reset clocks and sampling a pin's old level while atomically changing its output; literal encodings and PARITY_WORD source/destination aliases checked independently |
| JTAG sequence | Four-bit IR then 32-bit DR from all 16 initial states, selected instruction preserved at DR capture and both results retained; 4,032 clocked edges |
| Composition | 24 independent combined UART/SPI waveforms, including explicit shared-pin handoff; 1,952 edges; polling-success/timeout continuation adds 27 edges; resource/exit/capacity/rollback and all branch relocation forms tested |
| UART batch RX | 780 complete adjacent-frame scenarios: all byte values and polling phases in two-frame batches at period 8, six-frame batches at 8/16/32, and a later bad stop preserving earlier bytes; 143,532 clocked edges |

Clocked tests total 1,060,580 edge observations. They stream test vectors into an
Icarus testbench that fetches from `rom[pc]`; they do not bypass the fetch interface
by supplying the expected next instruction manually. Every ROM load first fills
unused words with zero. Checkers require simulator success and exact vector counts.
Resolved-bus cases instantiate `protocol_pads` with tri-state drivers and `tri1`
pull-ups; external peer drivers and expected sampled inputs are independently
specified. The simulator checks the resolved input before each execution edge,
all state including enables afterward, and that open-drain outputs never drive
high. Push-pull SWD cases additionally check that target and host never own SWDIO
simultaneously, including matching values. I²C's target decodes SCL/SDA transitions without reading PC/instructions;
JTAG's peer uses a sixteen-state TAP table; PS/2's host checks framing and parity.
Combinational tests separately inject arbitrary boundary states. These finite
checks do not exhaust the machine's complete state space or establish formal
equivalence. Maximum SPI/RX period construction is tested, but their complete
maximum-period waveforms are not simulated.

The processor IR records
2,418 IR nodes and 111,484 bytes for the combinational module in this revision;
the wrapper is additional. This is not a synthesis area or timing measurement.
See the [processor contract](protocol_processor.md) and [protocol interfaces](protocols.md).

## Processor migration equivalence

`bash scripts/processor_migration_equiv.sh` reconstructs the baseline from Git
revision `e4784a926d096a9fb3134eb3e1d08a6f6c4adaf1`, emits both processor cores,
hides incidental internal wire names and uses Yosys `equiv_simple` followed by
`equiv_status -assert`. All 122 next-state output bits are proven equivalent.
This proof compares the two emitted combinational processor functions for all
input bit patterns. It is not a general source-language soundness proof, a
native/IR equivalence proof or an electrical/timing claim. The wrapper and the
independent integer reference are unchanged. The script requires Git history
containing the baseline revision and the locked optional uv synthesis tools.

## Generic mapping and mapped regression

`make processor-mapped-test` passes with the locked optional uv tools. Yosys 0.70
maps the core to 2,165 generic logic cells and 122 synchronous-reset flops, checks
structure and emits a Verilog netlist. The same 25 processor tests pass with the
mapped modules: 327,190 transition vectors and 1,060,580 clocked observations.
This is a second implementation path, not additional unique input coverage or a
formal equivalence proof. The pad module remains a logical simulation model.
Exact commands, tool versions, cell types, tri-state warning and physical limits
are recorded in the [synthesis baseline](processor_synthesis.md).

## SKY130 routed block

`make processor-sky130-test` passes on the pinned offline ORFS container. It
checks the routed TT/25 °C/1.8 V block at a 20 ns clock: +10.3544 ns setup and
+0.253807 ns hold worst slack, zero setup/hold TNS, zero router DRC and antenna
violations, and an all-checks `check_setup` constraint audit. Standard-cell area
excluding fill is 15,713.8 µm²; the block has 122 sequential cells.

All 25 processor tests pass with `6_final.v` selected for their clocked paths,
covering 1,060,580 observations with Liberty-derived functional cell models.
The combinational-only test remains source RTL in this run. There is no SDF
annotation; extracted STA supplies separate timing evidence. Kepler's final
checkpoint comparison reports no difference against technology synthesis, with
unused clock-gating-model warnings retained. This is not a native/IR-to-HDL proof.

The [physical study](sky130_implementation.md) links checked-in raw reports,
artifact hashes, the reproducible flow, explicit IO/ROM assumptions and limits.
Full-corner timing, foundry-deck DRC/LVS, IO/CDC/chip integration, activity-based
power and board/silicon validation remain unverified.

## Structured frontend, dependencies and diagnostics

Six tests in `tests/structured.rs` check nested schemas and nominal identities,
field-declaration ordering despite reordered literals, native flatten/reconstruct
round trips, malformed values, typed-evaluation mismatch, bool versus Bits<1>,
whole-struct conditionals, immutable shadowing and qualified type paths. All 256
combinations of a nested state/reset/unused-input fixture match independent
expected values in native Rust, IR and Icarus. Dependency expectations include
conditional predicates and exclude unused inputs; `x ^ x` retains its structural
dependency. Composition tests check nominal mismatch, explicit adapters, both
identity laws and associativity over all sixteen states. Flattening collisions
on both interfaces are rejected. Bounded folds check index values through 255,
zero iterations and accumulator behavior; the processor and set exercise struct
accumulators. Invalid bit/slice/concat bounds fail during capture.

A macro unit test checks mutation/reference/assignment rejection, invalid
signatures, missing else/early returns, iteration/work limits and validation of
zero-iteration fold bodies, duplicate parameters and generic/non-path calls.
Fourteen compile-fail doctests cover mutation, references, missing fields,
incompatible branches, unsupported leaf types, `ir()` on a multi-input helper,
calls to ordinary Rust functions, reassignment, early return, struct updates,
tuple structs, run-time `[]` indexing and match guards (the enums-with-fields
case was replaced on 2026-11-21 by nested-pattern and `|`-binding cases).
Three executable doctests check pure structured native/IR evaluation and HDL
construction, helper calls over all 256 bytes, and an array/enum register file.

## Calls between logic functions (2026-11-18)

`tests/calls.rs` checks a function built from zero-, one-, two- and three-input
helpers, struct arguments and results, a module-path call, a call inside an `if`
and calls inside an eight-iteration fold. All 256 inputs match an independent
oracle (`format!("{:02X}")`, `count_ones`, wrapping addition) in native Rust,
the IR and Icarus-simulated HDL. It also checks that inlining adds no ports,
that a single-input helper still has its own `ir()`, and that a recursive call
terminates natively but fails during capture with a depth error.

## Arrays, enums and constant folding (2026-11-18)

`tests/arrays.rs` checks a request/response function over a five-entry table
(not a power of two) of `{valid, key, value}` structs: run-time reads with a
4-bit index (every value, including the out-of-range 5..=15), a 1-bit index
that reaches only two entries, `set` with a 2-bit index, a fold-based lookup,
aggregate `==`/`!=`, array literals, repeats and literal indexes. 2,000 random
requests match an oracle written with integer tuples and `Vec` in native Rust,
the IR and Icarus-simulated HDL. Further tests check that constant indexes and
constant writes (directly and inside a fold) build no muxes, that ports are
named by index, and that a 64-bit index far past the end reads zero.

`tests/enums.rs` runs every one of the 768 states of a machine with an
implicit-discriminant enum, a sparse explicit-discriminant enum (4 bits wide,
zero value `Low`), a `[bool; 2]` field, nested `match` on enums, `Bits` consts
and `bool`, alternatives and `_`, against an integer oracle in native Rust, the
IR and Icarus. It also checks encodings, widths, invalid-discriminant
rejection, nominal identity of look-alike enums, first-match-wins on
overlapping/unreachable arms, and the enum comments in emitted Verilog.

Mutations of the run-time read (padding, range check, tree order), `set`
(constant and run-time paths), aggregate `!=`, enum zero values, wildcard
handling and arm order were each applied and each made a test fail. Macro unit
tests and compile-fail doctests cover rejected forms (run-time `[]`, guards,
duplicate discriminants; field-carrying enums are now supported, see below).
Constant folding changes generated netlists, not behavior: the full workspace
suite, including the 25 processor tests, passes with it, and
`scripts/processor_migration_equiv.sh` (Yosys 0.70 `equiv_simple`) proves the
folded processor next-state HDL equivalent to the pre-migration baseline
(122 of 122 `$equiv` cells proven).

The [migration audit](pure_functions.md) records baseline and current evidence.
The integer processor reference and protocol oracle logic are preserved.

## Enums with fields (2026-11-21)

`tests/adts.rs` runs all 592 states of a machine. Its state includes an enum
with a unit variant, a brace variant, a tuple variant whose field is a sparse
enum (zero value encoded as 1), and a variant that wraps a struct containing an
array. The machine's logic constructs every form, matches with bindings,
renamed fields, `..`, `_` and `|`, compares data-enum values with `==`, and
passes one through a `#[logic]` call. Native Rust, the IR and Icarus all match
an oracle written over raw leaf integers. Further tests check:

- the leaf layout, zero value and `flatten`/`read` round trip of all 37 values;
- that `read` rejects non-canonical inactive fields and invalid discriminants;
- the tag-encoding comments and port names in the Verilog;
- run-time `at` reads from a `[Cmd; 3]`, including the out-of-range zero value,
  in native Rust, the IR and Icarus;
- a `system!` register holding a data enum, in the cycle model and generated
  Verilog;
- rejection of a register whose zero value is not all zero bits;
- that `<` on data enums is an `ir()` error.

Further tests check the lowering cost:

- `==` against a unit variant builds one comparison; disabling the
  known-variant shortcut makes it seven, and the test fails;
- an identity `match` over all four variants builds 9 muxes instead of 13 for a
  priority chain; disabling one-hot selection makes the test fail;
- `matches!` with `|` and `..` agrees with native Rust for all 37 values.

Three implementation mutations each made a test fail: inactive fields built as
0 instead of their zero value, field offsets that ignore the tag leaf, and an
inverted variant test. Compile-fail doctests cover non-logic field types,
nested patterns and bindings in `|` alternatives. Macro unit tests cover
generic and reference-typed variants.

## Values wider than 64 bits (2026-11-21)

`src/bitvec.rs` unit tests check the multi-word add, subtract, not, compare,
shifts and hexadecimal printing against `u128` arithmetic at widths 1, 7, 63,
64, 65, 100, 127 and 128, plus concat and slice across word boundaries.
`tests/wide.rs` then checks:

- 600 random pairs of 100-bit values, biased toward all-ones, zero, small
  values and `a == b + 1`: sum, difference, bitwise mix, both shifts across a
  word, comparisons, a 200-bit `concat` and a 40-bit slice across both the
  operand and the word boundary, in native Rust, the IR and Icarus against a
  `u128` oracle;
- `Bits` words, ordering, `to_u64`, `Debug` and wrapping at a width that is not
  a multiple of 64, and a `const` 128-bit value;
- an enum whose variants carry 96- and 160-bit fields, in native Rust, the IR
  and Icarus, including rejection of a 161-bit leaf value;
- a `system!` with a 96-bit register state (a scalar state, newly supported)
  and a 96-bit parameter, in the native and interpreted cycle models and in
  Icarus, which runs the generated core for three edges.

Compile-fail doctests reject `get()` on a 65-bit value and a 4097-bit width.
Each was checked to compile at 64 and 4096 bits, so the doctests fail for
the intended reason.

The arty_ethernet suite (51 tests, including Vivado xsim) passes, and its
nine generated Verilog files are byte-identical before and after the change. Native simulation is not slower: `≤64`-bit `Bits` operations and
interpreter nodes take single-word paths.

## Tagged-union layout for enums with fields (2026-11-21)

Enums with fields now use one payload leaf as wide as the widest variant,
instead of a leaf per field of every variant. The bits above the current
variant are don't-care. This replaces the layout and the canonical-zero rule
described in [Enums with fields](#enums-with-fields-2026-11-21), and the tests
there were rewritten for it. `tests/adts.rs` (13 tests) checks:

- **The 592-state machine.** Native Rust matches the integer oracle. The IR,
  fed each state's leaves with random bits in every don't-care payload bit
  (about a third of the inputs carry some), decodes to the native result.
  Icarus runs the same padded inputs and its outputs are compared under
  `Schema::valid_bits` masks.
- **Layout.** Payload width, `variant_layout` offsets, the zero value, the
  `flatten`/`read` round trip of all 37 values, and `read` ignoring padding
  while rejecting an invalid field encoding, an invalid tag and an over-wide
  payload. Also `encode_variant`, `decode_variant` and `valid_bits`.
- **Generated Verilog.** The `*_payload` ports and the payload-layout comment.
- **Nested enums with fields.** Payload offsets, and masks that also ignore the
  inner enum's padding.
- **Unknown-value `==` and `!=`.** Every pair of 37 `Cmd` values and of 21
  nested `Outer` values, each with random padding, in the IR and Icarus.
- **Lowering cost.**
  - Rebuilding a `Cmd` variant by variant builds 3 tag muxes and no payload
    mux, and the output payload is the input port.
  - Moving a field shared as the first field of two variants builds only
    1-bit (tag) muxes.
  - `==` against a unit variant is one comparison.
- **Registers.** A `system!` register holding a data enum agrees between the
  native and interpreted cycle models. A data enum whose zero value has a
  nonzero payload is rejected as a register state.

`tests/logic.rs` adds a builder test of the new folds:
- slices through concatenations and slices;
- adjacent slices rejoined;
- a straddling slice;
- don't-care mux operands;
- a mux split that leaves no mux;
- shared nodes.

It also checks 500 random evaluations against an oracle. `tests/wide.rs`
checks a 160-bit payload leaf and its 161-bit rejection.

Five implementation mutations each made at least two tests fail:
- `==` comparing only tags;
- a mux keeping its don't-care operand;
- no mux splitting;
- zero instead of don't-care padding;
- variant field reads taking every payload leaf.

**arty_ethernet.** All 51 tests pass, including Vivado xsim. The step tests now
evaluate the IR on raw leaves with random padding. The book's phase register
is a 4-bit tag plus a 194-bit payload, down from 354 bits of per-field leaves.
Vivado 2025.2 synthesis only (`synth_design`, hierarchy kept, xc7a100t),
before and after:

| | LUTs | FFs |
|---|---|---|
| `book_unit` | 1008 → 895 | 631 → 472 |
| `proto_unit` | 2426 → 2229 | 2001 → 2001 |
| whole design | 4724 → 4415 | 3659 → 3500 |

`proto_unit` has no enums with fields; it gains from node sharing and the new
folds. The other units move by at most 5 LUTs (`feed_unit` 765 → 770,
`tx_unit` 165 → 161). This is a synthesis estimate, not a placed-and-routed or timing result.
The generated RTL changes in every module that the new folds touch.

## Fixed-capacity sets and maps (2026-11-18)

`tests/collections.rs` drives a `fixed_set!` of 43-bit keys with 5 slots (not
a power of two, so `next` wraps by comparison) and a `fixed_map!` keyed by a
two-field struct with 4 slots, each through 3,000 random insert/remove/update
steps with eight or seven distinct keys, so containers fill, replace and
vacate. After every step the slot contents, `next`, membership, `get` and
`count` match an independent model (a `Vec<Option<_>>` with first-vacant
insertion and round-robin replacement) in native Rust and the IR; every fifth
step is also replayed in Icarus. A third test checks index and count widths at
capacities 1, 4, 5 and 256 and round-robin replacement in a full 256-slot set.
Nine mutations of the generated code (vacant-slot preference, replacement
advance, removal, count, wrap, `update` adding, map refresh, `find` ignoring
`valid`, last-match priority) were each applied and each made a test fail;
last-match priority is unobservable through the operations (a key is stored
once), so a hand-built state with a duplicate key checks first-match `find`.
Three compile-fail doctests reject a set with a value type, a map without
one, and capacity 0.

## Clocked systems (2026-11-19)

`tests/system.rs` declares a producer and a consumer joined through a 16-entry
RAM, plus an 8-entry ROM, a struct param, a registered and an unregistered
input, and outputs from registered state, a registered unit output, a
registered input, a param and RAM read data. Over 3,000 cycles of bursty random
input with two reset edges every 1,000 cycles, the generated cycle model
matches an independent integer model (written directly, not from the
declaration) and the IR-evaluated model on every output and on unit state and
RAM contents; the ring fills, drains, and reads an address on the same edge it
is written (read-first). The first 1,500 cycles are replayed against the
generated Verilog under Icarus. A third test checks that a RAM declared wider
than its address signals is reported by `verilog()` and that param defaults
reach the design.

`bitlogic-macros` unit tests accept a declaration using every connection kind
and reject 20 malformed ones (combinational unit outputs into units,
unregistered outputs from units or unregistered inputs, missing, duplicate or
ROM write ports, unknown names, non-name expressions, `..`, a missing or
renamed `state`, reserved and duplicate names, address widths, unknown items).

Nine injected bugs were each applied to the macro or backend: writing before
reading, never updating an input register, ignoring reset in the model,
skipping the input register or the output register in Verilog, reading a RAM
at its write address, and not resetting an output register were caught. Not
preloading the first ROM word or the RAM read data before the first edge
survived: reset edges overwrite both, so neither is observable.

In `arty_ethernet`, the generated core replaced a hand-written `eth_core.v`
and Rust cycle model. Before the hand-written files were deleted, the two
cycle models agreed on every output and every unit state over 526,184 cycles of
random ARP, ICMP, IPv4, beacon and corrupted-FCS traffic with RX-ring overflow
bursts, and the two Verilog cores produced identical `tx_en`, `txd`,
`rx_active` and `tx_frame_toggle` over 63,400 cycles in Vivado xsim.

## Ports (2026-11-23)

`tests/ports.rs` declares a unit that runs read-modify-write passes over a
256-word memory outside the system, through a `port`: read `mem[a]`, wait for
the response, write back `mem[a] + k + 1`, step an LFSR address. Its results
cannot depend on when the other side accepts or answers, so they are checked
against a plain sequential loop (memory contents and the sum of every value
read, allowing for a last pass that has read but not yet written) over 24
trials of 6,000 cycles with an `InOrder` memory of depth 1 to 4 and random
latency up to 24 edges, every third trial with a responder that also stalls
both channels on a random half of the cycles. Every transfer happens only
with both valid and ready high. The IR-evaluated model equals the native one
on all six port signals and every output for 3,000 cycles. The generated
Verilog runs under Icarus for 2,000 cycles with the other side's signals
replayed from the cycle model, and every signal the core drives (the
request while valid, `rsp_ready`, and the outputs, one of them a registered
port signal) is compared before and after each edge; the run makes over
300 transfers. An unconnected port never transfers. The macro accepts a
declaration with a port and rejects 12 malformed ones (missing `request` or
`respond`, either connected twice, wrong arity, a port driving a port, an
unknown port, an unregistered port output, reading a port other than through
its three signals, a duplicate name, a missing `=>`).

Three injected bugs were each caught: a cycle model that transfers requests
without `req_ready`, Verilog that wires `req_ready` to the response-valid
port, and an `InOrder` that offers a response before its latency.

## Build and CI

`make emit` writes `build/select_logic.v`; `make example` prints the HDL example.
The GitHub workflow installs Icarus, checks formatting and Clippy, runs workspace
tests, and emits the HDL example. No remote CI execution is claimed.

## Limits and retired coverage

The earlier RHDL ISA, processor, timed frontend, and processor backend were
removed, along with their tests. The new library example above has its own
verification. Historical processor/waveform results are not active bitlogic
coverage. See [project history](history.md).

There is no native/IR-to-HDL formal proof, full-corner/chip signoff, FPGA fit,
board test, CDC integration or electrical validation. The routed SKY130 block and
scoped mapped-netlist comparison above do not remove those limitations. Icarus
provides functional simulation and OpenROAD supplies the recorded block timing;
neither establishes correctness for every possible source program or target.

## Example artifact export (2026-11-09)

The examples accept no arguments and write all outputs into retained random
`/tmp` folders. `tests/artifacts.rs` checks distinct persistent directories,
Verilog fidelity, SVG interface labels, mux operand connectivity and an aliased
output in the complete Mermaid circuit graph. All four exported HDL modules
and the processor's separate/combined wrapper files are elaborated with Icarus
in the local acceptance check. The updated generic mapped regression and 122-bit migration equivalence proof
also pass. Prior physical evidence and hashes describe the earlier source
snapshots; the physical flow was not rerun for this artifact export change. The Makefile,
SKY130 script and migration-equivalence script now consume generated files.
See [artifact contents and commands](example_artifacts.md).
