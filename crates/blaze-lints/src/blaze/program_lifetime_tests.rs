//! `program_lifetime`, `could_be_program_lifetime` and `could_be_ref32`
//! (docs/v2/plan/design/facts.md): what each finds, what each stays silent on, and
//! `could_be_ref32`'s fix, which leaves the file clean of it.

use base_db::FileId;
use hir_expand::fixture;

use crate::{
    check::{Setup, check},
    conf::Conf,
    fix::fix,
    level::CommandLine,
    tests::lint,
};

const FLAGS: &str = "-A clippy::all -A blaze::general --warn=blaze::edition";

/// A `std` with the pointers and the cells the lints know by path, and a `blaze_mem` with
/// the arena's two types.
const CRATES: &str = r#"
//- minicore: sized, copy, clone, option, result, fn, deref, drop, builtin_impls
//- /std/lib.rs crate:std
pub mod prelude {
    pub mod v1 {
        pub use crate::{boxed::Box, vec::Vec};
        pub use core::prelude::v1::*;
    }
    pub mod rust_2015 { pub use super::v1::*; }
    pub mod rust_2018 { pub use super::v1::*; }
    pub mod rust_2021 { pub use super::v1::*; }
    pub mod rust_2024 { pub use super::v1::*; }
}
pub mod boxed {
    pub struct Box<T> { ptr: *mut T }
    impl<T> Box<T> {
        pub fn new(value: T) -> Box<T> { loop {} }
        pub fn leak(b: Box<T>) -> &'static mut T { loop {} }
    }
    impl<T> Drop for Box<T> { fn drop(&mut self) {} }
}
pub mod vec {
    #[rustc_diagnostic_item = "Vec"]
    pub struct Vec<T> { ptr: *mut T }
    impl<T> Vec<T> {
        pub fn new() -> Vec<T> { loop {} }
        pub fn push(&mut self, value: T) {}
    }
    impl<T> Drop for Vec<T> { fn drop(&mut self) {} }
}
pub mod sync {
    pub struct Arc<T> { ptr: *const T }
    impl<T> Arc<T> {
        pub fn new(value: T) -> Arc<T> { loop {} }
    }
    impl<T> Clone for Arc<T> { fn clone(&self) -> Arc<T> { loop {} } }
    impl<T> Drop for Arc<T> { fn drop(&mut self) {} }
    pub struct Mutex<T> { value: T }
    impl<T> Mutex<T> {
        pub const fn new(value: T) -> Mutex<T> { Mutex { value } }
    }
    pub struct LazyLock<T> { value: T }
    impl<T> LazyLock<T> {
        pub const fn new(make: fn() -> T) -> LazyLock<T> { loop {} }
    }
    impl<T> core::ops::Deref for LazyLock<T> {
        type Target = T;
        fn deref(&self) -> &T { &self.value }
    }
}
//- /blaze_mem/lib.rs crate:blaze_mem deps:std
pub struct Ref32<T> { units: u32, of: fn() -> T }
impl<T> Clone for Ref32<T> { fn clone(&self) -> Ref32<T> { *self } }
impl<T> Copy for Ref32<T> {}
impl<T: 'static> Ref32<T> {
    pub fn get(self) -> &'static T { loop {} }
}
pub struct ProgramArena { next: usize }
impl ProgramArena {
    pub fn alloc<T: 'static>(&mut self, value: T) -> Ref32<T> { loop {} }
}
//- /src/lib.rs crate:main deps:std,blaze_mem
"#;

fn fixture_of(main: &str) -> String {
    format!("{}{}", CRATES.trim_start_matches('\n'), main.trim_start_matches('\n'))
}

#[track_caller]
fn count(out: &str, message: &str, want: usize) {
    assert_eq!(out.matches(message).count(), want, "`{message}` in:\n{out}");
}

/// A declared type is refused for its shape (naming the field and the rule), for being
/// another crate's to name, and for a value built outside the arena; one that is none of
/// those is accepted.
#[test]
fn program_lifetime_refuses_naming_the_reason() {
    let out = lint(
        &fixture_of(
            r#"
use blaze_mem::{ProgramArena, Ref32};

#[blaze::program_lifetime]
struct Node {
    kind: u16,
    parent: Option<Ref32<Node>>,
    text: &'static str,
}

#[blaze::program_lifetime]
struct Owns {
    children: Vec<u32>,
}

#[blaze::program_lifetime]
struct Dropped {
    kind: u16,
}

impl Drop for Dropped {
    fn drop(&mut self) {}
}

#[blaze::program_lifetime]
pub struct Exported {
    kind: u16,
}

#[blaze::program_lifetime]
struct Stray {
    kind: u16,
}

fn build(arena: &mut ProgramArena) -> Ref32<Node> {
    let stray = Stray { kind: 1 };
    let _ = stray.kind;
    arena.alloc(Node { kind: 0, parent: None, text: "" })
}
"#,
        ),
        FLAGS,
    );
    count(&out, "`Owns` is `#[blaze::program_lifetime]`, but its field `children`", 1);
    count(&out, "holds a value with a `Drop` impl, which would never run", 1);
    count(&out, "`Dropped` is `#[blaze::program_lifetime]`, but has a `Drop` impl", 1);
    count(&out, "`Exported` is `#[blaze::program_lifetime]`, but another crate can name it", 1);
    count(&out, "`Stray` is `#[blaze::program_lifetime]`, but 1 of its values is built outside the program arena", 1);
    count(&out, "`Node` is `#[blaze::program_lifetime]`", 0);
}

