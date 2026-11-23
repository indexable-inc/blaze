//! CORE'S `<impl ErrorKind>::from_prim` CHECKED THE WALK'S WAY, ON A FRESH STORE, OVER THE REAL
//! TOOLCHAIN (lane RF13, 2026-11-17).
//!
//! The entry-programs test (tests/front2_main.rs `the_entry_programs_as_far_as_each_gets`)
//! refuses `from_prim` with E0282 at the whole `from_prim!(ek => ErrorKind { .. })` call, while
//! RF11's test (tests/front2_core_from_prim.rs: `lower_member` + `check_body`) checks it clean.
//! The walk (front2/reach.rs `reach_all`, the loop body after `queue.pop()`) checks a reached fn
//! by `lower_made(r, prepared, LowerTarget::Member(k))` (front2/lower_made.rs: `lower_member`
//! again after each head or constant it needs), `Spelling::of`, `Body::scope`, `check_body`
//! with the walk's ONE `Records` (`BuildOrder::AsAsked`, `consts: Some(..)`), then
//! `coroutine_recipes::recipes_of` on a clean check only. It checks no body-declared item of
//! the fn: a `Stmt::Item` is "checked as its own body", and the walk reads a local `const` only
//! when a checked body NAMES it (reach/initializers.rs), which a `const _` never is.
//!
//! So this file checks, each in seconds:
//! 1. `from_prim` the walk's way (`lower_made`, the store's `pointers` set as program.rs sets
//!    them), with the trait-goal trace on; then its body's local items as kept, and the inner
//!    `const _: fn(e: ErrorKind) = |e: ErrorKind| match e { .. };` lowered and checked as its
//!    own body (what a walk would do if it read it), its nodes printed;
//! 2. `from_prim` after its one caller in std (`io::error::repr_bitpacked::decode_repr`) was
//!    checked first in the same store, the head of `impl ErrorKind` made by THAT check, as in
//!    the walk.
//! Each prints `<what>: clean` or every diagnostic (the about names its node now: rule 19,
//! blaze-typeck diag/node.rs). Clean both ways points at the walk's larger state (h3), and the
//! file then prints what the store holds.
//! It needs the installed toolchain under `$HOME/.blaze/v2/toolchain`, as tests/front2_main.rs does.

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::check::{BuildOrder, CheckFail, Records, check_body};
use blaze_ladder::front2::consts::Consts;
use blaze_ladder::front2::heads::{Store, Want};
use blaze_ladder::front2::item_shown::ItemShown;
use blaze_ladder::front2::keys::{Keys, MemberKind, checker_of};
use blaze_ladder::front2::locals::LocalKind;
use blaze_ladder::front2::lower_made::{LowerTarget, lower_made};
use blaze_ladder::front2::receiver::Pointers;
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
use blaze_typeck as tc;
use blaze_typeck::ItemKey;

const MAIN: &str = "fn main() {}\n";

/// The goals traced and the candidates each (RF11's bound).
const GOALS: u32 = 3;
const CANDS: u32 = 40;
/// The most nodes of one kind printed per body (rule 18).
const MOST_SHOWN: usize = 64;

// ---- RF11's trace printer, copied whole (tests/front2_core_from_prim.rs) ----

fn def_text<'n>(names: &'n Names<'n>, keys: &Keys, d: Def) -> String {
    let k = ItemKey(d.0);
    let header = keys.library_in(names, k).and_then(|l| names.item_text(l)).and_then(|t| t.impl_header);
    // Lane RF13, step 2: each candidate with its crate.
    match header {
        Some(h) => format!("{} `{}` [crate {}]", ItemShown::of(names, keys, k), h.split_whitespace().collect::<Vec<_>>().join(" "), crate_name(names, keys, k)),
        None => format!("{} [crate {}]", ItemShown::of(names, keys, k), crate_name(names, keys, k)),
    }
}

