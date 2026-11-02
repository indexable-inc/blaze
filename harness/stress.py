#!/usr/bin/env python3
"""Edits of the kinds people and agents make, thrown at one warm daemon, each held to a fresh
build of the same source.

usage: stress.py [--only TEXT]

Copies harness/stress (three crates) to a scratch directory and applies STEPS in order. The
edits stay: each step builds on the ones before. After each, the daemon is asked for the
program by `blaze build`, and it is run and held to what a fresh build of the same source does
(rebuild.py). A step marked broken is one that is not Rust: both builds have to be refused and
the daemon has to survive it. Writes STRESS.md, and exits 1 when a step is wrong.
"""
import os, re, shutil, sys, time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from rebuild import WRONG, Project, copy, scratch

BASE, SHAPES, STACK = "base/src/lib.rs", "base/src/shapes.rs", "base/src/stack.rs"
MID, TEXT, TASKS = "mid/src/lib.rs", "mid/src/text.rs", "mid/src/tasks.rs"
MAIN, REPORT = "app/src/main.rs", "app/src/report.rs"


def sub(path, old, new):
    return ("sub", path, old, new)


def write(path, text):
    return ("write", path, text)


def append(path, text):
    return ("append", path, text)


def delete(path):
    return ("delete", path)


def step(name, *ops, broken=False):
    return (name, ops, broken)


PROBE = '    report::line("guarded", guarded());\n'


def probe(name, expr):
    """Adds a line to what `main` prints."""
    return sub(MAIN, PROBE, f'    report::line("{name}", {expr});\n' + PROBE)


