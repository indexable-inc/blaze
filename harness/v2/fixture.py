"""fixture.py: the invalidation oracle's workspace, as a vector of independent slots.

The workspace has three crates: `oracle_base` (a library with a build script, a file read by
`include_str!`, features and cfgs), `oracle_mid` (a library over it) and `oracle_app` (a
program over both, with integration tests). Its source is a pure function of a `State`: for
each `Slot`, the index of one of its variants. A slot is one thing an edit of section 6 of
docs/v2/plan/lanes/items.md changes (a body, a signature, an impl, a name, where an item lives, a file's
formatting or comments, a cfg, a feature, a closure's captures, a file `include_str!` reads, the
whitespace in a literal, an environment variable), and every variant of every slot builds
beside every variant of every other, so any state is a program: an edit is one slot moved to
another variant, and a random sequence of them is a random sequence of edits. A variant marked
`rejects` is a program Rust refuses (the file `include_str!` reads deleted, a non-`Send` type
leaking out of an `impl Trait`); both builds then have to refuse it the same way.

The program prints one line for each thing an edit can change: values (bodies, constants,
`const fn`s, macros, statics, drop order), resolution (a signature, method resolution through an
impl added or removed, integer inference through an impl, a glob import shadowed, a vtable),
layout (field order under a derive, `repr`, a private field's type), names a program can read
(`type_name`, `module_path!`), positions it can read (`line!`, `column!`, `#[track_caller]`,
a panic's location), cfgs and features, a build script's environment and `option_env!`. Its
tests pass or fail by the state, so a stale test result shows too.

`budget_zero` marks the slots whose edits docs/v2/plan/goals/targets.md says compile nothing (formatting, comments,
a rename or a move no program reads, the `PATH` a build is run with, #805).
"""

import random
import re
from collections.abc import Callable, Mapping
from dataclasses import dataclass, field
from pathlib import Path

type State = dict[str, int]


@dataclass(frozen=True)
class Variant:
    """What the slot's placeholders become (`@NAME@` in the templates), or what a file or an
    environment variable is (`file`, `env`); `rejects` when Rust refuses the program."""

    values: Mapping[str, str] = field(default_factory=dict[str, str])
    rejects: bool = False
    label: str = ""


@dataclass(frozen=True)
class Slot:
    name: str
    kind: str
    variants: tuple[Variant, ...]
    budget_zero: bool = False
    # What the edit is to section 6.3's table, for the report.
    about: str = ""


def v(label: str, rejects: bool = False, **values: str) -> Variant:
    return Variant(values, rejects, label)


# The files every formatting and comment slot is over: each of them is laid out by `layout`.
SOURCES = (
    "base/src/lib.rs",
    "base/src/a.rs",
    "base/src/b.rs",
    "base/src/probe.rs",
    "base/src/shapes.rs",
    "mid/src/lib.rs",
    "mid/src/left.rs",
    "mid/src/right.rs",
    "app/src/main.rs",
    "app/tests/it.rs",
)
# The line between two items of a template.
ITEM = "\n@@\n"
FN_HEAD = re.compile(r"^(\s*)((?:pub(?:\([\w:]+\))? )?(?:const )?fn \w+.*\)(?: -> .+)?) \{$")


def _sig(label: str, params: str, ret: str, body: str, extra: str) -> Variant:
    return v(label, SIG_PARAMS=params, SIG_RET=ret, SIG_BODY=body, EXTRA_ARG=extra)


