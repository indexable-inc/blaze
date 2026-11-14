//! `magic_number` and `magic_string` over a `std` with the shapes they read (a `Duration`, an
//! `env`, a `HashMap`, `u32::from_le_bytes`): every exemption, where it does not fire; every
//! finding, with its message; the fixes made and linted again clean; and a property over
//! generated literals.

use crate::tests::{CLIPPY_OFF, fixed_in, lint, offered_in};

const MINICORE: &str = "//- minicore: sized, copy, clone, option, result, fn, iterator, iterators, slice, index, range, deref, eq, ord, builtin_impls, try, from, str, fmt, panic, size_of\n";

const STD: &str = r#"
//- /std/lib.rs crate:std
pub mod prelude {
    pub mod v1 {
        pub use crate::test;
        pub use core::prelude::v1::*;
    }
    pub mod rust_2015 { pub use super::v1::*; }
    pub mod rust_2018 { pub use super::v1::*; }
    pub mod rust_2021 { pub use super::v1::*; }
    pub mod rust_2024 { pub use super::v1::*; }
}
#[rustc_builtin_macro]
pub macro test($item:item) {}
pub mod time {
    pub struct Duration { secs: u64 }
    impl Duration {
        pub const fn from_secs(secs: u64) -> Duration { Duration { secs } }
    }
}
pub mod env {
    pub fn var(key: &str) -> Option<u8> { None }
}
pub mod collections {
    pub struct HashMap<K, V> { k: K, v: V }
    impl<K, V> HashMap<K, V> {
        pub fn get(&self, k: &K) -> Option<&V> { None }
    }
}
pub mod num {
    impl u32 {
        #[rustc_allow_incoherent_impl]
        pub const fn from_le_bytes(bytes: [u8; 4]) -> u32 { 0 }
    }
}
"#;

const MAIN: &str = "//- /src/lib.rs crate:main deps:std\n";

fn header() -> String {
    format!("{MINICORE}{STD}{MAIN}")
}

fn flags(lint_name: &str) -> String {
    format!("{CLIPPY_OFF} --warn=blaze::{lint_name}")
}

/// What `blaze::<lint_name>` alone says of `main`.
fn found(lint_name: &str, main: &str) -> String {
    lint(&format!("{}{}", header(), main.trim_start_matches('\n')), &flags(lint_name))
}

fn numbers(main: &str) -> String {
    found("magic_number", main)
}

fn strings(main: &str) -> String {
    found("magic_string", main)
}

/// The invariant: a literal that is the named home (a `const`, a `static`, an enum
/// discriminant), an identity, a doubling `2`, an index or slice bound a fixed array's length
/// proves in range, a pattern, a byte, test code or what a macro wrote is never a finding.
#[test]
fn magic_number_leaves_the_exemptions_alone() {
    let out = numbers(
        r#"
pub const LIMIT: u32 = 4096;
pub static SEED: u64 = 7;
pub enum Code { A = 5, B = 9 }
pub fn identities(x: i64) -> i64 { x * 2 + 1 - 0 + x / 2 + (x << 2) - 1 }
pub fn floats(x: f64) -> f64 { x * 1.0 + 0.0 }
pub fn in_range(p: (u8, u8), q: [u8; 4]) -> u8 { p.0 + q[3] + q[1..3][0] }
pub fn bytes(b: u8) -> bool {
    match b {
        b'0'..=b'9' => true,
        0x7f => false,
        _ => b == b'_',
    }
}
pub fn inline() -> usize { const { 64 } }
pub fn take(n: u64) {}
#[test]
fn fixture_data() { take(4096); }
macro_rules! cap { () => { take(4096) } }
pub fn by_macro() { cap!(); }
"#,
    );
    assert!(!out.contains("magic_number"), "{out}");
}