STEPS = [
    # Bodies.
    step("body: constant in a leaf function", sub(TEXT, "out.push('!');", "out.push('?');")),
    step("body: in main", sub(MAIN, "local_sum(&[1, 2, 3])", "local_sum(&[1, 2, 3, 4])")),
    step("body: generic function used from another crate", sub(BASE, "if item > best {", "if item >= best {")),
    step("body: generic method of a generic struct", sub(STACK, "Some((top.clone(), top.clone()))", "Some((top.clone(), self.items[0].clone()))")),
    step("body: trait default method", sub(SHAPES, "self.area() * 2", "self.area() * 2 + 1")),
    step("body: closure inside a function", sub(MID, "move |x| x * by", "move |x| x * by + 1")),
    step("body: async fn", sub(TASKS, "x + 10", "x + 11")),
    step("body: async fn gets another await", sub(TASKS, "    a + b\n", "    let c = step(b).await;\n    a + b + c\n")),
    step("body: Drop impl", sub(MID, 'format!("drop {}", self.0)', 'format!("bye {}", self.0)')),
    step("body: match arm in trait impl", sub(SHAPES, "Shape::Circle(r) => 3 * r * r,", "Shape::Circle(r) => 4 * r * r,")),
    # Lines move.
    step("blank line at top of a file with line!() and track_caller", sub(BASE, "//! The bottom crate", "\n//! The bottom crate")),
    step("blank line above a panic site", sub(MAIN, "fn guarded() -> String {", "\nfn guarded() -> String {")),
    step("comment appended", append(MID, "// trailing comment\n")),
    # Items.
    step("new private function, unused", append(TEXT, "\nfn unused_helper(x: u32) -> u32 {\n    x + 1\n}\n")),
    step("new public function, called from main", append(TEXT, "\npub fn reversed(s: &str) -> String {\n    s.chars().rev().collect()\n}\n"),
         probe("reversed", 'smid::text::reversed("abc")')),
    step("function renamed, callers updated", sub(TEXT, "pub fn reversed(", "pub fn backwards("), sub(MAIN, "smid::text::reversed(", "smid::text::backwards(")),
    step("function removed", sub(TEXT, "\nfn unused_helper(x: u32) -> u32 {\n    x + 1\n}\n", "")),
    step("function moved to another file of the crate", sub(TEXT, "\npub fn backwards(s: &str) -> String {\n    s.chars().rev().collect()\n}\n", ""),
         append(MID, "\npub fn backwards(s: &str) -> String {\n    s.chars().rev().collect()\n}\n"), sub(MAIN, "smid::text::backwards(", "smid::backwards(")),
    # Signatures.
    step("signature: parameter added, callers in another crate updated", sub(TEXT, "pub fn shout(s: &str) -> String {", "pub fn shout(s: &str, times: usize) -> String {"),
         sub(TEXT, "out.push('?');", "out.push_str(&\"?\".repeat(times));"), sub(MAIN, 'smid::text::shout("hi")', 'smid::text::shout("hi", 3)')),
    step("signature: return type changed", sub(BASE, "pub fn digest(bytes: &[u8]) -> u32 {\n    let mut h = 17u32;", "pub fn digest(bytes: &[u8]) -> u64 {\n    let mut h = 17u64;"),
         sub(BASE, "h = h.wrapping_mul(31).wrapping_add(b as u32);", "h = h.wrapping_mul(31).wrapping_add(b as u64);")),
    step("signature: parameter type &str to String", sub(TEXT, "pub fn initials(s: &str) -> String {\n    words(s)", "pub fn initials(s: String) -> String {\n    words(&s)"),
         sub(MAIN, 'smid::text::initials("a big cat")', 'smid::text::initials("a big cat".to_string())')),
    step("signature: function made generic", sub(MAIN, "fn local_sum(items: &[u32]) -> u32 {", "fn local_sum<T: Copy + std::iter::Sum<T>>(items: &[T]) -> T {"),
         sub(MAIN, "items.iter().sum()", "items.iter().copied().sum()")),
    step("signature: impl Fn return becomes Box<dyn Fn>", sub(MID, "pub fn multiplier(by: u32) -> impl Fn(u32) -> u32 {\n    move |x| x * by + 1", "pub fn multiplier(by: u32) -> Box<dyn Fn(u32) -> u32> {\n    Box::new(move |x| x * by + 1)")),
    step("signature: const fn body and a const that uses it", sub(BASE, "    x * SCALE\n", "    x * SCALE + 1\n")),
    step("signature: fn becomes async, caller awaits through block_on", sub(TEXT, "pub fn words(s: &str) -> Vec<&str> {", "pub fn count_words(s: &str) -> usize {\n    words(s).len()\n}\n\npub async fn count_later(s: &str) -> usize {\n    count_words(s)\n}\n\npub fn words(s: &str) -> Vec<&str> {"),
         probe("later", 'smid::tasks::block_on(smid::text::count_later("a b c"))')),
    # Types.
    step("struct: field added, constructor updated", sub(MID, "    pub label: String,\n}", "    pub label: String,\n    pub weight: u8,\n}"),
         sub(MAIN, 'Tile { side: 5, label: "t".to_string() }', 'Tile { side: 5, label: "t".to_string(), weight: 2 }')),
    step("struct: fields reordered (derived PartialOrd changes)", sub(MID, "    pub side: u32,\n    pub label: String,\n    pub weight: u8,", "    pub weight: u8,\n    pub label: String,\n    pub side: u32,")),
    step("struct: field type widened across crates", sub(SHAPES, "    pub x: i32,\n    pub y: i32,", "    pub x: i64,\n    pub y: i64,"),
         sub(SHAPES, "pub fn new(x: i32, y: i32)", "pub fn new(x: i64, y: i64)"), sub(SHAPES, "pub fn manhattan(&self) -> i32", "pub fn manhattan(&self) -> i64")),
    step("struct: private field added to a generic struct", sub(STACK, "    items: Vec<T>,\n}", "    items: Vec<T>,\n    pushes: usize,\n}"),
         sub(STACK, "Stack { items: Vec::new() }", "Stack { items: Vec::new(), pushes: 0 }"), sub(STACK, "        self.items.push(item);", "        self.items.push(item);\n        self.pushes += 1;"),
         sub(STACK, "        self.items.len()\n", "        self.items.len() + self.pushes\n")),
    step("enum: variant added, matches updated", sub(SHAPES, "    Dot,\n}", "    Dot,\n    Square(u32),\n}"), sub(SHAPES, "            Shape::Dot => 0,", "            Shape::Dot => 0,\n            Shape::Square(s) => s * s,"),
         sub(SHAPES, '            Shape::Dot => "dot".to_string(),', '            Shape::Dot => "dot".to_string(),\n            Shape::Square(_) => "square".to_string(),'),
         sub(MAIN, "Shape::Rect { w: 3, h: 4 }, Shape::Dot]", "Shape::Rect { w: 3, h: 4 }, Shape::Dot, Shape::Square(3)]")),
    step("enum: variants reordered (discriminants move)", sub(SHAPES, "    Circle(u32),\n    Rect { w: u32, h: u32 },", "    Rect { w: u32, h: u32 },\n    Circle(u32),")),
    step("derive added", sub(MID, "#[derive(Debug)]\npub enum Failure", "#[derive(Debug, Clone, PartialEq)]\npub enum Failure"),
         probe("failure_eq", "smid::checked(0).unwrap_err() == smid::Failure::Empty")),
    step("derive removed, hand-written impl instead", sub(MID, "#[derive(Debug, Clone, PartialEq, PartialOrd, Default, Hash)]\npub struct Tile", "#[derive(Clone, PartialEq, PartialOrd, Default, Hash)]\npub struct Tile"),
         append(MID, "\nimpl fmt::Debug for Tile {\n    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {\n        write!(f, \"Tile#{}\", self.side)\n    }\n}\n")),
    step("repr and type alias", sub(SHAPES, "#[derive(Debug, Clone, Copy, PartialEq)]\npub struct Point", "pub type Coord = i64;\n\n#[derive(Debug, Clone, Copy, PartialEq)]\n#[repr(C)]\npub struct Point"),
         probe("point_size", "std::mem::size_of::<Point>() + std::mem::size_of::<sbase::shapes::Coord>()")),
    # Traits.
    step("trait: method added with a default (vtable grows)", sub(SHAPES, "    fn name(&self) -> String;\n", "    fn name(&self) -> String;\n\n    fn tag(&self) -> u32 {\n        7\n    }\n"),
         sub(SHAPES, 'items.iter().map(|it| it.describe())', 'items.iter().map(|it| format!("{}{}", it.describe(), it.tag()))')),
    step("trait: default overridden in one impl", sub(SHAPES, "impl Describe for Point {", "impl Describe for Point {\n    fn tag(&self) -> u32 {\n        9\n    }\n")),
    step("trait: required method added, all impls updated", sub(SHAPES, "    const SIDES: u32;\n", "    const SIDES: u32;\n\n    fn perimeter(&self) -> u32;\n"),
         sub(SHAPES, "impl Area for Shape {\n    const SIDES: u32 = 0;\n", "impl Area for Shape {\n    const SIDES: u32 = 0;\n\n    fn perimeter(&self) -> u32 {\n        1\n    }\n"),
         sub(MID, "    const SIDES: u32 = 4;\n", "    const SIDES: u32 = 4;\n\n    fn perimeter(&self) -> u32 {\n        self.side * 4\n    }\n"), probe("perimeter", "tile.perimeter() + shapes[0].perimeter()")),
    step("trait: associated const changed", sub(MID, "    const SIDES: u32 = 4;", "    const SIDES: u32 = 40;")),
    step("trait: new impl for a foreign-crate type", append(MID, "\nimpl Describe for Failure {\n    fn name(&self) -> String {\n        self.to_string()\n    }\n}\n"),
         probe("failure_name", "smid::Failure::TooBig(3).describe()")),
    step("trait: impl removed", sub(MID, "\nimpl Describe for Failure {\n    fn name(&self) -> String {\n        self.to_string()\n    }\n}\n", ""),
         sub(MAIN, '    report::line("failure_name", smid::Failure::TooBig(3).describe());\n', "")),
    step("trait: new trait with blanket impl", append(BASE, "\npub trait Twice {\n    fn twice(&self) -> String;\n}\n\nimpl<T: std::fmt::Debug> Twice for T {\n    fn twice(&self) -> String {\n        format!(\"{self:?}{self:?}\")\n    }\n}\n"),
         sub(MAIN, "use smid::Tile;", "use sbase::Twice;\nuse smid::Tile;"), probe("twice", "5u8.twice() + &tile.twice()")),
    step("trait: generic bound tightened", sub(BASE, "pub fn largest<T: PartialOrd + Copy>", "pub fn largest<T: PartialOrd + Copy + std::fmt::Debug>")),
    step("trait: associated type changed on an Iterator impl", sub(MID, "    type Item = u32;\n\n    fn next(&mut self) -> Option<u32> {", "    type Item = u64;\n\n    fn next(&mut self) -> Option<u64> {"),
         sub(MID, "        Some(now)", "        Some(now as u64 * 3)")),
    step("trait: operator impl added", append(SHAPES, "\nimpl std::ops::Add for Point {\n    type Output = Point;\n\n    fn add(self, other: Point) -> Point {\n        Point::new(self.x + other.x, self.y + other.y)\n    }\n}\n"),
         probe("point_add", "(Point::new(1, 2) + Point::new(3, 4)).manhattan()")),
    # Generics.
    step("generic: new instantiation of an existing generic", probe("largest_char", "sbase::largest(&['a', 'z', 'q'])")),
    step("generic: std collection never used before", probe("btree", "std::collections::BTreeMap::from([(2u8, 'b'), (1u8, 'a')]).values().collect::<String>()")),
    step("generic: type parameter added to a function", sub(MID, "pub fn sides_of<T: Area>(_: &T) -> u32 {\n    T::SIDES", "pub fn sides_of<T: Area, U: Area>(_: &T, _: &U) -> u32 {\n    T::SIDES + U::SIDES"),
         sub(MAIN, "smid::sides_of(&tile) + smid::sides_of(&shapes[0])", "smid::sides_of(&tile, &shapes[0])")),
    step("generic: const generic introduced", append(BASE, "\npub fn sum_n<const N: usize>(items: [u32; N]) -> u32 {\n    items.iter().sum::<u32>() + N as u32\n}\n"), probe("sum_n", "sbase::sum_n([1, 2, 3]) + sbase::sum_n([5])")),
    # Macros.
    step("macro: body of macro_rules used in another crate", sub(BASE, "        $x * $x\n", "        $x * $x + 1\n")),
    step("macro: function-making macro changed", sub(BASE, "            x + $n\n", "            x + $n + 100\n")),
    step("macro: new invocation makes a new function", append(MID, "\nsbase::make_adder!(add_nine, 9);\n"), probe("add_nine", "smid::add_nine(1)")),
    step("macro: new macro defined and used in the same edit", append(MID, "\nmacro_rules! triple {\n    ($x:expr) => {\n        [$x, $x, $x]\n    };\n}\n\npub fn triples() -> [u8; 3] {\n    triple!(4)\n}\n"),
         probe("triples", 'format!("{:?}", smid::triples())')),
    step("macro: macro that declares a struct and impl", append(BASE, "\n#[macro_export]\nmacro_rules! newtype {\n    ($name:ident) => {\n        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]\n        pub struct $name(pub u32);\n\n        impl $name {\n            pub fn get(self) -> u32 {\n                self.0\n            }\n        }\n    };\n}\n\nnewtype!(Meters);\n"),
         probe("meters", 'format!("{:?} {}", sbase::Meters(4), sbase::Meters(4).get())')),
    step("macro: format string changed", sub(SHAPES, 'format!("<{}>", self.name())', 'format!("[{:>6}]", self.name())')),
    step("macro: matches!, vec!, assert!, write! in a new function", append(MID, "\npub fn mixed(n: u32) -> String {\n    use std::fmt::Write;\n    let v = vec![n; 3];\n    assert!(v.len() == 3, \"len\");\n    let mut s = String::new();\n    write!(s, \"{}{}\", matches!(n, 1..=5), v.iter().sum::<u32>()).unwrap();\n    s\n}\n"),
         probe("mixed", "smid::mixed(4)")),
    # Statics and consts.
    step("const value changed (used in a const fn and an array length)", sub(BASE, "pub const SCALE: u32 = 3;", "pub const SCALE: u32 = 5;"), sub(BASE, "pub const TABLE: [u8; 4] = [1, 2, 3, 4];", "pub const TABLE: [u8; 6] = [1, 2, 3, 4, 5, 6];")),
    step("static value changed", sub(BASE, 'pub static GREETING: &str = "hello";', 'pub static GREETING: &str = "good morning";')),
    step("static added and used", append(BASE, "\npub static PRIMES: [u16; 4] = [2, 3, 5, 7];\npub static TOTAL: AtomicU32 = AtomicU32::new(40);\n"),
         probe("statics", "sbase::PRIMES[3] as u32 + sbase::TOTAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed)")),
    step("static initial value of an atomic changed", sub(BASE, "pub static CALLS: AtomicU32 = AtomicU32::new(0);", "pub static CALLS: AtomicU32 = AtomicU32::new(10);")),
    step("thread_local added", append(MID, "\nthread_local! {\n    static DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(3) };\n}\n\npub fn deeper() -> u32 {\n    DEPTH.with(|d| {\n        d.set(d.get() + 1);\n        d.get()\n    })\n}\n"),
         probe("deeper", "smid::deeper() + smid::deeper()")),
    step("static with a lazily made value", append(MID, "\npub static WORDS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();\n\npub fn word(i: usize) -> &'static str {\n    &WORDS.get_or_init(|| vec![\"zero\".to_string(), \"one\".to_string()])[i]\n}\n"),
         probe("word", "smid::word(1)")),
    # Modules and files.
    step("new file and module", write("mid/src/extra.rs", "pub fn extra() -> u32 {\n    41\n}\n"), sub(MID, "pub mod text;", "pub mod extra;\npub mod text;"), probe("extra", "smid::extra::extra()")),
    step("edit to the file just added", sub("mid/src/extra.rs", "    41\n", "    42\n")),
    step("module renamed (file moved)", write("mid/src/more.rs", "pub fn extra() -> u32 {\n    42\n}\n"), delete("mid/src/extra.rs"), sub(MID, "pub mod extra;", "pub mod more;"), sub(MAIN, "smid::extra::extra()", "smid::more::extra()")),
    step("module becomes a directory with a submodule", write("mid/src/more/mod.rs", "pub mod inner;\n\npub fn extra() -> u32 {\n    inner::deep() + 1\n}\n"), write("mid/src/more/inner.rs", "pub fn deep() -> u32 {\n    50\n}\n"), delete("mid/src/more.rs")),
    step("re-export added and used", sub(MID, "pub mod more;", "pub mod more;\npub use more::inner::deep as deepest;"), probe("deepest", "smid::deepest()")),
    step("inline module with nested function and impl", append(MAIN, "\nmod inner {\n    pub struct Counter(pub u32);\n\n    impl Counter {\n        pub fn bump(&mut self) -> u32 {\n            fn by() -> u32 {\n                2\n            }\n            self.0 += by();\n            self.0\n        }\n    }\n}\n"),
         probe("counter", "inner::Counter(1).bump()")),
    step("include_str! of a new file", write("mid/src/note.txt", "first note\n"), append(MID, "\npub static NOTE: &str = include_str!(\"note.txt\");\n"), probe("note", "smid::NOTE.trim()")),
    step("the included file changes", write("mid/src/note.txt", "second note\n")),
    step("module removed", delete("mid/src/more/mod.rs"), delete("mid/src/more/inner.rs"), sub(MID, "pub mod more;\npub use more::inner::deep as deepest;\n", ""),
         sub(MAIN, '    report::line("extra", smid::more::extra());\n', ""), sub(MAIN, '    report::line("deepest", smid::deepest());\n', "")),
    # Attributes and cfg.
    step("#[inline(never)] and #[cold] added", sub(TEXT, "pub fn count_words(", "#[inline(never)]\n#[cold]\npub fn count_words(")),
    step("cfg(debug_assertions) branch", append(MID, "\npub fn mode() -> &'static str {\n    if cfg!(debug_assertions) {\n        \"debug\"\n    } else {\n        \"release\"\n    }\n}\n\n#[cfg(not(debug_assertions))]\npub fn only_release() {}\n"), probe("mode", "smid::mode()")),
    step("visibility narrowed and widened", sub(MID, "pub struct Evens {", "pub(crate) struct Evens2;\n\npub struct Evens {")),
    # Text that is not Rust: the daemon has to survive and then recover.
    step("BROKEN: syntax error", sub(TEXT, "pub fn words(s: &str) -> Vec<&str> {", "pub fn words(s: &str -> Vec<&str> {"), broken=True),
    step("fixed again, with a body change", sub(TEXT, "pub fn words(s: &str -> Vec<&str> {\n    s.split_whitespace().collect()", "pub fn words(s: &str) -> Vec<&str> {\n    s.split(' ').collect()")),
    step("BROKEN: type error in a body", sub(MID, "    Ok(n + 1)", "    Ok(n + \"1\")"), broken=True),
    step("BROKEN: still broken, another file edited too", sub(BASE, "pub fn count() -> u32 {", "pub fn count() -> u32 {\n    let _ = 1;"), broken=True),
    step("fixed again", sub(MID, "    Ok(n + \"1\")", "    Ok(n + 2)")),
    step("BROKEN: unresolved name in main", sub(MAIN, "sbase::scaled(7)", "sbase::scaledd(7)"), broken=True),
    step("fixed again by adding the missing function", append(BASE, "\npub fn scaledd(x: u32) -> u32 {\n    scaled(x) + 1\n}\n")),
    step("BROKEN: caller not updated after a signature change", sub(BASE, "pub fn scaledd(x: u32) -> u32 {\n    scaled(x) + 1", "pub fn scaledd(x: u32, y: u32) -> u32 {\n    scaled(x) + y"), broken=True),
    step("fixed again: caller updated", sub(MAIN, "sbase::scaledd(7)", "sbase::scaledd(7, 2)")),
    step("BROKEN: file emptied", write(TEXT, ""), broken=True),
    # Many at once, and going back.
    step("file restored and three crates edited in one build", write(TEXT, "pub fn shout(s: &str, times: usize) -> String {\n    format!(\"{}{}\", s.to_uppercase(), \"*\".repeat(times))\n}\n\npub fn words(s: &str) -> Vec<&str> {\n    s.split_whitespace().collect()\n}\n\npub fn count_words(s: &str) -> usize {\n    words(s).len()\n}\n\npub async fn count_later(s: &str) -> usize {\n    count_words(s) + 1\n}\n\npub fn initials(s: String) -> String {\n    words(&s).iter().filter_map(|w| w.chars().next()).collect()\n}\n"),
         sub(BASE, "    let _ = 1;\n", ""), sub(MAIN, "local_sum(&[1, 2, 3, 4])", "local_sum(&[1u64, 2, 3, 4, 5])")),
    # What only a check of the edited files finds: nothing reaches these.
    step("thread_local of the first source given another value", sub(MID, "std::cell::Cell::new(4)", "std::cell::Cell::new(9)")),
    step("BROKEN: struct literal without one of its fields", append(BASE, "\nstruct Pair {\n    a: u8,\n    b: u8,\n}\n\nfn pair() -> Pair {\n    Pair { a: 1 }\n}\n"), broken=True),
    step("mended: the field given", sub(BASE, "    Pair { a: 1 }\n", "    Pair { a: 1, b: 2 }\n")),
    step("BROKEN: match without one of its arms", append(MID, "\nfn unwrapped(x: Option<u8>) -> u8 {\n    match x {\n        Some(n) => n,\n    }\n}\n"), broken=True),
    step("mended: the arm given", sub(MID, "        Some(n) => n,\n    }\n}\n", "        Some(n) => n,\n        None => 0,\n    }\n}\n")),
    step("BROKEN: impl without one of its trait's methods", append(MAIN, "\ntrait Greets {\n    fn greet(&self) -> u8;\n}\n\nimpl Greets for u8 {}\n"), broken=True),
    step("mended: the method given", sub(MAIN, "impl Greets for u8 {}\n", "impl Greets for u8 {\n    fn greet(&self) -> u8 {\n        *self\n    }\n}\n")),
    step("BROKEN: method that no type has", append(TEXT, "\nfn lost(s: &str) -> usize {\n    s.lenn()\n}\n"), broken=True),
    step("mended: the method named right", sub(TEXT, "s.lenn()", "s.len()")),
    step("BROKEN: import of a name that is not there", sub(MAIN, "\ntrait Greets {", "\nuse smid::text::nothing_here;\n\ntrait Greets {"), broken=True),
    step("mended: the import removed", sub(MAIN, "\nuse smid::text::nothing_here;\n", "")),
    step("whole file rewritten in another order", "reorder"),
    step("large generated function added", "bulk"),
    step("a file edited again while a build runs", "meanwhile"),
    step("everything back to the first source", "reset"),
    step("and forward again to the last source", "replay"),
]