/// With the crate's opt-in, a public type may be declared.
#[test]
fn a_crate_opts_its_public_types_in() {
    let out = lint(
        &fixture_of(
            r#"
#![blaze::program_lifetime]

#[blaze::program_lifetime]
pub struct Exported {
    kind: u16,
}
"#,
        ),
        FLAGS,
    );
    count(&out, "is `#[blaze::program_lifetime]`, but", 0);
}

const COULD_BE: &str = "lives until the program exits";

/// A private struct with no `Drop` whose every literal is leaked or is in a frozen static is
/// found, with where the first is kept and the `Arc` clones counted.
#[test]
fn could_be_program_lifetime_finds_types_only_leaked_or_in_statics() {
    let out = lint(
        &fixture_of(
            r#"
use std::sync::{Arc, LazyLock};

struct Keyword {
    id: u16,
    text: &'static str,
}

fn intern(id: u16, text: &'static str) -> &'static Keyword {
    Box::leak(Box::new(Keyword { id, text }))
}

struct Config {
    depth: u32,
}

static CONFIG: LazyLock<Arc<Config>> = LazyLock::new(|| Arc::new(Config { depth: 3 }));

fn share() -> Arc<Config> {
    CONFIG.clone()
}
"#,
        ),
        FLAGS,
    );
    count(&out, COULD_BE, 2);
    count(&out, "every `Keyword` the crate builds lives until the program exits (1 built, the first leaked with `Box::leak`)", 1);
    count(&out, "every `Config` the crate builds lives until the program exits (1 built, the first kept in a `static`)", 1);
    count(&out, "the crate clones an `Arc` or `Rc` of it in 1 places", 1);
    count(&out, "a `Ref32<Keyword>`: 4 bytes", 1);
}

/// Silent: a value that may be dropped (a local), a type with a `Drop` or a field that
/// drops, a public type, a static something can take values out of, a table filled at run
/// time, a type already declared, and a type never built.
#[test]
fn could_be_program_lifetime_is_silent_for_what_it_cannot_follow() {
    let out = lint(
        &fixture_of(
            r#"
use std::sync::{Arc, LazyLock, Mutex};

struct Local {
    id: u16,
}

fn local() -> u16 {
    let it = Local { id: 1 };
    it.id
}

struct Mixed {
    id: u16,
}

fn mixed(leak: bool) -> u16 {
    if leak {
        Box::leak(Box::new(Mixed { id: 1 })).id
    } else {
        Mixed { id: 2 }.id
    }
}

struct Dropped {
    id: u16,
}

impl Drop for Dropped {
    fn drop(&mut self) {}
}

fn dropped() -> &'static Dropped {
    Box::leak(Box::new(Dropped { id: 1 }))
}

struct Heavy {
    items: Vec<u32>,
}

fn heavy() -> &'static Heavy {
    Box::leak(Box::new(Heavy { items: Vec::new() }))
}

pub struct Exported {
    id: u16,
}

pub fn exported() -> &'static Exported {
    Box::leak(Box::new(Exported { id: 1 }))
}

struct Swapped {
    id: u16,
}

static SWAPPED: Mutex<Swapped> = Mutex::new(Swapped { id: 1 });

struct Tabled {
    id: u16,
}

static TABLE: LazyLock<Mutex<Vec<Arc<Tabled>>>> = LazyLock::new(|| Mutex::new(Vec::new()));

fn tabled(table: &mut Vec<Arc<Tabled>>) {
    table.push(Arc::new(Tabled { id: 1 }));
}

#[blaze::program_lifetime]
struct Declared {
    id: u16,
}

struct Unbuilt {
    id: u16,
}
"#,
        ),
        FLAGS,
    );
    count(&out, COULD_BE, 0);
}

const REF32: &str = "a `Ref32<Node>` is 4 bytes";

const HOLDERS: &str = r#"
use blaze_mem::{ProgramArena, Ref32};
use std::sync::Arc;

#[blaze::program_lifetime]
struct Node {
    kind: u16,
}

struct Edge {
    from: &'static Node,
    to: Arc<Node>,
    others: Vec<Box<Node>>,
    weight: u32,
}

