//! Where a block's TAIL expression's temporaries die, by edition: tracing's `event!` shape
//! (tracing 0.1.44, an edition-2018 crate: `valueset_all!` expands to `{ ..;
//! $fields.value_set_all(&[Some(&debug(&x) as &dyn Value), ..]) }`, the block passed to a
//! closure called at once).
//!
//! The rule. The Reference, "Destructors", "Temporary scopes": before 2024 the temporaries of
//! a block's tail expression live to the end of the statement enclosing the block, so the
//! `ValueSet` the block yields may borrow them (`debug(&x)`, the array); from 2024 ("Tail
//! expression temporary scope", RFC 3606) they are dropped when the block ends, and the same
//! body is E0716. rustc decides by the edition of the BLOCK's span (`blk.span.edition()`), so
//! a block written by a 2018 crate's macro keeps the old rule inside a 2024 body.
//!
//! tempo (a 2024 workspace) has 868 plain-item notes and 40 macro-body notes whose "taken" and
//! "later used" ranges are both a whole tracing macro call (c2300wave72 borrow-notes.txt): the
//! old rule not applied to tracing's blocks. This test pins the rule in ONE crate, so in each
//! edition (the cross-crate case, a 2018 macro's block in a 2024 body, needs the front end's
//! per-expression edition, blaze-lower typeck.rs `expr_editions`). Each body comes with and
//! without the closure call, to tell the closure's signature from the temporary scope.
//! NOT YET RUN: written by a write-only lane (2026-11-20). Every verdict printed first.

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::refusal::SourceFile;