def main():
    only = sys.argv[sys.argv.index("--only") + 1] if "--only" in sys.argv else None
    work = scratch("blaze-stress-")
    ws = copy(os.path.join(HERE, "stress"), os.path.join(work, "ws"))
    project = Project(ws, work)

    def files():
        found = {}
        for root, _, names in os.walk(ws):
            if "/target" in root:
                continue
            for n in names:
                if n.endswith((".rs", ".txt")):
                    p = os.path.relpath(os.path.join(root, n), ws)
                    found[p] = open(os.path.join(ws, p)).read()
        return found

    def put(state):
        for p in files():
            if p not in state:
                os.remove(os.path.join(ws, p))
        for p, text in state.items():
            os.makedirs(os.path.dirname(os.path.join(ws, p)), exist_ok=True)
            if not os.path.exists(os.path.join(ws, p)) or open(os.path.join(ws, p)).read() != text:
                open(os.path.join(ws, p), "w").write(text)

    first_state, rows, restarts = files(), [], 0
    for index, (name, ops, broken) in enumerate(STEPS):
        if only and not re.search(only, name):
            continue
        if ops and isinstance(ops[0], str):
            ops = ops[0]
        if ops == "meanwhile":
            # The file is edited once, a build of it is asked for, and the file is edited again
            # while that build runs: the build after it has to be of the second edit.
            path = os.path.join(ws, BASE)
            text = open(path).read()
            assert "wrapping_mul(3)" in text, name
            text = text.replace("wrapping_mul(3)", "wrapping_mul(7)", 1)
            open(path, "w").write(text)
            other = project.asking("sapp")
            time.sleep(0.05)
            open(path, "w").write(text.replace("wrapping_mul(7)", "wrapping_mul(9)", 1))
            project.answer(other, "sapp")
        elif ops == "reset":
            last_state = files()
            put(first_state)
        elif ops == "replay":
            put(last_state)
        elif ops == "reorder":
            text = open(os.path.join(ws, TEXT)).read()
            parts = [p for p in re.split(r"\n(?=pub |#\[)", text)]
            open(os.path.join(ws, TEXT), "w").write("\n".join(reversed([p.strip("\n") + "\n" for p in parts])))
        elif ops == "bulk":
            body = "".join(f"    acc = acc.wrapping_mul({i * 2 + 3}).wrapping_add(x >> {i % 13});\n" for i in range(400))
            open(os.path.join(ws, BASE), "a").write(f"\npub fn bulk(x: u64) -> u64 {{\n    let mut acc = x;\n{body}    acc\n}}\n")
            text = open(os.path.join(ws, MAIN)).read()
            open(os.path.join(ws, MAIN), "w").write(text.replace(PROBE, '    report::line("bulk", sbase::bulk(12345));\n' + PROBE))
        else:
            for op in ops:
                path = os.path.join(ws, op[1])
                if op[0] == "sub":
                    text = open(path).read()
                    assert op[2] in text, (name, op[2])
                    open(path, "w").write(text.replace(op[2], op[3], 1))
                elif op[0] == "write":
                    os.makedirs(os.path.dirname(path), exist_ok=True)
                    open(path, "w").write(op[2])
                elif op[0] == "append":
                    open(path, "a").write(op[2])
                elif op[0] == "delete":
                    os.remove(path)
        asked = time.time()
        built = project.build("sapp")
        took = (time.time() - asked) * 1000
        verdict, said = project.held(built, "sapp", broken=broken)
        said = said.replace("|", "/").replace("\n", " ")[:150]
        rows.append((index, name, took, verdict, said))
        print(f"{index:3} {verdict:16} {took:7.0f} ms  {said}  | {name}", flush=True)
        if verdict in WRONG:
            # Start again from the source as it is, so that one failure is not counted many times.
            project.stop()
            restarts += 1
    project.stop()
    good = sum(1 for row in rows if row[3] not in WRONG)
    with open(os.path.join(HERE, "STRESS.md"), "w") as md:
        md.write(f"# Stress edits\n\n{good} of {len(rows)} steps right, {restarts} restarts. Daemon logs: {project.home}\n\n"
                 "| # | Edit | ms | Result | Said |\n|---|---|---:|---|---|\n")
        for index, name, took, verdict, said in rows:
            md.write(f"| {index} | {name} | {took:.0f} | {verdict} | {said} |\n")
    print(f"{good} of {len(rows)} right, {restarts} restarts; daemon logs {project.home}; source {ws}")
    if good == len(rows):
        shutil.rmtree(work, ignore_errors=True)
    sys.exit(0 if good == len(rows) else 1)


if __name__ == "__main__":
    main()