SLOTS: tuple[Slot, ...] = (
    # Bodies, and the values computed at compile time from them.
    Slot("scale", "body", (v("x3", K="3"), v("x5", K="5"), v("x7", K="7")),
         about="body of a function inlined across crates"),
    Slot("limit", "const", (v("3", LIMIT="3"), v("4", LIMIT="4"), v("12", LIMIT="12")),
         about="a const's value, read by an array length in another crate"),
    Slot("width", "const-fn", (v("4", WIDTH_BODY="4"), v("6", WIDTH_BODY="6"), v("2+3", WIDTH_BODY="2 + 3")),
         about="a const fn's body, evaluated by a const of another crate"),
    Slot("tag", "macro", (v("angle", TAG_FMT='"<{}>"'), v("square", TAG_FMT='"[{}]"'), v("round", TAG_FMT='"({})"')),
         about="the body of an exported macro_rules!"),
    Slot("drop", "body", (v("short", DROP_BODY='println!("drop {}", self.0);'),
                          v("long", DROP_BODY='println!("dropped {} ({})", self.0, self.0.len());')),
         about="a Drop impl's body, run by drop glue in another crate"),
    Slot("label", "trait-default", (v("area", LABEL_BODY='format!("area {}", self.area())'),
                                    v("units", LABEL_BODY='format!("{} units", self.area())')),
         about="a trait's default method body, called through a vtable"),
    Slot("banner", "static", (v("plain", BANNER='"oracle"'), v("versioned", BANNER='"oracle v2"')),
         about="a static's initializer"),
    Slot("fold", "generic", (v("mul", FOLD="xs.iter().fold(0u64, |acc, &x| acc.wrapping_mul(31).wrapping_add(x.into()))"),
                             v("xor", FOLD="xs.iter().fold(17u64, |acc, &x| acc ^ Into::<u64>::into(x))"),
                             v("sum", FOLD="xs.iter().map(|&x| Into::<u64>::into(x)).sum::<u64>()")),
         about="a generic function's body, instantiated in another crate at two types"),
    # Signatures: every caller in the other crates follows.
    Slot("combine", "signature", (
        _sig("u32,u32->u64", "a: u32, b: u32", "u64", "(a as u64) << 8 | b as u64", ""),
        _sig("u64,u32->u64", "a: u64, b: u32", "u64", "a << 8 | b as u64", ""),
        _sig("u32,u32,u8->u64", "a: u32, b: u32, c: u8", "u64", "((a as u64) << 8 | b as u64) + c as u64", ", 9"),
        _sig("u32,u32->u128", "a: u32, b: u32", "u128", "((a as u128) << 64) | b as u128", ""),
    ), about="a function's signature, with its callers in two crates"),
    # Impls added and removed, and what resolution and inference make of them.
    Slot("specific", "impl", (v("absent", SPECIFIC_IMPL=""),
                              v("present", SPECIFIC_IMPL="impl Specific for Marker {\n    fn which(&self) -> &'static str {\n        \"specific\"\n    }\n}")),
         about="an impl added or removed that changes method resolution in another crate"),
    Slot("inference", "impl", (v("u64 only", INFER_IMPL=""),
                               v("and i32", INFER_IMPL="impl Width for i32 {\n    fn width_of(self) -> usize {\n        core::mem::size_of::<Self>()\n    }\n}")),
         about="an impl added that turns an integer's inference to its fallback"),
    Slot("shadow", "glob", (v("glob", LOCAL_PICK=""), v("local", LOCAL_PICK="fn pick() -> u32 {\n    2\n}")),
         about="an item that shadows a glob import appears or goes"),
    Slot("vtable", "vtable", (v("two methods", VTABLE_EXTRA=""),
                              v("three methods", VTABLE_EXTRA="fn extra(&self) -> u64 {\n    99\n}")),
         about="a provided method added to a trait used as dyn in another crate"),
    Slot("leak", "auto-trait", (v("arc", HELD="std::sync::Arc::new(5u32)"),
                                v("rc", rejects=True, HELD="std::rc::Rc::new(5u32)")),
         about="an impl Trait's hidden type stops being Send (auto traits leak)"),
    # Name resolution per module and the impl buckets (docs/v2/plan/design/frontend.md items 1-3):
    # `blaze-v2-proof`'s `nameres` cases hold each edit of these, and of `shadow` and `specific`, to
    # what the facts say it changes.
    Slot("nameres-item", "item", (v("none", A_EXTRA=""), v("fn", A_EXTRA="pub fn gamma() -> u32 {\n    3\n}"),
                                  v("use", A_EXTRA="pub use crate::b::beta as gamma;")),
         about="an item or a use added to a module another module imports one name of"),
    Slot("nameres-glob", "glob", (v("absent", GLOB_SPARE=""), v("present", GLOB_SPARE="pub fn spare() -> u32 {\n    5\n}")),
         about="a name nothing reads added to a module another one globs"),
    Slot("nameres-negative", "glob", (v("prelude drop", GLOB_DROP=""),
                                      v("glob drop", GLOB_DROP="pub fn drop(s: String) -> usize {\n    s.len()\n}")),
         about="a name a glob makes visible over the prelude's: the negative fact flips"),
    Slot("nameres-bucket", "impl", (v("u8, u16", BUCKET_IMPL=""),
                                    v("and u32", BUCKET_IMPL="impl Weight for u32 {\n    fn weight(self) -> u32 {\n        self * 3\n    }\n}")),
         about="an impl added for a self type no goal asks about"),
    Slot("nameres-inherent", "impl", (v("trait only", INHERENT_IMPL=""),
                                      v("inherent", INHERENT_IMPL="impl Probe {\n    pub fn name(&self) -> &'static str {\n        \"inherent\"\n    }\n}")),
         about="an inherent method added that a call now resolves to over a trait's"),
    # Layout.
    Slot("pair", "layout", (v("a,b", PAIR_FIELDS="pub a: u8,\npub b: u8,"), v("b,a", PAIR_FIELDS="pub b: u8,\npub a: u8,")),
         about="field order of a struct whose PartialOrd and Debug are derived"),
    Slot("repr", "layout", (v("C", REPR="#[repr(C)]\n"), v("Rust", REPR="")),
         about="repr(C) on or off"),
    Slot("hits", "layout", (v("u32", HITS_TY="u32"), v("u64", HITS_TY="u64"), v("u16", HITS_TY="u16")),
         about="a private field's type, under a public type used by value"),
    # Names and places.
    Slot("helper", "rename", (v("mix", HELPER="mix"), v("blend", HELPER="blend"), v("stir", HELPER="stir")),
         budget_zero=True, about="a private function renamed with its call; nothing reads the name"),
    Slot("gadget", "rename", (v("Gadget", GADGET="Gadget"), v("Gizmo", GADGET="Gizmo"), v("Contraption", GADGET="Contraption")),
         about="a type renamed whose name the program reads with type_name"),
    Slot("module", "rename", (v("inner", MODNAME="inner"), v("nested", MODNAME="nested"), v("deep", MODNAME="deep")),
         about="a module renamed whose path the program reads with module_path!"),
    Slot("triple", "move", (v("in a", TRIPLE_A="pub fn triple(x: u64) -> u64 {\n    x * 3\n}", TRIPLE_B="", TRIPLE_MOD="a"),
                            v("in b", TRIPLE_A="", TRIPLE_B="pub fn triple(x: u64) -> u64 {\n    x * 3\n}", TRIPLE_MOD="b")),
         budget_zero=True, about="a function moved to another file and module; nothing reads where"),
    Slot("where", "move", (v("left", WHERE_LEFT="pub fn where_am_i() -> &'static str {\n    module_path!()\n}", WHERE_RIGHT="", WHERE_MOD="left"),
                           v("right", WHERE_LEFT="", WHERE_RIGHT="pub fn where_am_i() -> &'static str {\n    module_path!()\n}", WHERE_MOD="right")),
         about="a function moved whose module_path! the program prints"),
    # cfg and features.
    Slot("cfg", "cfg", (v("on", CFG_PRED="all()"), v("off", CFG_PRED="any()")),
         about="a cfg predicate that picks one of two definitions"),
    Slot("default-features", "features", (v("fancy", DEFAULT_FEATURES='["fancy"]'), v("none", DEFAULT_FEATURES="[]")),
         about="a library's default features in its Cargo.toml"),
    Slot("app-features", "features", (v("default", APP_FEATURES=""), v("extra", APP_FEATURES=', features = ["extra"]')),
         about="a feature a dependent turns on"),
    # #906: a closure's captures change, its sibling closure calls it.
    Slot("capture", "closure", (v("self.b", CAPTURE="self.b.height(dest)"), v("self", CAPTURE="self.h(dest)"),
                                v("self, doubled", CAPTURE="self.h(dest) * 2")),
         about="what a closure captures (#906), called from a sibling closure"),
    # #1325: the file include_str! reads, deleted and back.
    Slot("motto", "include", (v("quick", MOTTO="the quick brown fox\n"), v("slow", MOTTO="the slow brown fox\n"),
                              v("deleted", rejects=True, MOTTO="")),
         about="the file an include_str! reads, changed or deleted (#1325)"),
    # #1326: whitespace inside a literal is not formatting.
    Slot("greeting", "literal", (v("one space", GREETING='"hello world"'), v("two spaces", GREETING='"hello  world"'),
                                 v("tab", GREETING='"hello\\tworld"'), v("leading", GREETING='" hello world"'),
                                 v("raw", GREETING='r"hello  world"')),
         about="whitespace inside a string literal (#1326)"),
    # The environment of a build: what a build script declares it reads, what option_env! reads,
    # and PATH, which neither reads (#805).
    Slot("script-env", "env", (v("unset", ORACLE_SCRIPT_VALUE=""), v("one", ORACLE_SCRIPT_VALUE="one"),
                               v("two", ORACLE_SCRIPT_VALUE="two")),
         about="an environment variable a build script reads (rerun-if-env-changed)"),
    Slot("source-env", "env", (v("unset", ORACLE_SOURCE_ENV=""), v("alpha", ORACLE_SOURCE_ENV="alpha"),
                               v("beta", ORACLE_SOURCE_ENV="beta")),
         about="an environment variable option_env! reads"),
    Slot("path", "env", (v("plain", PATH_EXTRA=""), v("extra directory", PATH_EXTRA="1")),
         budget_zero=True, about="PATH with another directory in front, read by nothing (#805)"),
    *(Slot(f"format:{name}", "format", (v("4, one blank"), v("2, two blanks"), v("4, none, braces below")),
           budget_zero=True, about="the file laid out otherwise: indentation, blank lines, braces")
      for name in SOURCES),
    *(Slot(f"comments:{name}", "comments", (v("none"), v("a line before each item"), v("one after each line")),
           budget_zero=True, about="comments added or taken away; the first kind moves every line")
      for name in SOURCES),
)
BY_NAME = {slot.name: slot for slot in SLOTS}
KINDS = tuple(dict.fromkeys(slot.kind for slot in SLOTS))
ENV_SLOTS = ("script-env", "source-env", "path")


