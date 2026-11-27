//! The crate's tests: the `.solve` cases, the prefilter's equality property, fulfillment's
//! strategies tested equal, and the planted-bug controls.

pub mod plant;

mod auto_copy_pub;
mod cost;
mod dyn_undecided;
mod for_fields;
mod head_shape;
mod hidden_cause;
mod member_not_filed;
mod overflow_cache;
mod share;
mod specialized_equal;
mod why_maybe;
mod why_no;

use std::format;
use std::string::String;
use std::vec::Vec;

use blaze_own::Alloc;
use blaze_sim::{Broke, Config, Tape};

use crate::assembly::Goal;
use crate::eval::Solver;
use crate::facts::Portable;
use crate::fold::Scratch;
use crate::fulfill::{Fulfill, Rescan, State, Strategy, Wake};
use crate::prefilter::{self, Columns, Fingerprint, WIDTH};
use crate::term::{children, Children, List, Mutbl, Ty, TyKind, TypingMode};
use crate::testing::run::{Report, run_all};
use crate::testing::stagger::staggered;
use crate::tys::Tys;
use plant::Plant;

fn print(r: &Report) {
    std::println!("solve cases: {} cases, {} goals, {} passed, {} failed", r.cases, r.goals, r.passed, r.failures.len());
    for f in &r.failures {
        std::println!("FAIL {f}");
    }
    for (rule, n) in &r.by_rule {
        std::println!("  {n:3}  {rule}");
    }
}

/// Every `.solve` case (or those named in `BLAZE_SOLVE_CASE`, a substring).
#[test]
fn solve_cases() {
    let want = std::env::var("BLAZE_SOLVE_CASE").unwrap_or_default();
    let r = match run_all(&|n: &str| n.contains(want.as_str())) {
        Ok(r) => r,
        Err(e) => panic!("a case file does not parse: {e}"),
    };
    print(&r);
    assert!(r.cases >= 150 || !want.is_empty(), "fewer than 150 cases: {}", r.cases);
    assert!(r.failures.is_empty(), "{} of {} goals failed", r.failures.len(), r.goals);
}

/// Each declaration must see the arguments of its own application. Losing its final slot
/// refuses the same program at that declaration, with the goal and the missing slot intact.
#[test]
fn control_missing_owner_argument() {
    for (case, head, emitter) in [
        ("owner_impl_and_gat_args", "ImplValue", "alias.rs"),
        ("owner_alias_bound_args", "AliasBound", "alias.rs"),
        ("owner_sized_tail_args", "SizedTail", "builtin.rs"),
        ("owner_summary_field_args", "Field", "structural.rs"),
        ("owner_adt_field_args", "Field", "builtin.rs"),
    ] {
        let run = || run_all(&|n| n == case).expect("solver cases parse");
        let good = run();
        assert_eq!(good.cases, 1, "{case}");
        assert_eq!(good.passed, 2, "{case}: {:?}", good.failures);
        let corrupt = plant::with(Plant::MissingHeadArgument, run);
        assert_eq!(corrupt.failures.len(), 2, "{case}: {:?}", corrupt.failures);
        for failure in corrupt.failures {
            assert!(failure.contains(&format!("head: Some({head}")), "{failure}");
            assert!(failure.contains(emitter) && failure.contains("goal: Some("), "{failure}");
        }
    }
}

/// The truth the prefilter must keep: impl `i` survives iff at every set goal byte its byte is
/// equal or a wildcard (prefilter.rs's module doc).
fn truth(g: &Fingerprint, cols: Columns<'_>) -> Vec<u32> {
    (0..cols.len)
        .filter(|&i| {
            g.iter().enumerate().all(|(p, &gb)| {
                let b = cols.bytes[p * cols.stride as usize + i as usize];
                gb == 0 || b == gb || b == 0
            })
        })
        .collect()
}

