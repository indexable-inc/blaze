//! What the solver costs, measured (solve.md, "Cost"): goals per second for ground and generic
//! goals, cold (a fresh solver per goal) and warm (the canonical cache); a 10,000-obligation
//! fulfillment loop with staggered unifications under wake lists and under a rescan; and the
//! prefilter's vector form against its scalar twin at bucket sizes 8, 64 and 1024.
//!
//! `cargo run --release -p blaze-solve --features testing --example cost`

#![feature(allocator_api)]

use std::hint::black_box;
use std::time::Instant;

use blaze_own::{Alloc, Vec as OVec};
use blaze_solve::assembly::{Clause, Goal};
use blaze_solve::eval::{Answer, Solver};
use blaze_solve::fulfill::{Fulfill, Rescan, Strategy, Wake};
use blaze_solve::prefilter::{self, Columns, Fingerprint, WIDTH};
use blaze_solve::term::{Polarity, Pred, TyKind, TypingMode, UintTy};
use blaze_solve::testing::parse::{DeclA, parse_file};
use blaze_solve::testing::run::PRELUDE;
use blaze_solve::testing::stagger::staggered;
use blaze_solve::testing::tys::TestTys;
use blaze_solve::testing::world::{World, def_of};
use blaze_solve::tys::Tys;

fn world() -> World {
    // core's `impl Clone for u32` (ints are not a builtin Clone candidate: "Implementations are
    // provided in core", structural_traits.rs).
    let text = format!("{PRELUDE}\nextern impl Clone for u32\n");
    let prelude = parse_file(&text).expect("prelude parses");
    let decls: Vec<(u32, &DeclA)> = prelude.decls.iter().map(|(l, d)| (*l, d)).collect();
    World::build(&decls).expect("prelude lowers")
}

/// `Vec<Vec<..<u32>>>` of depth `d`.
fn nested(tys: &mut TestTys, d: u32) -> blaze_solve::term::Ty {
    let vec = def_of("adt", "Vec");
    let mut t = tys.mk(TyKind::Uint(UintTy::U32)).expect("arena");
    for _ in 0..d {
        let l = tys.mk_list(&[t]).expect("arena");
        t = tys.mk(TyKind::Adt(vec, l)).expect("arena");
    }
    t
}

fn clone_goal(tys: &mut TestTys, t: blaze_solve::term::Ty) -> Pred {
    let args = tys.mk_list(&[t]).expect("arena");
    Pred::Trait { trait_: def_of("trait", "Clone"), args, polarity: Polarity::Positive }
}

fn goals_per_sec(label: &str, w: &World, tys: &mut TestTys, env_clauses: &[Clause], preds: &[Pred], cold: bool, alloc: Alloc<'_>) {
    let mut reads = OVec::new_in(alloc);
    let mut solver = Solver::new_in(alloc);
    let env = solver.env(tys, w, env_clauses, &mut reads).expect("env");
    let rounds = if cold { 1 } else { 20 };
    let t0 = Instant::now();
    let mut n = 0u64;
    for _ in 0..rounds {
        for &p in preds {
            if cold {
                solver = Solver::new_in(alloc);
            }
            let env = if cold { solver.env(tys, w, env_clauses, &mut reads).expect("env") } else { env };
            reads.clear();
            let a = solver.evaluate(tys, w, TypingMode::Analysis, Goal { env, pred: p, bound: 0 }, &mut reads).expect("solves");
            assert_eq!(a, Answer::Yes, "{label}: every goal holds");
            n += 1;
        }
    }
    let dt = t0.elapsed().as_secs_f64();
    println!(
        "{label:<40} {:>12.0} goals/s  ({n} goals, {:.1} ms; frames {}, cache hits {}, fast path {})",
        n as f64 / dt,
        dt * 1e3,
        solver.stats().frames,
        solver.stats().cache_hits,
        solver.stats().fast_path
    );
}

fn fulfill<S: Strategy>(label: &str, n: u32, waves: u32, alloc: Alloc<'_>) -> u64 {
    let mut s = staggered(n, waves, |i| (i.wrapping_mul(2_654_435_761) >> 7) % waves).expect("load builds");
    let mut solver = Solver::new_in(alloc);
    let mut reads = OVec::new_in(alloc);
    let env = solver.env(&mut s.tys, &s.world, &[], &mut reads).expect("env");
    let mut f: Fulfill<'_, S> = Fulfill::new_in(alloc);
    for &p in &s.goals {
        f.register(Goal { env, pred: p, bound: 0 }).expect("register");
    }
    let t0 = Instant::now();
    f.select(&mut solver, &mut s.tys, &s.world, TypingMode::Analysis, &mut reads).expect("select");
    let dt = t0.elapsed().as_secs_f64();
    println!(
        "{label:<40} {:>10.1} ms  evaluations {:>9}  pending after {}",
        dt * 1e3,
        f.evaluations,
        f.pending()
    );
    f.evaluations
}

