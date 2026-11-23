//! A dependency macro whose const-generic decoder body is needed by a member body.
//!
//! The macro's expansion owns `STRINGS`, while `len` and `decode` belong to the
//! dependency. Their checked body records must be available to the member's body
//! task. The old engine asked for one of those dependency bodies without a producer
//! and eventually reported `Rounds`.

use super::*;
use super::engine_check_tests::{engine_opts, opts};

// The default simulated shelf has an empty Rust 2024 prelude. Give the decoder
// the type, constructor and method it uses through the same prelude as real Rust.
const STD: &str = r#"#![no_std]
pub enum Option<T> { None, Some(T) }
impl<T> Option<T> {
    pub fn unwrap(self) -> T {
        match self { Self::Some(value) => value, Self::None => loop {} }
    }
}
pub mod prelude {
    pub mod rust_2021 {}
    pub mod rust_2024 {
        pub use crate::Option;
        pub use crate::Option::{None, Some};
    }
}
"#;

const DEP: &str = "pub const fn len(_: &[&[u8]]) -> usize { 2 }\n\npub const fn decode<const N: usize>(_: &[&[u8]]) -> Option<[u8; N]> { Some([0; N]) }\n\n#[macro_export]\nmacro_rules! hex {\n    ($s:literal) => {{\n        const STRINGS: &[&[u8]] = &[b\"00ff\"];\n        $crate::decode::<{ $crate::len(STRINGS) }>(STRINGS).unwrap()\n    }};\n}\n";

#[test]
fn engine_checks_a_body_that_reads_a_dependency_const_decoder() {
    let io = Io::simulated(SimWorld::new(73, Limits::USUAL));
    write(&io, "Cargo.toml", "[workspace]\nmembers = [\"dep\", \"app\"]\nresolver = \"2\"\n");
    write(&io, "dep/Cargo.toml", "[package]\nname = \"dep\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    write(&io, "dep/src/lib.rs", DEP);
    write(&io, "app/Cargo.toml", "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\ndep = { path = \"../dep\" }\n");
    write(&io, "app/src/lib.rs", "pub fn value() -> [u8; 2] {\n    let x: [u8; 2] = dep::hex!(\"00ff\");\n    x\n}\n");

    let mut cold = opts(&io, 2, true);
    let (shelf, files) = blaze_world::fixture::shelf_with_langs(graph::TRIPLE, STD, SHELF_CFG, cold.heap.alloc());
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let root = io.need_fs().expect("files").root();
    for (name, bytes) in files {
        let path = Path::new(&format!("h/{}/{name}", toolchain::SHELF)).expect("a shelf path");
        root.publish(&cx, &path, Content::bytes(&bytes), Durable::No).expect("publish the decoder's fixture shelf");
    }
    cold.toolchain = shelf;
    let before = stdout(&io).len();
    let (_, checked) = engine_opts(&io, cold);
    let printed = stdout(&io).split_off(before);
    let (notes, clean) = &checked.bodies;
    assert_eq!(checked.status.0, 0, "the dependency decoder and its caller have no findings: {notes:?}\nengine output:\n{printed}");
    assert!(!printed.contains("NOT checked: borrows"), "all stages checked:\n{printed}");
    assert!(notes.is_empty(), "no body is refused after the dependency body records are produced: {notes:?}");
    assert!(notes.iter().all(|note| !note.contains("Rounds")), "the old no-producer refusal is gone: {notes:?}");
    assert!(*clean >= 3, "the two dependency const fns and app::value are checked: {clean}");

    // The caller's anonymous constant reads `dep::len`'s definition cell.  Changing that
    // definition therefore re-enters the caller and reports the resulting array mismatch.
    write(&io, "dep/src/lib.rs", &DEP.replace("{ 2 }", "{ 3 }"));
    let mut warm = opts(&io, 2, false);
    warm.toolchain = shelf;
    let before = stdout(&io).len();
    let (_, edited) = engine_opts(&io, warm);
    assert_ne!(edited.status.0, 0, "the changed const-fn length reaches the caller: {:?}", edited.bodies.0);
    assert!(edited.bodies.0.is_empty(), "all edited bodies were checked: {:?}", edited.bodies.0);
    let printed = stdout(&io).split_off(before);
    assert!(printed.lines().any(|line| {
        line.contains("app/src/lib.rs:") && line.contains("error[types]") && line.contains("found `") && line.contains("expected `")
    }), "changing only the decoded length must produce a type mismatch in the caller:\n{printed}");
}

#[test]
fn engine_reads_a_moved_macro_and_rechecks_its_caller_after_a_definition_edit() {
    use super::engine_edit_rows_tests::{Seen, seen};

    const FILE: &str = "a/src/macros.rs";
    const MACRO: &str = "#[macro_export]\nmacro_rules! width { () => { 1u8 }; }\n";
    const CALLER: &str = "pub fn value() -> u8 {\n    let x = a::width!();\n    let y: u8 = x;\n    y\n}\n";
    let io = super::engine_heads_once_tests::module_members(FILE, MACRO, "mod macros;\n", CALLER);
    let check = |fresh| {
        let mut options = opts(&io, 2, fresh);
        options.scope_reads = crate::engine_scope_reads::ScopeReadsAs::Cells;
        let before = stdout(&io).len();
        let (_, checked) = engine_opts(&io, options);
        let printed = stdout(&io).split_off(before);
        assert!(fresh || checked.from_last, "the edit must use the stored answer:\n{printed}");
        assert!(checked.bodies.0.is_empty(), "all bodies must be checked: {:?}\n{printed}", checked.bodies.0);
        printed
    };
    assert!(!check(true).contains("error[types]"), "the expansion has type u8");

    // The names are unchanged. Move the definition in its file, and force the caller
    // to expand it again by changing the caller's expected type.
    let moved = format!("// the definition starts later now\n{MACRO}");
    write(&io, FILE, &moved);
    write(&io, "b/src/lib.rs", &CALLER.replace("u8", "u16"));
    let mismatch = check(false);
    assert!(mismatch.lines().any(|line| line.contains("b/src/lib.rs:") && line.contains("error[types]: found `u8`, expected `u16`")),
        "the moved definition still expands to u8, which cannot initialize a u16:\n{mismatch}");

    // Only the macro changes. The caller has no dependency const-fn or type that
    // could also make it read the defining file.
    write(&io, FILE, &moved.replace("1u8", "1u16"));
    let repaired = check(false);
    assert!(!repaired.contains("error[types]"), "the edited definition now supplies the expected u16:\n{repaired}");
    assert_eq!(seen(&io, "macro definition edit", "b/src/lib.rs::value"), Seen::Last);
}