#[test]
fn magic_number_names_a_bare_number_and_a_duration() {
    let out = numbers(
        r#"
pub fn take(n: u64) {}
pub fn once() { take(4096); }
pub fn negative() -> i64 { -40 }
pub fn ratio(x: f64) -> f64 { x * 0.75 }
pub fn three(x: u32) -> u32 { x * 3 }
pub fn wait() -> std::time::Duration { std::time::Duration::from_secs(30) }
"#,
    );
    assert_eq!(out.matches("is a magic number: name it").count(), 4, "{out}");
    assert!(out.contains("`-40` is a magic number"), "the sign is part of the value:\n{out}");
    assert_eq!(out.matches("`30` is a duration chosen here: name this duration").count(), 1, "{out}");
}

#[test]
fn magic_number_asks_for_one_const_where_a_number_is_repeated() {
    let out = numbers(
        r#"
pub fn take(capacity: usize) {}
pub fn a() { take(4096); }
pub fn b() { take(4096); }
pub fn c() { take(512); }
"#,
    );
    assert!(
        out.contains("`4096` is passed to `take`'s `capacity` here and in `fn b` (2 places): name it once, `const CAPACITY: usize = 4096;`"),
        "{out}"
    );
    assert!(out.contains("here and in `fn a` (2 places)"), "{out}");
    assert_eq!(out.matches("`512` is a magic number").count(), 1, "{out}");
}

#[test]
fn magic_number_derives_the_one_const_passed_there_elsewhere() {
    const CODE: &str = r#"
/// A page of the store.
pub const PAGE: usize = 4096;
pub fn take(capacity: usize) {}
pub fn a() { take(PAGE); }
pub fn b() { take(4096); }
"#;
    let out = numbers(CODE);
    assert!(
        out.contains("`4096` is the value of `PAGE`, passed to `take`'s `capacity` elsewhere in the crate: derive it, `PAGE`"),
        "{out}"
    );
    let after = offered_in(&header(), CODE.trim_start_matches('\n'), &flags("magic_number"));
    assert!(after.contains("pub fn b() { take(PAGE); }"), "{after}");
}

#[test]
fn magic_number_derives_an_arrays_length_and_its_fix_is_machine_applicable() {
    const CODE: &str = r#"
pub fn sum(a: [u32; 4]) -> u32 {
    let mut s = 0;
    for i in 0..4 {
        s += a[i];
    }
    s
}
"#;
    let out = numbers(CODE);
    assert!(out.contains("`4` is the length of `a`: derive it, `a.len()`"), "{out}");
    let after =
        fixed_in(&format!("{}{}", header(), CODE.trim_start_matches('\n')), &flags("magic_number"));
    assert!(after.contains("for i in 0..a.len() {"), "{after}");
}

#[test]
fn magic_number_derives_a_variant_count_and_finds_one_that_disagrees() {
    const CODE: &str = r#"
#![feature(variant_count)]
pub enum Tag { A, B, C }
pub fn counts(t: Tag) -> u32 {
    let mut counts = [0u32; 3];
    counts[t as usize] += 1;
    counts[0]
}
pub fn short(t: Tag) -> u32 {
    let mut seen = [0u32; 4];
    seen[t as usize] += 1;
    seen[0]
}
"#;
    let out = numbers(CODE);
    assert!(
        out.contains("`3` is the number of variants of `Tag`: derive it, `core::mem::variant_count::<Tag>()`"),
        "{out}"
    );
    assert!(
        out.contains("`4` is the length of an array indexed by `Tag`, which has 3 variants"),
        "{out}"
    );
}

#[test]
fn magic_number_derives_the_size_an_integer_is_read_from_and_finds_a_slice_that_disagrees() {
    let out = numbers(
        r#"
pub fn read(b: &[u8]) -> u32 { u32::from_le_bytes(b[0..4].try_into().unwrap()) }
pub fn wrong(b: &[u8]) -> u32 { u32::from_le_bytes(b[0..3].try_into().unwrap()) }
"#,
    );
    assert!(out.contains("`4` is the size of `u32`: derive it, `size_of::<u32>()`"), "{out}");
    assert!(out.contains("this slice is 3 bytes and a `u32` is 4"), "{out}");
}