/// The name of the crate checker key `k` is written in ("?" where it is of none).
fn crate_name<'n>(names: &'n Names<'n>, keys: &Keys, k: ItemKey) -> &'n str {
    match keys.library_in(names, k).and_then(|l| names.crate_of(l)).and_then(|c| names.crates().get(c)) {
        Some(c) => c.name,
        None => "?",
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

// ---- this lane's printing: no heap (println! and a sink that prints) ----

/// A `fmt::Write` that prints what it is given.
struct Stdout;

impl core::fmt::Write for Stdout {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        print!("{s}");
        Ok(())
    }
}

/// One body checked the walk's way (reach.rs `reach_all`): lowered by `lower_made`, spelled,
/// checked by `check_body` with `r`. Prints its name, key, outcome and every diagnostic, then
/// each of its casts and (`all_exprs`) every expression with its kind, range and shape, then
/// the trace when one was asked. `true` when it checked clean.
fn walks_way<'n>(r: &mut Records<'_, 'n>, prepared: &Prepares<'n>, target: LowerTarget, what: &str, all_exprs: bool) -> bool {
    let body = match lower_made(r, prepared, target) {
        Ok(b) => b,
        Err(e) => {
            println!("{what}: NOT LOWERED: {e}");
            return false;
        }
    };
    println!("{what}: {} (owner {:?}, item {:?})", ItemShown::of(r.names, &*r.keys, body.owner), body.owner, body.item);
    let lowered = body.lowered.view();
    let spell = match Spelling::of(&lowered) {
        Ok(s) => s,
        Err(e) => {
            println!("{what}: NOT SPELLED: {e:?}");
            return false;
        }
    };
    let scope = body.scope(&spell);
    let clean = match check_body(r, &body, &scope) {
        Ok(done) if done.diags.is_empty() => {
            println!("{what}: clean");
            true
        }
        Ok(done) => {
            // Never met: `check_rounds_as` returns a finished check with findings as `Diags`.
            println!("{what}: a Checked WITH {} findings (a checker bug)", done.diags.len());
            false
        }
        Err(CheckFail::Diags { owner, diags, filed }) => {
            println!("{what}: REFUSED with {} diagnostics (owner {owner:?})", diags.len());
            for (i, (code, at, about)) in diags.iter().enumerate() {
                println!("  {} at {}..{}: {about}", code.code(), at.start, at.end);
                match filed.get(i) {
                    Some(place) => println!("    filed: {place:?}"),
                    None => println!("    filed: none (a list shorter than the diagnostics)"),
                }
            }
            false
        }
        Err(e) => {
            println!("{what}: NOT CHECKED: {e}");
            false
        }
    };
    // The body's nodes, as the check read them.
    match body.tys.view(&r.store.tys) {
        Ok(tys) => {
            let v = body.typeck.view(tys);
            println!("{what}: {} expressions, {} patterns", v.exprs.len(), v.pats.len());
            let mut shown = 0usize;
            for (i, x) in v.exprs.iter().enumerate() {
                let is_cast = matches!(x, tc::Expr::Cast { .. });
                if !(all_exprs || is_cast) || shown == MOST_SHOWN {
                    continue;
                }
                shown += 1;
                let node = tc::Node::Expr(tc::ExprId(i as u32));
                let at = v.expr_ranges.get(i).copied().unwrap_or_default();
                print!("  expr #{i} {} at {}..{} (range shared by {}): `", tc::expr_form(x), at.start, at.end, tc::range_shared(&v, node));
                if tc::sketch(&v, node, &|n| spell.get(n).ok(), &mut Stdout).is_err() {
                    print!("<the sketch did not write>");
                }
                println!("`  {x:?}");
            }
        }
        Err(e) => println!("{what}: its types did not read: {e:?}"),
    }
    if let Some(trace) = r.store.traced.take() {
        println!("{what}: the trace of `u32: PartialEq<_>`:");
        print_trace(r.names, &*r.keys, &trace);
    }
    clean
}

