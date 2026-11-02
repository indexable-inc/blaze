//! blaze's own lints over fixtures with an `std` of their own: one whose `Vec`, `String`, `Box`
//! and `Arc` reach an allocator marked `#[rustc_allocator]` the way the real one's do, through
//! `A: Allocator`, `FromIterator` and the `box_new` intrinsic, and whose `HashMap` hashes with
//! `RandomState` by default.
//!
//! A line of the crate `main` says what it expects: `//~ hot_clone` that the line has a
//! `blaze::hot_clone` finding, `//~^ hot_alloc` that the line above has one (for a line whose
//! own comment is an exemption, which a marker would give a reason). Each test runs twice:
//! with `#[deny(blaze::hot)]` on the items that are hot, and with `--deny=blaze::hot` on the
//! crate and those attributes taken away; what is cold says `#[allow(blaze::hot)]`.

use crate::tests::lint;

/// `dispatch_from_dyn` (with `unsize`) makes `&self` methods dispatchable, as with the real
/// `core`: without its lang items no trait is dyn-compatible and no vtable call resolves.
const MINICORE: &str =
    "//- minicore: sized, clone, copy, derive, fn, default, from, deref, builtin_impls, dispatch_from_dyn\n";

const ALLOC_STD: &str = r#"
//- /std/lib.rs crate:std
#![feature(lang_items)]
pub mod prelude {
    pub mod v1 {
        pub use crate::{format, vec};
        pub use crate::borrow::ToOwned;
        pub use crate::boxed::Box;
        pub use crate::iter::{FromIterator, Iterator};
        pub use crate::slice::ToVec;
        pub use crate::string::{String, ToString};
        pub use crate::vec::Vec;
        pub use core::prelude::v1::*;
    }
    pub mod rust_2015 { pub use super::v1::*; }
    pub mod rust_2018 { pub use super::v1::*; }
    pub mod rust_2021 { pub use super::v1::*; }
    pub mod rust_2024 { pub use super::v1::*; }
}
pub mod alloc {
    extern "Rust" {
        #[rustc_allocator]
        fn __rust_alloc(size: usize, align: usize) -> *mut u8;
    }
    pub fn alloc(size: usize) -> *mut u8 {
        unsafe { __rust_alloc(size, 8) }
    }
    pub trait Allocator {
        fn allocate(&self, size: usize) -> *mut u8;
    }
    pub struct Global;
    impl Allocator for Global {
        fn allocate(&self, size: usize) -> *mut u8 {
            alloc(size)
        }
    }
    impl<A: Allocator> Allocator for &A {
        fn allocate(&self, size: usize) -> *mut u8 {
            (**self).allocate(size)
        }
    }
    #[lang = "exchange_malloc"]
    unsafe fn exchange_malloc(size: usize, _align: usize) -> *mut u8 {
        alloc(size)
    }
}
pub mod intrinsics {
    #[rustc_intrinsic]
    pub fn box_new<T>(x: T) -> crate::boxed::Box<T>;
}
pub mod boxed {
    #[lang = "owned_box"]
    pub struct Box<T: ?Sized>(pub *mut T);
    impl<T> Box<T> {
        pub fn new(x: T) -> Box<T> {
            crate::intrinsics::box_new(x)
        }
    }
    impl<T: ?Sized> core::ops::Deref for Box<T> {
        type Target = T;
        fn deref(&self) -> &T {
            loop {}
        }
    }
}
pub mod sync {
    pub struct Arc<T: ?Sized> {
        ptr: *mut T,
    }
    impl<T> Arc<T> {
        pub fn new(x: T) -> Arc<T> {
            Arc { ptr: crate::boxed::Box::new(x).0 }
        }
    }
    impl<T: ?Sized> Clone for Arc<T> {
        fn clone(&self) -> Arc<T> {
            Arc { ptr: self.ptr }
        }
    }
}
pub mod iter {
    pub trait FromIterator<A>: Sized {
        fn from_iter<I: Iterator<Item = A>>(iter: I) -> Self;
    }
    pub trait Iterator {
        type Item;
        fn collect<B: FromIterator<Self::Item>>(self) -> B
        where
            Self: Sized,
        {
            B::from_iter(self)
        }
    }
}
pub mod vec {
    use crate::alloc::{Allocator, Global};
    pub struct Vec<T, A: Allocator = Global> {
        ptr: *mut T,
        cap: usize,
        alloc: A,
    }
    impl<T> Vec<T> {
        pub const fn new() -> Vec<T> {
            Vec { ptr: 0 as *mut T, cap: 0, alloc: Global }
        }
        pub fn with_capacity(cap: usize) -> Vec<T> {
            Vec::with_capacity_in(cap, Global)
        }
    }
    impl<T, A: Allocator> Vec<T, A> {
        pub const fn new_in(alloc: A) -> Vec<T, A> {
            Vec { ptr: 0 as *mut T, cap: 0, alloc }
        }
        pub fn with_capacity_in(cap: usize, alloc: A) -> Vec<T, A> {
            let ptr = alloc.allocate(cap) as *mut T;
            Vec { ptr, cap, alloc }
        }
        pub fn push(&mut self, _value: T) {
            self.grow();
        }
        fn grow(&mut self) {
            self.ptr = self.alloc.allocate(self.cap) as *mut T;
        }
    }
    impl<T: Clone> Clone for Vec<T> {
        fn clone(&self) -> Vec<T> {
            Vec::with_capacity(self.cap)
        }
    }
    impl<T> crate::iter::FromIterator<T> for Vec<T> {
        fn from_iter<I: crate::iter::Iterator<Item = T>>(_iter: I) -> Vec<T> {
            Vec::with_capacity(0)
        }
    }
}
pub mod slice {
    use crate::vec::Vec;
    pub trait ToVec<T> {
        fn to_vec(&self) -> Vec<T>;
    }
    impl<T: Clone> ToVec<T> for [T] {
        fn to_vec(&self) -> Vec<T> {
            Vec::with_capacity(0)
        }
    }
}
pub mod string {
    use crate::vec::Vec;
    pub struct String {
        vec: Vec<u8>,
    }
    impl String {
        pub const fn new() -> String {
            String { vec: Vec::new() }
        }
        pub fn with_capacity(cap: usize) -> String {
            String { vec: Vec::with_capacity(cap) }
        }
    }
    impl Clone for String {
        fn clone(&self) -> String {
            String { vec: self.vec.clone() }
        }
    }
    impl From<&str> for String {
        fn from(_s: &str) -> String {
            String::with_capacity(0)
        }
    }
    pub trait ToString {
        fn to_string(&self) -> String;
    }
    impl ToString for str {
        fn to_string(&self) -> String {
            String::from(self)
        }
    }
}
pub mod borrow {
    pub trait ToOwned {
        type Owned;
        fn to_owned(&self) -> Self::Owned;
    }
    impl ToOwned for str {
        type Owned = crate::string::String;
        fn to_owned(&self) -> crate::string::String {
            crate::string::String::from(self)
        }
    }
}
pub mod fmt {
    pub fn format(_s: &str) -> crate::string::String {
        crate::string::String::with_capacity(0)
    }
}
#[macro_export]
macro_rules! format {
    ($s:literal) => {
        $crate::fmt::format($s)
    };
    ($s:literal, $($arg:expr),+) => {
        $crate::fmt::format($s)
    };
}
#[macro_export]
macro_rules! vec {
    () => {
        $crate::vec::Vec::new()
    };
    ($($x:expr),+ $(,)?) => {{
        let mut v = $crate::vec::Vec::with_capacity(0);
        $(v.push($x);)+
        v
    }};
}
pub mod hash {
    pub struct RandomState {
        k0: u64,
    }
    impl RandomState {
        pub fn new() -> RandomState {
            RandomState { k0: 0 }
        }
    }
    impl Default for RandomState {
        fn default() -> RandomState {
            RandomState::new()
        }
    }
}
pub mod collections {
    use crate::{hash::RandomState, iter::{FromIterator, Iterator}, vec::Vec};
    pub struct HashMap<K, V, S = RandomState> {
        keys: Vec<K>,
        values: Vec<V>,
        hasher: S,
    }
    impl<K, V> HashMap<K, V, RandomState> {
        pub fn new() -> HashMap<K, V, RandomState> {
            HashMap::with_hasher(RandomState::new())
        }
    }
    impl<K, V, S> HashMap<K, V, S> {
        pub fn with_hasher(hasher: S) -> HashMap<K, V, S> {
            HashMap { keys: Vec::new(), values: Vec::new(), hasher }
        }
    }
    impl<K, V, S: Default> Default for HashMap<K, V, S> {
        fn default() -> HashMap<K, V, S> {
            HashMap::with_hasher(S::default())
        }
    }
    impl<K, V, S: Default> FromIterator<(K, V)> for HashMap<K, V, S> {
        fn from_iter<I: Iterator<Item = (K, V)>>(_iter: I) -> HashMap<K, V, S> {
            HashMap::with_hasher(S::default())
        }
    }
    pub struct HashSet<T, S = RandomState> {
        map: HashMap<T, (), S>,
    }
    impl<T> HashSet<T, RandomState> {
        pub fn new() -> HashSet<T, RandomState> {
            HashSet { map: HashMap::new() }
        }
    }
}
//- /rustc_hash/lib.rs crate:rustc_hash deps:std library
pub struct FxBuildHasher;
impl Default for FxBuildHasher {
    fn default() -> FxBuildHasher {
        FxBuildHasher
    }
}
pub type FxHashMap<K, V> = std::collections::HashMap<K, V, FxBuildHasher>;
"#;