#[test]
fn magic_string_leaves_the_exemptions_alone() {
    let out = strings(
        r#"
pub const NAME: &str = "blaze";
pub fn prose() -> &'static str { "the value is there" }
pub fn punctuation() -> &'static str { ", " }
pub fn empty() -> &'static str { "" }
pub fn lone() -> &'static str { "lonely" }
pub fn run(name: &str) {}
#[test]
fn fixture_data() { run("cargo"); run("cargo"); }
"#,
    );
    assert!(!out.contains("magic_string"), "{out}");
}

#[test]
fn magic_string_finds_a_closed_set() {
    let out = strings(
        r#"
pub fn kind(k: &str) -> u8 {
    match k {
        "lib" => 1,
        "bin" | "example" => 0,
        _ => 0,
    }
}
pub fn is(k: &str) -> bool { k == "dep-info" || k == "metadata" }
"#,
    );
    assert!(
        out.contains("`k` is matched against \"lib\", \"bin\", \"example\": the set is closed, make it an enum with these variants"),
        "{out}"
    );
    assert!(
        out.contains("`k` is compared against \"dep-info\", \"metadata\": the set is closed, make it an enum with these variants"),
        "{out}"
    );
    assert!(!out.contains("is compared with `k`"), "the set says it once:\n{out}");
}

#[test]
fn magic_string_finds_a_near_miss_a_repeat_and_a_spelled_name() {
    let out = strings(
        r#"
pub fn emit(kind: &str) {}
pub fn a() { emit("dep-info"); }
pub fn b() { emit("dep_info"); }
pub fn run(program: &str) {}
pub fn spawn(program: &str) {}
pub fn c() { run("cargo"); }
pub fn d() { spawn("cargo"); }
pub enum Kind { Lib, Bin }
pub fn name(k: Kind) -> &'static str {
    match k {
        Kind::Lib => "lib",
        Kind::Bin => "bin",
    }
}
"#,
    );
    assert!(
        out.contains("`\"dep-info\"` differs from `\"dep_info\"`, passed to `emit`'s `kind` in `fn b`, only by `-` and `_`"),
        "{out}"
    );
    assert!(out.contains("`\"dep_info\"` differs from `\"dep-info\"`"), "{out}");
    assert!(out.contains("`\"cargo\"` is written here and in `fn d` (2 places): name it once"), "{out}");
    // One string per variant is the whole set: one finding at the `match`, none per arm.
    assert_eq!(
        out.matches("this `match` spells the name of every variant of `Kind` in lower case: keep the names in one `const` table beside `Kind`").count(),
        1,
        "{out}"
    );
    assert!(!out.contains("spells the name of the variant"), "{out}");
}

#[test]
fn magic_string_derives_a_whole_set_by_reflection_where_the_crate_has_it() {
    let out = strings(
        r#"
#![feature(blaze_comptime)]
pub enum Kind { Lib, Bin }
pub fn name(k: Kind) -> &'static str {
    match k {
        Kind::Lib => "lib",
        Kind::Bin => "bin",
    }
}
pub enum Tier { Hot, Warm, Cold }
pub fn tiers() -> [&'static str; 3] { ["hot", "warm", "cold"] }
pub fn others() -> [&'static str; 2] { ["fast", "slow"] }
"#,
    );
    assert!(
        out.contains("this `match` spells the name of every variant of `Kind` in lower case: derive the names, `for const` over `Kind`'s variants"),
        "{out}"
    );
    assert!(
        out.contains("this array spells the name of every variant of `Tier` in lower case: derive the names, `for const` over `Tier`'s variants"),
        "{out}"
    );
    // A finding quotes its literal in backticks; the bare `"hot"` is the source line the
    // array's own finding prints.
    assert!(!out.contains("`\"hot\"`"), "the array is the finding, not each name:\n{out}");
    assert!(!out.contains("`\"fast\"`"), "an array no enum's variants are is not a set:\n{out}");
    assert!(!out.contains("[\"fast\", \"slow\"]"), "an array no enum's variants are is not a set:\n{out}");
}

#[test]
fn magic_string_says_the_convention_and_finds_the_type_and_the_crate() {
    let out = strings(
        r#"
pub enum Kind { DepInfo, Metadata, Link }
pub fn emit(k: Kind) -> &'static str {
    match k {
        Kind::DepInfo => "dep-info",
        _ => "other",
    }
}
pub struct Store;
impl Store {
    pub fn kind(&self) -> &'static str { "store" }
}
pub fn which() -> &'static str { "main" }
"#,
    );
    assert!(
        out.contains("`\"dep-info\"` spells the name of the variant `Kind::DepInfo` in kebab-case (`DepInfo` is `\"dep-info\"`)"),
        "{out}"
    );
    assert!(out.contains("`\"store\"` spells the name of the type its method is of, `Store` in lower case"), "{out}");
    assert!(out.contains("`\"main\"` spells the name of its crate, `main`"), "{out}");
    assert!(out.contains("`env!(\"CARGO_PKG_NAME\")`"), "{out}");
    assert!(!out.contains("\"other\""), "{out}");
}

