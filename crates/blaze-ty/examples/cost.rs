//! What the arena and the tables cost, against their floor; a measurement, not a test.
//!
//! Run (release): `cargo run --release -p blaze-ty --example cost`.
//!
//! The floor: a type row is 12 bytes (`a`, `b`, `kind`, `flags`) and the solver's `markers`
//! 4; its index slot is 5.3 bytes (128-byte groups of 24) at a load of at most 7/8, about 6, so
//! about 21 bytes a type before growth's slack (columns grow by doubling). An intern hit is a
//! hash, one group's cache line and one row's: in a loop where each lookup waits on the last,
//! about six dependent L1 loads (about 30 cycles) hot, and two cache misses cold. A fold over
//! a subtree its flags say is untouched costs one load of the flags. A fold over a list's
//! elements interns them as a batch, prefetched (`Tys::intern_many`), so their misses overlap.

#![feature(allocator_api)]
#![expect(clippy::print_stdout, reason = "a measurement tool prints its table")]

use std::time::Instant;

use blaze_ty::{AllInvariant, CanonBuf, IntTy, Mode, Mutability, Region, Ty, TyCx, Tys};

/// `n` as a float (every count here is under 2^32).
fn f(n: usize) -> f64 {
    f64::from(u32::try_from(n).unwrap_or(u32::MAX))
}

fn per(start: Instant, n: usize) -> f64 {
    start.elapsed().as_secs_f64() * 1e9 / f(n)
}

fn chain(tys: &mut Tys<'_>, bottom: Ty, depth: usize) -> Ty {
    let mut t = bottom;
    for _ in 0..depth {
        t = tys.reference(Region::STATIC, t, Mutability::Not);
    }
    t
}

/// An intern hit with the working set in cache: a thousand types looked up a thousand times.
/// (The hit's pieces were timed apart once, through a module that existed only for this; the
/// findings are in `src/tys.rs` and `src/index.rs`, and the module is gone.)
fn hot_hit(heap: &'static blaze_own::Heap) {
    use std::hint::black_box;
    let mut hot = Tys::new_in(heap.alloc());
    let mut t = Ty::BOOL;
    for _ in 0..1000 {
        t = hot.slice(t);
    }
    let start = Instant::now();
    for _ in 0..1000 {
        let mut t = Ty::BOOL;
        for _ in 0..1000 {
            t = hot.slice(black_box(t));
        }
        black_box(t);
    }
    println!("intern hit, 1,000 hot types: {:.1} ns", per(start, 1_000_000));
}