const MAIN: &str = "//- /src/lib.rs crate:main deps:std,rustc_hash\n";

const HOT_ATTR: &str = "#[deny(blaze::hot)]";

/// The lint a message is blaze's of.
fn lint_of(message: &str) -> Option<&'static str> {
    if message.starts_with("this `clone` of") {
        Some("hot_clone")
    } else if message.contains("through the workspace's own code") {
        Some("hot_alloc_call")
    } else if message.contains("the global allocator") {
        Some("hot_alloc")
    } else if message.contains("goes through the vtable") {
        Some("hot_dyn")
    } else if message.contains("SipHash") {
        Some("std_hashmap")
    } else {
        None
    }
}

/// The findings of blaze's lints in `src/lib.rs` of `out`, as (line, lint).
fn found(out: &str) -> Vec<(u32, &'static str)> {
    let mut found = Vec::new();
    let mut lint = None;
    for line in out.lines() {
        if let Some(message) = line.strip_prefix("error: ").or_else(|| line.strip_prefix("warning: ")) {
            lint = lint_of(message);
            continue;
        }
        let Some(at) = line.trim_start().strip_prefix("--> src/lib.rs:") else { continue };
        let Some(name) = lint.take() else { continue };
        let number = at.split(':').next().and_then(|it| it.parse().ok());
        found.push((number.expect("a line number after the path"), name));
    }
    found.sort_unstable();
    found
}