/// One prefilter case: random columns at a random offset into their buffer (every alignment),
/// bytes from a small alphabet so matches and wildcards are common.
fn prefilter_case(t: &Tape) -> Result<(), Broke> {
    let alloc = blaze_own::testing::heap().alloc();
    let len = t.below(300, "impls") as usize;
    let alphabet = 1 + t.below(4, "alphabet");
    let byte = |t: &Tape| -> u8 { t.below(alphabet + 1, "byte") as u8 };
    let mut fps: Vec<Fingerprint> = Vec::new();
    for _ in 0..len {
        let mut fp = [0u8; WIDTH];
        for b in fp.iter_mut() {
            *b = byte(t);
        }
        fps.push(fp);
    }
    let mut g = [0u8; WIDTH];
    for b in g.iter_mut() {
        *b = if t.coin("goal byte set") { byte(t) } else { 0 };
    }
    let mut cols = blaze_own::Vec::new_in(alloc);
    let stride = prefilter::columns_of(&fps, &mut cols);
    let off = t.below(32, "alignment") as usize;
    let mut buf: Vec<u8> = std::vec![0xAA; off];
    buf.extend_from_slice(&cols);
    let c = Columns { bytes: &buf[off..], stride, len: len as u32 };
    let want = truth(&g, c);
    let got = |f: &dyn Fn(&mut blaze_own::Vec<u32, Alloc<'_>>)| -> Vec<u32> {
        let mut out = blaze_own::Vec::new_in(alloc);
        f(&mut out);
        out.iter().copied().collect()
    };
    let scalar = got(&|o| prefilter::filter_scalar(&g, c, o));
    let v16 = got(&|o| prefilter::filter_width(&g, c, o, 16));
    let v32 = got(&|o| prefilter::filter_width(&g, c, o, 32));
    let auto = got(&|o| prefilter::filter(&g, c, o));
    for (name, have) in [("scalar", &scalar), ("simd16", &v16), ("simd32", &v32), ("filter", &auto)] {
        if *have != want {
            return Err(Broke::Oracle(format!("the prefilter ({name}) keeps {have:?}, the truth is {want:?} (offset {off})")));
        }
    }
    Ok(())
}

#[test]
fn prefilter_equals_truth_at_every_alignment() {
    blaze_sim::check(Config::cases(400), |t: &Tape| prefilter_case(t));
}

#[test]
fn control_prefilter_drops_wildcard() {
    blaze_sim::control(Config::cases(200), Plant::None, Plant::PrefilterDropsWildcard, "the prefilter", |t, p| {
        plant::with(p, || prefilter_case(t))
    });
}

/// The former whole-prefix reachability walk, retained as the property oracle for the
/// worklist implementation. It deliberately scans only through the largest root, just as the
/// old production code did, and therefore has the same malformed-index outcomes.
fn old_params_reached(
    p: Portable<'_>,
    roots: &[Ty],
    which: fn(TyKind) -> Option<u32>,
    out: &mut [bool],
) -> Result<(), crate::error::SolveError> {
    let len = p.nodes.len();
    let Some(top) = roots.iter().map(|t| t.0 as usize).max() else { return Ok(()) };
    if top >= len {
        return Err(crate::error::SolveError::Malformed(crate::error::Malformed::PortableIndex {
            node: top as u32,
            len: len as u32,
        }));
    }
    let mut reach = std::vec![false; top + 1];
    for t in roots {
        reach[t.0 as usize] = true;
    }
    for i in (0..=top).rev() {
        if !reach[i] {
            continue;
        }
        let k = p.nodes[i];
        if let Some(x) = which(k)
            && let Some(slot) = out.get_mut(x as usize)
        {
            *slot = true;
        }
        let mut mark = |c: Ty| -> Result<(), crate::error::SolveError> {
            let ci = c.0 as usize;
            if ci >= i {
                return Err(crate::error::SolveError::Malformed(crate::error::Malformed::PortableIndex {
                    node: c.0,
                    len: i as u32,
                }));
            }
            reach[ci] = true;
            Ok(())
        };
        let list = |l: List, mark: &mut dyn FnMut(Ty) -> Result<(), crate::error::SolveError>| {
            for j in 0..l.len {
                let at = (l.start + j) as usize;
                let Some(&c) = p.lists.get(at) else {
                    return Err(crate::error::SolveError::Malformed(crate::error::Malformed::PortableIndex {
                        node: at as u32,
                        len: p.lists.len() as u32,
                    }));
                };
                mark(c)?;
            }
            Ok(())
        };
        match children(k) {
            Children::None => {}
            Children::One(a) => mark(a)?,
            Children::Two(a, b) => {
                mark(a)?;
                mark(b)?;
            }
            Children::List(l) => list(l, &mut mark)?,
            Children::ListThen(l, t) => {
                list(l, &mut mark)?;
                mark(t)?;
            }
        }
    }
    Ok(())
}

