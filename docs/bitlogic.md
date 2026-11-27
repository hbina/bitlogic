# Pure structured bit logic in Rust

Bitlogic captures a restricted Rust function as a typed expression graph. The
function maps an owned input value to a complete output value: `F: S1 -> S2`.
The graph can be interpreted, composed with another graph, or emitted as
combinational Verilog. Source functions and locals are immutable. There are no
clock, reset, wait or sampling primitives in the language.

```rust
use bitlogic::{logic, bits, Bits, LogicValue};

#[derive(Clone, Copy, LogicValue)]
struct Inputs { a: Bits<3>, b: Bits<3>, enabled: bool }
#[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
struct Outputs { value: Bits<3>, different: bool }

#[logic]
fn select(input: Inputs) -> Outputs {
    let combined = input.a ^ input.b;
    Outputs {
        value: if input.enabled { combined + bits::<3>(1) } else { input.a },
        different: input.a != input.b,
    }
}

let input = Inputs { a: bits(2), b: bits(3), enabled: true };
let native = select(input);
let ir = select::ir()?;
let interpreted: Outputs = ir.evaluate_typed(input)?;
assert_eq!(native, interpreted);
let verilog = ir.to_verilog("select_logic")?;
```

The macro retains the original function and supplies a same-named marker type
with `select::ir()`. Rust checks native expressions and field types. Generated
paths expect the crate name `bitlogic`. The function macro does not inspect type
definitions in other files: `LogicValue` derives provide schemas and native value
adapters through ordinary Rust trait resolution. No global registry is used.

## Values and complete results

