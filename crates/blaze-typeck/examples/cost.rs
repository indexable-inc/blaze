//! The cost of checking bodies that select impls (typeck.md, "Cost"): bodies per second on a
//! generic file against a stated floor (rule 67), the solver's Need rounds per body, and R, the
//! work task restarts repeat over the total, under the three arrival orders.
//!
//! The file (written below as `world` and `body` build it):
//!
//! ```text
//! trait T<k> { fn m<k>(&self) -> u32; }                 // k in 0..TRAITS
//! struct S<j> {}   impl T<k> for S<j> {}                // j in 0..STRUCTS, every k
//! struct Wrap<X> { inner: X }  impl<X: T<k>> T<k> for Wrap<X> {}
//! fn g<k><X: T<k>>(x: &X) -> u32;
//! trait Conv<Y> { fn conv(&self) -> Y; }  impl Conv<u8> for S<j> {}  impl Conv<u16> for S<j> {}
//! fn body<i>(a: S<j>, w: Wrap<S<j'>>) -> u32 {
//!     // one block, twice when i % 3 == 0:
//!     let x1 = a.m<k1>();      // a trait method on a concrete type (bucket of one impl)
//!     let x2 = g<k2>(&a);      // a generic fn: its bound proved by an impl
//!     let x3 = w.m<k3>();      // an impl with a where-clause, proved through it
//!     let x4 = a.conv();       // ambiguous until the next statement
//!     let y: u16 = x4;
//!     let z = 1;               // integer fallback
//!     x1
//! }
//! ```
//!
//! Run: `examplerun.sh blaze-typeck cost` (release).

use std::time::Instant;

use blaze_own::Alloc;
use blaze_typeck::fixture::{Bb, Fx};
use blaze_typeck::infer::InferCtx;
use blaze_typeck::{
    Checked, Clock, ExprId, ItemKey, LangItem, Mutbl, Needs, Order, Outcome, PNode, PTy, PortableTys, R_THRESHOLD,
    Restarts, StmtId, UintTy, check, check_logged, entries_histogram, restarts,
};

const TRAITS: usize = 16;
const STRUCTS: usize = 64;
const BODIES: usize = 2048;

struct Wall(Instant);

impl Clock for Wall {
    fn now_ns(&self) -> u64 {
        self.0.elapsed().as_nanos() as u64
    }
}

struct File {
    fx: Fx,
    bodies: Vec<(Bb, ExprId, ItemKey)>,
}

fn file() -> File {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let (p0, p1) = (fx.param(0), fx.param(1));
    let (u8_, u16_, u32_) = (fx.u8(), fx.u16(), fx.u32());
    let r_self = fx.r(Mutbl::Not, p0);
    let wrap = fx.strukt("Wrap", 1, &[("inner", p0)]);
    let wrap_p = fx.adt(wrap, &[p0]);
    let mut traits = Vec::new();
    for k in 0..TRAITS {
        let t = fx.tr(&format!("T{k}"), 1, Vec::new(), &[]);
        fx.trait_fn(t, &format!("m{k}"), true, &[r_self], u32_);
        fx.use_trait(t);
        let bound = fx.bound(t, &[p0]);
        fx.func(&format!("g{k}"), 1, &[r_self], u32_, vec![bound]);
        fx.imp(&format!("impl<X: T{k}> T{k} for Wrap<X>"), 1, wrap_p, Some((t, &[])), &[], vec![bound]);
        traits.push(t);
    }
    let conv = fx.tr("Conv", 2, Vec::new(), &[]);
    fx.trait_fn(conv, "conv", true, &[r_self], p1);
    fx.use_trait(conv);
    let mut structs = Vec::new();
    for j in 0..STRUCTS {
        let s = fx.strukt(&format!("S{j}"), 0, &[]);
        let s_t = fx.adt(s, &[]);
        for (k, &t) in traits.iter().enumerate() {
            fx.imp(&format!("impl T{k} for S{j}"), 0, s_t, Some((t, &[])), &[], Vec::new());
        }
        fx.imp(&format!("impl Conv<u8> for S{j}"), 0, s_t, Some((conv, &[u8_])), &[], Vec::new());
        fx.imp(&format!("impl Conv<u16> for S{j}"), 0, s_t, Some((conv, &[u16_])), &[], Vec::new());
        structs.push(s_t);
    }
    let mut bodies = Vec::with_capacity(BODIES);
    for i in 0..BODIES {
        let a_t = structs[i % STRUCTS];
        let inner = structs[(i * 7 + 3) % STRUCTS];
        let w_t = fx.adt(wrap, &[inner]);
        let owner = fx.func(&format!("body{i}"), 0, &[a_t, w_t], u32_, Vec::new());
        let reps = if i % 3 == 0 { 2 } else { 1 };
        bodies.push(body(i, reps, owner));
    }
    fx.finish();
    File { fx, bodies }
}