WORKSPACE = """[workspace]
members = ["base", "mid", "app"]
resolver = "2"
"""

BASE_TOML = """[package]
name = "oracle_base"
version = "0.1.0"
edition = "2021"
build = "build.rs"

[features]
default = @DEFAULT_FEATURES@
fancy = []
extra = []
"""

BUILD_RS = """use std::{env, fs, path::Path};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=ORACLE_SCRIPT_VALUE");
    let value = env::var("ORACLE_SCRIPT_VALUE").unwrap_or_else(|_| String::from("unset"));
    let out = Path::new(&env::var("OUT_DIR").unwrap()).join("generated.rs");
    fs::write(out, format!("pub const SCRIPT_VALUE: &str = {value:?};\\n")).unwrap();
}
"""

BASE_LIB = """//! The oracle's base crate (harness/v2/fixture.py).
@@
pub mod a;
pub mod b;
pub mod probe;
pub mod shapes;
@@
include!(concat!(env!("OUT_DIR"), "/generated.rs"));
@@
pub use @TRIPLE_MOD@::triple;
@@
pub const LIMIT: usize = @LIMIT@;
@@
pub const MOTTO: &str = include_str!("../data/motto.txt");
@@
pub const GREETING: &str = @GREETING@;
@@
pub const fn width() -> usize {
    @WIDTH_BODY@
}
@@
fn @HELPER@(x: u64) -> u64 {
    x.rotate_left(3) ^ 0x5a
}
@@
#[inline]
pub fn scale(x: u64) -> u64 {
    @HELPER@(x).wrapping_mul(@K@) % 1_000_003
}
@@
pub fn combine(@SIG_PARAMS@) -> @SIG_RET@ {
    @SIG_BODY@
}
@@
#[macro_export]
macro_rules! tag {
    ($e:expr) => {
        format!(@TAG_FMT@, $e)
    };
}
@@
pub trait Width {
    fn width_of(self) -> usize;
}
@@
impl Width for u64 {
    fn width_of(self) -> usize {
        core::mem::size_of::<Self>()
    }
}
@@
@INFER_IMPL@
@@
#[cfg(@CFG_PRED@)]
pub fn flavor() -> &'static str {
    "cfg-on"
}
@@
#[cfg(not(@CFG_PRED@))]
pub fn flavor() -> &'static str {
    "cfg-off"
}
@@
#[cfg(feature = "fancy")]
pub const FANCY: &str = "fancy";
@@
#[cfg(not(feature = "fancy"))]
pub const FANCY: &str = "plain";
@@
pub const EXTRA: bool = cfg!(feature = "extra");
@@
pub fn located() -> u32 {
    line!()
}
@@
#[track_caller]
pub fn checked(x: u32) -> u32 {
    if x > 100 {
        panic!("checked: {} is too big", x);
    }
    x
}
@@
pub fn origin() -> &'static core::panic::Location<'static> {
    core::panic::Location::caller()
}
@@
pub struct Noisy(pub &'static str);
@@
impl Drop for Noisy {
    fn drop(&mut self) {
        @DROP_BODY@
    }
}
@@
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scale_of_one() {
        assert_eq!(scale(1) % 7, 0);
    }

    #[test]
    fn greeting_has_one_space() {
        assert_eq!(GREETING.matches(' ').count(), 1);
    }

    #[test]
    fn limit_is_small() {
        assert!(LIMIT < 10);
    }
}
"""