Leaves are `bool`, unsigned `Bits<N>` (widths 1 through 4096) and fieldless
enums. Named non-generic structs derive `LogicValue` and may nest other derived
structs, [enums with fields](#enums-with-fields) and fixed-length arrays
`[T; N]` of any value type. Deriving `Copy`
and `Clone` is useful for native expression reuse. Leaf and struct values can be
inputs or outputs; the input and output types need not be the same. A circuit (a
function with `ir()`) groups multiple logical inputs into one struct; helpers
called from other `#[logic]` functions may take several arguments.

`bits::<N>(value)` takes a `u64`, masked to N bits (zero-extended when N > 64).
`Bits<N>` is stored as `⌈N/64⌉` little-endian words: `words()` and
`from_words()` reach every bit, `to_u64()` returns the value when it fits, and
`get()` returns it as a `u64` only for N ≤ 64 (otherwise a compile-time error).
Generic code over a width states `where Width<N>: Storage`; concrete widths
need nothing. Addition/subtraction wrap at N bits in every build profile.
Comparisons are unsigned; shifts that shift all bits out return zero, including
shift counts ≥ N. `bool` and `Bits<1>` have different
logical types despite equal physical widths. Conditionals select corresponding
leaves and require the same logical type on both branches.

Each immutable local names an expression value. Immutable shadowing is supported:
a new binding refers to a new expression; it does not mutate the prior value.
Every returned struct literal must provide every field exactly once. Fields may
be written in any source order and support shorthand; schema order follows the
struct declaration. Returning an existing aggregate or selecting complete
aggregates is legal. There are no implicit holds or default output definitions:
return an input field explicitly to preserve it.

A state machine is a pure next-state function. Its input struct can contain
current state, external inputs and instruction/control values; its output is a
complete next-state struct. The surrounding HDL module owns registers, reset,
clock domains, synchronization, validity and pads. Calling the function twice
composes transformations; it does not introduce two hardware cycles.

### Multiplication and signed values

`Bits<N>` is unsigned, but a value can be read as two's complement by the
operations that need a sign:

| Expression | Result | Meaning |
|---|---|---|
| `a * b` | `Bits<N>` | the product, wrapping at N bits (the same bits signed or unsigned) |
| `a.mul_wide::<M, O>(b)` | `Bits<O>`, `O = N + M` | the full unsigned product of `a: Bits<N>` and `b: Bits<M>` |
| `a.smul_wide::<M, O>(b)` | `Bits<O>`, `O = N + M` | the full product of both read as two's complement |
| `a.slt(b)`, `sle`, `sgt`, `sge` | `bool` | `<`, `<=`, `>`, `>=` of both read as two's complement |
| `a.sar(k)` | `Bits<N>` | `>>` by a literal `k`, filling with copies of the top bit (divides by 2^k, rounding down) |

The full products never wrap, so a fixed-point multiply is a product and a
slice: a 25-bit price difference times an 18-bit weight in Q2.16 is
`d.smul_wide::<18, 43>(w).slice::<27>(16)`.

The IR has one multiply node, `Operation::Mul { a, b, signed }`: operands of any
widths, each extended to the node's width (sign-extended when `signed`), and
the product modulo 2^width. The Verilog is `n_a * n_b` or
`$signed(n_a) * $signed(n_b)`, sized by the assignment. A zero constant operand
folds the product to zero. Signed comparisons and `sar` are built from the
existing nodes (a sign-bit mux, and a concatenation of fill bits with a slice).

On a 7-series FPGA a 25×18 signed product is one DSP48E1. Registers on its
operands and product are absorbed into the DSP's own registers when nothing
else uses them; wider products chain several DSPs (48×32 takes six). A
`#[logic]` function stays combinational, so the registers come from the
surrounding module or `system!`, as for any other logic, or from a
[pipelined unit](#pipelined-units).

## Arrays

`[T; N]` is a value of N elements, flattened element by element (ports are
named `field_0_...`, or `root_0` for a whole-array input or output).

- `t[2]` selects with an integer literal and builds no logic.
- `t.at(i)` selects with a run-time `Bits<K>` index through a binary mux tree
  on the low index bits. Indexes at or past N read the zero value
  (`LogicValue::zeroed`: all bits clear, enums at their first variant), so
  reads are total. A `set` or `at` whose index is a constant (for example a
  `fold` index) selects directly with no muxes.
- `t.set(i, v)` returns a copy with element `i` replaced; out-of-range writes
  change nothing. Hardware compares `i` with each reachable element index.
- `[a, b, c]` and `[v; N]` (literal N) build arrays.
- `==` and `!=` compare whole arrays and structs leaf by leaf.

`at` and `set` are the `LogicArray` trait's methods, so the native function
behaves identically; `#[logic]` imports the trait into the function body.

## Enums

`#[derive(LogicValue)]` on a fieldless enum stores the discriminant in the
fewest bits that hold the largest one. Discriminants are implicit (previous
plus one) or integer literals. The first variant is the zero value, so put the
reset state first when a register resets to zero. Enums support only
comparisons; arithmetic and bit methods are type errors. Enum variants and
`const` items of any `LogicValue` type are constants in logic bodies.
Generated Verilog names each enum port's encoding in a comment.

`match` works on a scalar (enum, `Bits` or `bool`) with constant patterns:
`Enum::Variant` paths, `const` names, `true`/`false`, `|` alternatives and `_`.
Arms are tried in source order, as in Rust; rustc's exhaustiveness check on the
native function lets the last arm act as the default. `matches!(value,
pattern)` is the same as `match value { pattern => true, _ => false }`. Guards
are unsupported. The graph is simplified while it is built:

- An operation on constants is a constant, and an identical operation on the
  same operands is the same node.
- A mux with a constant condition is the selected value. A mux between equal
  operands, or with a don't-care operand (see
  [Enums with fields](#enums-with-fields)), is the other operand.
- A mux of a concatenation is split at the concatenation's boundary, so each
  part folds on its own.
- A slice of a concatenation or of a slice is a slice of the underlying part,
  and adjacent slices of one value concatenate back to a single slice.

When the arms test distinct constants (distinct variants, or distinct `const`
or `true`/`false` values), at most one arm can match, and the lowering uses
that:

- **Last arm with a pattern (not `_`):** exactly one arm matches any valid
  value. Each result leaf starts from the constant most arms produce and adds a
  mux only for the arms that differ. A scrutinee with an invalid encoding,
  which native Rust cannot represent, then gives an unspecified value rather
  than the last arm's.
- **Last arm is `_`:** an earlier arm whose leaf equals the `_` arm's constant
  leaf needs no mux.

Overlapping patterns keep the plain priority chain.

### Enums with fields

An enum with at least one variant that has fields (`Load { addr: Bits<3> }`,
`Store(Bits<3>, Mode)`) also derives `LogicValue`, with `Schema::Tagged`. It is
not a leaf. It is stored as a tagged union with two leaves:

- `tag`: the discriminants of all variants, encoded exactly like a fieldless
  enum, with the same rules for implicit and literal discriminants;
- `payload`: `Bits<P>`, where `P` is the total leaf width of the widest
  variant. The current variant's field leaves are packed into it from bit 0
  upwards, in declaration order. Tuple fields are named `0`, `1`, ...
  Nested structs, arrays and enums, including other enums with fields, pack
  their own leaves recursively. There is no `payload` leaf when no variant has
  any bits, and `P` may be at most 4096.

Ports therefore look like `cmd_tag` and `cmd_payload`. Generated Verilog names
the tag encoding and the payload layout of each port in a comment, for example
`// i_cmd_payload: Cmd payload { Nop, Load { addr [2:0], wide [3:3] }, ... }`.
`Schema::variant_layout` returns the same layout as data. Variants share the
payload's bits, so a register holding the enum is as wide as the tag plus the
widest variant. Fields with the same position in several variants, such as a
shared first field, sit at the same bits, and moving them from one variant to
another needs no logic. There is no niche filling: the tag is always its own
leaf.

**Don't-care bits.** The payload bits above the current variant's fields
mean nothing. Generated logic may leave any value there, and it never reads
them:

- `match` and variant patterns test only the tag.
- `==`/`!=` compare like Rust's derived `PartialEq`: the tags, then the current
  variant's fields (recursively, so nested padding is ignored too).
- `LogicValue::read` ignores these bits. It still checks the tag, the payload
  width and each field's own encoding.
- `LogicValue::flatten` writes zeros there, so a native value has one encoding.
  Hardware outputs may have others for the same value. Raw leaves (`BitVec`,
  `Vec<BitVec>`) therefore compare and hash as bit patterns, padding included,
  and bitlogic does not rewrite them. To compare values, decode them with
  `read` and use the type's own `==`, or mask the leaves with
  `Schema::valid_bits`. `Schema::encode_variant` and `Schema::decode_variant`
  build and take apart the leaves directly.

Ordering comparisons and bit operations are `ir()` errors. Values supplied to
generated HDL input ports need a valid tag and valid field encodings, just as a
fieldless enum port must hold a valid discriminant. Their padding may be
anything. The zero value is the first variant with zero fields, and padding
zero. A `system!` register needs it to be all zero bits.

In `#[logic]` bodies:

- **Construction:** brace variants `Cmd::Load { addr, wide: true }`, tuple
  variants `Cmd::Store(a, Mode::High)` and unit variants `Cmd::Nop`. Rust's
  type checker decides whether a brace path names a struct or a variant. A call
  whose last path segment starts with an uppercase letter is treated as a
  tuple-variant constructor, which follows Rust naming conventions.
- **`match` patterns:** `Cmd::Load { addr, wide: w, .. }`, `Cmd::Store(a, _)`,
  `Cmd::Store(..)` and unit variants, alongside the constant patterns above.
  Field subpatterns must be plain names or `_`. `..` may only end a tuple
  pattern. Bindings may not appear in `|` alternatives. A pattern tests only the
  tag. Bound fields are wires of the scrutinee, and their values only matter
  when that arm is selected.
- **Comparisons:** `value == Cmd::Nop`, or `==` with any value whose tag is a
  constant, compares only the tag and that variant's fields. Two run-time
  values compare their tags and, for each variant, that variant's fields
  under its tag.

## Fixed-capacity sets and maps

`fixed_set!` and `fixed_map!` generate a module with concrete types and
`#[logic]` operations for one key type, value type and capacity, so each
container is declared in one line instead of written by hand:

```rust
bitlogic::fixed_set!(pub mod flows: Bits<43>, capacity = 16);
bitlogic::fixed_map!(pub mod peers: Bits<32> => Bits<48>, capacity = 16);

let s = flows::insert(state.flows, enable, key);
let hit = peers::get(state.peers, ip); // hit.hit, hit.value
```

The module contains `Entry {valid, key[, value]}`, `Set` or `Map`
`{entries: [Entry; N], next}`, `Found {hit, index}` and, for maps,
`Lookup {hit, value}`, plus `CAPACITY`, `Key`, `Value`, `Index` and `Count`
(`Bits` just wide enough for a slot index and for `N`). Keys may be any
`LogicValue`, including structs; they are compared leaf by leaf. Paths in the
key and value types resolve from inside the generated module, which imports
`super::*`, so use names in scope or `crate::` paths. The zero value is empty.

| Operation | Behavior |
|---|---|
| `find(m, key) -> Found` | first slot holding `key` |
| `contains(m, key) -> bool` | `find(m, key).hit` |
| `get(m, key) -> Lookup` | value for `key` (maps; `value` is meaningful only when `hit`) |
| `insert(m, enable, key[, value]) -> M` | add `key`; maps also overwrite a present key's value |
| `update(m, enable, key, value) -> M` | overwrite a present key only (maps) |
| `remove(m, enable, key) -> M` | vacate `key`'s slot |
| `count(m) -> Count` | occupied slots |
| `store(m, found, [refresh,] add, key[, value]) -> M` | write given an earlier `find`, so callers share one lookup and choose refresh/add policy |

A new key fills the first vacant slot; when none is vacant the slot at `next`
is replaced and `next` advances (wrapping at `N`). Without removals that
replaces the oldest addition first. All operations are combinational: a lookup
costs one key comparator per slot plus a priority chain, and an update costs a
compare-and-mux per slot, so the cost grows linearly with capacity. Capacity
is 1 to 256.

## Supported Rust subset

- Ordinary non-generic functions with named owned inputs and an explicit owned
  output type, all implementing `LogicValue`. Only single-input functions get
  `ir()`; functions with zero or several inputs are helpers for other `#[logic]`
  functions.
- Calls to other `#[logic]` functions by path (`helper(a, b)`,
  `module::helper(x)`), including inside conditionals and folds. Each call
  inlines the callee's graph at the call site: no ports, registers or sharing
  are added, and two calls produce two copies of the logic. Callees must be
  `#[logic]` functions; calling an ordinary Rust function is a compile error.
  Recursion is rejected by `ir()` at a nesting depth of 64.
- Initialized immutable `let` bindings, optional value type annotations, immutable
  shadowing, nested value blocks and a trailing expression.
- Named field access and complete named struct construction, including qualified
  type paths and shorthand fields; construction of enum variants with fields
  (see [Enums with fields](#enums-with-fields)).
- `if`/`else if`/`else` expressions, including whole-struct selection, and
  `match` as described under [Enums](#enums).
- Arrays, literal and run-time indexing, `set`, array literals and repeats as
  described under [Arrays](#arrays).
- Paths to enum variants and `const` items, as constants.
- Bitwise `&`, `|`, `^`, `!`; `+`, `-`, `*`; `==`, `!=`, `<`, `<=`, `>`, `>=`.
- `mul_wide`, `smul_wide`, `slt`, `sle`, `sgt`, `sge` and `sar` as described
  under [Multiplication and signed values](#multiplication-and-signed-values).
- Boolean `&&`, `||`; expressions in this subset have no side effects.
- Literal `<<`/`>>` counts, boolean literals, `bits::<N>(integer literal)`.
- `value.bit(index)`, `value.slice::<N>(offset)`, and
  `high.concat::<LowWidth, TotalWidth>(low)`, with literal indices and widths.
- Explicit bounded immutable folds as described below.

Mutation, references, assignment statements, compound assignments, early returns,
struct-update syntax, destructuring outside `match`, tuples, nested patterns,
`if let`, match guards,
run-time `[]` indexing, calls to non-`#[logic]` functions,
ordinary loops, async/unsafe functions, and arbitrary leaf types are unsupported.
`LogicValue` derives require nonempty named non-generic structs or nonempty
non-generic enums. Rust names are
resolved normally for schemas; recognized DSL operations such as `bits` are
syntactic and must use the supplied definitions rather than shadowed substitutes.
Some errors (including dynamic schema validation, flattening name collisions and
invalid bit-method bounds) are reported by `ir()`, while syntax errors and native
Rust type/field errors are compile-time diagnostics.

## Bounded immutable folds

`LogicValue::fold` expresses a pure recurrence and is the only supported closure
form. The method takes one literal count and `|acc, index| expression`:

```rust
#[logic]
fn repeated(input: Bits<8>) -> Bits<8> {
    input.fold::<4>(|acc, index| acc + index)
}
```

The index is `Bits<8>`, ranging from zero through count minus one. The closure
returns the accumulator's logical type and may capture immutable outer values.
Count zero returns the initial accumulator; its body must still use supported
syntax. At most 256 iterations and 65,536 total lowering steps are allowed.
Folds expand into acyclic expressions; they do not synthesize iterative engines,
registers or elapsed time. Struct accumulators support decoder expansion without
mutable locals; the set and processor examples use this form.

## Schemas, evaluation and dependencies

`LogicValue::schema()` retains nested field names, nominal Rust type identity,
and distinct logical leaf kinds. `flatten()` visits fields depth-first in
struct-declaration order. `from_values()` reconstructs the native value, rejecting
wrong counts and out-of-width values rather than silently truncating them.

`Logic::schemas()` exposes input/output schemas. `evaluate_typed::<I,O>(input)`
checks both declared schemas, evaluates the scalar graph, and reconstructs `O`.
`evaluate(&[...])` remains available and follows schema leaf order. Leaf and port
values without a static width are `BitVec`s: unsigned values of up to 4096 bits
that compare equal to a `u64` of the same value, so `evaluate`, `flatten` and
`from_values` also accept and compare with plain `u64`s. Low-level
`Builder` graphs use port insertion order and do not carry aggregate schemas
unless explicitly attached and checked through `with_schemas()`.

`nodes()`, `inputs()` and `outputs()` expose the expression DAG. `NodeId::index()`
indexes nodes. Every node references earlier nodes; storage and feedback are
absent. `dependencies()` returns each output field path and its contributing input
field paths in input order, including conditional predicates. Dependencies are
structural, not minimal Boolean support: `x ^ x` still reports `x`.

## Composition

`first.then(&second)` requires equal intermediate nominal type and full schema
for structured graphs. Equal leaf widths or coincidentally equal field names
alone are insufficient. Composition preserves the outer input and output schemas.
A dedicated pure adapter function expresses deliberate rewiring between types.
An identity function `fn identity(value: State) -> State { value }` composes on
either side without changing values. Grouping composition does not change values.
No composition adds a register or clock cycle.

Two low-level unstructured graphs can still compose by port count and width.
Mixing structured and unstructured interfaces is rejected; an explicit schema
attachment is required. Runtime `TypeId` identifies nominal types inside one Rust
program; the schema is not a portable persisted ABI or a serialized type identity.

The NetKAT inspiration remains a small compositional language with explicit
semantics. Bitlogic does not implement NetKAT packet history, union, Kleene star
or its complete equational decision procedure. Pure state functions do not need
a state monad: state is explicit in their input and output values.

## Clocked systems

`system!(pub mod name { ... })` declares how single-input step functions,
memories and pins connect on one clock:

```rust
bitlogic::system! {
    pub mod pipe {
        param cfg: Cfg = Cfg { step: bits(3), enable: true };
        input ctl: Ctl, registered;          // boundary register
        input poke: Bits<8>;                 // straight to the units
        ram ring: Bits<8>, addr_bits = 4;
        rom scale: Bits<6>, addr_bits = 3, contents = SCALES.map(bits::<6>);
        unit prod: ProdState = produce(ProdIn { state, cfg, ctl, rd: cons.state.rd });
        unit cons: ConsState = consume(ConsIn {
            state, wr: prod.state.wr, data: ring.rdata, scale: scale.rdata, poke,
        });
        write ring(prod.we, prod.waddr, prod.wdata);
        read ring(cons.raddr);
        read scale(cons.rom_addr);
        output sum: Bits<16> = cons.state.sum;
        output odd: bool = cons.odd, registered;
    }
}
```

A unit's input and output structs both have a `state` field of the declared
type, held in a register that resets to zero. The state may be a struct, an
enum or a single scalar such as `Bits<96>`. In a connection, `u.state.f` is
unit `u`'s registered state, `u.f` one of its other (combinational) outputs,
`m.rdata` a memory's registered read data, and a param or input is named
directly, with optional `.field` paths. Unit inputs take only registered values
(params, inputs, state, read data), so units can be evaluated in any order and
no combinational path crosses between them. Combinational unit outputs drive
memory ports and `registered` outputs. Memories are read-first with one cycle of
read latency; a ROM has a read port only.

The macro generates, inside module `pipe`:

- `Inputs` and `Outputs` structs, one field per pin group;
- `Core<E: Evaluate = Native>` with one public field per param, unit state,
  boundary register and memory (`Memory { cells, rdata }`). `Core::new` takes
  the evaluator and the params and applies two reset edges with zero inputs;
  `clock(inputs)` and `reset(inputs)` advance one edge and return the outputs
  after it. `Native` calls the Rust functions; `Interpreted` evaluates their
  IR graphs;
- `design()`, the declaration as data, and `verilog()`, the files: the core
  module named after the system, a registered wrapper `<unit>_unit` per unit
  (state register plus the `<unit>_unit_logic` combinational step), a
  `<rom>_rom` per ROM and `bitlogic_sdp_ram`.

In the core module, params become Verilog parameters named by their leaf path
in upper case (`CFG_STEP`), and inputs and outputs become ports named by their
leaf path (`ctl_go`, `sum`). The connection struct literal is pasted into the
cycle model, so rustc checks connection types; `verilog()` checks widths and
schemas again and rejects unconnected fields.

### Pipelined units

`unit u: S = step(In { ... }), pipeline;` keeps `step` an ordinary Rust
function and lets rewrite rules add registers to it in hardware. The rules are
structural, with no timing model. The one rule so far puts a register on each
operand of every multiply (constants excepted) and one on its product: on a
7-series FPGA those are a DSP48E1's input and output registers.

A unit is a loop, since its next state is computed from its state, so a
register inside the step function is only correct if the unit waits for it.
With `L` layers of registers on the deepest path to the next state, the unit
holds its state for `L` cycles before taking a next state that passes through
them. It holds only when it has to: following each path from a register to
the next state, through the muxes on the way, gives the **stall condition**,
the case in which some such path is selected. A unit that uses a product in
one phase holds only in that phase:

```text
mac: 2 multiplies, 6 registers, latency 2; holds up to 2 cycles while (state.phase == 1)
```

That is `design()?.pipeline_reports()`, one line per pipelined unit. In the
hardware, a counter holds the state register while the condition is set, for
at most `L` cycles; the pipeline registers have no reset or enable. The cycle
model holds the same way (`Core::__stall_<unit>`), so `Native`,
`Interpreted` and the Verilog stay cycle for cycle equal. The analysis runs
once per step function.

Holding is correct only if nothing a register depends on changes meanwhile,
so `verilog()` refuses a unit where:

- a register depends on an input other than `state` and params (another
  unit's state, read data or a system input can change while the unit
  holds);
- an output other than `state` depends on a register (it would be read before
  the register filled).

While the unit holds, its other outputs keep the held state's values, so a
write enable that is set in a held cycle writes again each cycle. A unit
without multiplies is unchanged: same Verilog, never holds.

### Ports

A `port` connects the system to something outside it whose timing the system
does not control, such as a DRAM controller behind clock-domain-crossing
FIFOs. It has a request channel out and a response channel in, each with a
valid/ready handshake that transfers on an edge where both are high:

```rust
bitlogic::system! {
    mod rmw {
        port mem: MemReq => MemRsp;
        unit client: ClientState = client(ClientIn {
            state,
            req_ready: mem.req_ready,     // the other side takes a request
            rsp_valid: mem.rsp_valid,     // a response is offered
            rsp: mem.rsp,                 // ... and this is it
        });
        request mem(client.req_valid, client.req);
        respond mem(client.rsp_ready);
    }
}
```

`port.req_ready`, `port.rsp_valid` and `port.rsp` (with optional `.field`
paths) are this cycle's values from outside, unregistered, so a unit can
answer them in the same cycle: `request port(valid, data)` and
`respond port(ready)` are driven by any connection, usually a unit's
combinational outputs. A port signal cannot drive a port, and drives an
`output` only through a register. The other side must not make
`req_ready`, `rsp_valid` or `rsp` depend combinationally on the system's
`req_valid`, `req` or `rsp_ready` (FIFO flags and registered outputs do
not), which keeps the handshake free of combinational loops.

In Verilog the core gains the ports `<port>_req_valid`, `<port>_req_<leaf>`
and `<port>_rsp_ready` (outputs, combinational) and `<port>_req_ready`,
`<port>_rsp_valid` and `<port>_rsp_<leaf>` (inputs). In the cycle model,
`Core` has a field `<port>: Box<dyn bitlogic::system::Responder<Req, Rsp>>`
that plays the other side: before each edge the model asks it what it offers
(`req_ready`, `rsp`), evaluates the units, then tells it what transferred
(`edge`). A new `Core` starts with `Unconnected` (never ready, never
responds). `bitlogic::system::InOrder` is a ready-made responder: up to
`depth` requests in flight, each served by a closure when accepted (so a
memory model sees requests in order) and answered `latency()` edges later, in
order; requests without a response (writes) keep their place. `Outputs` has
a `PortCycle` per port with all six signals of the cycle before the edge,
which is what a test replays against the generated Verilog.

### Clock domains

With `clock` items, a system has several clock domains. Each unit, input,
output, `port` and memory port names its domain with `in`; a RAM written in
one domain and read in another is a dual-clock RAM. Values move between
domains only through a `cross`:

```rust
bitlogic::system! {
    pub mod fifo {
        clock a, period = 10.0;             // nominal period in ns (optional)
        clock b, period = 13.7;
        input ctl: Ctl in a, registered;
        ram ring: Bits<8>, addr_bits = 4;   // written in a, read in b
        unit prod: ProdState in a = produce(ProdIn { state, ctl, rd: rd_a });
        unit cons: ConsState in b = consume(ConsIn { state, wr: wr_b, data: ring.rdata });
        cross wr_b: Bits<4> = gray(prod.state.wr) into b;
        cross rd_a: Bits<4> = gray(cons.state.rd) into a;
        write ring(prod.we, prod.waddr, prod.wdata) in a;
        read ring(cons.raddr) in b;
        output sum: Bits<16> in b = cons.state.sum;
    }
}
```

The macro checks that every connection stays in its consumer's domain (a
param is in every domain): a unit, output or memory port that names a value
of another domain is a compile error pointing at it. A crossing carries a
unit's state or an input (one `Bits` or `bool` value) from its domain into
another:

- `gray(...)`: a counter that moves by at most one (up or down, wrapping) per
  source edge, at most 64 bits. It crosses Gray-coded, so the destination
  sees either the value before a move or the one after it.
- `level(...)`: bits synchronized one by one, for flags and quasi-static
  values; while a multi-bit value changes, its bits may arrive a
  destination edge apart.

Either is a register in the source domain, then two synchronizer flip-flops
in the destination domain, which sees the second: a value arrives one source
edge and two destination edges after it is set.

The cycle model gains `enum Clock` (`Clock::ALL`, `name`, `period_ns`,
`schedule(ppm)`); `Core::new` resets every domain twice, `tick(clock,
inputs)` advances one edge of one domain and `reset(clock, inputs)` one with
that domain's reset asserted. Drive the edges in time order, for example
from `bitlogic::system::Schedule` (free-running clocks with periods and
phases in femtoseconds; `Schedule::period_fs(ns, ppm)`). Each crossing is
modelled register for register (`Core::__cross_<name>`); a Gray source that
moves by more than one in a source edge panics, naming the crossing.
`Core::cdc_noise` (`CdcNoise::new(seed, percent)`, off by default) samples a
value that changed since the destination's last edge, bit by bit at random,
as it was before the change, as a metastable first flip-flop may resolve: a
design that is correct under noise does not depend on when a crossing
resolves.

In Verilog the core has `clk_<clock>` and `rst_<clock>` ports (synchronous
resets, one per domain) instead of `clk` and `rst`; every unit, boundary
register, memory port and ROM is clocked in its domain, crossings use
`bitlogic_cdc_gray` / `bitlogic_cdc_level` (`bitlogic_cdc.v`, plain Verilog
with `ASYNC_REG`), and dual-clock RAMs `bitlogic_dc_ram`. `verilog()` also
returns `<system>_cdc.tcl`, to read with `read_xdc -unmanaged` once the
clocks exist: a datapath-only maximum delay of the faster clock's period
from each crossing's source register to its first flip-flop (with a bus
skew for a multi-bit Gray crossing) and from each dual-clock RAM's cells to
its read register. The system's domains must not be in asynchronous clock
groups, which would override these constraints.

## HDL integration

Ports use `i_`/`o_` prefixes. A nested path `state.counter` becomes `i_state_counter`
or `o_state_counter`. Struct roots omit the Rust argument name; a scalar input
uses its argument name, and a scalar output uses `result`. One-bit ports use
`[0:0]`. Flattening collisions such as `a_b` and `a.b` are errors. Raw Rust field
identifiers lose the `r#` prefix before HDL naming. All names are validated.

`rename_ports()` provides a checked, order-preserving external HDL adapter;
logical schemas and dependency paths remain unchanged. The processor uses it to
preserve its existing wrapper ABI while accepting a nested `StepInput` in Rust.
Intermediate nodes are continuous-assignment wires, with no inferred registers.
Electrical pad modes, voltage domains and clock-domain safety remain external.

## Bounded set example

```sh
cargo run --locked -p bitlogic --example set
```

`set_step(SetInput) -> SetOutput` represents keys `0..=7` with `Bits<8>`:

| Field | Meaning |
|---|---|
| Input `current` | Current membership bitmap |
| Input `key` | `Bits<3>` key |
| Input `operation` | `Bits<2>`: contains=0, insert=1, remove=2, clear=3 |
| Output `next` | Complete next membership bitmap |
| Output `present` | Key membership in the current bitmap, including during clear |

The immutable decoder fold selects the key's mask. Inserting existing keys or
removing absent keys preserves membership. Every key/operation bit pattern is
valid. Caller-owned storage is unchanged; this is not dynamically allocated
storage or an arbitrary-key collection. Two tests compare native/IR and Icarus
against a `BTreeSet` oracle for all 8,192 input combinations.

## Verification and migration

[Verification](verification.md) records the independent native/IR/HDL tests,
structured composition/dependency checks, diagnostics and processor regressions.
The [migration contract and audit](pure_functions.md) records the baseline and
implementation gates. The mutation-based frontend has been retired. Earlier RHDL
processor results are separately historical; see [project history](history.md).
Functional checks do not establish FPGA fit, electrical correctness or ASIC signoff.

All runnable examples generate HDL and diagrams without arguments; see the
[artifact guide](example_artifacts.md) for files and temporary-folder behavior.
