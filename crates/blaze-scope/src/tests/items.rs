//! Item-position expansion in the fill: derives and attribute macros, in the Reference's order
//! (the outermost attribute macro takes the item; derives run on what is left), so what they
//! write is in the tables (impls, items) like any written item.

use blaze_own::Alloc;
use blaze_surface::{Namespace, PathKind};

use super::entry::{Texts, input, resolved};
use crate::names::Written;

/// A `core` of two lines: the builtin `Clone` derive, and a `macro_rules!` attribute macro
/// (bstd's `main` shape: an `attr()` arm) that keeps its item and adds a fn beside it.
const DEP: &str = "#![no_core]
#[rustc_builtin_macro]
pub macro Clone($item:item) {}
#[macro_export]
macro_rules! wrap {
    attr() ($(#[$m:meta])* $v:vis fn $name:ident() {}) => {
        $(#[$m])* $v fn $name() {
            pub extern \"C\" fn hosted_start() {}
        }
        pub fn wrapped() {}
    };
}
";

const USER: &str = "#![no_core]
use dep::Clone;
#[derive(Clone)]
pub struct Placement;
#[dep::wrap]
#[derive(Clone)]
pub struct NotAFn;
#[dep::wrap]
pub fn main() {}
#[inline]
pub fn kept() {}
";

static TEXTS: Texts = Texts(&[&[("/user/lib.rs", USER)], &[("/dep/lib.rs", DEP)]]);

#[test]
fn a_builtin_derive_writes_its_impl() {
    let crates = [input("user", "/user/lib.rs", &[("dep", 1)]), input("dep", "/dep/lib.rs", &[])];
    let names = resolved(&crates, &TEXTS);
    let headers: std::vec::Vec<&str> = names.krate(0).tables.impls.iter().map(|i| i.header).collect();
    assert!(headers.iter().any(|h| h.contains("Clone") && h.contains("Placement")), "a Clone impl of Placement: {headers:?}");
}

/// A `macro_rules!` with function-like arms only, used as an attribute.
const FN_DEP: &str = "#![no_core]
#[macro_export]
macro_rules! plain {
    ($($t:tt)*) => {};
}
";

const FN_USER: &str = "#![no_core]
#[dep::plain]
pub fn f() {}
";

static FN_TEXTS: Texts = Texts(&[&[("/user/lib.rs", FN_USER)], &[("/dep/lib.rs", FN_DEP)]]);

#[test]
fn a_function_like_macro_used_as_an_attribute_is_a_finding() {
    let crates = [input("user", "/user/lib.rs", &[("dep", 1)]), input("dep", "/dep/lib.rs", &[])];
    let names = resolved(&crates, &FN_TEXTS);
    let user = names.krate(0);
    let root = names.root(0);
    let errors = &user.done.get(&root).expect("the root is done").scope.errors;
    let found = errors.iter().find_map(|e| match e {
        crate::ScopeError::Macro { call, path, why: crate::Unresolved::WrongMacroKind, .. } => Some((*call, path.segments.join("::"))),
        _ => None,
    });
    let (call, path) = found.unwrap_or_else(|| panic!("a wrong-kind finding: {errors:?}"));
    assert_eq!(path, "dep::plain", "the finding names the macro");
    let decls = &user.decls.get(&root).expect("the root is lowered").1;
    let shape = decls.calls.iter().find(|c| c.call == call).map(|c| c.kind);
    assert_eq!(shape, Some(crate::CallShape::Attr), "the finding's call is the attribute");
    let value = names.item_at(root, Namespace::Values, Written { kind: PathKind::Plain, segments: &["f"] }, Alloc::global());
    assert!(value.is_err(), "the item an unresolved attribute takes is not declared");
}

#[test]
fn rule_shapes_are_read_from_the_arms() {
    use crate::MacroShapes;
    use crate::task::rules_shapes;
    let e = blaze_surface::Edition::E2024;
    let g = Alloc::global();
    assert_eq!(rules_shapes("() => {}; ($x:expr) => { $x }", e, g), MacroShapes::FN_LIKE);
    assert_eq!(rules_shapes("attr() ($i:item) => { $i };", e, g), MacroShapes::ATTR);
    let all = MacroShapes::ATTR.with(crate::CallShape::Derive).with(crate::CallShape::FnLike);
    assert_eq!(rules_shapes("unsafe attr($a:tt) ($i:item) => {}; derive() ($i:item) => {}; (attr) => {}", e, g), all);
    // `attr` inside a pattern or a template is no arm's head.
    assert_eq!(rules_shapes("($x:ident) => { attr(1) }; (derive ()) => {}", e, g), MacroShapes::FN_LIKE);
    assert_eq!(rules_shapes("", e, g), MacroShapes::FN_LIKE);
}

#[test]
fn attribute_macros_take_their_item() {
    let crates = [input("user", "/user/lib.rs", &[("dep", 1)]), input("dep", "/dep/lib.rs", &[])];
    let names = resolved(&crates, &TEXTS);
    let user = names.krate(0);
    let headers: std::vec::Vec<&str> = user.tables.impls.iter().map(|i| i.header).collect();
    let root = names.root(0);
    let value = |segments: &'static [&'static str]| names.item_at(root, Namespace::Values, Written { kind: PathKind::Plain, segments }, Alloc::global());
    value(&["main"]).unwrap_or_else(|e| panic!("`main`, kept by `#[dep::wrap]`: {e}; notes {:?}; errors {:?}; failed {:?}", names.notes(0), user.done.get(&root).map(|d| &d.scope.errors), user.failed));
    value(&["wrapped"]).unwrap_or_else(|e| panic!("`wrapped`, written by `#[dep::wrap]`: {e}"));
    value(&["kept"]).unwrap_or_else(|e| panic!("`kept`, under a built-in attribute: {e}"));
    let texts: std::vec::Vec<Option<&str>> = user.tables.texts.values().map(|(t, _)| t.name).collect();
    assert!(texts.contains(&Some("main")) && texts.contains(&Some("wrapped")), "the expansion's items have texts: {texts:?}");
    // `#[dep::wrap]` on a struct matches no arm: the struct is gone, as rustc leaves it, and
    // its derive never runs.
    assert!(!headers.iter().any(|h| h.contains("NotAFn")), "no impl of an item an attribute macro refused: {headers:?}");
    let notes = names.notes(0).len();
    assert!(notes > 0, "the refused attribute call is noted");
}