/// What the markers of `main` expect, as (line, lint).
fn expected(main: &str) -> Vec<(u32, &'static str)> {
    let mut expected = Vec::new();
    for (index, line) in main.lines().enumerate() {
        let number = index as u32 + 1;
        let Some((_, marks)) = line.split_once("//~") else { continue };
        let (number, marks) = match marks.strip_prefix('^') {
            Some(marks) => (number - 1, marks),
            None => (number, marks),
        };
        for mark in marks.split_whitespace() {
            let lint = ["hot_clone", "hot_alloc", "hot_alloc_call", "hot_dyn", "std_hashmap"]
                .into_iter()
                .find(|&it| it == mark)
                .unwrap_or_else(|| panic!("`{mark}` is not a lint of blaze's"));
            expected.push((number, lint));
        }
    }
    expected.sort_unstable();
    expected
}

/// Runs `main` with its hot items marked, then with the crate denying `blaze::hot` and the
/// marks taken away; both must find what the markers say.
fn check(main: &str) {
    let main = main.trim_start_matches('\n');
    let want = expected(main);
    let crate_wide = main.replace(HOT_ATTR, "#[allow(unused)]");
    for (source, flags) in [
        (main, "-A clippy::all"),
        (crate_wide.as_str(), "-A clippy::all --deny=blaze::hot"),
    ] {
        let fixture = format!("{MINICORE}{ALLOC_STD}{MAIN}{source}");
        let out = lint(&fixture, flags);
        assert_eq!(found(&out), want, "with `{flags}`, the findings of:\n{source}\nwere:\n{out}");
    }
}