fn params_reached_case(t: &Tape, plant: Plant) -> Result<(), Broke> {
    plant::with(plant, || {
        let len = 1 + t.below(48, "portable nodes") as usize;
        let mut nodes = Vec::with_capacity(len);
        let mut lists = Vec::new();
        for i in 0..len {
            let kind = if i == 0 || t.below(5, "portable kind") == 0 {
                match t.below(4, "portable leaf") {
                    0 => TyKind::Bool,
                    1 => TyKind::Param(t.below(8, "type parameter") as u32),
                    2 => TyKind::ConstParam(t.below(8, "const parameter") as u32),
                    _ => TyKind::Never,
                }
            } else {
                let child = || Ty(t.below(i as u64, "portable child") as u32);
                match t.below(3, "portable pair") {
                    0 => TyKind::Ref(child(), child(), Mutbl::Not),
                    1 => TyKind::Array(child(), child()),
                    _ => {
                        let start = lists.len() as u32;
                        let count = t.below(3, "portable list length") as u32;
                        for _ in 0..count {
                            lists.push(child());
                        }
                        TyKind::Tuple(List { start, len: count })
                    }
                }
            };
            nodes.push(kind);
        }
        let roots_len = 1 + t.below(4, "portable roots") as usize;
        let roots: Vec<Ty> = (0..roots_len).map(|_| Ty(t.below(len as u64, "portable root") as u32)).collect();
        let p = Portable { nodes: &nodes, lists: &lists };
        let which = |k: TyKind| match k {
            TyKind::Param(x) | TyKind::ConstParam(x) => Some(x),
            _ => None,
        };
        let mut expected = std::vec![false; 8];
        let old = old_params_reached(p, &roots, which, &mut expected);
        let mut actual = std::vec![false; 8];
        let alloc = blaze_own::testing::heap().alloc();
        let mut scratch = Scratch::new_in(alloc);
        let new = crate::builtin::params_reached(p, &roots, which, &mut actual, scratch.params_work());
        if format!("{old:?}") != format!("{new:?}") || old.is_ok() && expected != actual {
            return Err(Broke::Oracle(format!("portable reachability differs: old {old:?} {expected:?}, new {new:?} {actual:?}")));
        }

        let bad = [Ty(len as u32)];
        let old_bad = old_params_reached(p, &bad, which, &mut expected);
        let new_bad = crate::builtin::params_reached(p, &bad, which, &mut actual, scratch.params_work());
        if format!("{old_bad:?}") != format!("{new_bad:?}") {
            return Err(Broke::Oracle(format!("malformed portable index differs: old {old_bad:?}, new {new_bad:?}")));
        }
        Ok(())
    })
}

#[test]
fn params_reached_equals_old_oracle_on_portable_dags() {
    blaze_sim::check(Config::cases(300), |t: &Tape| params_reached_case(t, Plant::None));
}

#[test]
fn control_params_reached_drops_child() {
    blaze_sim::control(
        Config::cases(300),
        Plant::None,
        Plant::ParamsReachedDropsChild,
        "portable reachability differs",
        params_reached_case,
    );
}

/// Runs the staggered load under strategy `S`; returns each obligation's state and every
/// chain variable's value.
fn run_load<S: Strategy>(n: u32, waves: u32, picks: &[u32]) -> Result<(Vec<State>, Vec<u32>, u64), String> {
    let alloc = blaze_own::testing::heap().alloc();
    let mut s = staggered(n, waves, |i| picks[i as usize % picks.len()]).map_err(|e| format!("{e}"))?;
    let mut solver = Solver::new_in(alloc);
    let mut reads = blaze_own::Vec::new_in(alloc);
    let env = solver.env(&mut s.tys, &s.world, &[], &mut reads).map_err(|e| format!("{e:?}"))?;
    let mut f: Fulfill<'_, S> = Fulfill::new_in(alloc);
    for &p in &s.goals {
        f.register(Goal { env, pred: p, bound: 0 }).map_err(|e| format!("{e:?}"))?;
    }
    f.select(&mut solver, &mut s.tys, &s.world, TypingMode::Analysis, &mut reads).map_err(|e| format!("{e:?}"))?;
    let vals = s.vars.iter().map(|&v| s.tys.shallow_resolve(v).0).collect();
    Ok((f.state.iter().copied().collect(), vals, f.evaluations))
}

