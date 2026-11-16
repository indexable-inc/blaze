//! What one evaluation costs, on constants shaped like real library code (rule 67): each is
//! written as Rust text, lowered and checked once, its dependencies' records made, then
//! evaluated again and again with `value_of`, the builder's seam.
//!
//! The floor, stated before the run, is the sum of two costs every evaluation pays in turn:
//! - its record key (`instance_key` and `RecordKey::of`, two BLAKE3 hashes), measured here,
//!   beside it;
//! - 5 ns per step (one operation of the compiled code), what a register bytecode
//!   interpreter's dispatch costs; the steps are counted exactly, by the smallest step bound
//!   the evaluation passes.

use std::time::Instant;

use blaze_build::instance_key;
use blaze_consteval::fixture::World;
use blaze_consteval::{Answer, Codes, Intrinsic, Layouts, Limits, RecordKey, value_of};
use blaze_mem::{Frame, Tag};
use blaze_own::{Alloc, Erased};
use blaze_typeck::{ItemKey, List, PTys, PortableTys};

const STEP_FLOOR_NS: f64 = 5.0;

fn world() -> World {
    let mut w = World::new();
    let usize_ = w.fx.usize();
    w.intrinsic("size_of", 1, &[], usize_, Intrinsic::SizeOf);
    w.strukt("Config", &[("size", "usize"), ("align", "usize"), ("flags", "u32"), ("name", "&str")]);
    w.enum_("Level", Some("u8"), &[("Off", &[], Some("0")), ("Error", &[], None), ("Warn", &[], None), ("Info", &[], None)]);
    w.const_fn("next_pow2", &[("n", "u64")], "u64", "{ let mut p = 1u64; while p < n { p <<= 1; } p }");
    w.const_fn(
        "level_name_len",
        &[("l", "Level")],
        "usize",
        "{ match l { Level::Off => 3, Level::Error => 5, Level::Warn => 4, Level::Info => 4 } }",
    );
    w.konst("MAX", "usize", "1 << 20");
    w.konst("MASK", "u64", "!(0xFFFF_FFFF_FFFF_FFFFu64 >> 3)");
    w.konst("LEN", "usize", "4 * size_of::<u64>() + 1");
    w.konst("DEFAULT", "Config", "Config { size: 4096, align: 8, flags: 0b101, name: \"default\" }");
    w.konst("POW", "u64", "next_pow2(1000)");
    w.konst("LVL", "usize", "level_name_len(Level::Warn) + Level::Info as usize");
    w.stat(
        "CRC",
        "[u32; 256]",
        "{ let mut t = [0u32; 256]; let mut i = 0; while i < 256 { let mut c = i as u32; let mut k = 0; while k < 8 { c = if c & 1 == 1 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 }; k += 1; } t[i] = c; i += 1; } t }",
    );
    w.build();
    w
}

/// A unit: its ground table and layouts, which live as long as it (one per world here).
struct Unit {
    ground: PortableTys<'static>,
    layout: Layouts<Alloc<'static>>,
    codes: Codes<Alloc<'static>>,
    /// The task's frame each evaluation runs in.
    frame: Frame,
}

impl Unit {
    fn new() -> Unit {
        Unit {
            ground: PortableTys::new_in(Alloc::global()),
            layout: Layouts::new_in(Alloc::global()),
            codes: Codes::new_in(Alloc::global()),
            frame: Frame::new(Tag::Mir).expect("a frame"),
        }
    }
}

/// One evaluation in a task's frame, rolled back after it, as the builder runs one (rule 11):
/// the machine's memory and the record's bytes are bump allocations.
fn eval(w: &World, unit: &mut Unit, item: ItemKey, limits: Limits) -> bool {
    let Unit { ground, layout, codes, frame } = unit;
    frame.scope(|f| {
        let erased = Erased::new(f);
        matches!(value_of(w, layout, codes, item, List::EMPTY, ground, limits, erased.handle()), Ok(Answer::Done(_)))
    })
}

/// The exact step count: the smallest step bound the evaluation passes.
fn steps(w: &World, unit: &mut Unit, item: ItemKey) -> u64 {
    let (mut lo, mut hi) = (0u64, Limits::DEFAULT.steps);
    while lo + 1 < hi {
        let mid = lo + (hi - lo) / 2;
        if eval(w, unit, item, Limits { steps: mid, ..Limits::DEFAULT }) { hi = mid } else { lo = mid }
    }
    hi
}

fn main() {
    let mut w = world();
    for name in ["MAX", "MASK", "LEN", "DEFAULT", "POW", "LVL", "CRC", "Level_Off_DISC"] {
        w.eval(name).unwrap_or_else(|e| panic!("{name}: {e}"));
    }
    // The key floor: what computing one record key costs, in a frame as an evaluation is.
    let max = w.key("MAX");
    let code = blaze_consteval::ConstWorld::code_id(&w, max);
    let n = 200_000u32;
    let mut frame = Frame::new(Tag::Mir).expect("a frame");
    let t = Instant::now();
    let mut sink = 0u8;
    for _ in 0..n {
        sink ^= frame.scope(|f| {
            let erased = Erased::new(f);
            let k = instance_key(code, List::EMPTY, PTys::EMPTY, erased.handle()).expect("key");
            RecordKey::of(k).0.0[0]
        });
    }
    let key_ns = t.elapsed().as_nanos() as f64 / f64::from(n);
    println!("floor: one record key {key_ns:.0} ns (sink {sink})");
    println!("{:<10} {:>8} {:>12} {:>10} {:>10} {:>8}", "constant", "steps", "evals/s", "ns/eval", "floor ns", "ratio");
    let mut total = 0.0;
    let mut count = 0;
    let mut unit = Unit::new();
    // `COST_ONLY=<name>`: that row alone, run 20 times longer (for a profiler to attach).
    let only = std::env::var("COST_ONLY").ok();
    for name in ["MAX", "MASK", "LEN", "DEFAULT", "POW", "LVL", "CRC"] {
        if only.as_deref().is_some_and(|o| o != name) {
            continue;
        }
        let item = w.key(name);
        let s = steps(&w, &mut unit, item);
        let iters: u32 = (if s > 10_000 { 200 } else { 50_000 }) * if only.is_some() { 20 } else { 1 };
        let t = Instant::now();
        for _ in 0..iters {
            assert!(eval(&w, &mut unit, item, Limits::DEFAULT));
        }
        let ns = t.elapsed().as_nanos() as f64 / f64::from(iters);
        // Both are paid, one after the other: the floor is their sum (until 2026-11-16 it was
        // the larger of the two, which flattered constants of a few steps).
        let floor = key_ns + s as f64 * STEP_FLOOR_NS;
        total += 1e9 / ns;
        count += 1;
        println!("{name:<10} {s:>8} {:>12.0} {ns:>10.0} {floor:>10.0} {:>7.1}x", 1e9 / ns, ns / floor);
    }
    println!("mean evaluations per second {:.0}", total / f64::from(count));
}