#[test]
fn hot_clone() {
    check(
        r#"
use std::sync::Arc;
#[derive(Clone, Copy)]
struct Id(u32);
#[derive(Clone)]
struct Pair {
    a: u32,
    b: Id,
}
#[derive(Clone)]
struct Named {
    name: String,
}
struct Manual(u32);
#[allow(blaze::hot)]
impl Clone for Manual {
    fn clone(&self) -> Manual {
        Manual(self.0)
    }
}
type Text = String;

#[deny(blaze::hot)]
fn hot<T: Clone>(s: &String, a: &Arc<u32>, p: &Pair, id: Id, n: &Named, m: &Manual, t: &T, x: &Text) {
    let _ = s.clone(); //~ hot_clone
    let _ = Clone::clone(s); //~ hot_clone
    let _ = String::clone(s); //~ hot_clone
    let _ = a.clone(); //~ hot_clone
    let _ = p.clone();
    let _ = id.clone();
    let _ = 7u32.clone();
    let _ = (1u32, id).clone();
    let _ = n.clone(); //~ hot_clone
    let _ = m.clone(); //~ hot_clone
    let _ = t.clone(); //~ hot_clone
    let _ = x.clone(); //~ hot_clone
}

#[allow(blaze::hot)]
fn cold(s: &String) {
    let _ = s.clone();
}

#[deny(blaze::hot)]
fn exempt(s: &String) {
    let _ = s.clone(); // clone: the caller keeps its own
    // clone: once per request
    let _ = s.clone();
    let _ = s.clone(); // clone:
    //~^ hot_clone
    let _ = s.clone(); // clone:    
    //~^ hot_clone
    let _ = s.clone(); // alloc: the wrong tag
    //~^ hot_clone
}
"#,
    );
}

#[test]
fn hot_alloc() {
    check(
        r#"
use std::sync::Arc;
struct Count(u32);
impl Iterator for Count {
    type Item = u32;
}

#[deny(blaze::hot)]
fn hot(s: &str, v: &[u32], buf: &mut Vec<u32>) {
    let _ = String::new(); //~ hot_alloc
    let _ = String::from("x"); //~ hot_alloc
    let _ = s.to_string(); //~ hot_alloc
    let _ = s.to_owned(); //~ hot_alloc
    let _ = format!("x"); //~ hot_alloc
    let _ = format!("{}", 1); //~ hot_alloc
    let _: Vec<u32> = Vec::new(); //~ hot_alloc
    let _: Vec<u32> = vec![]; //~ hot_alloc
    let _ = vec![1u32, 2]; //~ hot_alloc
    let _ = v.to_vec(); //~ hot_alloc
    let _ = Vec::<u32>::with_capacity(4); //~ hot_alloc
    let _ = Count(0).collect::<Vec<u32>>(); //~ hot_alloc
    let _: Vec<u32> = Count(0).collect(); //~ hot_alloc
    let _ = Box::new(1u32); //~ hot_alloc
    let _ = Arc::new(1u32); //~ hot_alloc
    buf.push(1);
    let _ = make(); //~ hot_alloc_call
    let _ = ping(true); //~ hot_alloc_call
    let _ = even(true);
}

#[allow(blaze::hot)]
fn make() -> String {
    String::from("made")
}

#[allow(blaze::hot)]
fn ping(stop: bool) -> String {
    if stop { make() } else { pong(stop) }
}

#[allow(blaze::hot)]
fn pong(stop: bool) -> String {
    ping(stop)
}

#[allow(blaze::hot)]
fn even(stop: bool) -> bool {
    if stop { true } else { odd(stop) }
}

#[allow(blaze::hot)]
fn odd(stop: bool) -> bool {
    even(stop)
}

#[allow(blaze::hot)]
fn cold() {
    let _ = String::from("x");
}

#[deny(blaze::hot)]
fn exempt(s: &str) {
    let _ = String::from("x"); // alloc: once per build
    // alloc: the error path
    let _ = s.to_string();
    let _ = format!("x"); // alloc:
    //~^ hot_alloc
    let _ = Box::new(1u32); // dyn: the wrong tag
    //~^ hot_alloc
}
"#,
    );
}