#[test]
fn core_from_prim_the_walks_way() {
    // The test's one allocator, as RF11's: a frame kept for the process, 32 GiB reserved.
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
    let std_ = crates.iter().position(|c| c.name == "std").expect("the toolchain's `std`");
    let sources = [SourceFile { path: "src/main.rs", text: MAIN }];
    let imp = names
        .items(core)
        .map(|(k, _, _)| k)
        .find(|&k| names.item_text(k).is_some_and(|t| t.impl_header.is_some_and(|h| h.contains("ErrorKind") && !h.contains(" for ")) && t.text.contains("fn from_prim")))
        .expect("core's `impl ErrorKind` with `from_prim`");
    // A free fn of core or std whose text calls `from_prim` (the first run found no
    // `decode_repr` holding it in std, 2026-11-18: the caller is looked for by the call).
    let calls = |k| names.item_text(k).is_some_and(|t| t.impl_header.is_none() && t.text.contains("fn ") && t.text.contains("from_prim("));
    let caller = names.items(std_).map(|(k, _, _)| k).find(|&k| calls(k)).or_else(|| names.items(core).map(|(k, _, _)| k).find(|&k| calls(k)));
    match caller {
        Some(k) => println!("the caller found by its text: {}", names.item_text(k).map_or("?", |t| t.text.lines().next().unwrap_or("?"))),
        None => println!("no free fn of core or std calls `from_prim(` in its text: part 2 is skipped"),
    }
    let prepared = Prepares::new_in(blaze_own::testing::bump());

    // 1. Fresh store; the impl's head made as an inherent lookup makes it; `from_prim` the
    //    walk's way; then its local items and the inner `const _` as its own body.
    {
        let mut front = Front2::new(&names, frame, &sources, a).unwrap_or_else(|e| panic!("front2: {e}"));
        let mut store = Store::new(front.index.lang("sized"), front.frame, a);
        store.pointers = Pointers::find(&front.index, front.resolver);
        let (mut buckets, mut consts) = (None, Consts::default());
        store.ensure(&names, &mut front.keys, Want::Impl(imp)).unwrap_or_else(|e| panic!("the impl's head: {e:?}"));
        let k = front.keys.filed_member(checker_of(imp), MemberKind::Fn, "from_prim").expect("`from_prim` filed as a member of `impl ErrorKind`");
        let eq = front.index.lang("eq").expect("core's `eq` lang item (`PartialEq`)");
        store.trace = Some(TraceAsk { trait_: Def(checker_of(eq).0), self_kind: Some(TyKind::Uint(UintTy::U32)), goals: GOALS, cands: CANDS });
        let clean = {
            let mut r = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: Some((&mut consts, prepared)) };
            walks_way(&mut r, prepared, LowerTarget::Member(k), "1 from_prim (fresh)", false)
        };
        store.trace = None;
        // The local items its lowering kept (locals.rs `Locals::keep`).
        let mut inner: Option<blaze_id::ItemKey> = None;
        match store.locals.body(k) {
            None => println!("1 from_prim: NO local items kept for the body"),
            Some(locals) => {
                println!("1 from_prim: {} local items kept", locals.rows.len());
                for row in locals.rows.iter() {
                    println!("  local {:?} `{}` at {}..{} in scope {} ({:?})", row.kind, row.name, row.range.start, row.range.end, row.scope, row.key);
                    match store.locals.head(checker_of(row.key)) {
                        Some(h) => println!("    its text: {}", h.text),
                        None => println!("    no local head kept (a kind whose head is not made from its text)"),
                    }
                    if matches!(row.kind, LocalKind::Const) && inner.is_none() {
                        inner = Some(row.key);
                    }
                }
            }
        }
        match inner {
            None => println!("1 inner const: none kept, so none checked"),
            Some(c) => {
                let mut r = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: Some((&mut consts, prepared)) };
                walks_way(&mut r, prepared, LowerTarget::Free(c), "1 inner const (as its own body)", true);
            }
        }
        println!(
            "1 state after: {} fn heads, {} impl heads (`impl ErrorKind` {}), `PartialEq`'s trait row {}",
            store.fns.len(),
            store.impls.len(),
            if store.impls.contains_key(&checker_of(imp)) { "held" } else { "NOT held" },
            if store.trait_defs.contains_key(&checker_of(eq)) { "held" } else { "not held" }
        );
        println!("1 from_prim the walk's way on a fresh store: {}", if clean { "CLEAN (h1 and h2 do not reproduce it alone)" } else { "REFUSED (see above)" });
    }

    // 2. Fresh store; std's `decode_repr` checked first (its check makes the head of `impl
    //    ErrorKind` by its own lookup, as in the walk); then `from_prim` the walk's way.
    if let Some(caller) = caller {
        let mut front = Front2::new(&names, frame, &sources, a).unwrap_or_else(|e| panic!("front2: {e}"));
        let mut store = Store::new(front.index.lang("sized"), front.frame, a);
        store.pointers = Pointers::find(&front.index, front.resolver);
        let (mut buckets, mut consts) = (None, Consts::default());
        let caller_clean = {
            let mut r = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: Some((&mut consts, prepared)) };
            walks_way(&mut r, prepared, LowerTarget::Free(caller), "2 decode_repr (the caller, first)", false)
        };
        let k = match front.keys.filed_member(checker_of(imp), MemberKind::Fn, "from_prim") {
            Some(k) => {
                println!("2 the caller's check filed `from_prim` (its impl's head made by that check)");
                k
            }
            None => {
                println!("2 the caller's check did NOT file `from_prim`: the impl's head is made here by `Want::Impl`");
                store.ensure(&names, &mut front.keys, Want::Impl(imp)).unwrap_or_else(|e| panic!("the impl's head: {e:?}"));
                front.keys.filed_member(checker_of(imp), MemberKind::Fn, "from_prim").expect("`from_prim` filed as a member of `impl ErrorKind`")
            }
        };
        let eq = front.index.lang("eq").expect("core's `eq` lang item (`PartialEq`)");
        store.trace = Some(TraceAsk { trait_: Def(checker_of(eq).0), self_kind: Some(TyKind::Uint(UintTy::U32)), goals: GOALS, cands: CANDS });
        let clean = {
            let mut r = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: Some((&mut consts, prepared)) };
            walks_way(&mut r, prepared, LowerTarget::Member(k), "2 from_prim (after its caller)", false)
        };
        println!(
            "2 state after: {} fn heads, {} impl heads; the caller {}",
            store.fns.len(),
            store.impls.len(),
            if caller_clean { "checked clean" } else { "did not check clean" }
        );
        println!(
            "2 from_prim after its caller: {}",
            if clean { "CLEAN: the refusal needs more of the walk's state than its caller's (h3: run the walk with a cap, see the report)" } else { "REFUSED: the caller's check leaves the state that refuses it (h3, small)" }
        );
    }
}

