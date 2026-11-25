//! A cast to a trait object takes no expectation for its operand (blaze-typeck rules/cast.rs
//! `dyn_cast_target`): rustc checks a cast's operand with `check_expr_without_expectation`.
//! Propagating `dyn Trait` into the operand's `if`/`match` arms failed each arm against
//! `dyn Trait` (E0308 "found `i32`, expected `dyn ToSql`") where rustc unifies the arms and
//! coerces the whole — the shape of rusqlite's `params![..]` (`&$x as &dyn ToSql`), six false
//! positives in omt's omp-stats. For a concrete target the hint stays (`&0 as &u8` infers
//! `0: u8`).
//!
//! It needs no toolchain: the program is `#![no_core]` (the harness of tests/front2_unsafety.rs).

use blaze_ladder::crates::{Inputs, Toolchain};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::body::{BodyError, lower_fn};
use blaze_ladder::front2::check::{BuildOrder, CheckFail, Records, check_body, eval_len};
use blaze_ladder::front2::consts::Consts;
use blaze_ladder::front2::heads::{HeadError, Store};
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::front2::types::Why;
use blaze_ladder::program_texts::ProgramTexts;
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::NewFront;
use blaze_lower::scoped::Prepares;
use blaze_own::Alloc;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};

const PRELUDE: &str = r#"#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = "pointee_sized"]
trait PointeeSized {}

#[lang = "meta_sized"]
trait MetaSized: PointeeSized {}

#[lang = "sized"]
trait Sized: MetaSized {}

#[lang = "unsize"]
trait Unsize<U: ?Sized> {}

trait ToSql {}
impl ToSql for i32 {}
impl ToSql for &str {}
trait Empty {}

const PENDING: &str = "pending";
const COMPLETE: &str = "complete";
"#;

const MOST_LENS: u32 = 8;

/// Each fn of `fns` in the crate `PRELUDE` + `text`, lowered and checked in order over one
/// store: its name and outcome ("clean", or each diagnostic's code and words, or the failure).
fn check_fns(text: &str, fns: &[&str]) -> Vec<(String, String)> {
    let program = format!("{PRELUDE}{text}");
    let files = [SourceFile { path: "src/main.rs", text: &program }];
    let tc = Toolchain::none(Alloc::global());
    let new = NewFront { package: "ladder", edition: "2021", shelf: None, overflow: blaze_build::OverflowChecks::On };
    let texts = ProgramTexts::new(&files, "src/main.rs", new, &tc, blaze_build::Panic::Unwind).unwrap_or_else(|e| panic!("the program's texts: {e}"));
    let user = texts.user();
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame");
    let mut front = Front2::new(&names, &frame, &files, Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut store = Store::new(front.index.lang("sized"), front.frame, Alloc::global());
    let (mut buckets, mut consts) = (None, Consts::default());
    let mut got = Vec::new();
    for &n in fns {
        let found = names::path(&names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments: &[n] }, Alloc::global()).unwrap_or_else(|e| panic!("`{n}`: {e}"));
        let k = found.whole(1).unwrap_or_else(|| panic!("`{n}` not named")).key;
        let mut lens = 0;
        let outcome = loop {
            match lower_fn(&names, prepared, &mut front.keys, &mut store, k) {
                Err(BodyError::Convert { why: Some(Why::NeedConst(c, _)), .. }) | Err(BodyError::Head { error: HeadError::Item(_, Why::NeedConst(c, _)), .. }) if lens < MOST_LENS => {
                    lens += 1;
                    let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: Some((&mut consts, prepared)) };
                    if let Err(e) = eval_len(&mut records, c) {
                        break format!("not lowered: the constant {c:x} its body names: {e}");
                    }
                }
                Err(e) => break format!("not lowered: {e}"),
                Ok(body) => {
                    let view = body.lowered.view();
                    let spell = Spelling::of(&view).unwrap_or_else(|e| panic!("spelling `{n}`: {e:?}"));
                    let scope = body.scope(&spell);
                    let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: Some((&mut consts, prepared)) };
                    break match check_body(&mut records, &body, &scope) {
                        Ok(done) if !done.diags.is_empty() => {
                            let d: Vec<String> = done.diags.iter().map(|d| format!("{} at {:?}", d.code.code(), d.at)).collect();
                            format!("diags {d:?}")
                        }
                        Ok(_) => "clean".to_string(),
                        Err(CheckFail::Diags { diags, .. }) => {
                            let d: Vec<String> = diags.iter().map(|(code, _, about)| format!("{} {about}", code.code())).collect();
                            d.join("; ")
                        }
                        Err(e) => format!("not checked: {e} ({e:?})"),
                    };
                }
            }
        };
        got.push((n.to_string(), outcome));
    }
    got
}

/// 1a. The `params!` shape: `&if c { 1 } else { 0 } as &dyn ToSql` is `&dyn ToSql` (the arms
/// unify to `i32` and the whole coerces); the `&str` constants are the same. The direct
/// `&1 as &dyn ToSql` still works.
#[test]
fn a_cast_to_a_trait_object_takes_no_expectation_for_its_operand() {
    let text = r#"
fn coerce_if_int() -> &'static [&'static dyn ToSql] { &[&if true { 1 } else { 0 } as &dyn ToSql] }
fn coerce_if_str() -> &'static [&'static dyn ToSql] { &[&if true { PENDING } else { COMPLETE } as &dyn ToSql] }
fn coerce_direct() -> &'static [&'static dyn ToSql] { &[&1 as &dyn ToSql] }
"#;
    let got = check_fns(text, &["coerce_if_int", "coerce_if_str", "coerce_direct"]);
    assert_eq!(
        got,
        vec![
            ("coerce_if_int".to_string(), "clean".to_string()),
            ("coerce_if_str".to_string(), "clean".to_string()),
            ("coerce_direct".to_string(), "clean".to_string()),
        ]
    );
}

/// 1b. CONTROL: `&if c { 1 } else { 0 } as &dyn Empty` — `i32: Empty` does not hold, so the
/// coercion of the whole fails: an E0308 at the cast, never at the arms (which are `i32`).
#[test]
fn control_a_failed_unsize_is_still_an_error_at_the_cast() {
    let text = r#"
fn coerce_bad() -> &'static [&'static dyn Empty] { &[&if true { 1 } else { 0 } as &dyn Empty] }
"#;
    let got = check_fns(text, &["coerce_bad"]);
    assert_eq!(got.len(), 1);
    let (name, outcome) = &got[0];
    assert_eq!(name, "coerce_bad");
    // The failure is of the whole coercion at the cast (`&i32` to `&dyn Empty`), never of the
    // arms (which are `i32`): a non-primitive-cast or a mismatch, both at the cast.
    assert!(
        outcome.contains("E0605") || outcome.contains("E0308"),
        "coerce_bad: want an error at the cast (E0605/E0308), got {outcome:?}"
    );
    assert!(
        outcome.contains("expected `&dyn Empty`"),
        "coerce_bad: the error must be the cast's (found `&i32`, expected `&dyn Empty`), got {outcome:?}"
    );
}
