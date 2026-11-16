//! What the arena and the tables cost, against their floor; a measurement, not a test.
//!
//! Run (release): `cargo run --release -p blaze-ty --example cost`.
//!
//! The floor: a type row is `kind` (1 byte), `a` and `b` (4 each), `flags` (2) and the
//! solver's `markers` (4): 15 bytes; its index slot is a tag byte and a 4-byte id at a load of
//! at most 7/8, about 5.7 bytes, so about 21 bytes a type before growth's slack (columns grow
//! by doubling: up to twice that just after a grow). An intern hit is a hash, one 16-byte
//! group compare and one row compare: a few nanoseconds. Relating or folding is linear in the
//! nodes visited, a few nanoseconds a node.

#![feature(allocator_api)]
#![expect(clippy::print_stdout, reason = "a measurement tool prints its table")]

use std::time::Instant;

use blaze_own::Alloc;
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

/// The intern hit's pieces, timed apart, on the million-row table `tys` (a chain of slices
/// over `bool`).
fn pieces(tys: &mut Tys<'_>, n: usize) {
    use blaze_ty::pieces::{find_slice, probe_only, row_hash};
    use std::hint::black_box;
    // The chain's types, in order, then in a scrambled order (an odd multiplier mod 2^20 is a
    // permutation of the first 2^20 numbers; those past `n` are skipped).
    let mut chain = Vec::with_capacity(n);
    let mut t = Ty::BOOL;
    for _ in 0..n {
        chain.push(t);
        t = tys.slice(t);
    }
    let mut scrambled = Vec::with_capacity(n);
    for i in 0..(1usize << 20) {
        let j = (i.wrapping_mul(0x9e37_79b1) ^ 0x5555) & ((1 << 20) - 1);
        if j < n {
            scrambled.push(chain[j]);
        }
    }

    let start = Instant::now();
    let mut acc = 0u64;
    for (i, e) in chain.iter().enumerate() {
        acc ^= row_hash(black_box(10), e.index() ^ u32::try_from(i & 1).unwrap_or(0), 0);
    }
    black_box(acc);
    println!("  piece: row hash alone: {:.2} ns", per(start, n));

    let start = Instant::now();
    let mut found = 0usize;
    for e in &scrambled {
        found += usize::from(probe_only(tys, row_hash(10, e.node().bits(), 0)));
    }
    black_box(found);
    println!("  piece: probe alone (groups and tags, no compare), scrambled order: {:.1} ns", per(start, scrambled.len()));

    let start = Instant::now();
    let mut hits = 0usize;
    for e in &chain {
        hits += usize::from(find_slice(tys, black_box(*e)).is_some());
    }
    println!("  piece: find (hash, probe, compare), chain order, independent: {:.1} ns", per(start, n));
    assert_eq!(hits, n);

    let start = Instant::now();
    for e in &scrambled {
        hits += usize::from(find_slice(tys, black_box(*e)).is_some());
    }
    println!("  piece: find, scrambled order, independent: {:.1} ns", per(start, scrambled.len()));

    let start = Instant::now();
    let mut t = Ty::BOOL;
    for _ in 0..n {
        t = match find_slice(tys, t) {
            Some(x) => x,
            None => break,
        };
    }
    black_box(t);
    println!("  piece: find, each waiting on the last (the hit loop's shape): {:.1} ns", per(start, n));

    // Hot: a thousand types looked up a thousand times, the working set in cache.
    let mut hot = Tys::new_in(Alloc::global());
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
    println!("  piece: intern hit, 1,000 hot types: {:.1} ns", per(start, 1_000_000));
    let start = Instant::now();
    for _ in 0..1000 {
        let mut t = Ty::BOOL;
        for _ in 0..1000 {
            t = find_slice(&hot, black_box(t)).unwrap_or(Ty::BOOL);
        }
        black_box(t);
    }
    println!("  piece: find alone, 1,000 hot types: {:.1} ns", per(start, 1_000_000));
}

fn main() {
    const N: usize = 1_000_000;
    let i32_ = Ty::int(IntTy::I32);

    // Intern miss: a million new types (each a slice of the last).
    let mut tys = Tys::new_in(Alloc::global());
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
    pieces(&mut tys, N);

    for n in [10_000usize, 1_000_000] {
        let mut tys = Tys::new_in(Alloc::global());
        let mut t = Ty::BOOL;
        for _ in 0..n {
            t = tys.slice(t);
        }
        let b = f(tys.bytes()) / f(tys.type_count());
        println!("bytes per type at {n}: {b:.1} (floor about 21: ratio {:.2})", b / 21.0);
    }

    // Unify small: a fresh variable with `(i32, bool)`, in one context.
    let mut cx = TyCx::new_in(Alloc::global());
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

    // Canonicalize: a small goal with three variables, key and all.
    let (a, b, c) = (cx.next_ty_var(), cx.next_ty_var(), cx.next_int_var());
    let goal = cx.tys.tuple_of(&[a, b, c, a, i32_]);
    let mut buf = CanonBuf::new_in(Alloc::global());
    const CANON: usize = 200_000;
    let start = Instant::now();
    for _ in 0..CANON {
        let r = cx.canonicalize(goal.node(), Mode::Input { param_env: false }, &mut buf);
        assert!(r.is_ok());
    }
    println!("canonicalize (6-node goal, 3 variables): {:.1} ns, key {} words", per(start, CANON), buf.key.len());
}
