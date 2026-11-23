//! A BODY SEES THE IMPLS OF ITS CRATE AND OF THE CRATES IT LINKS, NEVER OF A CRATE DOWNSTREAM
//! (front2/buckets.rs `Seen`, lane RF13, 2026-11-18).
//!
//! The entry programs refused core's `<impl ErrorKind>::from_prim` with E0282 at its first
//! `v == (ErrorKind::NotFound as _)` (expression #3, a cast sharing its range with 401 nodes:
//! `from_prim!` expands in the fn). `_` is inferred from the goal `u32: PartialEq<?T>`, whose
//! one impl in core is `impl PartialEq for u32`; a program that also links a crate with `impl
//! PartialEq<Mine> for u32` gave that goal a second candidate that holds, and the answer of the
//! question (`PartialEq` at `u32`) was the whole program's, so core's body saw it. rustc checks
//! core seeing core's impls alone: by the orphan rules an impl downstream of core names a type
//! or trait of its own crate, which core's text cannot name, so the dependency closure is
//! complete for every goal core's body can write.
//!
//! Two crates: the toolchain's `core` (upstream: `from_prim`, the real body) and the program's
//! crate (downstream: `pub struct Mine; impl PartialEq<Mine> for u32`, and `fn g` comparing
//! `1u32 == Mine`, which only its own impl answers). Each order over fresh state:
//! - core's body first, then `g`: both clean;
//! - `g` first, then core's body: both clean; core's answer holds no impl of the program's crate,
//!   the program's crate's answer holds its own impl;
//! - the control (rule 23), `g` first with `Buckets::plant_whole_program_sight` (every question
//!   sees every crate, as before the fix): core's body is E0282.
//!
//! Expected lines:
//! ```text
//! seen from: core first: from_prim clean, g clean
//! seen from: program first: g clean, from_prim clean
//! seen from: planted, program first: from_prim refused with E0282 (the control)
//! ```
//! It needs the installed toolchain under `$HOME/.blaze/v2/toolchain`, as tests/front2_main.rs does.

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::buckets::{Buckets, Seen};
use blaze_ladder::front2::check::{BuildOrder, CheckFail, Records, check_body};
use blaze_ladder::front2::consts::Consts;
use blaze_ladder::front2::heads::{Store, Want};
use blaze_ladder::front2::keys::{MemberKind, checker_of};
use blaze_ladder::front2::lower_made::{LowerTarget, lower_made};
use blaze_ladder::front2::receiver::Pointers;
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::refusal::SourceFile;
use blaze_lower::scoped::Prepares;
use blaze_mem::Frame;
use blaze_own::Erased;
use blaze_typeck::{DiagCode, Shape, UintTy};

const MAIN: &str = "pub struct Mine;

impl PartialEq<Mine> for u32 {
    fn eq(&self, _: &Mine) -> bool {
        true
    }
}

fn g() -> bool {
    1u32 == Mine
}

fn main() {
    let _ = g();
}
";

/// What one body's check came to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Clean,
    /// Refused with at least one E0282.
    E0282,
    /// Refused or not checked otherwise.
    Other,
}

/// `target` lowered and checked the walk's way (front2/reach.rs `reach_all`), its findings
/// printed under `what`.
fn check<'n>(r: &mut Records<'_, 'n>, prepared: &Prepares<'n>, target: LowerTarget, what: &str) -> Outcome {
    let body = match lower_made(r, prepared, target) {
        Ok(b) => b,
        Err(e) => {
            println!("{what}: not lowered: {e}");
            return Outcome::Other;
        }
    };
    let lowered = body.lowered.view();
    let spell = match Spelling::of(&lowered) {
        Ok(s) => s,
        Err(e) => {
            println!("{what}: not spelled: {e:?}");
            return Outcome::Other;
        }
    };
    let scope = body.scope(&spell);
    match check_body(r, &body, &scope) {
        Ok(done) if done.diags.is_empty() => Outcome::Clean,
        Ok(_) => Outcome::Other,
        Err(CheckFail::Diags { diags, .. }) => {
            for (code, at, about) in diags.iter() {
                println!("{what}: {} at {}..{}: {about}", code.code(), at.start, at.end);
            }
            match diags.iter().any(|(code, _, _)| *code == DiagCode::AnnotationsNeeded) {
                true => Outcome::E0282,
                false => Outcome::Other,
            }
        }
        Err(e) => {
            println!("{what}: not checked: {e}");
            Outcome::Other
        }
    }
}

