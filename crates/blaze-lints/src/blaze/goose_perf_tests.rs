//! One test per speed lint drawn from Goose (`push_without_reserve`, `scratch_in_loop`,
//! `boxed_recursive_node`, `wide_index`, `bounded_heap_field`, `enum_vec_padded`,
//! `deep_drop_at_exit`, `dyn_fn_param`, `box_large_new`, `pod_serialized_by_field`), two for
//! `checkfree`, and two for `packable` (from Zig's packed structs), over an `std` of their
//! own: a `Vec` and a `String` known by their diagnostic items, a `Box` with `new`, and a
//! `serde` with `Serialize`.

use crate::tests::lint;

const MINICORE: &str = "//- minicore: sized, copy, clone, option, result, fn, iterator, iterators, slice, deref, drop, index, builtin_impls, range, derive, eq, ord, panic, assert, int_impl\n";

const STD: &str = r#"
//- /std/lib.rs crate:std
pub mod prelude {
    pub mod v1 {
        pub use crate::vec::Vec;
        pub use crate::string::String;
        pub use crate::boxed::Box;
        pub use core::prelude::v1::*;
    }
    pub mod rust_2015 { pub use super::v1::*; }
    pub mod rust_2018 { pub use super::v1::*; }
    pub mod rust_2021 { pub use super::v1::*; }
    pub mod rust_2024 { pub use super::v1::*; }
}
pub mod boxed {
    pub struct Box<T: ?Sized> { ptr: *const T }
    impl<T> Box<T> {
        pub fn new(value: T) -> Box<T> { loop {} }
    }
    impl<T: ?Sized> core::ops::Deref for Box<T> {
        type Target = T;
        fn deref(&self) -> &T { loop {} }
    }
}
pub mod string {
    #[rustc_diagnostic_item = "String"]
    pub struct String { ptr: *const u8, cap: usize, len: usize }
    impl String {
        pub const fn new() -> String { loop {} }
        pub fn from(text: &str) -> String { loop {} }
        pub fn push_str(&mut self, text: &str) {}
        pub fn len(&self) -> usize { self.len }
    }
}
pub mod vec {
    #[rustc_diagnostic_item = "Vec"]
    pub struct Vec<T> { ptr: *const T, cap: usize, len: usize }
    impl<T> Vec<T> {
        pub const fn new() -> Vec<T> { loop {} }
        pub fn with_capacity(n: usize) -> Vec<T> { loop {} }
        pub fn push(&mut self, value: T) {}
        pub fn clear(&mut self) {}
        pub fn len(&self) -> usize { self.len }
        pub fn iter(&self) -> core::slice::Iter<'_, T> { loop {} }
    }
    impl<T> core::ops::Index<usize> for Vec<T> {
        type Output = T;
        fn index(&self, index: usize) -> &T { loop {} }
    }
}
pub mod io {
    pub fn _print(text: &str) {}
}
"#;

const SERDE: &str = r#"
//- /serde/lib.rs crate:serde
pub trait Serialize {}
"#;

const MAIN: &str = "//- /src/lib.rs crate:main deps:std,serde\n";

/// What `blaze::<lint_name>` alone says of `main`, over [`STD`] and [`SERDE`].
fn only(lint_name: &str, main: &str) -> String {
    let fixture = format!("{MINICORE}{STD}{SERDE}{MAIN}{}", main.trim_start_matches('\n'));
    lint(&fixture, &format!("-A clippy::all --warn=blaze::{lint_name}"))
}

/// A `Vec` pushed to once per round of a range loop is found; one made `with_capacity` is not.
#[test]
fn push_without_reserve_finds_growth_a_known_count_could_reserve() {
    let found = only(
        "push_without_reserve",
        r#"
pub fn squares(n: usize) -> Vec<usize> {
    let mut out = Vec::new();
    for i in 0..n {
        out.push(i);
    }
    out
}
pub fn reserved(n: usize) -> Vec<usize> {
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        out.push(i);
    }
    out
}
"#,
    );
    assert_eq!(found.matches("reallocates as it grows").count(), 1, "{found}");
    assert!(found.contains("`Vec::with_capacity(n)` allocates once"), "{found}");
}

/// A buffer made and grown in every round of a loop is found; one made before it is not.
#[test]
fn scratch_in_loop_finds_a_buffer_made_every_round() {
    let found = only(
        "scratch_in_loop",
        r#"
pub fn rounds(n: usize) -> usize {
    let mut total = 0;
    for i in 0..n {
        let mut scratch = Vec::new();
        scratch.push(i);
        total += scratch.len();
    }
    total
}
pub fn hoisted(n: usize) -> usize {
    let mut total = 0;
    let mut scratch = Vec::new();
    for i in 0..n {
        scratch.clear();
        scratch.push(i);
        total += scratch.len();
    }
    total
}
"#,
    );
    assert_eq!(found.matches("each round allocates it again").count(), 1, "{found}");
    assert!(found.contains("`scratch` is a `Vec` made, grown and dropped"), "{found}");
}

