//! `fixed_set!` and `fixed_map!`: random operation sequences checked against
//! an independent slot model, then the same steps through the IR and Icarus.
mod support;
use bitlogic::{Bits, LogicValue, bits, fixed_map, fixed_set, logic};
use support::{Rng, check_hdl};

// Five slots: not a power of two, so `next` wraps by comparison.
fixed_set!(mod flows: Bits<43>, capacity = 5);
// Struct keys work too; four slots wrap for free.
#[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
pub struct Endpoint {
    host: Bits<8>,
    port: Bits<4>,
}
fixed_map!(pub mod routes: Endpoint => Bits<20>, capacity = 4);

#[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
enum Op {
    Insert,
    Remove,
    Update,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
struct SetStep {
    set: flows::Set,
    op: Op,
    enable: bool,
    key: Bits<43>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
struct SetOut {
    set: flows::Set,
    had: bool,
    count: flows::Count,
}

#[logic]
fn set_step(s: SetStep) -> SetOut {
    SetOut {
        set: match s.op {
            Op::Insert | Op::Update => flows::insert(s.set, s.enable, s.key),
            Op::Remove => flows::remove(s.set, s.enable, s.key),
        },
        had: flows::contains(s.set, s.key),
        count: flows::count(s.set),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
struct MapStep {
    map: routes::Map,
    op: Op,
    enable: bool,
    key: Endpoint,
    value: Bits<20>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, LogicValue)]
struct MapOut {
    map: routes::Map,
    got: routes::Lookup,
    count: routes::Count,
}

#[logic]
fn map_step(s: MapStep) -> MapOut {
    MapOut {
        map: match s.op {
            Op::Insert => routes::insert(s.map, s.enable, s.key, s.value),
            Op::Remove => routes::remove(s.map, s.enable, s.key),
            Op::Update => routes::update(s.map, s.enable, s.key, s.value),
        },
        got: routes::get(s.map, s.key),
        count: routes::count(s.map),
    }
}

/// Independent model: `Some((key, value))` per slot plus the replacement
/// pointer. Adds fill the first vacant slot, else replace `next`.
#[derive(Clone, Debug, PartialEq)]
struct Model {
    slots: Vec<Option<(u64, u64)>>,
    next: usize,
}

impl Model {
    fn new(n: usize) -> Self {
        Model {
            slots: vec![None; n],
            next: 0,
        }
    }
    fn position(&self, key: u64) -> Option<usize> {
        self.slots
            .iter()
            .position(|s| matches!(s, Some((k, _)) if *k == key))
    }
    fn get(&self, key: u64) -> Option<u64> {
        self.position(key).map(|i| self.slots[i].unwrap().1)
    }
    fn count(&self) -> u64 {
        self.slots.iter().flatten().count() as u64
    }
    fn apply(&mut self, op: Op, key: u64, value: u64) {
        match (op, self.position(key)) {
            (Op::Insert | Op::Update, Some(i)) => self.slots[i] = Some((key, value)),
            (Op::Insert, None) => {
                if let Some(i) = self.slots.iter().position(Option::is_none) {
                    self.slots[i] = Some((key, value));
                } else {
                    self.slots[self.next] = Some((key, value));
                    self.next = (self.next + 1) % self.slots.len();
                }
            }
            (Op::Remove, Some(i)) => self.slots[i] = None,
            (Op::Update | Op::Remove, None) => {}
        }
    }
}

fn pick_op(rng: &mut Rng) -> Op {
    match rng.next() % 8 {
        0..=3 => Op::Insert,
        4..=5 => Op::Remove,
        _ => Op::Update,
    }
}

#[test]
fn set_matches_model_ir_and_hdl() {
    let ir = set_step::ir().unwrap();
    let mut rng = Rng(0x005e_70ff_10e5);
    let mut set = flows::Set::zeroed();
    let mut model = Model::new(flows::CAPACITY);
    let mut cases = Vec::new();
    // Eight wide keys over five slots: hits, misses and replacement.
    let keys: Vec<u64> = (0..8).map(|k| (k << 40) | 0x123_4567 | k).collect();
    for step in 0..3_000 {
        let key = keys[(rng.next() % 8) as usize];
        let op = pick_op(&mut rng);
        let enable = !rng.next().is_multiple_of(6);
        let input = SetStep {
            set,
            op,
            enable,
            key: bits(key),
        };
        let out = set_step(input);
        assert_eq!(out.had, model.position(key).is_some(), "step {step}");
        assert_eq!(out.count.get(), model.count(), "step {step}");
        if enable {
            // Sets ignore Update's value; it inserts like Insert.
            let op = if op == Op::Update { Op::Insert } else { op };
            model.apply(op, key, 0);
        }
        assert_eq!(out.set.next.get() as usize, model.next, "step {step}");
        for (e, m) in out.set.entries.iter().zip(&model.slots) {
            assert_eq!(
                e.valid.then_some(e.key.get()),
                m.map(|(k, _)| k),
                "step {step}"
            );
        }
        assert_eq!(ir.evaluate_typed::<_, SetOut>(input).unwrap(), out);
        if step % 5 == 0 {
            cases.push((input.flatten(), out.flatten()));
        }
        set = out.set;
    }
    check_hdl(&ir, "set_step", &cases);
}

#[test]
fn map_matches_model_ir_and_hdl() {
    let ir = map_step::ir().unwrap();
    let mut rng = Rng(0x0000_abc0_ffee);
    let mut map = routes::Map::zeroed();
    let mut model = Model::new(routes::CAPACITY);
    let mut cases = Vec::new();
    let key_of = |e: Endpoint| e.host.get() << 4 | e.port.get();
    for step in 0..3_000 {
        let k = rng.next() % 7;
        let key = Endpoint {
            host: bits(0x40 + k),
            port: bits(k * 3),
        };
        let op = pick_op(&mut rng);
        let enable = !rng.next().is_multiple_of(6);
        let value = rng.next() & 0xf_ffff;
        let input = MapStep {
            map,
            op,
            enable,
            key,
            value: bits(value),
        };
        let out = map_step(input);
        let want = model.get(key_of(key));
        assert_eq!(out.got.hit, want.is_some(), "step {step}");
        if let Some(v) = want {
            assert_eq!(out.got.value.get(), v, "step {step}");
        }
        assert_eq!(out.count.get(), model.count(), "step {step}");
        if enable {
            model.apply(op, key_of(key), value);
        }
        assert_eq!(out.map.next.get() as usize, model.next, "step {step}");
        for (e, m) in out.map.entries.iter().zip(&model.slots) {
            let got = e.valid.then_some((key_of(e.key), e.value.get()));
            assert_eq!(got, *m, "step {step}");
        }
        assert_eq!(ir.evaluate_typed::<_, MapOut>(input).unwrap(), out);
        if step % 5 == 0 {
            cases.push((input.flatten(), out.flatten()));
        }
        map = out.map;
    }
    check_hdl(&ir, "map_step", &cases);
}

#[test]
fn widths_follow_capacity() {
    use bitlogic::Schema;
    fixed_set!(mod one: bool, capacity = 1);
    fixed_set!(mod full: Bits<16>, capacity = 256);
    let width = |s: Schema| match s {
        Schema::Bits(w) => w,
        _ => unreachable!(),
    };
    assert_eq!(width(<flows::Index as LogicValue>::schema()), 3);
    assert_eq!(width(<flows::Count as LogicValue>::schema()), 3);
    assert_eq!(width(<routes::Index as LogicValue>::schema()), 2);
    assert_eq!(width(<routes::Count as LogicValue>::schema()), 3);
    assert_eq!(width(<one::Index as LogicValue>::schema()), 1);
    assert_eq!(width(<one::Count as LogicValue>::schema()), 1);
    assert_eq!(width(<full::Index as LogicValue>::schema()), 8);
    assert_eq!(width(<full::Count as LogicValue>::schema()), 9);
    // A full 256-slot set replaces round-robin, wrapping `next` at 8 bits.
    let mut s = full::Set::zeroed();
    for k in 0..300u64 {
        s = full::insert(s, true, bits(k));
    }
    assert_eq!(full::count(s).get(), 256);
    assert_eq!(s.next.get(), 44);
    assert!(full::contains(s, bits(299)) && !full::contains(s, bits(43)));
    let o = one::insert(one::insert(one::Set::zeroed(), true, false), true, true);
    assert!(one::contains(o, true) && !one::contains(o, false));
}

#[test]
fn find_reports_the_first_matching_slot() {
    // Unreachable through the operations (a key is stored once), but `find`
    // documents first-match priority for hand-built states.
    let mut s = flows::Set::zeroed();
    for slot in [1, 3] {
        s.entries[slot] = flows::Entry {
            valid: true,
            key: bits(7),
        };
    }
    let want = flows::Found {
        hit: true,
        index: bits(1),
    };
    assert_eq!(flows::find(s, bits(7)), want);
    assert_eq!(find_seven(s), want);
    assert_eq!(
        find_seven::ir()
            .unwrap()
            .evaluate_typed::<_, flows::Found>(s)
            .unwrap(),
        want
    );
}

#[logic]
fn find_seven(s: flows::Set) -> flows::Found {
    flows::find(s, bits::<43>(7))
}