fn main() {
    let heap = blaze_own::Heap::process();
    const N: usize = 1_000_000;
    let i32_ = Ty::int(IntTy::I32);

    // Intern miss: a million new types (each a slice of the last).
    let mut tys = Tys::new_in(heap.alloc());
    let start = Instant::now();
    let mut t = Ty::BOOL;
    for _ in 0..N {
        t = tys.slice(t);
    }
    let miss = per(start, N);
    // Intern hit: the same million again.
    let start = Instant::now();
    let mut u = Ty::BOOL;
    for _ in 0..N {
        u = tys.slice(u);
    }
    let hit = per(start, N);
    assert_eq!(t, u);
    println!("intern miss: {miss:.1} ns/type; intern hit: {hit:.1} ns/type (1,000,000 rows, each lookup waiting on the last)");
    hot_hit(heap);

    for n in [10_000usize, 1_000_000] {
        let mut tys = Tys::new_in(heap.alloc());
        let mut t = Ty::BOOL;
        for _ in 0..n {
            t = tys.slice(t);
        }
        let b = f(tys.bytes()) / f(tys.type_count());
        println!("bytes per type at {n}: {b:.1} (floor about 21: ratio {:.2})", b / 21.0);
    }

    // Unify small: a fresh variable with `(i32, bool)`, in one context.
    let mut cx = TyCx::new_in(heap.alloc());
    let small = cx.tys.tuple_of(&[i32_, Ty::BOOL]);
    const SMALL: usize = 200_000;
    let start = Instant::now();
    for _ in 0..SMALL {
        let v = cx.next_ty_var();
        let r = cx.eq(v.arg(), small.arg(), &mut AllInvariant);
        assert!(r.is_ok());
    }
    println!("unify small (var with a 3-node type): {:.1} ns", per(start, SMALL));

    // Unify large: a 10,000-deep chain with a variable at the bottom against the plain one.
    const DEEP: usize = 10_000;
    const ROUNDS: usize = 100;
    let plain = chain(&mut cx.tys, i32_, DEEP);
    let mut total = 0.0;
    for _ in 0..ROUNDS {
        let v = cx.next_ty_var();
        let with_var = chain(&mut cx.tys, v, DEEP);
        let start = Instant::now();
        let r = cx.eq(with_var.arg(), plain.arg(), &mut AllInvariant);
        total += per(start, DEEP);
        assert!(r.is_ok());
    }
    println!("unify large (10,000-node chains): {:.1} ns/node", total / f(ROUNDS));

    // Subst: a 10,000-deep chain with a parameter at the bottom.
    let p = cx.tys.param(0);
    let with_param = chain(&mut cx.tys, p, DEEP);
    let args = cx.tys.mk_ty_list(&[Ty::BOOL]);
    let start = Instant::now();
    for _ in 0..ROUNDS {
        let r = cx.subst_ty(with_param, args);
        assert!(r.is_ok());
    }
    println!("subst: {:.1} ns/node (every node changes: each is re-interned, a hit)", per(start, DEEP * ROUNDS));
    // Subst over a wide tuple where one field of 10,000 holds the parameter: the others are
    // skipped on their flags.
    let mut fields = vec![Ty::BOOL; DEEP];
    for (i, x) in fields.iter_mut().enumerate() {
        *x = cx.tys.array_n(Ty::BOOL, u64::try_from(i).unwrap_or(0));
    }
    fields[DEEP / 2] = p;
    let wide = cx.tys.tuple_of(&fields);
    let start = Instant::now();
    for _ in 0..ROUNDS {
        let r = cx.subst_ty(wide, args);
        assert!(r.is_ok());
    }
    println!("subst, one field of 10,000 changed: {:.2} ns/field", per(start, DEEP * ROUNDS));
    // Subst over a tuple of a million fields `[T; i]`, every one changed: siblings whose
    // interns do not wait on each other, on a table past the cache (the first round inserts,
    // the rest hit).
    const WIDE: usize = 1_000_000;
    let mut fields = vec![Ty::BOOL; WIDE];
    for (i, x) in fields.iter_mut().enumerate() {
        *x = cx.tys.array_n(p, u64::try_from(i).unwrap_or(0));
    }
    let wide = cx.tys.tuple_of(&fields);
    let r = cx.subst_ty(wide, args);
    assert!(r.is_ok());
    const WIDE_ROUNDS: usize = 5;
    let start = Instant::now();
    for _ in 0..WIDE_ROUNDS {
        let r = cx.subst_ty(wide, args);
        assert!(r.is_ok());
    }
    println!("subst, 1,000,000 fields all changed (siblings): {:.1} ns/field", per(start, WIDE * WIDE_ROUNDS));

    // Canonicalize: a small goal with three variables, key and all.
    let (a, b, c) = (cx.next_ty_var(), cx.next_ty_var(), cx.next_int_var());
    let goal = cx.tys.tuple_of(&[a, b, c, a, i32_]);
    let mut buf = CanonBuf::new_in(heap.alloc());
    const CANON: usize = 200_000;
    let start = Instant::now();
    for _ in 0..CANON {
        let r = cx.canonicalize(goal.node(), Mode::Input { param_env: false }, &mut buf);
        assert!(r.is_ok());
    }
    println!("canonicalize (6-node goal, 3 variables): {:.1} ns, key {} words", per(start, CANON), buf.key.len());
}
