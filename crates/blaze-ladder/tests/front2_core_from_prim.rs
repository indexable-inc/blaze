//! CORE'S `<impl ErrorKind>::from_prim`, ONE BODY OVER THE REAL TOOLCHAIN, WITH THE SOLVER'S
//! TRACE OF ITS COMPARISONS (lane RF11, 2026-11-17).
//!
//! The entry-programs test (tests/front2_main.rs `the_entry_programs_as_far_as_each_gets`)
//! refused this body with E0282 at `from_prim!(ek => ErrorKind {` (library/core/src/io/error.rs:1065):
//! the cast target `_` of `ErrorKind::NotFound as _` stayed a variable, so the goal
//! `u32: PartialEq<?T>` that should give it `u32` (rules/op.rs `binop`, state 2) did not settle.
//! The same body over a fixture core settles it (tests/front2_cast_infer.rs). This test checks
//! that one body alone, in seconds, and prints what the solver did with each such goal:
//!
//! ```text
//! from_prim: <the check's outcome, each diagnostic>
//! goal #<input> u32: PartialEq<[args]> (frames n, candidates kept k, past the cap d)
//!   cand <source> <rule> <impl path and header>: held Yes / refused (header) / nested no (<trait> <self>)
//!   merge: kept n -> <verdict>
//! root: [args before] -> [args after]: <answer> (<how answered>)
//! ```
//!
//! It is a measurement (blaze-solve witness.rs, "the trait-goal trace"); its one assertion is
//! that the trace saw the goal, so it never passes without the check having asked it (rule 61).
//! The finding it is for is printed, not asserted: the fix's own rows are tests/front2_cast_infer.rs's.
//! It needs the installed toolchain under `$HOME/.blaze/v2/toolchain`, as tests/front2_main.rs does.

#![feature(allocator_api)]

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::body::lower_member;
use blaze_ladder::front2::check::{BuildOrder, CheckFail, Records, check_body};
use blaze_ladder::front2::consts::Consts;
use blaze_ladder::front2::heads::{Store, Want};
use blaze_ladder::front2::item_shown::ItemShown;
use blaze_ladder::front2::keys::{Keys, MemberKind, checker_of};
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::refusal::SourceFile;
use blaze_lower::scoped::Prepares;
use blaze_mem::Frame;
use blaze_own::Erased;
use blaze_scope::resolve::Names;
use blaze_solve::Def;
use blaze_solve::assembly::Source;
use blaze_solve::term::{TyKind, UintTy};
use blaze_solve::witness::{Fate, Trace, TraceAsk, TRACED_ARGS};
use blaze_typeck::ItemKey;

const MAIN: &str = "fn main() {}\n";

/// The goals traced and the candidates each (the brief's bound).
const GOALS: u32 = 3;
const CANDS: u32 = 40;

/// A definition the solver names, as a path with its key, and an impl's header as written.
fn def_text<'n>(names: &'n Names<'n>, keys: &Keys, d: Def) -> String {
    let k = ItemKey(d.0);
    let header = keys.library_in(names, k).and_then(|l| names.item_text(l)).and_then(|t| t.impl_header);
    match header {
        Some(h) => format!("{} `{}`", ItemShown::of(names, keys, k), h.split_whitespace().collect::<Vec<_>>().join(" ")),
        None => format!("{}", ItemShown::of(names, keys, k)),
    }
}

fn kind_text<'n>(names: &'n Names<'n>, keys: &Keys, k: Option<TyKind>) -> String {
    match k {
        None => "-".to_string(),
        Some(TyKind::Adt(d, _)) => format!("adt {}", ItemShown::of(names, keys, ItemKey(d.0))),
        Some(k) => format!("{k:?}"),
    }
}

fn args_text<'n>(names: &'n Names<'n>, keys: &Keys, args: &[Option<TyKind>; TRACED_ARGS]) -> String {
    let shown: Vec<String> = args.iter().filter(|a| a.is_some()).map(|a| kind_text(names, keys, *a)).collect();
    format!("[{}]", shown.join(", "))
}