BASE_A = """pub fn alpha() -> u32 {
    1
}
@@
@TRIPLE_A@
@@
@A_EXTRA@
"""

BASE_B = """pub fn beta() -> u32 {
    2
}
@@
@TRIPLE_B@
"""

BASE_PROBE = """pub struct Marker;
@@
pub trait Specific {
    fn which(&self) -> &'static str;
}
@@
pub trait Fallback {
    fn which(&self) -> &'static str;
}
@@
impl<T> Fallback for &T {
    fn which(&self) -> &'static str {
        "fallback"
    }
}
@@
@SPECIFIC_IMPL@
@@
pub trait Weight {
    fn weight(self) -> u32;
}
@@
impl Weight for u8 {
    fn weight(self) -> u32 {
        u32::from(self)
    }
}
@@
impl Weight for u16 {
    fn weight(self) -> u32 {
        u32::from(self) * 2
    }
}
@@
@BUCKET_IMPL@
@@
pub struct Probe;
@@
pub trait Named {
    fn name(&self) -> &'static str;
}
@@
impl Named for Probe {
    fn name(&self) -> &'static str {
        "trait"
    }
}
@@
@INHERENT_IMPL@
"""

BASE_SHAPES = """#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct Pair {
    @PAIR_FIELDS@
}
@@
@REPR@pub struct Packed {
    pub a: u8,
    pub b: u32,
    pub c: u8,
}
@@
pub struct Counter {
    hits: @HITS_TY@,
}
@@
impl Counter {
    pub fn new() -> Self {
        Counter { hits: 0 }
    }

    pub fn bump(&mut self) -> u64 {
        self.hits += 1;
        self.hits as u64
    }
}
@@
pub struct @GADGET@ {
    pub id: u32,
}
"""

MID_TOML = """[package]
name = "oracle_mid"
version = "0.1.0"
edition = "2021"

[dependencies]
oracle_base = { path = "../base" }
"""