#[test]
fn fulfill_wake_equals_rescan() {
    blaze_sim::check(Config::cases(40), |t: &Tape| -> Result<(), Broke> {
        let n = 1 + t.below(60, "obligations") as u32;
        let waves = 1 + t.below(12, "waves") as u32;
        let picks: Vec<u32> = (0..n).map(|_| t.below(u64::from(waves) + 2, "wave") as u32).collect();
        let a = run_load::<Wake>(n, waves, &picks).map_err(Broke::Harness)?;
        let b = run_load::<Rescan>(n, waves, &picks).map_err(Broke::Harness)?;
        if a.0 != b.0 || a.1 != b.1 {
            return Err(Broke::Oracle(format!("wake {:?} / {:?}, rescan {:?} / {:?}", a.0, a.1, b.0, b.1)));
        }
        if a.0.iter().any(|&s| s != State::Holds) {
            return Err(Broke::Oracle(format!("an obligation did not hold: {:?}", a.0)));
        }
        Ok(())
    });
}

/// The auto-trait cycle cases (coinductive): the control's subject.
fn cycle_cases(t: &Tape) -> Result<(), Broke> {
    let _ = t.below(2, "draw");
    let r = run_all(&|n: &str| n.contains("cycle")).map_err(|e| Broke::Harness(format!("{e}")))?;
    if r.cases == 0 {
        return Err(Broke::Harness(String::from("no cycle case")));
    }
    if let Some(f) = r.failures.first() {
        return Err(Broke::Oracle(format!("a cycle case answers wrong: {f}")));
    }
    Ok(())
}

/// The cases named in `names` (exact names), each answering as written: the trait-object
/// controls' subject.
fn named_cases(t: &Tape, names: &[&str]) -> Result<(), Broke> {
    let _ = t.below(2, "draw");
    let r = run_all(&|n: &str| names.contains(&n)).map_err(|e| Broke::Harness(format!("{e}")))?;
    if r.cases != names.len() as u32 {
        return Err(Broke::Harness(format!("{} of {} named cases found", r.cases, names.len())));
    }
    if let Some(f) = r.failures.first() {
        return Err(Broke::Oracle(format!("an object case answers wrong: {f}")));
    }
    Ok(())
}

/// The Reference, "Dynamically sized types": trait objects are DSTs, so `dyn Tr: Sized` fails;
/// with the bug (an object is `Sized`) `sized_dyn_no` answers yes.
#[test]
fn control_object_sized() {
    blaze_sim::control(Config::cases(2), Plant::None, Plant::ObjectSized, "an object case answers wrong", |t, p| {
        plant::with(p, || named_cases(t, &["sized_dyn_no", "meta_sized_dyn_yes"]))
    });
}

/// The Reference, "Trait object types": "the trait object implements the base trait, its auto
/// traits, and any supertraits of the base trait": an unlisted auto trait does not hold.
#[test]
fn control_object_any_auto() {
    blaze_sim::control(Config::cases(2), Plant::None, Plant::ObjectAnyAuto, "an object case answers wrong", |t, p| {
        plant::with(p, || {
            named_cases(t, &["auto_dyn_with_auto_bound_yes", "auto_dyn_without_bound_no", "auto_dyn_other_auto_no"])
        })
    });
}

/// The Reference, "Trait object types" (`dyn Iterator<Item = u32>`): the object's associated
/// type is its binding, so `<dyn Iterator<Item = u32> as Iterator>::Item == u8` fails.
#[test]
fn control_object_projection_ignores_binding() {
    blaze_sim::control(
        Config::cases(2),
        Plant::None,
        Plant::ObjectProjIgnoresBinding,
        "an object case answers wrong",
        |t, p| plant::with(p, || named_cases(t, &["dyn_projection", "dyn_projection_mismatch"])),
    );
}

#[test]
fn control_coinductive_cycle_as_inductive() {
    blaze_sim::control(Config::cases(2), Plant::None, Plant::CoinductiveAsInductive, "a cycle case answers wrong", |t, p| {
        plant::with(p, || cycle_cases(t))
    });
}