#[test]
fn magic_string_uses_the_const_that_already_holds_it() {
    const CODE: &str = r#"
/// The variable that moves blaze's home.
pub const BLAZE_HOME: &str = "BLAZE_HOME";
pub fn home() -> Option<u8> { std::env::var("BLAZE_HOME") }
"#;
    let out = strings(CODE);
    assert_eq!(out.matches("`BLAZE_HOME` already names this; use it").count(), 1, "the const is the home, the call the copy:\n{out}");
    let after = fixed_in(&format!("{}{}", header(), CODE.trim_start_matches('\n')), &flags("magic_string"));
    assert!(after.contains("std::env::var(BLAZE_HOME)"), "{after}");
}

#[test]
fn magic_string_uses_a_dependencys_const() {
    let fixture = format!(
        "{MINICORE}{STD}//- /dep/lib.rs crate:dep deps:std\n{}//- /src/lib.rs crate:main deps:std,dep\n{}",
        "/// The variable that moves the store.\npub const STORE_VAR: &str = \"BLAZE_STORE\";\n/// A tool's name.\npub const TOOL: &str = \"cargo\";\n",
        "pub fn store() -> Option<u8> { std::env::var(\"BLAZE_STORE\") }\npub fn run(program: &str) {}\npub fn tool() { run(\"cargo\"); }\n",
    );
    let out = lint(&fixture, &flags("magic_string"));
    assert!(out.contains("`dep::STORE_VAR` already names this; use it"), "{out}");
    assert!(
        !out.contains("`dep::TOOL`"),
        "a dependency's `const` is asked of only for a key-like string or one that spells its name:\n{out}"
    );
}

#[test]
fn repeated_format_finds_one_sentence_written_twice() {
    let out = found(
        "repeated_format",
        r#"
pub fn a(p: &str) { panic!("{} cannot be read from {}", p, p) }
pub fn b(p: &str) { panic!("{0} cannot be read from {1:?}", p, p) }
pub fn c(p: &str) { panic!("took {} ms", p) }
pub fn d(p: &str) { panic!("took {} ms", p) }
pub fn e(p: &str) { panic!("{} was written once in the crate", p) }
"#,
    );
    assert!(
        out.contains("the message `\"{} cannot be read from {}\"` is written here and in `fn b` (2 places): give the sentence one owner, one function that formats it"),
        "{out}"
    );
    assert!(out.contains("here and in `fn a` (2 places)"), "{out}");
    assert!(!out.contains("took {} ms"), "below the words floor:\n{out}");
    assert!(!out.contains("written once"), "a sentence once is not looked at:\n{out}");
}

#[test]
fn templates_are_one_sentence_up_to_their_placeholders() {
    use super::template::{TEMPLATE_WORDS_FLOOR, normalise, words};
    let plain = normalise("\"{0}: cannot read {path:?}\"").expect("a string token");
    assert_eq!(plain.as_str(), "{}: cannot read {}");
    assert_eq!(normalise("r#\"{}: cannot read {}\"#"), Some(plain));
    assert_eq!(normalise("\"{{literal}} {}\"").map(|it| it.as_str().to_owned()), Some("{{literal}} {}".to_owned()));
    assert_eq!(normalise("\"{ never closed\""), None);
    assert_eq!(words("{} cannot be read from {}"), 4);
    assert_eq!(words("took {} ms\\n"), 2);
    assert!(words("took {} ms") < TEMPLATE_WORDS_FLOOR);
}