fn body(i: usize, reps: usize, owner: ItemKey) -> (Bb, ExprId, ItemKey) {
    let mut b = Bb::new();
    let (a, w) = (b.param("a"), b.param("w"));
    let u16_ = b.ty(PNode::Uint(UintTy::U16));
    let mut stmts: Vec<StmtId> = Vec::new();
    let mut tail = None;
    for r in 0..reps {
        let (k1, k2, k3) = ((i + r) % TRAITS, (i * 5 + r) % TRAITS, (i * 11 + r) % TRAITS);
        let e = b.local(a);
        let e = b.method(e, &format!("m{k1}"), &[]);
        let x1 = b.bind("x1");
        stmts.push(b.let_(x1, None, e));
        let g = b.path(&[&format!("g{k2}")]);
        let e = b.local(a);
        let e = b.borrow(Mutbl::Not, e);
        let e = b.call(g, &[e]);
        let x2 = b.bind("x2");
        stmts.push(b.let_(x2, None, e));
        let e = b.local(w);
        let e = b.method(e, &format!("m{k3}"), &[]);
        let x3 = b.bind("x3");
        stmts.push(b.let_(x3, None, e));
        let e = b.local(a);
        let e = b.method(e, "conv", &[]);
        let x4 = b.bind("x4");
        stmts.push(b.let_(x4, None, e));
        let e = b.local(x4);
        let y = b.bind("y");
        stmts.push(b.let_(y, Some(u16_), e));
        let e = b.int(None);
        let z = b.bind("z");
        stmts.push(b.let_(z, None, e));
        tail = Some(b.local(x1));
    }
    let root = b.block(&stmts, tail);
    (b, root, owner)
}

fn check_one<'a>(f: &File, i: usize, alloc: Alloc<'a>) -> (Checked<'a>, u64) {
    let (b, root, owner) = &f.bodies[i];
    let mut cx = InferCtx::new_in(alloc);
    match check(&b.view(*owner, *root), &f.fx, &mut cx, &Needs::new_in(alloc), alloc) {
        Ok(Outcome::Done(c)) => (c, cx.need_rounds),
        Ok(Outcome::Need(n)) => panic!("body{i}: the world missed {:?}", n.questions),
        Err(e) => panic!("body{i}: {e:?}"),
    }
}

/// The floor: writing the record and nothing else. Every type the record names is interned once
/// into a fresh table (its nodes are in child-first order, so each child is already there), and
/// the expression column is copied: a checker that knew every answer still writes this.
fn floor_one(c: &Checked<'_>, alloc: Alloc<'_>) -> usize {
    let view = c.types.view();
    let mut out = PortableTys::new_in(alloc);
    for &n in view.nodes() {
        let _ = out.intern(n);
    }
    let col: Vec<PTy> = c.expr_ty.iter().copied().collect();
    col.len() + out.view().len()
}