MID_LIB = """//! The oracle's middle crate (harness/v2/fixture.py).
@@
pub mod left;
pub mod right;
@@
pub mod @MODNAME@ {
    pub fn path() -> &'static str {
        module_path!()
    }
}
@@
pub use @WHERE_MOD@::where_am_i;
@@
mod helpers {
    pub fn pick() -> u32 {
        1
    }

    @GLOB_SPARE@

    @GLOB_DROP@
}
@@
mod chooser {
    use super::helpers::*;

    @LOCAL_PICK@

    pub fn chosen() -> u32 {
        pick()
    }

    pub fn released() -> String {
        format!("{:?}", drop(String::from("held")))
    }
}
@@
pub use chooser::chosen;
@@
pub use chooser::released;
@@
pub fn fold<T: Into<u64> + Copy>(xs: &[T]) -> u64 {
    @FOLD@
}
@@
pub fn stride() -> usize {
    core::mem::size_of::<oracle_base::shapes::Packed>() * oracle_base::LIMIT
}
@@
pub fn buffer_len() -> usize {
    [0u8; oracle_base::LIMIT].len()
}
@@
pub fn combined() -> u64 {
    oracle_base::combine(5, 6@EXTRA_ARG@) as u64
}
@@
pub fn gadget() -> oracle_base::shapes::@GADGET@ {
    oracle_base::shapes::@GADGET@ { id: 7 }
}
@@
pub trait Shape {
    @VTABLE_EXTRA@

    fn area(&self) -> u64;

    fn label(&self) -> String {
        @LABEL_BODY@
    }
}
@@
pub struct Square(pub u64);
@@
impl Shape for Square {
    fn area(&self) -> u64 {
        self.0 * self.0
    }
}
@@
pub fn shapes() -> Vec<Box<dyn Shape>> {
    vec![Box::new(Square(3)), Box::new(Square(4))]
}
@@
pub fn make() -> impl Fn() -> u32 {
    let held = @HELD@;
    move || *held + 1
}
@@
pub struct Building {
    heights: Vec<u32>,
}
@@
impl Building {
    pub fn height(&self, i: usize) -> u32 {
        self.heights[i % self.heights.len()]
    }
}
@@
pub struct Sim {
    b: Building,
    table: Vec<u32>,
}
@@
impl Sim {
    pub fn new() -> Self {
        let heights: Vec<u32> = (0..16).map(|i| (i * 7 + 3) % 11).collect();
        Sim { table: heights.clone(), b: Building { heights } }
    }

    pub fn h(&self, i: usize) -> u32 {
        self.table[i % self.table.len()]
    }

    pub fn compact_front(&self, dests: &[usize]) -> Vec<(u32, usize)> {
        let gap_of = |dest: usize| {
            let hd = @CAPTURE@;
            hd + 1
        };
        dests.iter().enumerate().map(|(at, &q)| (gap_of(q), at)).collect()
    }
}
"""

MID_LEFT = """pub fn side() -> &'static str {
    "left"
}
@@
@WHERE_LEFT@
"""

MID_RIGHT = """pub fn side() -> &'static str {
    "right"
}
@@
@WHERE_RIGHT@
"""

APP_TOML = """[package]
name = "oracle_app"
version = "0.1.0"
edition = "2021"

[dependencies]
oracle_base = { path = "../base"@APP_FEATURES@ }
oracle_mid = { path = "../mid" }
"""

APP_MAIN = """use oracle_base::probe::{Fallback, Named, Specific, Weight};
use oracle_mid::Shape;
use std::panic::{self, Location};
@@
const W: usize = oracle_base::width();
@@
static BANNER: &str = @BANNER@;
@@
fn assert_send<T: Send>(_: &T) {}
@@
fn here() -> (u32, u32) {
    (line!(), column!())
}
@@
#[track_caller]
fn caller() -> &'static Location<'static> {
    Location::caller()
}
@@
fn main() {
    panic::set_hook(Box::new(|info| {
        let at = info.location().map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()));
        println!("panic at {}", at.unwrap_or_default());
    }));
    println!("banner: {}", BANNER);
    println!("scale: {}", oracle_base::scale(41));
    println!("combine: {}", oracle_base::combine(3, 4@EXTRA_ARG@) as u64);
    println!("combined: {}", oracle_mid::combined());
    println!("limit: {} {}", oracle_base::LIMIT, oracle_mid::buffer_len());
    println!("width: {}", W);
    println!("motto: {:?}", oracle_base::MOTTO);
    println!("greeting: [{}] {}", oracle_base::GREETING, oracle_base::GREETING.len());
    println!("tag: {}", oracle_base::tag!(5));
    println!("triple: {}", oracle_base::triple(14));
    let n = 7;
    println!("inferred width: {}", oracle_base::Width::width_of(n));
    println!("which: {}", (&oracle_base::probe::Marker).which());
    println!("weight: {} {}", oracle_base::probe::Weight::weight(5u8), 7u16.weight());
    println!("named: {}", oracle_base::probe::Probe.name());
    println!("flavor: {} {} {}", oracle_base::flavor(), oracle_base::FANCY, oracle_base::EXTRA);
    println!("script: {}", oracle_base::SCRIPT_VALUE);
    println!("source env: {:?}", option_env!("ORACLE_SOURCE_ENV"));
    let low = oracle_base::shapes::Pair { a: 1, b: 2 };
    let high = oracle_base::shapes::Pair { a: 2, b: 1 };
    println!("pair: {} {:?}", low < high, low);
    let packed = std::mem::size_of::<oracle_base::shapes::Packed>();
    println!("packed: {} {}", packed, std::mem::offset_of!(oracle_base::shapes::Packed, b));
    let mut counter = oracle_base::shapes::Counter::new();
    counter.bump();
    println!("counter: {} {}", counter.bump(), std::mem::size_of::<oracle_base::shapes::Counter>());
    println!("stride: {}", oracle_mid::stride());
    let gadget = oracle_mid::gadget();
    println!("gadget: {} {}", std::any::type_name_of_val(&gadget), gadget.id);
    println!("module: {} {}", oracle_mid::@MODNAME@::path(), oracle_mid::where_am_i());
    println!("chosen: {}", oracle_mid::chosen());
    println!("released: {}", oracle_mid::released());
    println!("fold: {} {}", oracle_mid::fold(&[1u8, 2, 3]), oracle_mid::fold(&[70000u32, 9]));
    for shape in oracle_mid::shapes() {
        println!("shape: {} {}", shape.area(), shape.label());
    }
    let make = oracle_mid::make();
    assert_send(&make);
    println!("made: {}", make());
    let sim = oracle_mid::Sim::new();
    println!("compact: {:?}", sim.compact_front(&[1, 5, 9, 14, 2]));
    println!("here: {:?}", here());
    let at = caller();
    println!("caller: {}:{}:{}", at.file(), at.line(), at.column());
    println!("located: {}", oracle_base::located());
    let origin = oracle_base::origin();
    println!("origin: {}:{}", origin.file(), origin.line());
    let caught = panic::catch_unwind(|| oracle_base::checked(200));
    println!("caught: {}", caught.is_err());
    {
        let _first = oracle_base::Noisy("first");
        let _second = oracle_base::Noisy("second");
    }
}
"""