struct Graph {
    edges: Vec<Edge>,
}

struct Lone {
    node: &'static Node,
}

fn walk(node: &'static Node, shared: Arc<Node>) -> &'static Node {
    let _local: Arc<Node> = shared;
    node
}
"#;

/// A field or a collection's element of a holder stored many times is found, each pointer
/// kind named; a holder nothing stores many of, a local, a parameter and a return are not.
#[test]
fn could_be_ref32_finds_fields_of_holders_stored_many_times() {
    let out = lint(&fixture_of(HOLDERS), FLAGS);
    count(&out, REF32, 3);
    count(&out, "the field `from` of `Edge`", 1);
    count(&out, "a `&'static` reference; a `Ref32<Node>` is 4 bytes", 1);
    count(&out, "the field `to` of `Edge`", 1);
    count(&out, "an `Arc`, with two counts beside the value", 1);
    count(&out, "the field `others` of `Edge`", 1);
    count(&out, "the field `node` of `Lone`", 0);
}

const FIXABLE: &str = r#"
use blaze_mem::{ProgramArena, Ref32};

#[blaze::program_lifetime]
struct Node {
    kind: u16,
}

#[blaze::program_lifetime]
struct Link {
    node: &'static Node,
    weight: u32,
}

fn link(arena: &mut ProgramArena, node: Ref32<Node>) -> Ref32<Link> {
    arena.alloc(Link { node: node.get(), weight: 1 })
}

fn kind(link: &Link) -> u16 {
    link.node.kind
}

fn node(link: &Link) -> &'static Node {
    link.node
}
"#;

const FIXED: &str = r#"
use blaze_mem::{ProgramArena, Ref32};

#[blaze::program_lifetime]
struct Node {
    kind: u16,
}

#[blaze::program_lifetime]
struct Link {
    node: blaze_mem::Ref32<Node>,
    weight: u32,
}

fn link(arena: &mut ProgramArena, node: Ref32<Node>) -> Ref32<Link> {
    arena.alloc(Link { node: node, weight: 1 })
}

fn kind(link: &Link) -> u16 {
    link.node.get().kind
}

fn node(link: &Link) -> &'static Node {
    link.node.get()
}
"#;

/// The fix of a private `&'static T` field every literal writes from a `Ref32`: the field's
/// type, each literal and each read change together, and the fixed file has nothing left for
/// the lint (each read is the `&'static Node` it was, so it type-checks as before).
#[test]
fn could_be_ref32_fix_rewrites_the_field_its_writes_and_its_reads() {
    let before = lint(&fixture_of(FIXABLE), FLAGS);
    count(&before, "the field `node` of `Link`", 1);
    let fx = fixture::db(&fixture_of(FIXABLE));
    let file = *fx.files().last().expect("src/lib.rs");
    let mut db = fx.into_db();
    let krate = blaze_query::db::attach(&db, || {
        hir::Crate::all(&db)
            .into_iter()
            .find(|it| it.display_name(&db).is_some_and(|it| it.to_string() == "main"))
            .expect("the fixture has `main`")
    });
    let file = file.file_id(&db);
    let name_of = |it: FileId| (it == file).then(|| "src/lib.rs".to_owned());
    let flags: Vec<String> = FLAGS.split_whitespace().map(str::to_owned).collect();
    let setup = Setup {
        command_line: CommandLine::parse(&flags).expect("the flags read"),
        conf: Conf::default(),
    };
    fix(&mut db, krate, &setup, &name_of).unwrap_or_else(|error| panic!("{error}"));
    let text = db.file_text(file).text(&db).to_string();
    assert_eq!(text, FIXED.trim_start_matches('\n'));
    blaze_query::db::attach(&db, || {
        let found = check(&db, krate, &setup, &name_of).expect("the file is named");
        let left: Vec<&str> = found
            .iter()
            .map(|it| it.diagnostic.message.as_str())
            .filter(|it| it.contains("could be") || it.contains(REF32) || it.contains("mismatched"))
            .collect();
        assert!(left.is_empty(), "the fixed file has nothing left:\n{text}\n{left:#?}");
    });
}

/// No fix where a literal writes the field from anything but a `Ref32`'s `get`: the finding
/// stays, and `--fix` changes nothing.
#[test]
fn could_be_ref32_offers_no_fix_for_a_field_written_from_a_plain_reference() {
    let main = FIXABLE.replace(
        "arena.alloc(Link { node: node.get(), weight: 1 })",
        "arena.alloc(Link { node: Box::leak(Box::new(Node { kind: 0 })), weight: 1 })",
    );
    let out = lint(&fixture_of(&main), FLAGS);
    count(&out, "the field `node` of `Link`", 1);
    count(&out, "hold it as a `Ref32`: the literals already have one", 0);
}
