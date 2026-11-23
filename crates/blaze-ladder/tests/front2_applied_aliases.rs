//! Associated type arguments and callable type aliases through the project's body checker.

use blaze_ladder::crates::{Inputs, Toolchain};
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::program_texts::ProgramTexts;
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::NewFront;
use blaze_own::Alloc;

const PRELUDE: &str = include_str!("../../blaze-cli/tests/ladder/front2/x0-clo-04-generic-fn/src/prelude.rs");
const HEADER: &str = r#"#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures)]
#![no_core]
#![no_main]
mod prelude;
use prelude::*;
"#;

const PARTS_TRAIT: &str = "
trait Parts {
    type Part<U>;
    fn part<U>(&self, value: U) -> Self::Part<U>;
}
";

const PARTS_IMPL: &str = "
struct Identity;
impl Parts for Identity {
    type Part<U> = U;
    fn part<U>(&self, value: U) -> Self::Part<U> { value }
}
fn pair<T: Parts>(source: &T) -> (T::Part<u8>, <T as Parts>::Part<bool>) {
    (source.part(7u8), source.part(true))
}
fn concrete(source: &Identity) -> (u8, bool) { pair(source) }
fn bound<T>(source: &T) -> (u8, bool)
where T: Parts<Part<u8> = u8>, T: Parts<Part<bool> = bool> {
    (source.part(7u8), source.part(true))
}
";

const WRITTEN: &str = "
trait Accept<V> { fn accept(&self, value: V); }
trait FromSelf: Accept<Self::Part<bool>> + Parts {}
fn from_self<T: FromSelf>(t: &T, value: <T as Parts>::Part<bool>) { t.accept(value); }
fn from_param<T: Accept<T::Part<u8>> + Parts>(t: &T, value: <T as Parts>::Part<u8>) {
    t.accept(value);
}
";

const BOXED: &str = r#"
#[lang = "owned_box"]
struct Box<T: ?Sized>(*mut T);
#[lang = "deref"]
trait Deref: PointeeSized {
    #[lang = "deref_target"]
    type Target: ?Sized;
    fn deref(&self) -> &Self::Target;
}
impl<T: ?Sized> Deref for Box<T> {
    type Target = T;
    fn deref(&self) -> &T { unsafe { &*self.0 } }
}
type Callback<A, B> = Box<dyn Fn(A) -> B>;
struct Handler(Callback<u16, bool>);
impl Handler {
    fn apply(&self, parent: u16) -> bool { (self.0)(parent) }
}
"#;

/// Check exactly the named bodies. A refusal or a missing record cannot satisfy a diagnostic.
fn check(tail: &str, cases: &[(&str, &[&str])]) {
    let source = [HEADER, tail].concat();
    let files = [SourceFile { path: "src/main.rs", text: &source }, SourceFile { path: "src/prelude.rs", text: PRELUDE }];
    let alloc = Alloc::global();
    let tc = Toolchain::none(alloc);
    let new = NewFront { package: "applied", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::Off };
    let texts = ProgramTexts::new(&files, "src/main.rs", new, &tc, blaze_build::Panic::Unwind).expect("fixture texts");
    let user = texts.user();
    let inputs = Inputs::new(&user, &tc).expect("fixture crates");
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).expect("fixture names");
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("fixture frame");
    let mut world = ProjectWorld::new(&names, &frame, &files, alloc).expect("front2");
    let bodies = world.bodies(alloc);
    for &(name, expected) in cases {
        let mut found = bodies.iter().filter_map(|&(body, _)| {
            let matches = match body {
                BodyRef::Free(key) => names.item_text(key)?.name? == name,
                BodyRef::Member { name: member, is_trait, .. } => {
                    let prefix = if is_trait { "trait::" } else { "impl::" };
                    name.strip_prefix(prefix) == Some(member)
                }
            };
            matches.then_some(body)
        });
        let body = found.next().unwrap_or_else(|| panic!("missing body of {name}"));
        assert!(found.next().is_none(), "ambiguous fixture body {name}");
        let verdict = world.check(body, alloc);
        assert!(verdict.reads.is_some() && verdict.gaps.is_empty() && verdict.refused.is_none() && verdict.undecided.is_empty(), "{name}: {verdict:?}");
        let codes: Vec<_> = verdict.diagnostics.iter().map(|d| d.code).collect();
        assert_eq!(codes.as_slice(), expected, "{name}: {verdict:?}");
    }
}

#[test]
fn trait_self_gat_keeps_each_calls_argument() {
    let source = [PARTS_TRAIT, PARTS_IMPL].concat();
    check(&source, &[("impl::part", &[]), ("pair", &[]), ("concrete", &[]), ("bound", &[])]);
    println!("Self::Part<U> calls: u8 and bool");
}

#[test]
fn trait_self_gat_arguments_are_distinct() {
    let compare = "
trait Compare: Parts {
    fn same(&self, x: u8) -> Self::Part<u8> { self.part(x) }
    fn different(&self, x: u8) -> Self::Part<bool> { self.part(x) }
}
";
    let source = [PARTS_TRAIT, compare].concat();
    check(&source, &[("trait::same", &[]), ("trait::different", &["E0308"])]);
    // Model the lost arguments in source so the same two bodies go through front2 again.
    // The method still takes U, but now both calls return the one associated type.
    let erased = source.replace("Part<U>", "Part").replace("Part<u8>", "Part").replace("Part<bool>", "Part");
    check(&erased, &[("trait::same", &[]), ("trait::different", &[])]);
    println!("Self::Part<u8> as Self::Part<bool>: E0308, with arguments erased: clean");
}

#[test]
fn gat_arguments_survive_lookup_in_written_bounds() {
    let source = [PARTS_TRAIT, WRITTEN].concat();
    check(&source, &[("from_self", &[]), ("from_param", &[])]);
    println!("written bounds: Self::Part<bool> and T::Part<u8>");
}

#[test]
fn applied_boxed_dyn_fn_field_is_callable() {
    check(BOXED, &[("impl::apply", &[])]);
    println!("(self.0)(parent): u16 -> bool");
}