#[test]
fn a_body_sees_its_crates_impls_whichever_is_checked_first() {
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
    let sources = [SourceFile { path: "src/main.rs", text: MAIN }];
    let core = crates.iter().position(|c| c.name == "core").expect("the toolchain's `core`");
    let program = 0usize;
    let imp = names
        .items(core)
        .map(|(k, _, _)| k)
        .find(|&k| names.item_text(k).is_some_and(|t| t.impl_header.is_some_and(|h| h.contains("ErrorKind") && !h.contains(" for ")) && t.text.contains("fn from_prim")))
        .expect("core's `impl ErrorKind` with `from_prim`");
    let g = names.items(program).map(|(k, _, _)| k).find(|&k| names.item_text(k).is_some_and(|t| t.text.starts_with("fn g()"))).expect("the program's `fn g`");
    let prepared = Prepares::new_in(blaze_own::testing::bump());

    // (planted, core's body first)
    for (planted, core_first) in [(false, true), (false, false), (true, false)] {
        let mut front = Front2::new(&names, frame, &sources, a).unwrap_or_else(|e| panic!("front2: {e}"));
        let mine_impl = front.index.impls.iter().find(|at| at.krate == program && at.header.contains("Mine")).map(|at| checker_of(at.key)).expect("the program's `impl PartialEq<Mine> for u32`");
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
        let eq = checker_of(front.index.lang("eq").expect("core's `eq` lang item (`PartialEq`)"));
        let mut r = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: Some((&mut consts, prepared)) };
        let (from_prim, g_got) = match core_first {
            true => {
                let f = check(&mut r, prepared, LowerTarget::Member(k), "from_prim");
                (f, check(&mut r, prepared, LowerTarget::Free(g), "g"))
            }
            false => {
                let g_got = check(&mut r, prepared, LowerTarget::Free(g), "g");
                (check(&mut r, prepared, LowerTarget::Member(k), "from_prim"), g_got)
            }
        };
        let order = if core_first { "core first" } else { "program first" };
        if planted {
            println!("seen from: planted, {order}: from_prim {from_prim:?} (the control wants E0282), g {g_got:?}");
            assert_eq!(from_prim, Outcome::E0282, "the control: with every crate's impls seen, core's `from_prim` is E0282 after the program's `g` (else the program's impl is not what refused it)");
            continue;
        }
        println!("seen from: {order}: from_prim {from_prim:?}, g {g_got:?}");
        assert_eq!(from_prim, Outcome::Clean, "core's `from_prim`, {order}: sees core's impls alone");
        assert_eq!(g_got, Outcome::Clean, "the program's `g` ({order}): `1u32 == Mine` is its own impl's");
        let u32_ = Shape::Uint(UintTy::U32);
        let Some(b) = r.buckets.as_ref() else { panic!("the checks made the buckets") };
        match b.impls_seen(eq, u32_, Seen::From(core)) {
            Some(got) => assert!(!got.contains(&mine_impl), "core's answer of `u32: PartialEq<_>` holds the program's impl ({order}): {got:?}"),
            None => panic!("core's body asked `u32: PartialEq<_>` ({order}) and its answer is not filed under `Seen::From(core)`"),
        }
        match b.impls_seen(eq, u32_, Seen::From(program)) {
            Some(got) => assert!(got.contains(&mine_impl), "the program's answer of `u32: PartialEq<_>` misses its own impl ({order}): {got:?}"),
            None => panic!("`g` asked `u32: PartialEq<_>` ({order}) and its answer is not filed under `Seen::From(program)`"),
        }
    }
}