#[test]
fn record_union_tuple_distinct_unification_and_shapes() {
    use crate::canon::{Canon, Decode};
    use crate::term::{IntTy, Shape, UintTy, VarKind, shape_of_kind};
    use crate::testing::tys::TestTys;
    use crate::tys::Relate;

    let alloc = blaze_own::testing::heap().alloc();
    let mut tys = TestTys::new();
    let mut deferred = blaze_own::Vec::new_in(alloc);

    let i32_ty = tys.mk(TyKind::Int(IntTy::I32)).expect("mk i32");
    let u8_ty = tys.mk(TyKind::Uint(UintTy::U8)).expect("mk u8");
    let elems = tys.mk_list(&[i32_ty, u8_ty]).expect("mk list");

    let tup = tys.mk(TyKind::Tuple(elems)).expect("mk tuple");
    let rec_a = tys.mk(TyKind::Record(0x1111_2222_3333_4444, elems)).expect("mk record a");
    let rec_b = tys.mk(TyKind::Record(0x5555_6666_7777_8888, elems)).expect("mk record b");
    let uni = tys.mk(TyKind::Union(elems)).expect("mk union");

    // Distinct Shape buckets.
    assert_eq!(shape_of_kind(tys.kind(tup)), Some(Shape::Tuple(2)));
    assert_eq!(shape_of_kind(tys.kind(rec_a)), Some(Shape::Record(0x1111_2222_3333_4444, 2)));
    assert_eq!(shape_of_kind(tys.kind(rec_b)), Some(Shape::Record(0x5555_6666_7777_8888, 2)));
    assert_eq!(shape_of_kind(tys.kind(uni)), Some(Shape::Union(2)));
    assert_ne!(shape_of_kind(tys.kind(tup)), shape_of_kind(tys.kind(rec_a)));
    assert_ne!(shape_of_kind(tys.kind(tup)), shape_of_kind(tys.kind(uni)));
    assert_ne!(shape_of_kind(tys.kind(rec_a)), shape_of_kind(tys.kind(uni)));
    assert_ne!(shape_of_kind(tys.kind(rec_a)), shape_of_kind(tys.kind(rec_b)));

    // Tuple, Record, Union NEVER unify with each other; distinct Record schemas NEVER unify.
    assert!(!tys.unify(tup, rec_a, Relate::Lazy, &mut deferred).expect("unify"));
    assert!(!tys.unify(tup, uni, Relate::Lazy, &mut deferred).expect("unify"));
    assert!(!tys.unify(rec_a, uni, Relate::Lazy, &mut deferred).expect("unify"));
    assert!(!tys.unify(rec_a, rec_b, Relate::Lazy, &mut deferred).expect("unify"));

    // Same-schema Record unifies element-wise and binds variables.
    let v1 = tys.fresh(VarKind::Ty).expect("fresh v1");
    let rec_a_var_list = tys.mk_list(&[v1, u8_ty]).expect("mk list");
    let rec_a_var = tys.mk(TyKind::Record(0x1111_2222_3333_4444, rec_a_var_list)).expect("mk rec var");
    assert!(tys.unify(rec_a, rec_a_var, Relate::Lazy, &mut deferred).expect("unify same schema"));
    assert_eq!(tys.shallow_resolve(v1), i32_ty);

    // Union unifies element-wise and binds variables.
    let v2 = tys.fresh(VarKind::Ty).expect("fresh v2");
    let uni_var_list = tys.mk_list(&[i32_ty, v2]).expect("mk list");
    let uni_var = tys.mk(TyKind::Union(uni_var_list)).expect("mk union var");
    assert!(tys.unify(uni, uni_var, Relate::Lazy, &mut deferred).expect("unify union"));
    assert_eq!(tys.shallow_resolve(v2), u8_ty);

    // Canonicalization roundtrip preserves Record schema and Union identity.
    let mut canon = Canon::new_in(alloc);
    let mut words = blaze_own::Vec::new_in(alloc);
    let mut dec = Decode::new_in(alloc);
    canon.term(&tys, &mut words, rec_a).expect("canon rec");
    dec.run(&mut tys, &words).expect("decode rec");
    assert_eq!(tys.kind(rec_a), TyKind::Record(0x1111_2222_3333_4444, elems));
    words.clear();
    canon.clear();
    canon.term(&tys, &mut words, uni).expect("canon uni");
    let mut dec2 = Decode::new_in(alloc);
    dec2.run(&mut tys, &words).expect("decode uni");
}

