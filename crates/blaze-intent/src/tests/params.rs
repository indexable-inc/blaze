//! A fn's parameters and every call, against the front end: each kind of call gets its argument,
//! the new text type-checks in a front end of it, and each use no rewrite can reach fails the
//! request, naming its place.

use std::collections::BTreeMap;

use base_db::FileId;
use blaze_api::{Edit, Param, Target};
use hir::{Crate, diagnostics::AnyDiagnostic};
use hir_expand::fixture;

use super::{applied, db::TestDB};
use crate::{Files, apply};

/// The view's files: path, the fixture's header for it, and its text.
type Spec<'a> = [(&'a str, &'a str, &'a str)];

const LIB: &str = "pub mod shapes;
pub use shapes::area as surface;

macro_rules! id {
    ($($t:tt)*) => { $($t)* };
}

pub trait Grow {
    fn grow(&self, by: u32) -> u32;
}

pub struct Leaf(pub u32);

impl Grow for Leaf {
    fn grow(&self, step: u32) -> u32 { self.0 + step }
}

pub fn local() -> u32 {
    let leaf = Leaf(1);
    id!(shapes::area(2)) + leaf.grow(1) + Leaf::grow(&leaf, 2) + <Leaf as Grow>::grow(&leaf, 3)
}
";

const SHAPES: &str = "pub fn area(side: u32) -> u32 {
    side * side
}

mod inner {
    use super::area as square;
    fn other() -> u32 {
        square(3) + super::area(4)
    }
}
";

const APP: &str = "use lib::Grow;

pub fn run() -> u32 {
    lib::surface(5) + lib::shapes::area(6) + lib::Leaf(2).grow(7)
}
";