/// A crate that denies `hot_alloc` (`hot_alloc = "deny"` in `[workspace.metadata.blaze.lints]`
/// gives the flag) fails on a fresh `Vec::new()`, `vec![]` or `String::new()`: each is an error,
/// though none allocates until it grows. A `Vec` that names its allocator, and a line saying why
/// it needs the global heap, pass.
#[test]
fn a_fresh_global_vec_in_a_crate_denying_hot_alloc_is_an_error() {
    let main = r#"
use std::alloc::Allocator;
struct Frame;
impl Allocator for Frame {
    fn allocate(&self, _size: usize) -> *mut u8 {
        0 as *mut u8
    }
}

pub fn task(frame: &Frame) -> usize {
    let fresh: Vec<u32> = Vec::new(); //~ hot_alloc
    let empty: Vec<u32> = vec![]; //~ hot_alloc
    let text = String::new(); //~ hot_alloc
    let framed: Vec<u32, &Frame> = Vec::new_in(frame);
    let kept: Vec<u32> = Vec::new(); // alloc: handed to the caller, who outlives the task
    let _ = (fresh, empty, text, framed, kept);
    0
}
"#;
    let main = main.trim_start_matches('\n');
    let fixture = format!("{MINICORE}{ALLOC_STD}{MAIN}{main}");
    let out = lint(&fixture, "-A clippy::all --deny=blaze::hot_alloc");
    assert_eq!(found(&out), expected(main), "the findings of:\n{main}\nwere:\n{out}");
    let errors = out.lines().filter(|it| it.starts_with("error: ")).count();
    assert_eq!(errors, 3, "each finding is an error:\n{out}");
}

/// Under `hot_alloc` a function of the workspace's allocation is found once, in that function:
/// a call of it is none, while a call into std that allocates is one.
#[test]
fn hot_alloc_finds_a_helpers_allocation_in_the_helper_not_at_its_caller() {
    let main = r#"
fn helper() -> String {
    String::from("made") //~ hot_alloc
}

pub fn caller() {
    let _ = helper();
    let _ = Box::new(1u32); //~ hot_alloc
}
"#;
    let main = main.trim_start_matches('\n');
    let fixture = format!("{MINICORE}{ALLOC_STD}{MAIN}{main}");
    let out = lint(&fixture, "-A clippy::all --deny=blaze::hot_alloc");
    assert_eq!(found(&out), expected(main), "the findings of:\n{main}\nwere:\n{out}");
}

/// A function marked hot promises that nothing it calls allocates: a call of a function of the
/// workspace that does is a finding at the call, through any depth of calls.
#[test]
fn a_hot_function_calling_an_allocating_helper_is_a_finding_at_the_call() {
    check(
        r#"
#[allow(blaze::hot)]
fn helper() -> String {
    String::from("made")
}

#[allow(blaze::hot)]
fn wrapper() -> String {
    helper()
}

#[deny(blaze::hot)]
fn caller() {
    let _ = helper(); //~ hot_alloc_call
    let _ = wrapper(); //~ hot_alloc_call
}
"#,
    );
}

#[test]
fn hot_dyn() {
    check(
        r#"
trait Shape {
    fn area(&self) -> u32;
}
struct Square;
impl Shape for Square {
    fn area(&self) -> u32 {
        1
    }
}

#[deny(blaze::hot)]
fn hot(s: &dyn Shape, b: Box<dyn Shape>, f: &dyn Fn(u32) -> u32, g: Box<dyn Fn()>, q: &Square) {
    let _ = s.area(); //~ hot_dyn
    let _ = b.area(); //~ hot_dyn
    let _ = Shape::area(s); //~ hot_dyn
    let _ = f(1); //~ hot_dyn
    g(); //~ hot_dyn
    let _ = q.area();
    let _: [&dyn Shape; 0] = [];
}

#[deny(blaze::hot)]
fn in_types<'a>(shapes: &[&'a dyn Shape]) -> [&'a dyn Shape; 0] {
    let _ = shapes;
    []
}

#[allow(blaze::hot)]
fn cold(s: &dyn Shape) {
    let _ = s.area();
}

#[deny(blaze::hot)]
fn exempt(s: &dyn Shape) {
    let _ = s.area(); // dyn: one call per frame
    // dyn: a plugin's
    let _ = s.area();
    let _ = s.area(); // dyn:
    //~^ hot_dyn
    let _ = s.area(); // clone: the wrong tag
    //~^ hot_dyn
}
"#,
    );
}

