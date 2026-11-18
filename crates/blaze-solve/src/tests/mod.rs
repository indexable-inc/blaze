//! The crate's tests: the `.solve` cases, the prefilter's equality property, fulfillment's
//! strategies tested equal, and the planted-bug controls.

pub mod plant;

mod cost;
mod dyn_undecided;
mod head_shape;
mod share;

use std::format;
use std::string::String;
use std::vec::Vec;

use blaze_own::Alloc;
use blaze_sim::{Broke, Config, Tape};

use crate::assembly::Goal;
use crate::eval::Solver;
use crate::fulfill::{Fulfill, Rescan, State, Strategy, Wake};
use crate::prefilter::{self, Columns, Fingerprint, WIDTH};
use crate::term::TypingMode;
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
    let mut got = |f: &dyn Fn(&mut blaze_own::Vec<u32, Alloc<'_>>)| -> Vec<u32> {
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