APP_TEST = """#[test]
fn chosen_is_one() {
    assert_eq!(oracle_mid::chosen(), 1);
}
@@
#[test]
fn compact_is_stable() {
    let sim = oracle_mid::Sim::new();
    let got: Vec<u32> = sim.compact_front(&[1, 5, 9]).into_iter().map(|(gap, _)| gap).collect();
    assert_eq!(got, vec![sim.h(1) + 1, sim.h(5) + 1, sim.h(9) + 1]);
}
@@
#[test]
fn shapes_label() {
    let labels: Vec<String> = oracle_mid::shapes().iter().map(|s| oracle_mid::Shape::label(&**s)).collect();
    assert_eq!(labels[0], "area 9");
}
@@
#[test]
fn which_is_specific() {
    use oracle_base::probe::{Fallback, Specific};
    assert_eq!((&oracle_base::probe::Marker).which(), "specific");
}
"""

TEMPLATES: dict[str, str] = {
    "Cargo.toml": WORKSPACE,
    "base/Cargo.toml": BASE_TOML,
    "base/build.rs": BUILD_RS,
    "base/src/lib.rs": BASE_LIB,
    "base/src/a.rs": BASE_A,
    "base/src/b.rs": BASE_B,
    "base/src/probe.rs": BASE_PROBE,
    "base/src/shapes.rs": BASE_SHAPES,
    "mid/Cargo.toml": MID_TOML,
    "mid/src/lib.rs": MID_LIB,
    "mid/src/left.rs": MID_LEFT,
    "mid/src/right.rs": MID_RIGHT,
    "app/Cargo.toml": APP_TOML,
    "app/src/main.rs": APP_MAIN,
    "app/tests/it.rs": APP_TEST,
}
PLACEHOLDER = re.compile(r"@([A-Z_]+)@")


def default_state() -> State:
    return {slot.name: 0 for slot in SLOTS}


def values_of(state: State) -> dict[str, str]:
    """Every placeholder's text in `state`."""
    out: dict[str, str] = {}
    for slot in SLOTS:
        out.update(slot.variants[state[slot.name]].values)
    return out


def rejects(state: State) -> bool:
    return any(slot.variants[state[slot.name]].rejects for slot in SLOTS)


def _indent(text: str, by: str) -> str:
    """`text` (an item written with four spaces a level) with each level `by`, every line of
    it after the first indented to `by` more than the first, as a placeholder sits."""
    lines = text.split("\n")
    return "\n".join([lines[0], *(by + line if line else line for line in lines[1:])])


def fill(template: str, values: Mapping[str, str]) -> str:
    """The template with each placeholder replaced, indented as the placeholder is."""
    out: list[str] = []
    for line in template.split("\n"):
        found = PLACEHOLDER.search(line)
        if found and line.strip() == found.group(0):
            text = values[found.group(1)]
            prefix = line[: found.start()]
            out.append(prefix + _indent(text, prefix) if text else "")
            continue
        out.append(PLACEHOLDER.sub(lambda m: values[m.group(1)], line))
    return "\n".join(out)


def layout(items: list[str], style: int, comments: int) -> str:
    """The items of a file laid out: `style` 0 is four spaces and a blank line between items, 1
    is two spaces and two blank lines, 2 is four spaces, no blank lines and every function's
    brace on a line of its own; `comments` 0 is none, 1 a comment line before each item, 2 a
    comment at the end of every line that ends a statement or opens a block."""
    unit = "  " if style == 1 else "    "
    laid: list[str] = []
    for at, item in enumerate(i for i in items if i.strip()):
        lines: list[str] = []
        for line in item.split("\n"):
            depth = (len(line) - len(line.lstrip(" "))) // 4
            line = unit * depth + line.lstrip(" ")
            head = FN_HEAD.match(line) if style == 2 else None
            if head:
                lines += [head.group(1) + head.group(2), head.group(1) + "{"]
            else:
                lines.append(line)
        if comments == 2:
            lines = [line + " // noted" if line.rstrip().endswith(("{", ";")) else line for line in lines]
        if comments == 1:
            lines.insert(0, f"// item {at}")
        laid.append("\n".join(lines))
    gap = {0: "\n\n", 1: "\n\n\n", 2: "\n"}[style]
    return gap.join(laid).rstrip("\n") + "\n"