#[test]
fn std_hashmap() {
    check(
        r#"
use std::collections::{HashMap, HashSet};
use std::hash::RandomState;
use rustc_hash::{FxBuildHasher, FxHashMap};
struct Pairs;
impl Iterator for Pairs {
    type Item = (u32, u32);
}

#[deny(blaze::hot)]
struct Index {
    names: HashMap<u32, u32>, //~ std_hashmap
    fast: FxHashMap<u32, u32>,
}

#[deny(blaze::hot)]
type Map = HashMap<u32, u32>; //~ std_hashmap

#[deny(blaze::hot)]
fn hot(m: &HashMap<u32, u32>) { //~ std_hashmap
    let _ = m;
    let _: HashMap<u32, u32> = HashMap::new(); //~ std_hashmap std_hashmap
    let _ = HashSet::<u32>::new(); //~ std_hashmap
    let _: HashMap<u32, u32> = Default::default(); //~ std_hashmap std_hashmap
    let _ = Pairs.collect::<HashMap<u32, u32>>(); //~ std_hashmap
    let _: HashMap<u32, u32> = Pairs.collect(); //~ std_hashmap std_hashmap
    let _: HashMap<u32, u32, RandomState> = HashMap::with_hasher(RandomState::new()); //~ std_hashmap std_hashmap
    let _: FxHashMap<u32, u32> = FxHashMap::default(); //~ hot_alloc
    let _: HashMap<u32, u32, FxBuildHasher> = HashMap::with_hasher(FxBuildHasher); //~ hot_alloc
    let _: FxHashMap<u32, u32> = Pairs.collect(); //~ hot_alloc
}

#[deny(blaze::hot)]
fn generic<S: Default>() -> HashMap<u32, u32, S> {
    HashMap::with_hasher(S::default()) //~ hot_alloc
}

#[allow(blaze::hot)]
fn cold() -> HashMap<u32, u32> {
    HashMap::new()
}

#[deny(blaze::hot)]
fn exempt() {
    let _: HashMap<u32, u32> = HashMap::new(); // alloc: built once at startup
    // alloc: the slow path
    let _: HashMap<u32, u32> = HashMap::new();
    let _: HashMap<u32, u32> = HashMap::new(); // alloc:
    //~^ std_hashmap std_hashmap
    let _: HashMap<u32, u32> = HashMap::new(); // dyn: the wrong tag
    //~^ std_hashmap std_hashmap
}
"#,
    );
}

/// `fiber_boundary` over a `blaze_graph` whose `sched` has the fibers, a `corosensei`, and a
/// crate `main` depending on both. A line of the crate linted says what it expects:
/// `//~ fiber` that the line names `blaze_graph::sched::fiber` outside `sched`, `//~ corosensei`
/// that it names `corosensei`, `//~ driver` that it names the fiber driver.
mod fiber_boundary {
    use base_db::FileId;
    use blaze_diagnostics::emit::{Sources, Style};
    use blaze_hash::HashMap;
    use hir_expand::fixture;

    use crate::{
        check::{Setup, check},
        conf::Conf,
        level::CommandLine,
    };

    const FLAGS: &str = "-A clippy::all --deny=blaze::fiber_boundary";

    const SCHED: &str = r#"
pub mod sched {
    pub mod fiber {
        pub struct Driver;
        pub struct Carried;
        pub fn wait() {}
    }
    pub use fiber::Driver;
    pub fn carry() {}
}
"#;

    fn fixture(graph: &str, main: &str) -> String {
        format!(
            "//- minicore: sized\n\
             //- /corosensei/lib.rs crate:corosensei\n\
             pub struct Coroutine;\n\
             //- /blaze_graph/lib.rs crate:blaze_graph deps:corosensei\n\
             {}\n\
             //- /src/lib.rs crate:main deps:blaze_graph,corosensei\n\
             {}\n",
            graph.trim_matches('\n'),
            main.trim_matches('\n'),
        )
    }