#[test]
fn spellings_say_their_convention() {
    use super::{Case, spelling};
    assert_eq!(spelling("DepInfo", "DepInfo"), Some(Case::AsWritten));
    assert_eq!(spelling("DepInfo", "dep_info"), Some(Case::Snake));
    assert_eq!(spelling("DepInfo", "dep-info"), Some(Case::Kebab));
    assert_eq!(spelling("DepInfo", "DEP_INFO"), Some(Case::ScreamingSnake));
    assert_eq!(spelling("DepInfo", "depInfo"), Some(Case::Camel));
    assert_eq!(spelling("dep_info", "DepInfo"), Some(Case::Pascal));
    assert_eq!(spelling("Lib", "lib"), Some(Case::Lower));
    assert_eq!(spelling("DepInfo", "depinfo"), Some(Case::OtherCase));
    assert_eq!(spelling("Lib", "bin"), None);
}

mod shared_strings {
    use intern::Symbol;
    use syntax::{TextRange, TextSize};

    use super::super::shared::{
        Access, SharedString, StringRow, UseClass, merge_strings, normalise,
    };

    fn row(text: &str, class: UseClass, access: Access, item: &str, at: u32) -> StringRow {
        StringRow {
            norm: normalise(text),
            exact: Symbol::intern(text),
            class,
            access,
            item: Symbol::intern(item),
            file: span::FileId::from_raw(0),
            range: TextRange::at(TextSize::from(at), TextSize::from(1)),
        }
    }

    /// The rows as `string_rows` hands them over: sorted by normalised spelling, then exact.
    fn sorted(mut rows: Vec<StringRow>) -> Vec<StringRow> {
        rows.sort_by(|a, b| (a.norm.as_str(), a.exact.as_str()).cmp(&(b.norm.as_str(), b.exact.as_str())));
        rows
    }

    fn merged(crates: &[(&str, Vec<StringRow>)]) -> Vec<SharedString, std::alloc::Global> {
        let lists: Vec<(Symbol, Vec<StringRow>)> =
            crates.iter().map(|(name, rows)| (Symbol::intern(name), sorted(rows.clone()))).collect();
        let views: Vec<(Symbol, &[StringRow])> = lists.iter().map(|(name, rows)| (name.clone(), rows.as_slice())).collect();
        merge_strings(&views)
    }

    #[test]
    fn spellings_of_one_name_normalise_together() {
        assert_eq!(normalise("build_ids"), normalise("Build-Id"));
        assert_eq!(normalise("buildId"), normalise("build_id"));
        assert_ne!(normalise("bus"), normalise("bu"), "a short word keeps its `s`");
    }

    /// The invariant: a string is reported across crates only when two crates write it, the
    /// same or one name spelled two ways, and never when it is prose.
    #[test]
    fn the_same_string_in_two_crates_is_one_finding() {
        let out = merged(&[
            ("a", vec![row("store.json", UseClass::Path, Access::Writes, "a::save", 0)]),
            ("b", vec![row("store.json", UseClass::Path, Access::Reads, "b::load", 0)]),
        ]);
        let [SharedString::Same { spelling, sites }] = out.as_slice() else { panic!("{out:?}") };
        assert_eq!(spelling.as_str(), "store.json");
        assert_eq!(sites.len(), 2);
        assert!(out[0].message().contains("`\"store.json\"` is written in `a` and `b`"), "{}", out[0].message());
        assert_eq!(out[0].lint(), super::super::Lint::StringSharedAcrossCrates);
    }

    #[test]
    fn a_near_miss_across_two_crates_says_who_writes_and_who_reads() {
        let out = merged(&[
            ("writer", vec![row("build_id", UseClass::Key, Access::Writes, "writer::put", 0)]),
            ("reader", vec![row("buildId", UseClass::Key, Access::Reads, "reader::get", 0)]),
        ]);
        let [near @ SharedString::NearMiss { .. }] = out.as_slice() else { panic!("{out:?}") };
        let message = near.message();
        assert!(
            message.contains("`writer` writes `\"build_id\"` and `reader` reads `\"buildId\"`, which differ only by the case convention"),
            "{message}"
        );
        assert_eq!(near.lint(), super::super::Lint::StringNearMissAcrossCrates);
    }