fn main() {
    let alloc = Alloc::global();
    let t0 = Instant::now();
    let f = file();
    let exprs: usize = f.bodies.iter().map(|(b, _, _)| b.exprs.len()).sum();
    println!(
        "file: {} traits, {} structs, {} impls, {} bodies, {} expressions; built in {:.1} ms",
        TRAITS,
        STRUCTS,
        TRAITS * (STRUCTS + 1) + 2 * STRUCTS,
        BODIES,
        exprs,
        t0.elapsed().as_secs_f64() * 1e3
    );

    // Correctness first: every body checks with no diagnostic.
    let mut rounds: Vec<u64> = Vec::with_capacity(BODIES);
    let mut records = Vec::with_capacity(BODIES);
    for i in 0..BODIES {
        let (c, r) = check_one(&f, i, alloc);
        assert!(c.diags.is_empty(), "body{i}: {:?}", c.diags);
        rounds.push(r);
        records.push(c);
    }

    // The floor, then the check, best of five passes.
    let mut floor_ns = u64::MAX;
    for _ in 0..5 {
        let t = Instant::now();
        let mut sink = 0usize;
        for c in &records {
            sink += floor_one(c, alloc);
        }
        std::hint::black_box(sink);
        floor_ns = floor_ns.min(t.elapsed().as_nanos() as u64);
    }
    let mut check_ns = u64::MAX;
    for _ in 0..5 {
        let t = Instant::now();
        for i in 0..BODIES {
            std::hint::black_box(check_one(&f, i, alloc));
        }
        check_ns = check_ns.min(t.elapsed().as_nanos() as u64);
    }
    let per_s = |ns: u64| BODIES as f64 / (ns as f64 / 1e9);
    println!(
        "floor (write the record only): {:.0} bodies/s ({:.0} ns/body, {:.1} ns/expr)",
        per_s(floor_ns),
        floor_ns as f64 / BODIES as f64,
        floor_ns as f64 / exprs as f64
    );
    println!(
        "check (InferCtx, fresh per body): {:.0} bodies/s ({:.0} ns/body, {:.1} ns/expr); ratio to floor {:.1}x",
        per_s(check_ns),
        check_ns as f64 / BODIES as f64,
        check_ns as f64 / exprs as f64,
        check_ns as f64 / floor_ns as f64
    );
    let total_rounds: u64 = rounds.iter().sum();
    let max_rounds = rounds.iter().copied().max().unwrap_or(0);
    println!(
        "need_rounds per body: mean {:.2}, max {} (solver re-runs after a fact fill)",
        total_rounds as f64 / BODIES as f64,
        max_rounds
    );

    // R under each order, from one logged run per body.
    let clock = Wall(Instant::now());
    let mut logs = Vec::with_capacity(BODIES);
    for i in 0..BODIES {
        let (b, root, owner) = &f.bodies[i];
        let mut cx = InferCtx::new_in(alloc);
        let (o, log) = check_logged(&b.view(*owner, *root), &f.fx, &mut cx, &Needs::new_in(alloc), &clock, alloc)
            .unwrap_or_else(|e| panic!("body{i}: {e:?}"));
        assert!(matches!(o, Outcome::Done(_)), "body{i}: the logged run stopped");
        logs.push(log);
    }
    for order in [Order::Pull, Order::Kind, Order::KindWithGoals] {
        let all: Vec<Restarts> = logs.iter().map(|l| restarts(l, order, alloc)).collect();
        let wasted: u64 = all.iter().map(|r| r.wasted_steps).sum();
        let total: u64 = all.iter().map(|r| r.total_steps).sum();
        let wasted_ns: u64 = all.iter().map(|r| r.wasted_ns).sum();
        let total_ns: u64 = all.iter().map(|r| r.total_ns).sum();
        let r = wasted as f64 / total.max(1) as f64;
        let hist = entries_histogram::<8>(&all);
        let serial = all.iter().map(|r| r.serial_rounds).max().unwrap_or(0);
        println!(
            "{order:?}: R = {r:.3} by steps ({wasted}/{total}), {:.3} by time; {} R_THRESHOLD {R_THRESHOLD}; \
             entries per body [1..8+] {hist:?}; longest serial rounds {serial}",
            wasted_ns as f64 / total_ns.max(1) as f64,
            if r <= R_THRESHOLD { "within" } else { "over" },
        );
        let mut idx: Vec<usize> = (0..BODIES).collect();
        idx.sort_by_key(|&i| std::cmp::Reverse(all[i].wasted_steps));
        let top: u64 = idx.iter().take(BODIES / 10).map(|&i| all[i].wasted_steps).sum();
        let show: Vec<String> = idx
            .iter()
            .take(4)
            .map(|&i| {
                format!(
                    "body{i} ({} exprs, {} entries, {} wasted steps)",
                    f.bodies[i].0.exprs.len(),
                    all[i].entries,
                    all[i].wasted_steps
                )
            })
            .collect();
        println!("  top 10% of bodies hold {:.0}% of the waste; largest: {}", 100.0 * top as f64 / wasted.max(1) as f64, show.join(", "));
    }
}