fn print_trace<'n>(names: &'n Names<'n>, keys: &Keys, t: &Trace<'_>) {
    for g in t.goals() {
        println!(
            "goal #{} {}: {} (frames {}, candidates kept {}, past the cap {})",
            g.input,
            def_text(names, keys, g.trait_),
            args_text(names, keys, &g.args),
            g.frames,
            g.cands,
            g.cands_dropped
        );
        for c in t.cands().iter().filter(|c| c.input == g.input) {
            let what = match c.choice.source {
                Source::Impl(d) => def_text(names, keys, d),
                _ => format!("def {:x}", c.choice.def.0),
            };
            let fate = match c.fate {
                Fate::Refused(why) => format!("refused ({why:?})"),
                Fate::NestedNo { trait_: Some(tr), self_kind } => {
                    format!("nested no ({} for {})", def_text(names, keys, tr), kind_text(names, keys, self_kind))
                }
                Fate::NestedNo { trait_: None, .. } => "nested no (a goal of no trait)".to_string(),
                Fate::Held { certainty, constrains } => format!("held {certainty:?}{}", if constrains { ", binds" } else { "" }),
            };
            println!("  cand {:?} {:?} {what}: {fate}", c.choice.source, c.choice.rule);
        }
        for m in t.merges().iter().filter(|m| m.input == g.input) {
            println!("  merge: kept {} -> {:?}", m.kept, m.verdict);
        }
    }
    for r in t.roots() {
        println!("root: {} -> {}: {:?} ({:?})", args_text(names, keys, &r.before), args_text(names, keys, &r.after), r.answer, r.how);
    }
    println!("rows past the trace's caps: {}", t.dropped());
}

#[test]
fn core_from_prim_alone_with_the_trace() {
    // The test's one allocator, made at its top: a frame kept for the process (the store and
    // the front end take `Alloc<'static>`), as tests/front2_cast_infer.rs keeps it.
    // 32 GiB of address space (committed only as used): the whole toolchain's names are solved
    // into this one frame, and a frame of the default reserve refused a block part-way
    // (2026-11-17: "the allocator refused 86016 bytes .. for `OriginRow`").
    let frame: &'static Frame = Box::leak(Box::new(Frame::with_reserve(32 << 30, blaze_mem::tag::Tag::Infer).expect("the test's frame")));
    let erased: &'static Erased<&'static Frame> = frame.alloc(Erased::new(frame));
    let a = erased.handle();
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/probe/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest("2024").expect("2024 is an edition");
    let user = UserCrate { name: "probe", dir: "/probe/", root: "/probe/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let core = crates.iter().position(|c| c.name == "core").expect("the toolchain's `core`");
    let sources = [SourceFile { path: "src/main.rs", text: MAIN }];
    let mut front = Front2::new(&names, frame, &sources, a).unwrap_or_else(|e| panic!("front2: {e}"));
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut store = Store::new(front.index.lang("sized"), front.frame, a);
    let (mut buckets, mut consts) = (None, Consts::default());
    // `impl ErrorKind`, the inherent impl that holds `from_prim` (io/error.rs).
    let imp = names
        .items(core)
        .map(|(k, _, _)| k)
        .find(|&k| names.item_text(k).is_some_and(|t| t.impl_header.is_some_and(|h| h.contains("ErrorKind") && !h.contains(" for ")) && t.text.contains("fn from_prim")))
        .expect("core's `impl ErrorKind` with `from_prim`");
    store.ensure(&names, &mut front.keys, Want::Impl(imp)).unwrap_or_else(|e| panic!("the impl's head: {e:?}"));
    let k = front.keys.filed_member(checker_of(imp), MemberKind::Fn, "from_prim").expect("`from_prim` filed as a member of `impl ErrorKind`");
    let body = lower_member(&names, prepared, &mut front.keys, &mut store, k).unwrap_or_else(|e| panic!("lowering `from_prim`: {e}"));
    let eq = front.index.lang("eq").expect("core's `eq` lang item (`PartialEq`)");
    store.trace = Some(TraceAsk { trait_: Def(checker_of(eq).0), self_kind: Some(TyKind::Uint(UintTy::U32)), goals: GOALS, cands: CANDS });
    let view = body.lowered.view();
    let spell = Spelling::of(&view).unwrap_or_else(|e| panic!("spelling `from_prim`: {e:?}"));
    let scope = body.scope(&spell);
    let outcome = {
        let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: Some((&mut consts, prepared)) };
        match check_body(&mut records, &body, &scope) {
            Ok(done) if done.diags.is_empty() => "clean".to_string(),
            Ok(done) => {
                let d: Vec<String> = done.diags.iter().map(|d| format!("{} at {:?}", d.code.code(), d.at)).collect();
                format!("diags {d:?}")
            }
            Err(CheckFail::Diags { diags, .. }) => format!("diags {diags:?}"),
            Err(e) => format!("not checked: {e}"),
        }
    };
    println!("from_prim: {outcome}");
    let trace = store.traced.take().expect("the check ran a round with the trace on");
    print_trace(&names, &front.keys, &trace);
    assert!(!trace.goals().is_empty() || !trace.roots().is_empty(), "the check asked no goal `u32: PartialEq<_>`: the trace saw nothing");
}