fn spec<'a>(lib: &'a str, shapes: &'a str, app: &'a str) -> [(&'a str, &'a str, &'a str); 3] {
    [
        ("lib.rs", "crate:lib edition:2021", lib),
        ("shapes.rs", "", shapes),
        ("app.rs", "crate:app deps:lib edition:2021", app),
    ]
}

struct View {
    files: Vec<(FileId, String, String)>,
}

impl Files for View {
    fn file(&self, path: &str) -> Option<(FileId, &[u8])> {
        self.files.iter().find(|it| it.1 == path).map(|it| (it.0, it.2.as_bytes()))
    }

    fn path(&self, file: FileId) -> Option<&str> {
        self.files.iter().find(|it| it.0 == file).map(|it| &*it.1)
    }
}

fn view(spec: &Spec<'_>) -> (TestDB, View) {
    let fixture: String =
        spec.iter().map(|(path, header, text)| format!("//- /{path} {header}\n{text}")).collect();
    let fx = fixture::db(&fixture);
    let ids = fx.files().to_vec();
    let db = fx.into_db();
    let files = ids
        .iter()
        .zip(spec)
        .map(|(file, (path, ..))| {
            let id = file.file_id(&db);
            (id, (*path).to_owned(), db.file_text(id).text(&db).to_string())
        })
        .collect();
    (db, View { files })
}

/// The errors of a front end of `spec` that say a name names nothing, a value has the wrong
/// type or a call the wrong number of arguments.
fn errors(spec: &Spec<'_>) -> Vec<&'static str> {
    let (db, _) = view(spec);
    let mut all = Vec::new();
    for krate in Crate::all(&db) {
        for module in krate.modules(&db) {
            module.diagnostics(&db, &mut all, false);
        }
    }
    all.iter()
        .filter_map(|it| match it {
            AnyDiagnostic::UnresolvedPath(_) => Some("unresolved path"),
            AnyDiagnostic::UnresolvedIdent(_) => Some("unresolved ident"),
            AnyDiagnostic::UnresolvedMethodCall(_) => Some("unresolved method call"),
            AnyDiagnostic::TypeMismatch(_) => Some("type mismatch"),
            AnyDiagnostic::MismatchedArgCount(_) => Some("mismatched arg count"),
            _ => None,
        })
        .collect()
}

fn params(file: &str, item: &str, params: &[Param]) -> Edit {
    Edit::Params {
        item: Target { file: file.to_owned(), item: item.to_owned() },
        params: params.to_vec(),
    }
}

fn kept(name: &str) -> Param {
    Param::Kept { name: name.to_owned() }
}

fn added(param: &str, default: &str) -> Param {
    Param::Added { param: param.to_owned(), default: default.to_owned() }
}

/// The files after `edits` on `spec`, checked to type-check in a front end of them.
#[track_caller]
fn after(spec: &Spec<'_>, edits: &[Edit]) -> BTreeMap<String, String> {
    let (db, view) = view(spec);
    assert_eq!(errors(spec), Vec::<&str>::new(), "the fixture type-checks before");
    let made = apply(&db, &view, edits).unwrap_or_else(|error| panic!("{edits:?}: {error:?}"));
    let before: BTreeMap<String, String> =
        view.files.iter().map(|(_, path, text)| (path.clone(), text.clone())).collect();
    let after = applied(&before, &made.edited.ops);
    let next: Vec<(&str, &str, &str)> =
        spec.iter().map(|(path, header, _)| (*path, *header, after[*path].as_str())).collect();
    assert_eq!(errors(&next), Vec::<&str>::new(), "the edited files type-check: {after:#?}");
    after
}

/// The request's error, which must name `place`.
#[track_caller]
fn refused(spec: &Spec<'_>, edit: Edit, place: &str) -> String {
    let (db, view) = view(spec);
    let Err(error) = apply(&db, &view, &[edit]) else { panic!("refused at {place}") };
    let message = error.what;
    assert!(message.contains(place), "names {place}: {message}");
    message
}

#[test]
fn an_added_parameter_reaches_every_path_call_alias_re_export_and_macro_input_call() {
    let spec = spec(LIB, SHAPES, APP);
    let edit = params("shapes.rs", "area", &[kept("side"), added("scale: u32", "2 * 3")]);
    let after = after(&spec, &[edit]);
    assert_eq!(
        after["shapes.rs"],
        SHAPES
            .replace("side: u32)", "side: u32, scale: u32)")
            .replace("square(3)", "square(3, 2 * 3)")
            .replace("area(4)", "area(4, 2 * 3)")
    );
    assert!(after["lib.rs"].contains("id!(shapes::area(2, 2 * 3))"), "{}", after["lib.rs"]);
    assert!(after["lib.rs"].contains("pub use shapes::area as surface;"));
    assert!(after["app.rs"].contains("lib::surface(5, 2 * 3) + lib::shapes::area(6, 2 * 3)"));
}

#[test]
fn a_trait_method_changes_in_the_trait_every_impl_and_every_call_form() {
    let spec = spec(LIB, SHAPES, APP);
    let edit = params("lib.rs", "Grow::grow", &[added("extra: u32", "0"), kept("by")]);
    let after = after(&spec, &[edit]);
    let lib = &after["lib.rs"];
    for want in [
        "fn grow(&self, extra: u32, by: u32) -> u32;",
        "fn grow(&self, extra: u32, step: u32) -> u32 { self.0 + step }",
        "leaf.grow(0, 1) + Leaf::grow(&leaf, 0, 2) + <Leaf as Grow>::grow(&leaf, 0, 3)",
    ] {
        assert!(lib.contains(want), "{want}: {lib}");
    }
    assert!(after["app.rs"].contains("lib::Leaf(2).grow(0, 7)"), "{}", after["app.rs"]);
}

#[test]
fn a_parameter_is_removed_and_the_rest_reordered_with_plain_arguments() {
    let shapes = "pub fn area(side: u32) -> u32 { side }
pub fn mix(a: u32, b: u32, unused: u32) -> u32 { a - b }
pub fn call(x: u32) -> u32 { mix(1, x, 3) + mix(x, 2, x) }
";
    let spec = spec("pub mod shapes;\n", shapes, "");
    let edit = params("shapes.rs", "mix", &[kept("b"), kept("a")]);
    let after = after(&spec, &[edit]);
    assert_eq!(
        after["shapes.rs"],
        shapes
            .replace("a: u32, b: u32, unused: u32", "b: u32, a: u32")
            .replace("mix(1, x, 3) + mix(x, 2, x)", "mix(x, 1) + mix(2, x)")
    );
}

#[test]
fn each_use_no_rewrite_reaches_is_refused_naming_its_place() {
    let base = "pub fn area(side: u32) -> u32 { side }\n";
    let add = || params("shapes.rs", "area", &[kept("side"), added("scale: u32", "1")]);
    let with = |more: &str| format!("{base}{more}");

    let value = with("pub fn take() -> fn(u32) -> u32 { let g = area; g }\n");
    let message = refused(&spec("pub mod shapes;\n", &value, ""), add(), "shapes.rs:2");
    assert!(message.contains("as a value"), "{message}");

    let made = "macro_rules! twice { () => { crate::shapes::area(1) + crate::shapes::area(2) }; }
pub mod shapes;
pub fn both() -> u32 { twice!() }
";
    let message = refused(&spec(made, base, ""), add(), "lib.rs:1");
    assert!(message.contains("a call a macro makes"), "{message}");

    let count = with("pub fn wrong() -> u32 { area(1, 2) }\n");
    let message = refused(&spec("pub mod shapes;\n", &count, ""), add(), "shapes.rs:2");
    assert!(message.contains("a call with 2 arguments, where `area` takes 1"), "{message}");

    for (decl, why) in [
        ("pub extern \"C\" fn area(side: u32) -> u32 { side }\n", "`extern` ABI"),
        ("#[no_mangle]\npub fn area(side: u32) -> u32 { side }\n", "exported by `#[no_mangle]`"),
    ] {
        let message = refused(&spec("pub mod shapes;\n", decl, ""), add(), "area");
        assert!(message.contains(why), "{message}");
    }

    let reads = params("shapes.rs", "area", &[]);
    let message = refused(&spec("pub mod shapes;\n", base, ""), reads, "area");
    assert!(message.contains("removes parameter `side`, which the body still reads"), "{message}");

    let dropped = "pub fn area(side: u32, _old: u32) -> u32 { side }
fn two() -> u32 { 2 }
pub fn call() -> u32 { area(1, two()) }
";
    let drop = params("shapes.rs", "area", &[kept("side")]);
    let message = refused(&spec("pub mod shapes;\n", dropped, ""), drop, "shapes.rs:3");
    assert!(message.contains("argument `two()` would be dropped"), "{message}");

    let not_one = params("shapes.rs", "area", &[kept("side"), added("scale: u32", "1, 2")]);
    let message = refused(&spec("pub mod shapes;\n", base, ""), not_one, "`area`");
    assert!(message.contains("`1, 2` is no one expression"), "{message}");

    let in_impl = params("lib.rs", "<Leaf as Grow>::grow", &[kept("step")]);
    let message = refused(&spec(LIB, SHAPES, APP), in_impl, "`<Leaf as Grow>::grow`");
    assert!(message.contains("change its parameters in the trait"), "{message}");
}