/// A private list linked by `Option<Box<Self>>` is found; a public one is not.
#[test]
fn boxed_recursive_node_finds_a_private_self_linked_type() {
    let found = only(
        "boxed_recursive_node",
        r#"
struct Node { value: u32, next: Option<Box<Node>> }
pub struct Shared { value: u32, next: Option<Box<Shared>> }
pub fn head(value: u32) -> u32 {
    let node = Node { value, next: None };
    node.value
}
"#,
    );
    assert_eq!(found.matches("links to itself through the heap").count(), 1, "{found}");
    assert!(found.contains("`Node` links to itself"), "{found}");
}

/// A `usize` read only as an index into one `Vec` field is found; one also returned is not.
#[test]
fn wide_index_finds_a_usize_field_read_only_as_an_index() {
    let found = only(
        "wide_index",
        r#"
struct Ring { items: Vec<u32>, at: usize, len: usize }
impl Ring {
    fn current(&self) -> u32 { self.items[self.at] }
    fn advance(&mut self) { self.at += 1; }
    fn count(&self) -> usize { self.len }
    fn last(&self) -> u32 { self.items[self.len] }
}
pub fn make() -> u32 {
    let mut ring = Ring { items: Vec::new(), at: 0, len: 0 };
    ring.advance();
    ring.current() + ring.last() + ring.count() as u32
}
"#,
    );
    assert_eq!(found.matches("a `u32` index halves the link").count(), 1, "{found}");
    assert!(found.contains("the field `at` of `Ring`"), "{found}");
}

/// A `String` field built only from a short literal is found; one built from a parameter is not.
#[test]
fn bounded_heap_field_finds_a_small_literal_field() {
    let found = only(
        "bounded_heap_field",
        r#"
struct Unit { name: String, label: String }
pub fn make(label: String) -> usize {
    let unit = Unit { name: String::from("meters"), label };
    unit.name.len() + unit.label.len()
}
"#,
    );
    assert_eq!(found.matches("it fits inline").count(), 1, "{found}");
    assert!(found.contains("the field `name` of `Unit`"), "{found}");
}

/// A field built only empty and never changed is advised away, not given an inline buffer of
/// no items: removed, or a `PhantomData` when it alone uses one of the struct's parameters.
#[test]
fn bounded_heap_field_advises_removing_an_always_empty_field() {
    let found = only(
        "bounded_heap_field",
        r#"
struct Plain { names: Vec<u32>, count: usize }
struct Typed<T> { items: Vec<T>, count: usize }
pub fn make() -> usize {
    let plain = Plain { names: Vec::new(), count: 1 };
    let typed: Typed<u8> = Typed { items: Vec::new(), count: 2 };
    plain.names.len() + plain.count + typed.items.len() + typed.count
}
"#,
    );
    assert_eq!(found.matches("it is always empty").count(), 2, "{found}");
    assert!(!found.contains("[T; 0]") && !found.contains("fits inline"), "{found}");
    assert!(found.contains("remove the field"), "{found}");
    assert!(found.contains("`core::marker::PhantomData<T>`: no other field uses `T`"), "{found}");
}

/// A `Vec` of an enum with a large and an empty variant, only pushed and walked, is found.
#[test]
fn enum_vec_padded_finds_a_vec_of_a_lopsided_enum() {
    let found = only(
        "enum_vec_padded",
        r#"
enum Op { Halt, Load([u64; 4]) }
struct Program { ops: Vec<Op> }
impl Program {
    fn add(&mut self, op: Op) { self.ops.push(op); }
    fn size(&self) -> usize { self.ops.len() }
}
pub fn run() -> usize {
    let mut program = Program { ops: Vec::new() };
    program.add(Op::Halt);
    program.add(Op::Load([0; 4]));
    program.size()
}
"#,
    );
    assert_eq!(found.matches("only pushed and iterated").count(), 1, "{found}");
    assert!(found.contains("the field `ops` of `Program`"), "{found}");
}

/// A `Vec<String>` alive where `main` returns is found when the pedantic lint is on.
#[test]
fn deep_drop_at_exit_finds_a_nested_collection_freed_at_exit() {
    let found = only(
        "deep_drop_at_exit",
        r#"
fn main() {
    let mut names: Vec<String> = Vec::new();
    names.push(String::from("a"));
    let mut counts: Vec<u32> = Vec::new();
    counts.push(1);
}
"#,
    );
    assert_eq!(found.matches("is dropped where `main` ends").count(), 1, "{found}");
    assert!(found.contains("`names`, a `Vec<String>`"), "{found}");
}

/// A `&dyn Fn` parameter called in a loop is found; one called once is not.
#[test]
fn dyn_fn_param_finds_a_closure_trait_object_called_in_a_loop() {
    let found = only(
        "dyn_fn_param",
        r#"
fn apply(n: u32, f: &dyn Fn(u32) -> u32) -> u32 {
    let mut total = 0;
    for i in 0..n {
        total = f(i);
    }
    total
}
fn once(f: &dyn Fn(u32) -> u32) -> u32 { f(1) }
pub fn run() -> u32 { apply(3, &|x| x) + once(&|x| x) }
"#,
    );
    assert_eq!(found.matches("every call goes through the vtable").count(), 1, "{found}");
    assert!(found.contains("`apply` takes `f` as a `dyn Fn`"), "{found}");
}