const SOURCE: &str = r#"
pub trait Value {}
impl Value for u64 {}
pub struct Debug<T>(T);
impl<T> Value for Debug<T> {}
pub fn debug<T>(t: T) -> Debug<T> { Debug(t) }
pub struct FieldSet;
pub struct ValueSet<'a> { pub fields: &'a FieldSet, pub values: &'a [Option<&'a (dyn Value + 'a)>] }
impl FieldSet {
    pub fn value_set_all<'v>(&'v self, values: &'v [Option<&'v (dyn Value + 'v)>]) -> ValueSet<'v> {
        ValueSet { fields: self, values }
    }
}
static FIELDS: FieldSet = FieldSet;
fn take(v: ValueSet<'_>) { let _ = v.values.len(); }

fn through_closure(x: u64) {
    (|value_set: ValueSet<'_>| { let _ = value_set.values.len(); })({ FIELDS.value_set_all(&[Some(&debug(&x) as &dyn Value)]) });
}
fn through_call(x: u64) {
    take({ FIELDS.value_set_all(&[Some(&debug(&x) as &dyn Value)]) });
}
fn no_block(x: u64) {
    take(FIELDS.value_set_all(&[Some(&debug(&x) as &dyn Value)]));
}

fn look(v: &ValueSet<'_>) { let _ = v.values.len(); }
fn look_mut(v: &mut ValueSet<'_>) { let _ = v.values.len(); }
fn look_values(v: &[Option<&dyn Value>]) { let _ = v.len(); }
impl<'a> ValueSet<'a> {
    fn count(&self) -> usize { self.values.len() }
}

fn borrowed(x: u64) {
    look(&{ FIELDS.value_set_all(&[Some(&debug(&x) as &dyn Value)]) });
}
fn borrowed_mut(x: u64) {
    look_mut(&mut { FIELDS.value_set_all(&[Some(&debug(&x) as &dyn Value)]) });
}
fn receiver(x: u64) {
    let _ = { FIELDS.value_set_all(&[Some(&debug(&x) as &dyn Value)]) }.count();
}
fn field(x: u64) {
    look_values({ FIELDS.value_set_all(&[Some(&debug(&x) as &dyn Value)]) }.values);
}
fn match_scrutinee(x: u64) {
    look(&match FIELDS.value_set_all(&[Some(&debug(&x) as &dyn Value)]) { v => v });
}
fn match_arm_binding(x: u64) {
    look(&match x { a => FIELDS.value_set_all(&[Some(&debug(&a) as &dyn Value)]) });
}

fn main() {}
"#;

/// Each body's verdict in a crate of `edition`: `Some(true)` a borrow finding, `Some(false)`
/// clean, `None` refused (not checked), each printed.
fn verdicts(edition: &str) -> Vec<(&'static str, Option<bool>)> {
    verdicts_of(edition, &["through_closure", "through_call", "no_block"])
}

/// [`verdicts`] of the bodies `bodies`.
fn verdicts_of(edition: &str, bodies: &[&'static str]) -> Vec<(&'static str, Option<bool>)> {
    let alloc = blaze_own::testing::heap().alloc();
    let home = std::env::var("HOME").expect("HOME");
    let tc = Toolchain::open(&mut DirShelf::new(format!("{home}/.blaze/v2/toolchain"))).expect("installed toolchain");
    let root = "/bte/src/main.rs";
    let files = [(root.to_owned(), SOURCE.to_owned())];
    let user = UserCrate {
        name: "bte", dir: "/bte/", root, files: &files,
        edition: edition::PackageEdition::of_manifest(edition).expect("an edition"), panic: blaze_build::Panic::Unwind,
    };
    let inputs = Inputs::new(&user, &tc).expect("fixture crate");
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).expect("fixture names");
    let sources = [SourceFile { path: "src/main.rs", text: SOURCE }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("checker frame");
    let mut world = ProjectWorld::new(&names, &frame, &sources, alloc).expect("body checker");
    let key_of = |name: &str| names.items(0).find_map(|(key, _, _)| (names.item_text(key)?.name == Some(name)).then_some(key)).expect(name);
    let mut out = Vec::new();
    for &name in bodies {
        let (types, v) = world.check_borrows(BodyRef::Free(key_of(name)), alloc);
        println!("{edition} {name}: types clean {}, {v:?}", types.is_clean());
        assert!(types.is_clean(), "{edition} `{name}`'s types: {types:?}");
        let got = if v.refused.is_some() { None } else { Some(!v.findings.is_empty()) };
        out.push((name, got));
    }
    out
}

/// 2021: the tail's temporaries live to the end of the call statement: every body is clean.
/// TWIN, 2024: the block's tail temporaries die at the block's end, before the `ValueSet` is
/// used: both block bodies are E0716; the body with no block is clean in both (its
/// temporaries are the statement's).
#[test]
fn a_block_tails_temporaries_die_by_edition() {
    let old = verdicts("2021");
    let new = verdicts("2024");
    println!("2021: {old:?}\n2024: {new:?}");
    let want_old = [("through_closure", Some(false)), ("through_call", Some(false)), ("no_block", Some(false))];
    let want_new = [("through_closure", Some(true)), ("through_call", Some(true)), ("no_block", Some(false))];
    assert_eq!(old, want_old, "2021: a block tail's temporaries live to the end of the enclosing statement");
    assert_eq!(new, want_new, "2024: a block tail's temporaries are dropped at the block's end");
}

/// The block's value in a place context (`&`, `&mut`, a receiver's autoref, a field read) is a
/// temporary of the enclosing statement (the Reference, "Temporary scopes"), dropped BEFORE the
/// tail's temporaries it borrows (rustc `as_temp` schedules its drop after evaluating into it;
/// blaze-flow-lower lib.rs `settle_dest`). 2021: each clean (tracing's `dispatch(&valueset!(..))`,
/// tempo's largest false group). TWIN, 2024: the tail's temporaries die at the block's `}`
/// while the `ValueSet` is used: each found (E0716).
/// A `match`'s value likewise drops before its scrutinee's temporaries, which live to the
/// statement's end in every edition: `match_scrutinee` clean in both. Its TWIN,
/// `match_arm_binding`, borrows the arm's binding `a`, dead at the arm's end: found in both
/// (E0597).
#[test]
fn a_borrowed_blocks_value_drops_before_its_tail_temporaries() {
    let names = ["borrowed", "borrowed_mut", "receiver", "field", "match_scrutinee", "match_arm_binding"];
    let old = verdicts_of("2021", &names);
    let new = verdicts_of("2024", &names);
    println!("2021: {old:?}\n2024: {new:?}");
    let want_old = [
        ("borrowed", Some(false)),
        ("borrowed_mut", Some(false)),
        ("receiver", Some(false)),
        ("field", Some(false)),
        ("match_scrutinee", Some(false)),
        ("match_arm_binding", Some(true)),
    ];
    let want_new = [
        ("borrowed", Some(true)),
        ("borrowed_mut", Some(true)),
        ("receiver", Some(true)),
        ("field", Some(true)),
        ("match_scrutinee", Some(false)),
        ("match_arm_binding", Some(true)),
    ];
    assert_eq!(old, want_old, "2021: the block's value drops before its tail's temporaries");
    assert_eq!(new, want_new, "2024: the tail's temporaries die at the block's end");
}