    /// The diagnostics of the lints in the crate `linted` of `fixture`, each as rustc writes it.
    fn lint_crate(fixture: &str, linted: &str) -> String {
        let db = fixture::db(fixture).into_db();
        hir::attach_db(&db, || {
            let krate = hir::Crate::all(&db)
                .into_iter()
                .find(|it| it.display_name(&db).is_some_and(|it| it.to_string() == linted))
                .expect("the fixture has the crate linted");
            let mut names: HashMap<FileId, String> = HashMap::default();
            let mut texts = Vec::new();
            for krate in hir::Crate::all(&db) {
                for module in krate.modules(&db) {
                    let Some(file) = module.definition_source_file_id(&db).file_id() else { continue };
                    let file = file.file_id(&db);
                    let root = db.file_source_root(file).source_root_id(&db);
                    let root = db.source_root(root).source_root(&db);
                    let Some(path) = root.path_for_file(&file) else { continue };
                    let name = path.to_string().trim_start_matches('/').to_owned();
                    if names.insert(file, name.clone()).is_none() {
                        texts.push((name, db.file_text(file).text(&db).to_string()));
                    }
                }
            }
            let flags: Vec<String> = FLAGS.split_whitespace().map(str::to_owned).collect();
            let setup = Setup {
                command_line: CommandLine::parse(&flags).expect("the flags read"),
                conf: Conf::default(),
            };
            let name_of = |file: FileId| names.get(&file).cloned();
            let found = check(&db, krate, &setup, &name_of).expect("every file of the crates is named");
            let mut sources = Sources::of(texts);
            crate::serve::written(found.iter(), &mut sources, |_| None, Style::default())
                .expect("every file a lint points into is given")
                .into_iter()
                .map(|it| it.rendered)
                .collect()
        })
    }