/// `Box::new` of an 8 KiB array already in a local is found; of an 8 KiB array literal, which
/// the `BoxInPlace` MIR pass builds in the box, is not, nor of a small one.
#[test]
fn box_large_new_finds_a_large_value_built_on_the_stack() {
    let found = only(
        "box_large_new",
        r#"
pub fn moved() -> Box<[u8; 8192]> { let a = [1; 8192]; Box::new(a) }
pub fn in_place() -> Box<[u8; 8192]> { Box::new([0; 8192]) }
pub fn small() -> Box<[u8; 64]> { Box::new([0; 64]) }
"#,
    );
    assert_eq!(
        found.matches("built on the stack, then copied into the heap").count(),
        1,
        "{found}"
    );
    assert!(found.contains("(8192 bytes)"), "{found}");
    assert!(found.contains("new_uninit"), "{found}");
}

/// A `#[repr(C)]` pair of floats with serde's `Serialize` is found; one with a `bool` is not.
#[test]
fn pod_serialized_by_field_finds_plain_numbers_through_serde() {
    let found = only(
        "pod_serialized_by_field",
        r#"
#[repr(C)]
struct Point { x: f32, y: f32 }
impl serde::Serialize for Point {}
#[repr(C)]
struct Flagged { x: f32, on: bool }
impl serde::Serialize for Flagged {}
"#,
    );
    assert_eq!(
        found.matches("goes through serde's `Serialize` field by field").count(),
        1,
        "{found}"
    );
    assert!(found.contains("`Point` is `#[repr(C)]`"), "{found}");
}

/// A checkfree function whose loop's indexes an `assert!` proves is clean: the assert's own
/// panic is the one it wrote, and the bounds check of `v[i]` is one LLVM drops.
#[test]
fn checkfree_passes_a_loop_proven_by_an_assert() {
    let found = only(
        "checkfree",
        r#"
#[blaze::checkfree]
pub fn sum(v: &[u32], n: usize) -> u32 {
    assert!(n <= v.len());
    let mut total = 0u32;
    for i in 0..n {
        total = total ^ v[i];
    }
    total
}
"#,
    );
    assert!(!found.contains("blaze::checkfree"), "{found}");
}

/// A checkfree function that indexes with nothing proving the index is refused, at the line of
/// the index, naming the check; the same function without the attribute is not checked.
#[test]
fn checkfree_refuses_a_kept_bounds_check_naming_the_line() {
    let found = only(
        "checkfree",
        r#"
#[blaze::checkfree]
pub fn pick(v: &[u32], i: usize) -> u32 {
    v[i]
}
pub fn unmarked(v: &[u32], i: usize) -> u32 {
    v[i]
}
"#,
    );
    assert_eq!(found.matches("is `#[blaze::checkfree]`").count(), 1, "{found}");
    assert!(
        found.contains("`pick` is `#[blaze::checkfree]`, but a bounds check at `v[i]` stays"),
        "{found}"
    );
    assert!(found.contains("src/lib.rs:3:"), "the index's line:\n{found}");
}

/// A struct of five `bool`s and a four-variant enum, 6 bytes that need 7 bits, pushed to a
/// `Vec` is found (`packable`, from Zig's packed structs), naming the `Vec` and its line.
#[test]
fn packable_finds_a_struct_of_flags_stored_in_a_vec() {
    let found = only(
        "packable",
        r#"
enum Kind { Leaf, Branch, Root, Gone }
struct Flags { open: bool, dirty: bool, pinned: bool, shared: bool, hidden: bool, kind: Kind }
pub fn count(open: bool) -> usize {
    let mut all = Vec::new();
    all.push(Flags { open, dirty: false, pinned: false, shared: false, hidden: false, kind: Kind::Leaf });
    all.len()
}
"#,
    );
    assert_eq!(found.matches("but its fields need").count(), 1, "{found}");
    assert!(
        found.contains("`Flags` takes 6 bytes but its fields need 7 bits: packed, it would take 1 byte, and it is the element of a `Vec`"),
        "{found}"
    );
    assert!(found.contains("lib.rs:4)"), "the line of the `Vec`:\n{found}");
    assert!(found.contains("`kind` (`Kind`, 2 bits)"), "{found}");
}

/// The same struct, built and read but never held in a collection, is not found.
#[test]
fn packable_leaves_a_struct_no_collection_holds() {
    let found = only(
        "packable",
        r#"
enum Kind { Leaf, Branch, Root, Gone }
struct Flags { open: bool, dirty: bool, pinned: bool, shared: bool, hidden: bool, kind: Kind }
pub fn first(open: bool) -> bool {
    let flags = Flags { open, dirty: false, pinned: false, shared: false, hidden: false, kind: Kind::Leaf };
    flags.open
}
"#,
    );
    assert!(!found.contains("but its fields need"), "{found}");
}