def render(state: State) -> dict[str, str | None]:
    """Every file of the workspace in `state`, by its path; None for a file that is absent."""
    values = values_of(state)
    files: dict[str, str | None] = {}
    for path, template in TEMPLATES.items():
        if path in SOURCES:
            items = [fill(item, values) for item in template.split(ITEM)]
            files[path] = layout(items, state[f"format:{path}"], state[f"comments:{path}"])
        else:
            files[path] = fill(template, values)
    motto = values["MOTTO"]
    files["base/data/motto.txt"] = motto or None
    return files


def environment(state: State) -> dict[str, str | None]:
    """The variables a build of `state` is run with (None: unset), and `PATH_EXTRA`: whether a
    directory goes in front of PATH."""
    values = values_of(state)
    return {name: values[name] or None for name in ("ORACLE_SCRIPT_VALUE", "ORACLE_SOURCE_ENV", "PATH_EXTRA")}


def write(directory: Path, state: State, before: dict[str, str | None] | None = None) -> dict[str, str | None]:
    """Writes the workspace of `state` under `directory`; only files that changed since
    `before` (what the last call returned) are written, so nothing else is touched."""
    files = render(state)
    for path, text in files.items():
        if before is not None and before.get(path) == text and (text is None) == (not (directory / path).exists()):
            continue
        target = directory / path
        if text is None:
            target.unlink(missing_ok=True)
            continue
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(text)
    return files


def random_state(seed: str) -> State:
    """A state of `seed` with no slot that makes the program refused."""
    r = random.Random(f"{seed}/state")
    state = {slot.name: r.randrange(len(slot.variants)) for slot in SLOTS}
    for slot in SLOTS:
        if slot.variants[state[slot.name]].rejects:
            state[slot.name] = 0
    return state


def write_workspace(directory: Path, seed: str = "base") -> None:
    """The workspace of the default state (`seed` "base") or of a random one, written whole."""
    write(directory, default_state() if seed == "base" else random_state(seed))


@dataclass(frozen=True)
class Edit:
    slot: str
    kind: str
    old: int
    new: int

    def label(self) -> str:
        slot = BY_NAME[self.slot]
        return f"{self.kind} {self.slot}: {slot.variants[self.old].label} -> {slot.variants[self.new].label}"


# How often each kind of edit is drawn. A kind of rejecting edits is drawn rarely; its next
# edit is as likely as any to make the program build again.
WEIGHTS: dict[str, float] = {"format": 2.0, "comments": 2.0, "body": 2.0, "auto-trait": 0.4, "include": 0.8}


def editor(seed: str, kinds: tuple[str, ...] = ()) -> Callable[[State], Edit]:
    """An endless sequence of edits from `seed`, each moving one slot of the given kinds (all
    when none) to another variant. Takes the state to edit, gives the edit and edits it."""
    r = random.Random(f"{seed}/edits")
    slots = [slot for slot in SLOTS if not kinds or slot.kind in kinds]
    if not slots:
        raise ValueError(f"no slot of the kinds {kinds}; the kinds are {', '.join(KINDS)}")
    weights = [WEIGHTS.get(slot.kind, 1.0) / sum(s.kind == slot.kind for s in SLOTS) for slot in slots]

    def edit(state: State) -> Edit:
        slot = r.choices(slots, weights)[0]
        old = state[slot.name]
        new = r.choice([i for i in range(len(slot.variants)) if i != old])
        state[slot.name] = new
        return Edit(slot.name, slot.kind, old, new)

    return edit


def by_label(slot: str, label: str) -> int:
    for at, variant in enumerate(BY_NAME[slot].variants):
        if variant.label == label:
            return at
    raise KeyError(f"slot {slot} has no variant {label!r}")