    /// What a message of `fiber_boundary` says is named.
    fn named_in(message: &str) -> Option<&'static str> {
        let named = message.strip_suffix(" is named outside `blaze_graph::sched`")?;
        match named {
            "`blaze_graph::sched::fiber`" => Some("fiber"),
            "`corosensei`" => Some("corosensei"),
            _ if named.starts_with("the fiber driver `") => Some("driver"),
            _ => None,
        }
    }

    /// The findings of `out`, as (file, line, what is named).
    fn found(out: &str) -> Vec<(String, u32, &'static str)> {
        let mut found = Vec::new();
        let mut named = None;
        for line in out.lines() {
            if let Some(message) = line.strip_prefix("error: ").or_else(|| line.strip_prefix("warning: ")) {
                named = named_in(message);
                continue;
            }
            let Some(at) = line.trim_start().strip_prefix("--> ") else { continue };
            let Some(named) = named.take() else { continue };
            let mut parts = at.rsplitn(3, ':');
            let (_column, number, file) = (parts.next(), parts.next(), parts.next());
            let number = number.and_then(|it| it.parse().ok()).expect("a line number after the path");
            found.push((file.expect("a path before the line").to_owned(), number, named));
        }
        found.sort_unstable();
        found
    }

    /// What the markers of `fixture` expect, as (file, line, what is named).
    fn expected(fixture: &str) -> Vec<(String, u32, &'static str)> {
        let mut expected = Vec::new();
        let mut file = None;
        let mut number = 0;
        for line in fixture.lines() {
            if let Some(header) = line.strip_prefix("//- ") {
                file = header.strip_prefix('/').and_then(|it| it.split_whitespace().next());
                number = 0;
                continue;
            }
            number += 1;
            let Some((_, marks)) = line.split_once("//~") else { continue };
            let file = file.expect("a marker is in a file of the fixture");
            for mark in marks.split_whitespace() {
                let named = ["fiber", "corosensei", "driver"]
                    .into_iter()
                    .find(|&it| it == mark)
                    .unwrap_or_else(|| panic!("`{mark}` is not what `fiber_boundary` names"));
                expected.push((file.to_owned(), number, named));
            }
        }
        expected.sort_unstable();
        expected
    }

    /// Lints the crate `linted` of the fixture of `graph` and `main`; it must find what the
    /// markers say, each finding with the way through the scheduler's API as its help.
    fn check_fibers(graph: &str, main: &str, linted: &str) {
        let fixture = fixture(graph, main);
        let out = lint_crate(&fixture, linted);
        let want = expected(&fixture);
        assert_eq!(found(&out), want, "the findings in `{linted}` of:\n{fixture}\nwere:\n{out}");
        if !want.is_empty() {
            assert!(out.contains("fibers are the scheduler's alone"), "{out}");
        }
    }

    #[test]
    fn an_import_of_sched_fiber_outside_sched_is_rejected() {
        check_fibers(
            SCHED,
            r#"
use blaze_graph::sched::fiber::Carried; //~ fiber
use blaze_graph::sched::{carry, fiber}; //~ fiber
pub fn take(_: Carried) {}
"#,
            "main",
        );
    }

    #[test]
    fn a_path_through_sched_fiber_in_an_expression_is_rejected() {
        check_fibers(
            SCHED,
            r#"
pub fn run() {
    blaze_graph::sched::fiber::wait(); //~ fiber
    let _ = blaze_graph::sched::fiber::Carried; //~ fiber
}
pub fn pattern(it: blaze_graph::sched::fiber::Carried) { //~ fiber
    let blaze_graph::sched::fiber::Carried = it; //~ fiber
}
"#,
            "main",
        );
    }

    #[test]
    fn corosensei_named_outside_sched_is_rejected() {
        check_fibers(
            SCHED,
            r#"
use corosensei::Coroutine; //~ corosensei
extern crate corosensei; //~ corosensei
pub fn make() -> corosensei::Coroutine { //~ corosensei
    Coroutine //~ corosensei
}
"#,
            "main",
        );
        check_fibers(
            r#"
pub mod sched {
    pub mod fiber {
        pub struct Driver;
        pub struct Carried;
        pub fn wait() {}
    }
    pub use fiber::Driver;
    pub fn carry() {}
}
pub mod util {
    use corosensei::Coroutine; //~ corosensei
    pub mod sched {
        use corosensei::Coroutine; //~ corosensei
    }
}
"#,
            "",
            "blaze_graph",
        );
    }

    #[test]
    fn the_driver_reexported_from_sched_is_still_rejected() {
        check_fibers(
            SCHED,
            r#"
use blaze_graph::sched::Driver; //~ driver
pub fn drive(it: blaze_graph::sched::Driver) -> Driver { //~ driver driver
    it
}
"#,
            "main",
        );
    }

    #[test]
    fn sched_and_its_children_may_name_fibers() {
        check_fibers(
            r#"
pub mod sched {
    use corosensei::Coroutine;
    pub mod fiber {
        use corosensei::Coroutine;
        pub struct Driver;
        pub struct Carried;
        pub fn wait() {}
        pub fn drive(_: Driver, _: Coroutine) {
            self::wait();
            crate::sched::fiber::wait();
        }
    }
    pub use fiber::Driver;
    pub fn carry() {
        fiber::wait();
        let _ = crate::sched::fiber::Carried;
    }
    pub fn driver(it: Driver) -> Driver {
        it
    }
    pub mod nested {
        pub fn deeper(_: corosensei::Coroutine) {
            super::fiber::wait();
        }
    }
}
"#,
            "",
            "blaze_graph",
        );
    }

    #[test]
    fn the_schedulers_own_api_is_accepted() {
        check_fibers(
            SCHED,
            r#"
use blaze_graph::sched;
use blaze_graph::sched::carry;
pub fn run() {
    blaze_graph::sched::carry();
    sched::carry();
    carry();
}
"#,
            "main",
        );
    }
}

/// The compile's registry has blaze's lints and group as this crate has them, in the same
/// order, so a level set for one in the build is known there and only this crate gives it.
#[test]
fn the_registry_has_blazes_lints_and_group_as_here() {
    use blaze_diagnostics::lints::registry;
    let registered: Vec<&str> =
        registry::blaze_lints().iter().filter(|it| it.is_externally_loaded).map(|it| it.name).collect();
    let here: Vec<String> = super::Lint::ALL.iter().map(|it| format!("blaze::{}", it.name())).collect();
    assert_eq!(registered, here);
    let hot = registry::group("blaze::hot").expect("the registry's `blaze::hot`");
    let hot: Vec<&str> = hot.lints.iter().map(|it| it.name).collect();
    let here: Vec<String> = super::HOT.iter().map(|it| format!("blaze::{}", it.name())).collect();
    assert_eq!(hot, here);
}