fn prefilter_cost(len: usize, alloc: Alloc<'_>) {
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15 ^ len as u64;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let fps: Vec<Fingerprint> = (0..len)
        .map(|_| {
            let mut fp = [0u8; WIDTH];
            for b in fp.iter_mut() {
                // A quarter wildcards, the rest over 16 heads: a realistic mix of blanket and
                // concrete impls.
                let r = next();
                *b = if r % 4 == 0 { 0 } else { 1 + (r >> 8) as u8 % 16 };
            }
            fp
        })
        .collect();
    let mut g = [0u8; WIDTH];
    g[0] = 3;
    g[1] = 5;
    g[4] = 7;
    let mut cols = OVec::new_in(alloc);
    let stride = prefilter::columns_of(&fps, &mut cols);
    let c = Columns { bytes: &cols, stride, len: len as u32 };
    let iters = (4_000_000 / len.max(1)).max(1000);
    let mut out = OVec::new_in(alloc);
    let time = |f: &dyn Fn(&mut OVec<u32, Alloc<'_>>), out: &mut OVec<u32, Alloc<'_>>| {
        let t0 = Instant::now();
        for _ in 0..iters {
            out.clear();
            f(black_box(out));
        }
        t0.elapsed().as_secs_f64() * 1e9 / iters as f64
    };
    let s = time(&|o| prefilter::filter_scalar(black_box(&g), c, o), &mut out);
    let survivors = out.len();
    let v = time(&|o| prefilter::filter(black_box(&g), c, o), &mut out);
    assert_eq!(out.len(), survivors, "the vector form keeps what the scalar one keeps");
    println!(
        "prefilter bucket {len:>5}: scalar {s:>9.1} ns, simd {v:>9.1} ns, {:>5.1}x  ({survivors} survive)",
        s / v
    );
}

fn main() {
    // An entry that takes no grant yet (a plain `fn main`): the run's memory is one frame
    // taken from the OS here and handed down.
    let frame = blaze_mem::Frame::with_reserve(1 << 36, blaze_mem::Tag::Solver).expect("the entry's frame");
    let erased = blaze_own::Erased::new(&frame);
    let alloc = erased.handle();
    let w = world();
    let mut tys = TestTys::new();
    // Ground: `Vec^d<u32>: Clone` for 200 depths; each needs d+1 impl steps cold.
    let ground: Vec<Pred> = (0..200).map(|d| { let t = nested(&mut tys, d % 8); clone_goal(&mut tys, t) }).collect();
    goals_per_sec("ground, cold (fresh solver per goal)", &w, &mut tys, &[], &ground, true, alloc);
    goals_per_sec("ground, warm (canonical cache)", &w, &mut tys, &[], &ground, false, alloc);
    // Generic: `Vec^d<T>: Clone` with `T: Clone` in the env.
    let p = tys.mk(TyKind::Param(0)).expect("arena");
    let env = [Clause { pred: clone_goal(&mut tys, p), bound: 0 }];
    let vec = def_of("adt", "Vec");
    let generic: Vec<Pred> = (0..200)
        .map(|d| {
            let mut t = p;
            for _ in 0..d % 8 {
                let l = tys.mk_list(&[t]).expect("arena");
                t = tys.mk(TyKind::Adt(vec, l)).expect("arena");
            }
            clone_goal(&mut tys, t)
        })
        .collect();
    goals_per_sec("generic, cold (fresh solver per goal)", &w, &mut tys, &env, &generic, true, alloc);
    goals_per_sec("generic, warm (canonical cache)", &w, &mut tys, &env, &generic, false, alloc);

    println!();
    let a = fulfill::<Wake>("fulfill 10000, 100 waves, wake lists", 10_000, 100, alloc);
    let b = fulfill::<Rescan>("fulfill 10000, 100 waves, rescan", 10_000, 100, alloc);
    println!("rescan / wake evaluations: {:.1}x", b as f64 / a as f64);

    println!();
    for len in [8, 64, 1024] {
        prefilter_cost(len, alloc);
    }
}

/// The same report under the test harness (`cargo test --release --example cost`), for a lane
/// that may only run tests: `println!` is captured there, so the run is also asked to fail
/// with nothing else to say when `BLAZE_SOLVE_COST_SHOW` is set, which prints the captured lines.
#[test]
fn cost_report() {
    main();
    assert!(std::env::var_os("BLAZE_SOLVE_COST_SHOW").is_none(), "the cost report above (BLAZE_SOLVE_COST_SHOW is set)");
}