# Sequences the oracle always runs, each from the default state, each step `(slot, variant)`:
# the reproductions of the issues docs/v2/plan/lanes/proof.md gives the oracle, and one of each kind
# of name and position a program can read.
REGRESSIONS: dict[str, tuple[tuple[str, str], ...]] = {
    # #906: back and forth, so that the store has both versions, then the other capture.
    "906": (("capture", "self"), ("capture", "self.b"), ("capture", "self"), ("capture", "self, doubled"),
            ("capture", "self.b")),
    # #1325: the file deleted, back with other text, deleted again, back as it was.
    "1325": (("motto", "deleted"), ("motto", "slow"), ("motto", "deleted"), ("motto", "quick")),
    # #1326: whitespace inside the literal, each way, and back.
    "1326": (("greeting", "two spaces"), ("greeting", "tab"), ("greeting", "leading"), ("greeting", "raw"),
             ("greeting", "one space")),
    # #805: PATH changes and nothing else does; then what the build script reads does.
    "805": (("path", "extra directory"), ("path", "plain"), ("script-env", "one"), ("path", "extra directory"),
            ("script-env", "two"), ("script-env", "unset"), ("source-env", "alpha"), ("source-env", "unset")),
    "names": (("gadget", "Gizmo"), ("module", "nested"), ("where", "right"), ("helper", "blend"),
              ("triple", "in b"), ("gadget", "Gadget"), ("where", "left")),
    "positions": (("comments:app/src/main.rs", "a line before each item"), ("format:base/src/lib.rs", "2, two blanks"),
                  ("comments:base/src/lib.rs", "one after each line"), ("format:app/src/main.rs", "4, none, braces below"),
                  ("comments:app/src/main.rs", "none"), ("format:base/src/lib.rs", "4, one blank")),
    "resolution": (("specific", "present"), ("inference", "and i32"), ("shadow", "local"), ("vtable", "three methods"),
                   ("specific", "absent"), ("vtable", "two methods"), ("combine", "u32,u32,u8->u64"),
                   ("combine", "u32,u32->u128"), ("leak", "rc"), ("leak", "arc")),
    # docs/v2/plan/design/frontend.md items 1-3: each step is held to the `nameres` case of its slot
    # (crates/blaze-v2-proof/src/nameres.rs), each slot edited there and back.
    "nameres": (("nameres-item", "fn"), ("nameres-item", "use"), ("nameres-glob", "present"),
                ("nameres-negative", "glob drop"), ("shadow", "local"), ("nameres-bucket", "and u32"),
                ("specific", "present"), ("nameres-inherent", "inherent"), ("nameres-negative", "prelude drop"),
                ("nameres-item", "none"), ("nameres-bucket", "u8, u16"), ("nameres-inherent", "trait only"),
                ("specific", "absent"), ("shadow", "glob"), ("nameres-glob", "absent")),
    "features": (("default-features", "none"), ("app-features", "extra"), ("cfg", "off"), ("default-features", "fancy"),
                 ("cfg", "on"), ("app-features", "default")),
}


def regression(name: str) -> list[Edit]:
    state = default_state()
    edits: list[Edit] = []
    for slot, label in REGRESSIONS[name]:
        new = by_label(slot, label)
        edits.append(Edit(slot, BY_NAME[slot].kind, state[slot], new))
        state[slot] = new
    return edits


def strip_layout(text: str) -> str:
    """`text` without comments or whitespace outside string literals: what a formatting or
    comment edit must leave as it was."""
    out: list[str] = []
    at, n = 0, len(text)
    while at < n:
        c = text[at]
        if text.startswith("//", at):
            at = text.find("\n", at)
            at = n if at < 0 else at
        elif c == '"' or text.startswith('r"', at):
            raw = c == "r"
            end = at + (2 if raw else 1)
            while end < n and text[end] != '"':
                end += 2 if (text[end] == "\\" and not raw) else 1
            out.append(text[at : end + 1])
            at = end + 1
        elif c.isspace():
            at += 1
        else:
            out.append(c)
            at += 1
    return "".join(out)


def selftest() -> list[str]:
    """What is wrong with the slots and templates, without building anything."""
    wrong: list[str] = []
    base = render(default_state())
    names = [slot.name for slot in SLOTS]
    if len(names) != len(set(names)):
        wrong.append("two slots have one name")
    owners: dict[str, str] = {}
    for slot in SLOTS:
        keys = {key for variant in slot.variants for key in variant.values}
        if any(set(variant.values) != keys for variant in slot.variants):
            wrong.append(f"{slot.name}: its variants set different placeholders")
        for key in keys:
            if key in owners:
                wrong.append(f"{key} is set by both {owners[key]} and {slot.name}")
            owners[key] = slot.name
        if len({variant.label for variant in slot.variants}) != len(slot.variants):
            wrong.append(f"{slot.name}: two variants have one label")
    used = {m for template in TEMPLATES.values() for m in PLACEHOLDER.findall(template)}
    for key in sorted(used - set(owners)):
        wrong.append(f"@{key}@ is in a template and no slot sets it")
    for key in sorted(set(owners) - used - {"MOTTO", *environment(default_state())}):
        wrong.append(f"{key} is set by {owners[key]} and no template has it")
    for slot in SLOTS:
        for at in range(len(slot.variants)):
            state = default_state() | {slot.name: at}
            files = render(state)
            for path, text in files.items():
                if text is None:
                    continue
                if "@" in text and path.endswith((".rs", ".toml")) and PLACEHOLDER.search(text):
                    wrong.append(f"{slot.name}={at}: {path} has a placeholder left")
                if path.endswith(".rs") and text.count("{") != text.count("}"):
                    wrong.append(f"{slot.name}={at}: {path} has unbalanced braces")
                if slot.kind in ("format", "comments") and strip_layout(text) != strip_layout(base[path] or ""):
                    wrong.append(f"{slot.name}={at}: {path} differs from the default in more than its layout")
            if slot.kind not in ("format", "comments", "env") and files == base and at != 0:
                wrong.append(f"{slot.name}={at}: renders the same files as the default state")
    first = editor("selftest")
    second = editor("selftest")
    a, b = default_state(), default_state()
    for _ in range(200):
        x, y = first(a), second(b)
        if x != y or a != b:
            wrong.append("two editors of one seed made different edits")
            break
        if x.old == x.new:
            wrong.append(f"an edit left {x.slot} as it was")
    for name in REGRESSIONS:
        try:
            regression(name)
        except KeyError as e:
            wrong.append(f"regression {name}: {e}")
    if rejects(default_state()) or rejects(random_state("selftest")):
        wrong.append("a starting state rejects")
    return wrong
