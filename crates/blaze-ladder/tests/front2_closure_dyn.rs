//! A closure borrowed as a Fn trait object reads its signature from the trait object
//! (blaze-typeck rules/op.rs `expr_borrow` → `rvalue_hint`, closure.rs
//! `deduce_closure_signature` on `ty::Dynamic`): the closure's parameters come from the expected
//! `dyn Fn(&A) -> R`, and the closure coerces to the object. A hard `HasType(dyn Fn)` starves it
//! (E0282 "the type of parameter #0 of the closure (none written, none expected)"), and a flat
//! `None` loses the coercion ("found `&{closure}`, expected `&dyn Fn`") — the shapes of
//! omp-stats session.rs (`&mut |entry| ..` as `&mut dyn FnMut(&Value)`) and pa-agent agent.rs
//! (`&|m| ..` as `&dyn Fn(&AgentMessage) -> bool`).
//!
//! It needs no toolchain: the program is `#![no_core]` with the Fn lang traits of
//! tests/front2_foreign_closure.rs.

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

const PRELUDE: &str = r#"#![feature(no_core, lang_items, unboxed_closures, rustc_attrs)]
#![no_core]
#![no_main]

#[lang = "pointee_sized"]
trait PointeeSized {}

#[lang = "meta_sized"]
trait MetaSized: PointeeSized {}

#[lang = "sized"]
trait Sized: MetaSized {}

#[lang = "tuple_trait"]
trait Tuple {}

#[lang = "unsize"]
trait Unsize<U: ?Sized> {}

#[lang = "fn_once"]
#[rustc_paren_sugar]
trait FnOnce<Args: Tuple> {
    #[lang = "fn_once_output"]
    type Output;
    extern "rust-call" fn call_once(self, args: Args) -> Self::Output;
}

#[lang = "fn_mut"]
#[rustc_paren_sugar]
trait FnMut<Args: Tuple>: FnOnce<Args> {
    extern "rust-call" fn call_mut(&mut self, args: Args) -> Self::Output;
}

#[lang = "fn"]
#[rustc_paren_sugar]
trait Fn<Args: Tuple>: FnMut<Args> {
    extern "rust-call" fn call(&self, args: Args) -> Self::Output;
}

fn visit(v: &mut dyn FnMut(&u8)) {}
fn take(f: &dyn Fn(&u8) -> bool) {}
"#;

const MOST_LENS: u32 = 8;

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

/// session.rs: a closure passed as `&mut dyn FnMut(&u8)` reads its parameter from the object.
#[test]
fn a_mut_closure_param_comes_from_the_dyn_fn_object() {
    let text = r#"
fn go(bytes: &[u8]) {
    visit(&mut |entry| {
        let _ = entry;
    });
}
"#;
    let got = check_fns(text, &["go"]);
    assert_eq!(got, vec![("go".to_string(), "clean".to_string())]);
}

/// agent.rs: `&|m| ..` passed as `&dyn Fn(&u8) -> bool` coerces to the object.
#[test]
fn a_closure_borrowed_as_a_dyn_fn_coerces() {
    let text = r#"
fn go() {
    take(&|m| *m == 0);
}
"#;
    let got = check_fns(text, &["go"]);
    assert_eq!(got, vec![("go".to_string(), "clean".to_string())]);
}

/// CONTROL: a closure whose body does not match the object's signature is still an error.
#[test]
fn control_a_closure_of_the_wrong_shape_is_still_an_error() {
    let text = r#"
fn go() {
    take(&|m| *m + 1);
}
"#;
    let got = check_fns(text, &["go"]);
    assert_eq!(got.len(), 1);
    let (name, outcome) = &got[0];
    assert_eq!(name, "go");
    assert!(outcome.contains("E0308") || outcome.contains("E0277") || outcome.contains("E0282") || outcome.contains("E0605"), "go: want an error, got {outcome:?}");
}