    #[test]
    fn one_character_apart_in_two_crates_is_a_near_miss() {
        let out = merged(&[
            ("a", vec![row("metadata", UseClass::Key, Access::Names, "a::f", 0)]),
            ("b", vec![row("metadsta", UseClass::Key, Access::Names, "b::f", 0)]),
        ]);
        let [SharedString::NearMiss { how, .. }] = out.as_slice() else { panic!("{out:?}") };
        assert_eq!(*how, "one character");
    }

    #[test]
    fn prose_and_a_string_of_one_crate_are_not_reported() {
        let out = merged(&[
            (
                "a",
                vec![
                    row("cannot read the file", UseClass::Compared, Access::Reads, "a::f", 0),
                    row("store.json", UseClass::Path, Access::Writes, "a::save", 1),
                    row("store.json", UseClass::Path, Access::Reads, "a::load", 2),
                    row("dep-info", UseClass::Key, Access::Names, "a::g", 3),
                    row("dep_info", UseClass::Key, Access::Names, "a::h", 4),
                ],
            ),
            ("b", vec![row("cannot read the file", UseClass::Compared, Access::Reads, "b::f", 0)]),
        ]);
        assert!(out.is_empty(), "prose in two crates, a string twice in one crate, and one crate's own near-miss:\n{out:?}");
    }

    #[test]
    fn worker_names_and_short_words_are_two_names() {
        let out = merged(&[
            ("a", vec![row("worker1", UseClass::Key, Access::Names, "a::f", 0), row("get", UseClass::Key, Access::Names, "a::g", 1)]),
            ("b", vec![row("worker2", UseClass::Key, Access::Names, "b::f", 0), row("set", UseClass::Key, Access::Names, "b::g", 1)]),
        ]);
        assert!(out.is_empty(), "{out:?}");
    }
}

#[test]
fn magic_string_names_what_the_program_looks_up_by() {
    let out = strings(
        r#"
pub fn home() -> Option<u8> { std::env::var("HOME") }
pub fn look(m: &std::collections::HashMap<&str, u32>) -> Option<&u32> { m.get(&"limit") }
pub fn state(s: &str) -> bool { s == "ready" }
"#,
    );
    assert!(out.contains("`\"HOME\"` names an environment variable: name it"), "{out}");
    assert!(out.contains("`\"limit\"` is a map key: name it"), "{out}");
    assert!(out.contains("`\"ready\"` is compared with `s`: name it, a `const`"), "{out}");
}

#[test]
fn near_misses_are_one_spelling_twice_and_nothing_wider() {
    use super::near;
    assert_eq!(near("dep-info", "dep_info"), Some("`-` and `_`"));
    assert_eq!(near("Ready", "ready"), Some("case"));
    assert_eq!(near("target", "targets"), Some("a plural `s`"));
    assert_eq!(near("metadata", "metadsta"), Some("one character"));
    assert_eq!(near("get", "set"), None, "short words one letter apart are two words");
    assert_eq!(near("worker1", "worker2"), None, "a number's digits are not a typo");
    assert_eq!(near("lib", "bin"), None);
}

/// The invariant: a number other than the identities passed as an argument is always a
/// finding, and the same number as a `const`'s initializer never is.
#[test]
fn magic_number_fires_on_a_bare_argument_and_never_on_a_const() {
    blaze_sim::check(blaze_sim::Config::cases(16), |tape| {
        let value: u64 = tape.int_in(2..=1 << 40, "value");
        let other: u64 = tape.int_in(2..=1 << 40, "other");
        let out = numbers(&format!(
            "pub const C: u64 = {value} * {other} + {value};\npub fn take(n: u64) {{}}\npub fn f() {{ take({value}); }}\n"
        ));
        assert_eq!(out.matches("magic_number").count(), 1, "{out}");
        assert!(out.contains(&format!("`{value}` is a magic number")), "{out}");
    });
}