/// A higher-ranked region bound (`for<'s> From<&'s str>`) on an impl's where-clause must reject
/// `impl From<&'static str> for Bytes` while still accepting `impl<'a> From<&'a str> for Owned`.
#[test]
fn higher_ranked_region_clause_rejects_static_impl_and_accepts_universal_impl() {
    use crate::eval::Answer;
    use crate::facts::{
        AdtDef, AssocDef, ConstHolds, Facts, Fetch, FnSigDef, ImplFlags, ImplHeader, ImplPolarity, ImplSet, Missing,
        OpaqueDef, Seal, TraitDef, TraitFlags,
    };
    use crate::term::{Def, Lang, Polarity, Pred, Shape};
    use crate::testing::tys::TestTys;

    const TR_FROM: Def = Def(1);
    const TR_PICK: Def = Def(2);
    const ADT_BYTES: Def = Def(10);
    const ADT_OWNED: Def = Def(11);
    const IMPL_FROM_BYTES_STATIC: Def = Def(20);
    const IMPL_FROM_OWNED_ANY: Def = Def(21);
    const IMPL_PICK_HRTB: Def = Def(22);

    struct MiniWorld;

    // Portable tables:
    // Bytes: Adt(ADT_BYTES, [])
    // &'static str: [ReStatic, Str, Ref(0, 1, Not)]
    static NODES_BYTES_STATIC: [TyKind; 4] = [
        TyKind::Adt(ADT_BYTES, List::EMPTY),
        TyKind::ReStatic,
        TyKind::Str,
        TyKind::Ref(Ty(1), Ty(2), Mutbl::Not),
    ];
    static LISTS_BYTES_STATIC: [Ty; 2] = [Ty(0), Ty(3)];

    // Owned: Adt(ADT_OWNED, [])
    // &'a str (erased): [ReErased, Str, Ref(0, 1, Not)]
    static NODES_OWNED_ANY: [TyKind; 4] = [
        TyKind::Adt(ADT_OWNED, List::EMPTY),
        TyKind::ReErased,
        TyKind::Str,
        TyKind::Ref(Ty(1), Ty(2), Mutbl::Not),
    ];
    static LISTS_OWNED_ANY: [Ty; 2] = [Ty(0), Ty(3)];

    // impl<T: for<'s> From<&'s str>> Pick for T
    static NODES_PICK: [TyKind; 4] = [
        TyKind::Param(0),
        TyKind::ReBound(0, 0),
        TyKind::Str,
        TyKind::Ref(Ty(1), Ty(2), Mutbl::Not),
    ];
    static LISTS_PICK: [Ty; 3] = [Ty(0), Ty(0), Ty(3)];
    static WHERES_PICK: [Pred; 1] = [Pred::Trait {
        trait_: TR_FROM,
        args: List { start: 1, len: 2 },
        polarity: Polarity::Positive,
    }];
    static WHERE_BOUNDS_PICK: [u32; 1] = [1];

    static IMPLS_FROM_BYTES: [Def; 1] = [IMPL_FROM_BYTES_STATIC];
    static IMPLS_FROM_OWNED: [Def; 1] = [IMPL_FROM_OWNED_ANY];
    static IMPLS_PICK: [Def; 1] = [IMPL_PICK_HRTB];

    impl Facts for MiniWorld {
        fn trait_def(&self, _: Seal, t: Def) -> Fetch<TraitDef<'_>> {
            let generics = match t {
                TR_FROM => 2,
                TR_PICK => 1,
                _ => return Err(Missing),
            };
            Ok(TraitDef {
                flags: TraitFlags(0),
                generics,
                lang: None,
                supertraits: &[],
                where_clauses: &[],
                assoc: &[],
                item_bounds: &[],
                portable: Portable::EMPTY,
                digest: t.0 as u64,
            })
        }

        fn impls(&self, _: Seal, t: Def, shape: Shape) -> Fetch<ImplSet<'_>> {
            let impls: &[Def] = match (t, shape) {
                (TR_FROM, Shape::Adt(ADT_BYTES)) => &IMPLS_FROM_BYTES,
                (TR_FROM, Shape::Adt(ADT_OWNED)) => &IMPLS_FROM_OWNED,
                (TR_PICK, _) => &IMPLS_PICK,
                _ => &[],
            };
            Ok(ImplSet { impls, columns: Columns::EMPTY, digest: (t.0 as u64) ^ (impls.len() as u64) })
        }

        fn impl_header(&self, _: Seal, i: Def) -> Fetch<ImplHeader<'_>> {
            match i {
                IMPL_FROM_BYTES_STATIC => Ok(ImplHeader {
                    generics: 0,
                    polarity: ImplPolarity::Positive,
                    flags: ImplFlags(0),
                    trait_: TR_FROM,
                    args: List { start: 0, len: 2 },
                    where_clauses: &[],
                    where_clause_bounds: &[],
                    assoc: &[],
                    portable: Portable { nodes: &NODES_BYTES_STATIC, lists: &LISTS_BYTES_STATIC },
                    digest: 20,
                }),
                IMPL_FROM_OWNED_ANY => Ok(ImplHeader {
                    generics: 0,
                    polarity: ImplPolarity::Positive,
                    flags: ImplFlags(0),
                    trait_: TR_FROM,
                    args: List { start: 0, len: 2 },
                    where_clauses: &[],
                    where_clause_bounds: &[],
                    assoc: &[],
                    portable: Portable { nodes: &NODES_OWNED_ANY, lists: &LISTS_OWNED_ANY },
                    digest: 21,
                }),
                IMPL_PICK_HRTB => Ok(ImplHeader {
                    generics: 1,
                    polarity: ImplPolarity::Positive,
                    flags: ImplFlags(0),
                    trait_: TR_PICK,
                    args: List { start: 0, len: 1 },
                    where_clauses: &WHERES_PICK,
                    where_clause_bounds: &WHERE_BOUNDS_PICK,
                    assoc: &[],
                    portable: Portable { nodes: &NODES_PICK, lists: &LISTS_PICK },
                    digest: 22,
                }),
                _ => Err(Missing),
            }
        }

        fn adt(&self, _: Seal, _: Def) -> Fetch<AdtDef<'_>> { Err(Missing) }
        fn fn_sig(&self, _: Seal, _: Def) -> Fetch<FnSigDef<'_>> { Err(Missing) }
        fn assoc(&self, _: Seal, _: Def) -> Fetch<AssocDef> { Err(Missing) }
        fn opaque(&self, _: Seal, _: Def) -> Fetch<OpaqueDef<'_>> { Err(Missing) }
        fn lang(&self, _: Seal, _: Lang) -> Fetch<Option<Def>> { Ok(None) }
        fn is_local(&self, _: Seal, _: Def) -> Fetch<bool> { Ok(true) }
        fn const_holds(&self, _: Seal, _: Def, _: &[Ty]) -> Fetch<ConstHolds> { Err(Missing) }
        fn specializes(&self, _: Seal, _: Def, _: Def) -> Fetch<bool> { Ok(false) }
    }

    let alloc = blaze_own::testing::heap().alloc();
    let mut tys = TestTys::new();
    let mut solver = Solver::new_in(alloc);
    let mut reads = blaze_own::Vec::new_in(alloc);
    let env = solver.env(&mut tys, &MiniWorld, &[], &mut reads).expect("env");

    // Bytes only has `impl From<&'static str>`: MUST NOT satisfy `Pick` (`for<'s> From<&'s str>`).
    let bytes = tys.mk(TyKind::Adt(ADT_BYTES, List::EMPTY)).expect("bytes");
    let bytes_args = tys.mk_list(&[bytes]).expect("bytes args");
    let bytes_goal = Goal {
        env,
        pred: Pred::Trait { trait_: TR_PICK, args: bytes_args, polarity: Polarity::Positive },
        bound: 0,
    };
    let ans_bytes = solver.evaluate(&mut tys, &MiniWorld, TypingMode::Analysis, bytes_goal, &mut reads).expect("eval bytes");
    assert_eq!(ans_bytes, Answer::No, "Bytes with only From<&'static str> must not satisfy for<'s> From<&'s str>");

    // Owned has `impl<'a> From<&'a str>`: MUST satisfy `Pick` (`for<'s> From<&'s str>`).
    let owned = tys.mk(TyKind::Adt(ADT_OWNED, List::EMPTY)).expect("owned");
    let owned_args = tys.mk_list(&[owned]).expect("owned args");
    let owned_goal = Goal {
        env,
        pred: Pred::Trait { trait_: TR_PICK, args: owned_args, polarity: Polarity::Positive },
        bound: 0,
    };
    let ans_owned = solver.evaluate(&mut tys, &MiniWorld, TypingMode::Analysis, owned_goal, &mut reads).expect("eval owned");
    assert_eq!(ans_owned, Answer::Yes, "Owned with impl<'a> From<&'a str> must satisfy for<'s> From<&'s str>");
}