/// A text printed with each run of white space as one space (a header over several lines on
/// one line), with no copy made.
struct Words<'t>(&'t str);

impl core::fmt::Display for Words<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for (i, w) in self.0.split_whitespace().enumerate() {
            if i > 0 {
                f.write_str(" ")?;
            }
            f.write_str(w)?;
        }
        Ok(())
    }
}

/// The most impl headers part 3 prints (rule 18).
const MOST_HEADERS: usize = 64;

/// 3. THE WALK'S PROGRAM (lane RF13, step 2). Parts 1 and 2 resolve an edition-2024 program; the
/// entry programs are `blaze-2026.11` ones, whose `std` is bstd (run.rs `build_new`:
/// `library_set::toolchain_for`, `ProgramTexts`), so their names hold more crates and more
/// impls. Over THAT program: every impl header naming `PartialEq` and `u32` with its crate; the
/// whole program's answer of `u32: PartialEq<_>` (`answer_impls`, `Seen::Program`), each impl
/// with its crate; then `from_prim` the walk's way with the trace on (each candidate with its
/// crate), once as the tree checks it (its crate's impls only: buckets.rs `Seen`) and once with
/// `Buckets::plant_whole_program_sight` (every crate's, as before the fix). The lead is proved
/// when the planted check is REFUSED with a second held candidate of a crate downstream of core
/// and the tree's is CLEAN.
///
/// Expected lines: `3 from_prim, its crate's impls: CLEAN` and, if the lead holds, `3 from_prim,
/// every crate's impls (planted): REFUSED`.
#[test]
fn core_from_prim_in_the_walks_program() {
    use blaze_ladder::front2::buckets::Buckets;
    use blaze_ladder::front2::check::answer_impls;
    use blaze_ladder::program_texts::ProgramTexts;
    use blaze_ladder::run::{NewFront, OverflowChecks};

    let frame: &'static Frame = Box::leak(Box::new(Frame::with_reserve(32 << 30, blaze_mem::tag::Tag::Infer).expect("the test's frame")));
    let erased: &'static Erased<&'static Frame> = frame.alloc(Erased::new(frame));
    let a = erased.handle();
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let sources = [SourceFile { path: "src/main.rs", text: MAIN }];
    let new = NewFront { package: "entry", edition: "blaze-2026.11", shelf: Some(&shelf), overflow: OverflowChecks::On };
    let panic = blaze_build::Panic::Unwind;
    let tc = blaze_ladder::library_set::toolchain_for(&sources, "src/main.rs", new, panic, a).unwrap_or_else(|e| panic!("the walk's toolchain: {e}"));
    let texts = ProgramTexts::new(&sources, "src/main.rs", new, &tc, panic).unwrap_or_else(|e| panic!("the walk's texts: {e}"));
    let user = texts.user();
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    println!("3 the walk's program: {} crates", names.crates().len());
    let core = crates.iter().position(|c| c.name == "core").expect("the toolchain's `core`");
    let imp = names
        .items(core)
        .map(|(k, _, _)| k)
        .find(|&k| names.item_text(k).is_some_and(|t| t.impl_header.is_some_and(|h| h.contains("ErrorKind") && !h.contains(" for ")) && t.text.contains("fn from_prim")))
        .expect("core's `impl ErrorKind` with `from_prim`");
    let prepared = Prepares::new_in(blaze_own::testing::bump());

    // Every impl header of the program naming both `PartialEq` and `u32`, with its crate (a
    // header a macro wrote shows its expansion's text, `$t` replaced).
    {
        let front = Front2::new(&names, frame, &sources, a).unwrap_or_else(|e| panic!("front2: {e}"));
        let mut shown = 0usize;
        for at in front.index.impls.iter().filter(|at| at.header.contains("PartialEq") && at.header.contains("u32")) {
            if shown == MOST_HEADERS {
                println!("  (more headers past {MOST_HEADERS})");
                break;
            }
            shown += 1;
            let krate = names.crates().get(at.krate).map_or("?", |c| c.name);
            println!("3 header [crate {krate}]: {}", Words(at.header));
        }
        println!("3 headers naming `PartialEq` and `u32`: {shown}");
    }

    // Twice over fresh state: as the tree checks it, then with every crate's impls planted.
    for planted in [false, true] {
        let what = if planted { "3 from_prim, every crate's impls (planted)" } else { "3 from_prim, its crate's impls" };
        let mut front = Front2::new(&names, frame, &sources, a).unwrap_or_else(|e| panic!("front2: {e}"));
        let mut store = Store::new(front.index.lang("sized"), front.frame, a);
        store.pointers = Pointers::find(&front.index, front.resolver);
        let mut buckets = Some(Buckets::for_source(&names, &front.index));
        if planted {
            if let Some(b) = buckets.as_mut() {
                b.plant_whole_program_sight();
            }
        }
        let mut consts = Consts::default();
        store.ensure(&names, &mut front.keys, Want::Impl(imp)).unwrap_or_else(|e| panic!("the impl's head: {e:?}"));
        let k = front.keys.filed_member(checker_of(imp), MemberKind::Fn, "from_prim").expect("`from_prim` filed as a member of `impl ErrorKind`");
        let eq = front.index.lang("eq").expect("core's `eq` lang item (`PartialEq`)");
        let eq_key = checker_of(eq);
        store.trace = Some(TraceAsk { trait_: Def(eq_key.0), self_kind: Some(TyKind::Uint(UintTy::U32)), goals: GOALS, cands: CANDS });
        let mut r = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: Some((&mut consts, prepared)) };
        let clean = walks_way(&mut r, prepared, LowerTarget::Member(k), what, false);
        r.store.trace = None;
        // The whole program's answer, after the check (a question at ground arguments asks it).
        if !planted {
            match answer_impls(&mut r, eq_key, tc::Shape::Uint(tc::UintTy::U32)) {
                None => println!("3 the whole program's `u32: PartialEq<_>`: a gap (a candidate's header refused)"),
                Some(impls) => {
                    println!("3 the whole program's `u32: PartialEq<_>`: {} impls", impls.len());
                    for i in impls.iter() {
                        println!("  {} [crate {}]", ItemShown::of(&names, &*r.keys, *i), crate_name(&names, &*r.keys, *i));
                    }
                }
            }
        }
        println!("{what}: {}", if clean { "CLEAN" } else { "REFUSED" });
    }
}
