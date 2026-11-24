//! The rules on written cases and their look-alikes, the fix round trip, the item-alone
//! property over generated files, and planted bugs those checks must catch.

use std::collections::BTreeSet;
use std::format;
use std::string::{String as StdString, ToString};
use std::vec;
use std::vec::Vec as StdVec;

use blaze_file_items::outline;
use blaze_lex::{Cpu, FrontmatterAllowed, TokenKind, tokenize};
use blaze_own::{String as OwnString};
use blaze_sim::{Broke, Config, Tape};
use blaze_surface::Edition;

use crate::matcher::{LIVE, Live, NO_SEQ, Pattern, Trie, nodes_for};
use crate::sequences::{OTHER, PATTERNS, TRIE};
use crate::{
    ALLOWED_STATICS, FixError, Findings, Level, OwnHeads, RootMarks, Rule, RuleSet, Scope, Span, Step, Template, TestBits, Unit, Units, Variants, apply,
    apply_all, is_hermetic_path, lint, lint_in, show_message, statics_of_path,
};

const FILE: Scope = Scope::file(TestBits::NONE, false, false);

/// What `rules` finds in `text` linted as one unit: each finding's kind and range text.
fn found<'t>(rules: &RuleSet, text: &'t str, scope: Scope) -> StdVec<(&'static str, &'t str)> {
    let alloc = blaze_own::testing::heap().alloc();
    let mut out = Findings::new_in(alloc);
    let unit = Unit { text, holes: &[], scope };
    match lint(rules, &unit, alloc, &mut out) {
        Ok(_) => out.list.iter().map(|f| (f.template.kind(), f.range.of(text))).collect(),
        Err(error) => panic!("lint refused {text:?}: {error:?}"),
    }
}

/// The kinds `rule` finds in `text`, in order.
fn kinds(rule: Rule, text: &str) -> StdVec<&'static str> {
    found(&RuleSet::only(&[rule]), text, FILE).into_iter().map(|(kind, _)| kind).collect()
}

fn none(rule: Rule, texts: &[&str]) {
    for text in texts {
        assert_eq!(kinds(rule, text), StdVec::<&str>::new(), "{} fired on {text:?}", rule.name());
    }
}

#[test]
fn casts() {
    let rule = Rule::Casts;
    assert_eq!(found(&RuleSet::only(&[rule]), "fn f(x: u64) -> u32 { x as u32 }", FILE), [("cast", "as u32")]);
    assert_eq!(kinds(rule, "fn f() -> usize {\n    y.len() as usize\n}"), ["cast"]);
    none(rule, &[
        "fn f() { let a = x as f64; }",
        "use a::b as u8;",
        "fn f() { let s = \"x as u32\"; }",
        "fn f() { let s = r#\"x as u32\"#; }",
        "fn f() {\n    // x as u32\n    /* y as u8 */\n}",
        "fn f() { let has_u32 = as_u32; }",
        "fn f() { let a = b as u32::Assoc; }",
        "#[test]\nfn t() { let a = x as u32; }",
        "#[cfg(test)]\nmod t {\n    fn t() { let a = x as u32; }\n}",
        "#[blaze_sim::test]\nfn t() { let a = x as u32; }",
    ]);
}

#[test]
fn clones() {
    let rule = Rule::Clones;
    assert_eq!(found(&RuleSet::only(&[rule]), "fn f() { let a = x.clone(); }", FILE), [("unknown", "clone()")]);
    assert_eq!(kinds(rule, "fn f() {\n    // clone: a refcount\n    let a = x.clone();\n}"), ["handle"]);
    assert_eq!(kinds(rule, "fn f() {\n    let a = x.clone(); // clone: error path only\n}"), ["error-path"]);
    assert_eq!(kinds(rule, "fn f() {\n    // clone: copies the vec\n    let a = x.clone();\n}"), ["deep"]);
    assert_eq!(kinds(rule, "fn f() {\n    // clone: for the cache\n    let a = x.clone();\n}"), ["unknown"]);
    // A comment line goes on, a statement's unended line goes on; a finished statement stops.
    assert_eq!(kinds(rule, "fn f() {\n    // clone: arc\n    // more\n    let a = g(\n        x.clone());\n}"), ["handle"]);
    assert_eq!(kinds(rule, "fn f() {\n    // clone: arc\n    let b = 1;\n    let a = x.clone();\n}"), ["unknown"]);
    assert_eq!(kinds(rule, "fn f(h: &Arc<u8>) {\n    let a = h.clone();\n}"), ["handle"]);
    assert_eq!(kinds(rule, "fn f() {\n    let h = std::rc::Rc::new(1);\n    let a = h.clone();\n}"), ["handle"]);
    assert_eq!(kinds(rule, "fn f() { let a = Clone::clone(&x); }"), ["unknown"]);
    none(rule, &[
        "fn clone(&self) -> Self { Self }",
        "fn f() { let s = \"x.clone()\"; }",
        "fn f() {\n    // x.clone()\n}",
        "fn f() { let my_clone = x.clone_from(&y); }",
        "#[test]\nfn t() { let a = x.clone(); }",
    ]);
}

#[test]
fn allocs() {
    let rule = Rule::Allocs;
    assert_eq!(found(&RuleSet::only(&[rule]), "fn f() -> String { g() }", FILE), [("signature", "fn f")]);
    assert_eq!(kinds(rule, "fn f() -> Vec<u8> { g() }"), ["signature"]);
    assert_eq!(kinds(rule, "fn f(m: BTreeMap<u8, u8>) {}"), ["signature"]);
    assert_eq!(kinds(rule, "fn f(v: Vec<u8, Global>) {}"), ["signature", "global"]);
    assert_eq!(kinds(rule, "fn f() {\n    let x = 1; // alloc: the root's\n}"), ["comment"]);
    assert_eq!(kinds(rule, "#[expect(blaze::implicit_allocator, reason = \"x\")]\nfn f() {}"), ["expect"]);
    // A test function is not exempt from allocs, `#[cfg(test)]` is.
    assert_eq!(kinds(rule, "#[test]\nfn t() { let g = Global; }"), ["global"]);
    none(rule, &[
        "fn f(v: Vec<u8, A>) {}",
        "fn main() -> String { g() }",
        "fn f(s: AString<'_>) {}",
        "use std::alloc::Global;",
        "fn f() {\n    // alloc:\n}",
        "fn f() { let s = \"Global // alloc: x\"; }",
        "impl Display for S {\n    fn fmt(&self) -> String { g() }\n}",
        "#[cfg(test)]\nfn t() { let g = Global; }",
        "fn f() { let global = Globals; }",
    ]);
}

#[test]
fn untyped() {
    let rule = Rule::Untyped;
    assert_eq!(found(&RuleSet::only(&[rule]), "fn f() -> Result<u8, String> { g() }", FILE), [("string", "fn f")]);
    assert_eq!(kinds(rule, "fn f() -> Result<(), &'static str> { g() }"), ["str"]);
    assert_eq!(kinds(rule, "fn f() -> Result<u8, Box<dyn std::error::Error + Send>> { g() }"), ["boxed"]);
    assert_eq!(kinds(rule, "fn f() -> anyhow::Result<u8> { g() }"), ["anyhow"]);
    assert_eq!(kinds(rule, "fn f(e: anyhow::Error) {}"), ["anyhow"]);
    assert_eq!(kinds(rule, "type R = Result<u8, String>;"), ["alias"]);
    assert_eq!(kinds(rule, "fn f() -> Result<String, String> { g() }"), ["string"]);
    none(rule, &[
        "fn f() -> Result<u8, MyError> { g() }",
        "fn f() -> Result<String> { g() }",
        "impl T for S {\n    fn f() -> Result<u8, String> { g() }\n}",
        "fn f() { let s = \"Result<u8, String>\"; }",
        "#[test]\nfn t() -> Result<u8, String> { g() }",
    ]);
    let script = Scope::file(TestBits::NONE, false, true);
    assert!(found(&RuleSet::only(&[rule]), "fn f() -> Result<u8, String> { g() }", script).is_empty());
    let bin = Scope::file(TestBits::NONE, true, false);
    assert!(found(&RuleSet::only(&[rule]), "fn f() -> Result<u8, String> { g() }", bin).is_empty());
    assert_eq!(found(&RuleSet::only(&[rule]), "mod m {\n    fn f() -> Result<u8, String> { g() }\n}", bin).len(), 1);
}

#[test]
fn recursion() {
    let rule = Rule::Recursion;
    assert_eq!(
        found(&RuleSet::only(&[rule]), "fn f(n: u32) -> u32 {\n    f(n - 1)\n}", FILE),
        [("recursion", "f")]
    );
    assert_eq!(kinds(rule, "impl S {\n    fn f(&self) {\n        self.f();\n    }\n}"), ["recursion"]);
    assert_eq!(kinds(rule, "impl S {\n    fn f() {\n        Self::f();\n    }\n}"), ["recursion"]);
    assert_eq!(kinds(rule, "fn f() {\n    self::f::<u8>();\n}"), ["recursion"]);
    none(rule, &[
        "impl S {\n    fn f() {\n        f();\n    }\n}",
        "impl S {\n    fn f() {\n        self.f();\n    }\n}",
        "fn f() {\n    Self::f();\n}",
        "// recursion: calls another impl\nfn f() {\n    f();\n}",
        "fn f() {\n    let f = 1;\n    g.f();\n    x.y.f();\n}",
        "fn f() {\n    let s = \"f()\";\n}",
        "#[test]\nfn f() {\n    f();\n}",
    ]);
}

#[test]
fn stops() {
    let rule = Rule::Stops;
    assert_eq!(found(&RuleSet::only(&[rule]), "fn f() { x.unwrap(); }", FILE), [("unwrap", "unwrap()")]);
    assert_eq!(kinds(rule, "fn f() { x.expect(\"why\"); }"), ["expect"]);
    assert_eq!(kinds(rule, "fn f() { panic!(\"x\"); unreachable!(); todo!(); unimplemented!() }"), [
        "panic",
        "unreachable",
        "todo",
        "unimplemented"
    ]);
    assert_eq!(found(&RuleSet::only(&[rule]), "fn f() { let a = v[i]; }", FILE), [("index", "[i]")]);
    assert_eq!(kinds(rule, "fn f() { let a = g()[i + 1]; }"), ["index"]);
    none(rule, &[
        "fn f() { x.unwrap_or(1); }",
        "fn f() { let unwrap = 1; }",
        "fn unwrap(self) -> T { self.0 }",
        "macro_rules! panic { () => {} }",
        "fn f() { if a != b {} }",
        "fn f() { let a = v[0]; let b = v[1..3]; let c = v[..]; }",
        "fn f() { let a = [1, 2]; let b: [u8; 4] = z; }",
        "fn f() { let s = \"x.unwrap() v[i]\"; }",
        "fn f() {\n    // x.unwrap()\n}",
        "#[test]\nfn t() { x.unwrap(); }",
        "#[cfg(test)]\nmod t {\n    fn t() { x.unwrap(); }\n}",
    ]);
}

#[test]
fn ambient() {
    let rule = Rule::Ambient;
    assert_eq!(found(&RuleSet::only(&[rule]), "use std::mem;", FILE), [("std", "std")]);
    assert_eq!(kinds(rule, "fn f() { std::mem::take(x); }"), ["std"]);
    assert_eq!(kinds(rule, "extern crate alloc;"), ["alloc"]);
    assert_eq!(
        found(&RuleSet::only(&[rule]), "fn f() { let s = std::string::String::new(); }", FILE),
        [("std", "std"), ("vec_box", "std::string::String")]
    );
    none(rule, &[
        "fn f() { self.std; }",
        "fn f() { my_std::x(); }",
        "fn f() { crate::std::x(); }",
        "fn f() { let s = \"std::mem\"; }",
        "#[test]\nfn t() { std::mem::take(x); }",
    ]);
}

#[test]
fn unsafe_comment() {
    let rule = Rule::UnsafeComment;
    assert_eq!(found(&RuleSet::only(&[rule]), "fn f() {\n    unsafe { g() }\n}", FILE), [("no-safety-comment", "unsafe")]);
    assert_eq!(kinds(rule, "unsafe impl Send for S {}"), ["no-safety-comment"]);
    // A test is not exempt: the comment states the assumption wherever the block is.
    assert_eq!(kinds(rule, "#[test]\nfn t() {\n    unsafe { g() }\n}"), ["no-safety-comment"]);
    none(rule, &[
        "fn f() {\n    // SAFETY: g has no precondition\n    unsafe { g() }\n}",
        "fn f() {\n    // Safety: the caller holds the lock.\n    // More.\n    unsafe { g() }\n}",
        "// SAFETY: S holds no thread-bound state\nunsafe impl Send for S {}",
        "fn f() {\n    let a = unsafe { g() }; // SAFETY: none\n}",
        "unsafe fn f() {}",
        "fn f() { let s = \"unsafe { }\"; }",
    ]);
}

#[test]
fn format_macro() {
    let rule = Rule::FormatMacro;
    assert_eq!(found(&RuleSet::only(&[rule]), "fn f() { let s = format!(\"x\"); }", FILE), [("format", "format!")]);
    assert_eq!(kinds(rule, "fn f() { write!(out, \"x\"); println!(); }"), ["format", "format"]);
    none(rule, &[
        "macro_rules! format { () => {} }",
        "fn format(&self) {}",
        "fn f() { out.write(b); let formatter = 1; }",
        "fn f() { let s = \"format!()\"; }",
        "#[test]\nfn t() { println!(\"x\"); }",
        "#[cfg(test)]\nmod t {\n    fn t() { println!(\"x\"); }\n}",
    ]);
}

#[test]
fn ambient_heap() {
    let rule = Rule::AmbientHeap;
    let all = RuleSet::only(&[rule]);
    assert_eq!(found(&all, "fn f() { let a = Alloc::global(); }", FILE), [("reach", "Alloc::global")]);
    assert_eq!(kinds(rule, "fn f() { let h = blaze_own::Heap::process(); let t = testing::heap(); }"), ["reach", "reach"]);
    assert_eq!(found(&all, "fn f() { let v = Vec::new(); }", FILE), [("constructor", "Vec::new")]);
    assert_eq!(kinds(rule, "fn f() { let w = Vec::with_capacity(4); let b = Box::new(1); }"), ["constructor", "constructor"]);
    assert_eq!(kinds(rule, "fn f() { let s = String::from(\"x\"); let r = std::rc::Rc::new(1); }"), [
        "constructor",
        "constructor"
    ]);
    assert_eq!(kinds(rule, "fn f() { let v = vec![1]; let s = format!(\"x\"); }"), ["macro", "macro"]);
    assert_eq!(found(&all, "fn f() { let s = x.to_string(); }", FILE), [("method", "to_string")]);
    assert_eq!(kinds(rule, "fn f() { let o = y.to_owned(); let v = z.to_vec(); }"), ["method", "method"]);
    none(rule, &[
        "fn f(al: Alloc<'_>) { let v = Vec::new_in(al); let b = Box::new_in(1, al); }",
        "fn f(al: A) { let v = vec_in![al; 1]; let s = format_in!(al, \"x\"); }",
        "fn f() { let g = Alloc::global; }",
        "fn f() { let r = a..to_vec(); }",
        "fn f() { let s = \"Vec::new() x.to_string()\"; }",
        "fn f() {\n    // Vec::new()\n}",
        "#[test]\nfn t() { let v = Vec::new(); }",
        "#[cfg(test)]\nmod t {\n    fn h() { let a = Alloc::global(); }\n}",
    ]);
}

#[test]
fn no_async() {
    let rule = Rule::NoAsync;
    assert_eq!(found(&RuleSet::only(&[rule]), "async fn f() {}", FILE), [("async", "async")]);
    assert_eq!(kinds(rule, "pub async unsafe fn f() {}"), ["async"]);
    assert_eq!(kinds(rule, "fn f() { let a = async move { 1 }; let b = async { 2 }; let c = async |x| x; }"), [
        "async", "async", "async"
    ]);
    assert_eq!(found(&RuleSet::only(&[rule]), "fn f() { let r = fut.await; }", FILE), [("await", "await")]);
    none(rule, &[
        "fn f() { let async_ = 1; let r = fut.r#await; }",
        "fn f() { let r = x..await_all(); }",
        "fn f() { let s = \"async fn .await\"; }",
        "#[test]\nfn t() { let a = async { x.await }; }",
    ]);
}

#[test]
fn ambient_read() {
    let rule = Rule::AmbientRead;
    assert_eq!(found(&RuleSet::only(&[rule]), "fn f() { let t = Instant::now(); }", FILE), [("clock", "Instant::now")]);
    assert_eq!(kinds(rule, "fn f() { let s = std::time::SystemTime::now(); }"), ["clock"]);
    assert_eq!(kinds(rule, "fn f() { let v = std::env::var(\"X\"); let a = env::args(); let d = env::current_dir(); }"), [
        "env", "env", "env"
    ]);
    assert_eq!(kinds(rule, "fn f() { let r = rand::thread_rng(); }"), ["random"]);
    assert_eq!(kinds(rule, "fn f() { let m: HashMap<u8, u8> = HashMap::new(); let s = HashSet::with_capacity(4); }"), [
        "hash-seed",
        "hash-seed"
    ]);
    assert_eq!(kinds(rule, "fn f() { std::process::exit(1); let i = process::id(); }"), ["process", "process"]);
    none(rule, &[
        "fn f(clock: &C) { let t = clock.now(); }",
        "fn thread_rng() -> R { R }",
        "fn f() { let m = HashMap::with_hasher(seed); let t = Instant::from(x); }",
        "fn f() { let v = env::var; }",
        "#[cfg(test)]\nmod t {\n    fn h() { let t = Instant::now(); }\n}",
    ]);
}

#[test]
fn left_undone() {
    let rule = Rule::LeftUndone;
    assert_eq!(found(&RuleSet::only(&[rule]), "fn f() { todo!() }", FILE), [("todo", "todo!")]);
    assert_eq!(kinds(rule, "fn f() { unimplemented!(\"later\") }"), ["unimplemented"]);
    assert_eq!(found(&RuleSet::only(&[rule]), "fn f() { let a = dbg!(g(1)); }", FILE), [("dbg", "dbg!")]);
    assert_eq!(found(&RuleSet::only(&[rule]), "fn f() { unreachable!() }", FILE), [(
        "unreachable-no-message",
        "unreachable!()"
    )]);
    none(rule, &[
        "fn f() { unreachable!(\"the length was checked above\") }",
        "fn f() { if todo != 1 {} }",
        "fn f() { x.dbg(); }",
        "fn todo() {}",
        "#[test]\nfn t() { dbg!(x); todo!() }",
    ]);
}

/// How many fixes `rule` offers in `text`.
fn fix_count(rule: Rule, text: &str) -> usize {
    let alloc = blaze_own::testing::heap().alloc();
    let mut out = Findings::new_in(alloc);
    let unit = Unit { text, holes: &[], scope: FILE };
    assert!(lint(&RuleSet::only(&[rule]), &unit, alloc, &mut out).is_ok());
    out.list.iter().map(|f| out.fixes_of(f).len()).sum()
}

#[test]
fn dbg_fix_keeps_the_value() {
    assert_eq!(fixed("fn f() { let a = dbg!(g(1)); }"), "fn f() { let a = g(1); }");
    assert_eq!(fixed("fn f() { let n = dbg!(a.len()?); }"), "fn f() { let n = a.len()?; }");
    assert_eq!(fixed("fn f() { let v = dbg!(vec_in![al; 1]); }"), "fn f() { let v = vec_in![al; 1]; }");
    // A place would be moved by the macro and borrowed without it; an operator would bind to
    // its neighbours; a turbofish is not read: each is found, none fixed.
    for text in [
        "fn f() { dbg!(x); }",
        "fn f() { let a = dbg!(a + b) * 2; }",
        "fn f() { let a = dbg!(s.field); }",
        "fn f() { let a = dbg!(v[0]); }",
        "fn f() { let a = dbg!(f::<u8>()); }",
        "fn f() { let a = dbg!(a, b); }",
    ] {
        assert_eq!(kinds(Rule::LeftUndone, text), ["dbg"], "{text}");
        assert_eq!(fix_count(Rule::LeftUndone, text), 0, "{text}");
    }
}

#[test]
fn unbounded_wait() {
    let rule = Rule::UnboundedWait;
    assert_eq!(found(&RuleSet::only(&[rule]), "fn f() { let m = rx.recv(); }", FILE), [("call", "recv()")]);
    assert_eq!(kinds(rule, "fn f() { let g = m.lock(); h.join(); c.wait(); }"), ["call", "call", "call"]);
    assert_eq!(found(&RuleSet::only(&[rule]), "fn f() {\n    loop {\n        g();\n    }\n}", FILE), [("loop", "loop")]);
    none(rule, &[
        "fn f() { let m = rx.recv_timeout(d); let g = m.lock(deadline); h.join(t); }",
        "fn f() {\n    loop {\n        if g() {\n            break;\n        }\n    }\n}",
        "fn f() -> R {\n    loop {\n        g()?;\n    }\n}",
        "fn f() -> u8 {\n    loop {\n        return 1;\n    }\n}",
        "fn f() { let r = 0..lock(); }",
        "#[test]\nfn t() { rx.recv(); }",
    ]);
}

#[test]
fn ambient_hasher() {
    let rule = Rule::AmbientHasher;
    let only = RuleSet::only(&[rule]);
    assert_eq!(found(&only, "use std::collections::HashMap;", FILE), [("std-table", "collections::HashMap")]);
    assert_eq!(kinds(rule, "use std::collections::{hash_map::RandomState, HashSet};"), ["std-table"]);
    assert_eq!(kinds(rule, "fn f() { let h = std::hash::DefaultHasher::new(); let s: hashbrown::HashSet<u8> = g(); }"), [
        "std-table",
        "std-table"
    ]);
    assert_eq!(found(&only, "fn f(a: A) { let m = HashMap::new_in(a); }", FILE), [("default-hasher", "HashMap::new_in")]);
    assert_eq!(
        kinds(rule, "fn f(a: A) { let s = IndexSet::with_capacity_in(4, a); let b = ByHash::from_iter_in(i, a); }"),
        ["default-hasher", "default-hasher"]
    );
    none(rule, &[
        "fn f(a: A) { let m = Map::new(MapOpts { hasher: FixedState, alloc: a, capacity: None }); }",
        "fn f(a: A) { let m = HashMap::with_hasher_in(FixedState, a); }",
        "use core::hash::{BuildHasher, Hash};",
        "use blaze_hash::hash_map::Entry;",
        "fn f() { let m: my_collections::HashMap = g(); let h = my_hash::RandomState; }",
        "fn new_in(alloc: A) -> Self { g(alloc) }",
        "fn f() { let s = \"std::collections::HashMap HashMap::new_in(a)\"; }",
        "fn f() {\n    // HashMap::new_in(a)\n}",
        "#[test]\nfn t() { let m = HashMap::new_in(a); }",
        "#[cfg(test)]\nmod t {\n    use std::collections::HashMap;\n}",
    ]);
    // The message names the one constructor and its options (D126).
    let alloc = blaze_own::testing::heap().alloc();
    let text = "fn f(a: A) { let m = HashMap::with_capacity_in(8, a); }";
    let mut out = Findings::new_in(alloc);
    let unit = Unit { text, holes: &[], scope: FILE };
    assert!(lint(&only, &unit, alloc, &mut out).is_ok());
    let mut said = StdVec::new();
    for finding in out.list.iter() {
        let mut line = OwnString::new_in(alloc);
        show_message(&only, text, &out, finding, &mut line);
        said.push(line.as_str().to_string());
    }
    assert!(said.iter().any(|s| s.contains("Map::new(MapOpts { hasher: FixedState, alloc, capacity: Some(n) })")), "{said:?}");
}

/// Where a std path and a constructor overlap on one table, one finding: the std path's, the
/// more specific; the constructor's only where `ambient-hasher` does not run.
#[test]
fn a_std_table_path_is_one_finding() {
    let both = RuleSet::only(&[Rule::AmbientHasher, Rule::AmbientRead]);
    assert_eq!(found(&both, "fn f() { let m = std::collections::HashMap::new(); }", FILE), [(
        "std-table",
        "collections::HashMap"
    )]);
    assert_eq!(found(&both, "fn f() { let s = hashbrown::HashSet::with_capacity(4); }", FILE), [(
        "std-table",
        "hashbrown::HashSet"
    )]);
    assert_eq!(found(&both, "fn f() { let r = std::hash::RandomState::new(); }", FILE), [(
        "std-table",
        "hash::RandomState"
    )]);
    assert_eq!(found(&both, "fn f(a: A) { let m = hashbrown::HashMap::new_in(a); }", FILE), [(
        "std-table",
        "hashbrown::HashMap"
    )]);
    // Not one site: each is its own finding.
    assert_eq!(kinds(Rule::AmbientHasher, "use std::collections::HashMap;\nfn f(a: A) { let m = HashMap::new_in(a); }"), [
        "std-table",
        "default-hasher"
    ]);
    // Without ambient-hasher the constructor is ambient-read's finding, as before.
    assert_eq!(kinds(Rule::AmbientRead, "fn f() { let m = std::collections::HashMap::new(); }"), ["hash-seed"]);
}

/// `variant-count`'s findings in `text` (one unit, its whole file): kind, range text, and the
/// text after its fixes are applied.
fn counted(text: &str) -> (StdVec<(&'static str, &str)>, StdString) {
    let only = RuleSet::only(&[Rule::VariantCount]);
    let alloc = blaze_own::testing::heap().alloc();
    let mut out = Findings::new_in(alloc);
    let unit = Unit { text, holes: &[], scope: FILE };
    if let Err(error) = lint(&only, &unit, alloc, &mut out) {
        panic!("lint refused {text:?}: {error:?}");
    }
    let found = out.list.iter().map(|f| (f.template.kind(), f.range.of(text))).collect();
    match apply_all(text, &out, alloc) {
        Ok(new) => (found, new.as_str().to_string()),
        Err(error) => panic!("fixes refused on {text:?}: {error:?}"),
    }
}

/// `variant-count`'s messages in `text`.
fn count_messages(text: &str) -> StdVec<StdString> {
    let only = RuleSet::only(&[Rule::VariantCount]);
    let alloc = blaze_own::testing::heap().alloc();
    let mut out = Findings::new_in(alloc);
    let unit = Unit { text, holes: &[], scope: FILE };
    assert!(lint(&only, &unit, alloc, &mut out).is_ok());
    out.list
        .iter()
        .map(|finding| {
            let mut line = OwnString::new_in(alloc);
            show_message(&only, text, &out, finding, &mut line);
            line.as_str().to_string()
        })
        .collect()
}

const KIND: &str = "enum Kind {\n    A,\n    B(u8),\n    #[default]\n    C { x: u8 } ,\n}\n";

#[test]
fn variant_count_finds_the_three_positions() {
    // (a) a `const`'s whole initializer, fixed.
    let text = format!("{KIND}pub const KINDS: usize = 3;\n");
    let (found, fixed) = counted(&text);
    assert_eq!(found, [("count", "3")]);
    assert_eq!(fixed, format!("{KIND}pub const KINDS: usize = core::mem::variant_count::<Kind>();\n"));
    let text = format!("{KIND}static S: usize = 0x3;\n");
    assert_eq!(counted(&text).0, [("count", "0x3")]);
    // (b) an array type's and a repeat's length, both `usize`, both fixed.
    let text = format!("{KIND}fn f() -> [u8; 3] {{\n    [0; 3_usize]\n}}\n");
    let (found, fixed) = counted(&text);
    assert_eq!(found, [("count", "3"), ("count", "3_usize")]);
    assert_eq!(
        fixed,
        format!("{KIND}fn f() -> [u8; core::mem::variant_count::<Kind>()] {{\n    [0; core::mem::variant_count::<Kind>()]\n}}\n")
    );
    // (c) a range's end: found, not fixed (its type is not known from text).
    let text = format!("{KIND}fn f() {{\n    for i in 0..3 {{}}\n    let s = &v[..0b11];\n}}\n");
    let (found, fixed) = counted(&text);
    assert_eq!(found, [("count", "3"), ("count", "0b11")]);
    assert_eq!(fixed, text);
    // A `u32` constant and a suffixed literal are found, not fixed: the call is a `usize`.
    let text = format!("{KIND}const N: u32 = 3;\nconst M: usize = 3u8;\n");
    let (found, fixed) = counted(&text);
    assert_eq!(found, [("count", "3"), ("count", "3u8")]);
    assert_eq!(fixed, text);
    // Test code is not exempt: a stale count is stale there too.
    let text = format!("{KIND}#[cfg(test)]\nmod t {{\n    use super::*;\n    #[test]\n    fn t() {{ let a = [0; 3]; }}\n}}\n");
    assert_eq!(counted(&text).0, [("count", "3")]);
}

#[test]
fn variant_count_is_silent_off_its_positions() {
    for case in [
        // Two variants: too common a number.
        "enum Two { A, B }\nconst N: usize = 2;",
        // A `cfg`'d variant: the count is not one number.
        "enum K { A, B, #[cfg(x)] C, D }\nconst N: usize = 3;\nconst M: usize = 4;",
        "enum K { A, B, #[cfg_attr(x, cfg(y))] C }\nconst N: usize = 3;",
        // Explicit discriminants are the enum's own literals.
        "enum K { A = 3, B = 4, C = 5 }",
        // A larger expression, either side.
        "enum K { A, B, C }\nconst N: usize = 3 + 1;\nconst M: usize = 1 + 3;\nfn f() { let a = [0; 3 * 2]; let r = 0..3 + 1; }",
        // An inclusive range, a different number, a float, a tuple index, a `let`, a compare.
        "enum K { A, B, C }\nfn f() { let r = 0..=2; let s = 0..=3; let t = 0..4; let x = t.3; let y = 3; }\nconst F: f64 = 3.0;",
        "enum K { A, B, C }\nfn f() { if a == 3 { g(); } b <= 3; }",
        // An attribute, a macro_rules body.
        "enum K { A, B, C }\n#[a([0; 3], x = 3)]\nfn f() {}\nmacro_rules! m { () => { [0; 3] } }",
        // No enum in the file: the file is not lexed.
        "const N: usize = 3;\nfn f() -> [u8; 3] { [0; 3] }",
        // An enum inside an attribute or a macro body is no fact.
        "macro_rules! m { () => { enum K { A, B, C } } }\nconst N: usize = 3;",
    ] {
        assert_eq!(counted(case).0, StdVec::<(&str, &str)>::new(), "variant-count fired on {case:?}");
    }
}

#[test]
fn variant_count_names_each_enum_and_fixes_only_by_a_bare_name() {
    // Two enums of one count: both named, no fix.
    let text = "enum A { X, Y, Z }\nenum B { P, Q, R }\nconst N: usize = 3;\n";
    let (found, fixed) = counted(text);
    assert_eq!(found, [("count-many", "3")]);
    assert_eq!(fixed, text);
    let said = count_messages(text);
    assert!(said[0].starts_with("`3` is the number of variants of `A`, `B`: if it counts one of them"), "{said:?}");
    // The one enum of the count, its message and fix.
    let said = count_messages(&format!("{KIND}pub const KINDS: usize = 3;\n"));
    assert_eq!(said, [
        "`3` is the number of variants of `Kind`: write `core::mem::variant_count::<Kind>()` (feature `variant_count`) so a \
         new variant cannot leave it stale"
    ]);
    // In another module: found, not fixed, the message says why.
    let text = "mod m {\n    pub enum K { A, B, C }\n}\nconst N: usize = 3;\n";
    let (found, fixed) = counted(text);
    assert_eq!(found, [("count", "3")]);
    assert_eq!(fixed, text);
    assert!(count_messages(text)[0].contains("not fixed: `K` is in another module"), "{:?}", count_messages(text));
    // In the same module, nested: fixed.
    let text = "mod m {\n    pub enum K { A, B, C }\n    fn f() -> [u8; 3] { g() }\n}\n";
    assert_eq!(counted(text).1, "mod m {\n    pub enum K { A, B, C }\n    fn f() -> [u8; core::mem::variant_count::<K>()] { g() }\n}\n");
    // In a function body: not reached by a bare name from the module.
    let text = "fn f() {\n    enum K { A, B, C }\n}\nconst N: usize = 3;\n";
    assert_eq!(counted(text), (vec![("count", "3")], text.to_string()));
    // Generic: not fixed.
    let text = "enum K<T> { A(T), B, C }\nconst N: usize = 3;\n";
    assert_eq!(counted(text), (vec![("count", "3")], text.to_string()));
}

/// The enum and the literal are different units: the file's fact joins them, and the literal's
/// unit's key moves when the enum's count does, the enum's own unit's key never.
#[test]
fn variant_count_reads_the_file_and_keys_the_unit() {
    let alloc = blaze_own::testing::heap().alloc();
    let rules = RuleSet::only(&[Rule::VariantCount]);
    let find = |text: &str| {
        let lines = outline(text, Edition::E2024, alloc);
        let units = match Units::plan(lines.items(), text.len(), alloc) {
            Ok(units) => units,
            Err(error) => panic!("{error:?}"),
        };
        let mut out = Findings::new_in(alloc);
        let mut scratch = blaze_own::AVec::new_in(alloc);
        let mut by_unit = StdVec::new();
        let linted = units.lint_all(&rules, text, FILE, alloc, &mut out, &mut scratch, |done| by_unit.push(done.findings.len()));
        assert!(linted.is_ok(), "{linted:?}");
        let variants = match Variants::gather(text, alloc) {
            Ok(variants) => variants,
            Err(error) => panic!("{error:?}"),
        };
        // The constant's unit (its text starts the line `pub const`), and every other's key.
        let Some(at) = (0..units.list.len()).find(|u| text[units.list[*u].base as usize..].starts_with("pub const")) else {
            panic!("no unit is the constant in {text:?}")
        };
        let key = |u: usize| {
            let plan = units.list[u];
            variants.unit_key(plan.base, plan.end - plan.base, units.holes_of(u))
        };
        let others_none = (0..units.list.len()).filter(|u| *u != at).all(|u| key(u).is_none());
        let found: StdVec<&'static str> = out.list.iter().map(|f| f.template.kind()).collect();
        (found, by_unit[at], key(at), others_none)
    };
    let three = "enum Kind { A, B, C }\npub const KINDS: usize = 3;\n";
    let (found, in_const, key, others_none) = find(three);
    // The finding is the constant's unit's; no other unit reads the enums.
    assert_eq!(found, ["count"]);
    assert_eq!(in_const, 1);
    assert!(key.is_some() && others_none, "{key:?}");
    // A fourth variant: the constant's unit's key moves (its text did not), and it finds nothing.
    let (found, in_const, moved, _) = find("enum Kind { A, B, C, D }\npub const KINDS: usize = 3;\n");
    assert_eq!((found.len(), in_const), (0, 0));
    assert_eq!(moved, None);
    // A renamed enum moves it too; an edit elsewhere that keeps the enums leaves it.
    let (_, _, renamed, _) = find("enum Kinds { A, B, C }\npub const KINDS: usize = 3;\n");
    assert!(renamed.is_some() && renamed != key);
    let (_, _, kept, _) = find("enum Kind { A, B, C }\n\n\npub const KINDS: usize = 3;\n");
    assert_eq!(kept, key);
}

#[test]
fn variant_count_runs_from_suggest_up() {
    let rule = Rule::VariantCount;
    assert!(!Step::Rust.rules().runs(rule));
    for step in [Step::Suggest, Step::Edition, Step::Strict] {
        assert!(step.rules().runs(rule), "{} does not run {}", step.name(), rule.name());
    }
    assert_eq!(Rule::from_name("variant-count"), Some(rule));
    assert_eq!(Template::VariantCount.rule(), rule);
    assert_eq!(Template::VariantCountMany.rule(), rule);
    assert_eq!(Template::HasherDefault.rule(), Rule::AmbientHasher);
}

/// The trie's size as the matcher's module doc states it: change both together.
#[test]
fn the_rule_trie_has_the_nodes_its_doc_says() {
    assert_eq!(nodes_for(&PATTERNS), 292);
}

#[test]
fn new_rules_are_off_until_named() {
    for rule in [
        Rule::AmbientHeap,
        Rule::NoAsync,
        Rule::AmbientRead,
        Rule::LeftUndone,
        Rule::UnboundedWait,
        Rule::AmbientHasher,
        Rule::ProcessStatic,
        Rule::SansIoCore,
        Rule::LockWithoutReason,
        Rule::DoubleIndirectionArc,
        Rule::RawSandboxExec,
        Rule::MutRef,
        Rule::MagicSliceLen,
        Rule::BoolAsInt,
        Rule::ManualSliceIdioms,
        Rule::TestAmbientLeak,
        Rule::MutForUniqBorrow,
        Rule::IoRef,
        Rule::UnusedImport,
        Rule::UnusedMut,
        Rule::HeapLeak,
    ] {
        assert!(!Step::Strict.rules().runs(rule), "{} runs in strict", rule.name());
        assert!(Step::Strict.rules().with(rule).runs(rule));
        assert_eq!(Rule::from_name(rule.name()), Some(rule));
        assert_ne!(Step::Strict.rules().with(rule).id(), Step::Strict.rules().id());
    }
    assert!(Rule::HOUSE.iter().all(|rule| Step::Strict.rules().runs(*rule)));
    assert_eq!(Rule::from_name("no_async"), None);
    assert_eq!(Rule::ALL.len(), Rule::COUNT);
    assert!(Rule::ALL.iter().enumerate().all(|(at, rule)| rule.id() as usize == at + 1));
}

#[test]
fn process_static() {
    let rule = Rule::ProcessStatic;
    let only = RuleSet::only(&[rule]);
    assert_eq!(found(&only, "thread_local! { static K: u8 = 0; }", FILE), [("thread-local", "thread_local!")]);
    assert_eq!(found(&only, "static mut N: u32 = 0;", FILE), [("static-mut", "static mut")]);
    assert_eq!(found(&only, "static N: AtomicU64 = AtomicU64::new(0);", FILE), [("static-state", "static N: AtomicU64")]);
    assert_eq!(found(&only, "pub static T: std::sync::Mutex<u8> = Mutex::new(0);", FILE), [(
        "static-state",
        "static T: std::sync::Mutex"
    )]);
    assert_eq!(kinds(rule, "static S: OnceLock<u8> = OnceLock::new();"), ["static-state"]);
    assert_eq!(kinds(rule, "static C: [RefCell<u8>; 2] = [RefCell::new(0), RefCell::new(0)];"), ["static-state"]);
    none(rule, &[
        "static NAME: &str = \"x\";",
        "fn f(x: &'static str) -> &'static str { x }",
        "const C: AtomicU8 = AtomicU8::new(0);",
        "static N: u64 = AtomicU64::new(0).into_inner();",
        // A type of the crate's own whose name says no state: text cannot see its state (the
        // rule says less). This case was `SpinLock`, written when engine.rs's
        // `global_allocator_above` exempted any static under the attribute; D173 deleted that
        // exemption for the `ALLOWED_STATICS` table (engine.rs `process_static`'s doc, and
        // `allowed_statics_by_crate_and_name`: "the allocator's attribute alone allows nothing"),
        // and a `..Lock` name is state as `stateful` reads it, so `SpinLock` fires by the rule's
        // doc and the two cases contradicted each other; this one was the stale one.
        "#[global_allocator]\nstatic A: Spin = Spin::new();",
        "fn f() { let s = \"static mut x; thread_local!\"; }",
        "fn f() {\n    // static mut N: u8 = 0;\n}",
        "#[test]\nfn t() { static N: AtomicU8 = AtomicU8::new(0); }",
        "#[cfg(test)]\nmod t {\n    thread_local! { static K: u8 = 0; }\n}",
    ]);
    // The message names the type that holds the state.
    let alloc = blaze_own::testing::heap().alloc();
    let text = "static N: AtomicU64 = AtomicU64::new(0);";
    let mut out = Findings::new_in(alloc);
    let unit = Unit { text, holes: &[], scope: FILE };
    assert!(lint(&only, &unit, alloc, &mut out).is_ok());
    let mut said = StdVec::new();
    for finding in out.list.iter() {
        let mut line = OwnString::new_in(alloc);
        show_message(&only, text, &out, finding, &mut line);
        said.push(line.as_str().to_string());
    }
    assert!(said.len() == 1 && said[0].contains("holds `AtomicU64`") && said[0].contains("C3"), "{said:?}");
}

/// Expected: "test tests::allowed_statics_by_crate_and_name ... ok".
#[test]
fn allowed_statics_by_crate_and_name() {
    let only = RuleSet::only(&[Rule::ProcessStatic]);
    // The table is sorted by crate then name, one row per (crate, name).
    assert!(ALLOWED_STATICS.windows(2).all(|w| (w[0].krate, w[0].name) < (w[1].krate, w[1].name)));
    let core = FILE.with_file_facts(OwnHeads::NONE, statics_of_path("crates/blaze-core/src/run_signals.rs"));
    let trace = FILE.with_file_facts(OwnHeads::NONE, statics_of_path("crates/blaze-trace/src/record.rs"));
    let cli = FILE.with_file_facts(OwnHeads::NONE, statics_of_path("crates/blaze-cli/src/main.rs"));
    let none = StdVec::<(&str, &str)>::new();
    // One of the three, in its crate: no finding.
    assert_eq!(found(&only, "static ASKED: Requests = Requests::new();\nstatic HELD: AtomicBool = AtomicBool::new(false);", core), none);
    assert_eq!(found(&only, "pub static PROFILE: Profile = Profile::new();", trace), none);
    assert_eq!(found(&only, "#[global_allocator]\nstatic HEAP: Counted<AtomicU64> = Counted::new();", cli), none);
    // A `thread_local!` whose every static is a row is none either (the rule reads names).
    assert_eq!(found(&only, "std::thread_local! {\n    static HEAP: Cell<u8> = const { Cell::new(0) };\n}", cli), none);
    // A fourth in the same crate, or a row's name in another crate, is refused.
    assert_eq!(found(&only, "static SPARE: AtomicU64 = AtomicU64::new(0);", core), [("static-state", "static SPARE: AtomicU64")]);
    assert_eq!(found(&only, "static HELD: AtomicBool = AtomicBool::new(false);", trace), [("static-state", "static HELD: AtomicBool")]);
    assert_eq!(
        found(&only, "thread_local! {\n    static HEAP: Cell<u8> = Cell::new(0);\n    static MORE: Cell<u8> = Cell::new(0);\n}", cli),
        [("thread-local", "thread_local!"), ("static-state", "static MORE: Cell")]
    );
    // The allocator's attribute alone allows nothing: the table decides (the foreign heap is
    // blaze-cli's alone, D173).
    assert_eq!(found(&only, "#[global_allocator]\nstatic HEAP: Counted<AtomicU64> = Counted::new();", FILE).len(), 1);
    // A path outside `crates/`, or a crate whose name only starts like a row's, has no rows.
    assert_eq!(statics_of_path("scripts/src/x.rs"), 0);
    assert_eq!(statics_of_path("crates/blaze-core-x/src/lib.rs"), 0);
    // The rows are in the key: a file of a crate with rows is not one without.
    assert_ne!(core.key(), FILE.key());
}

/// Expected: "test tests::own_module_heads_are_not_std ... ok".
#[test]
fn own_module_heads_are_not_std() {
    let only = RuleSet::only(&[Rule::SansIoCore]);
    let in_file = |text: &'static str| found(&only, text, CORE.with_file_facts(OwnHeads::read(text), 0));
    let none = StdVec::<(&str, &str)>::new();
    // Declared or imported from the crate: the head is the crate's module.
    assert_eq!(in_file("mod io;\nfn f() { let p = io::Plan::new(); }"), none);
    assert_eq!(in_file("use crate::machine::{fs, other};\nfn f() { fs::step(); }"), none);
    assert_eq!(in_file("use super::process as process;\nfn f() { process::next(); }"), none);
    // Imported from std, or not named at all: std's, reported.
    assert_eq!(in_file("use std::io;\nfn f() { io::stdout(); }"), [("io", "io"), ("io", "io")]);
    assert_eq!(in_file("fn f() { io::stdout(); }"), [("io", "io")]);
    // Making `io` its own does not make `fs` its own, nor `std::io` the crate's.
    assert_eq!(in_file("mod io;\nfn f() { fs::read(p); std::io::stdout(); }"), [("io", "fs"), ("io", "io")]);
    assert_eq!(OwnHeads::read("mod io;\nuse crate::fs;\nuse std::env;\n").0, 0b1001);
    // The heads are in the key.
    assert_ne!(CORE.with_file_facts(OwnHeads(1), 0).key(), CORE.key());
}

/// A file of a sans-IO core: what the driver gives it when its crate root says
/// `#![blaze::sans_io]`.
const CORE: Scope = FILE.in_core();

#[test]
fn sans_io_core() {
    let rule = Rule::SansIoCore;
    let only = RuleSet::only(&[rule]);
    let core = |text| found(&only, text, CORE);
    assert_eq!(core("use std::fs;"), [("io", "fs")]);
    assert_eq!(core("fn f(p: &P) { let b = fs::read(p); }"), [("io", "fs")]);
    assert_eq!(core("fn f() { std::thread::sleep(d); }"), [("io", "thread")]);
    assert_eq!(core("fn f() { process::exit(1) }"), [("io", "process")]);
    assert_eq!(core("fn f() { let v = env::var(\"X\"); }"), [("io", "env")]);
    assert_eq!(core("fn f(p: &P) { let f = File::open(p); }"), [("io", "File::open")]);
    assert_eq!(core("fn f() { unsafe { libc::write(1, p, n) }; }"), [("io", "libc")]);
    assert_eq!(core("fn f() { let s = TcpStream::connect(a); }"), [("io", "TcpStream")]);
    assert_eq!(core("fn f() { let t = Instant::now(); let w = SystemTime::now(); }"), [
        ("clock", "Instant::now"),
        ("clock", "SystemTime::now")
    ]);
    assert_eq!(core("fn f() { rx.recv(); h.join(); m.lock(); c.wait(g); block_on(fut); }"), [
        ("blocking", "recv"),
        ("blocking", "join"),
        ("blocking", "lock"),
        ("blocking", "wait"),
        ("blocking", "block_on")
    ]);
    assert_eq!(core("fn f() { println!(\"x\"); dbg!(y); }"), [("print", "println!"), ("print", "dbg!")]);
    // Look-alikes in a core: none is IO.
    for text in [
        "fn f() { let s = parts.join(\",\"); }",
        "fn f() { let p = crate::io::Plan::new(); }",
        "fn f() { let p = blaze_fs::Path::new(); }",
        "fn recv(&mut self) {}",
        "fn f(file: File) -> u8 { file.len() }",
        "fn f() { let n = self.io.len(); }",
        "fn f() { let s = \"fs::read(p) Instant::now()\"; }",
        "fn f() {\n    // println!(\"x\");\n}",
        "fn f(clock: &C) { let t = clock.now(); }",
        "#[test]\nfn t() { std::fs::read(p); }",
        "#[cfg(test)]\nmod t {\n    fn t() { println!(\"x\"); }\n}",
    ] {
        assert_eq!(core(text), StdVec::<(&str, &str)>::new(), "sans-io-core fired on {text:?}");
    }
    // Outside a core the same code is no finding: the rule reads the scope's mark.
    for text in ["use std::fs;", "fn f() { rx.recv(); println!(\"x\"); let t = Instant::now(); }"] {
        assert_eq!(found(&only, text, FILE), StdVec::<(&str, &str)>::new(), "fired outside a core on {text:?}");
    }
    // A child item of a core is in the core too.
    let text = "mod m {\n    fn g() {}\n}";
    let start = text.find("fn g").map_or(0, |at| at as u32);
    let alloc = blaze_own::testing::heap().alloc();
    let mut out = Findings::new_in(alloc);
    let holes = [Span::new(start, start + "fn g() {}".len() as u32)];
    let unit = Unit { text, holes: &holes, scope: CORE };
    match lint(&only, &unit, alloc, &mut out) {
        Ok(linted) => {
            assert_eq!(linted.children.len(), 1);
            assert!(linted.children.iter().all(|child| child.sans_io), "a child left the core");
        }
        Err(error) => panic!("lint refused {text:?}: {error:?}"),
    }
    assert_ne!(CORE.key(), FILE.key());
}

#[test]
fn root_marks_and_lint_names() {
    let text = "//! A core.\n#![no_std]\n#![blaze::sans_io]\n// a comment between\n#![deny(blaze::std_use, blaze::process_static, unused, reason = \"C7\")]\n#![forbid(blaze::implicit_allocator)]\n\nuse a::b;\n#![blaze::driver]\n";
    let marks = RootMarks::read(text);
    assert!(marks.no_std && marks.sans_io && !marks.driver, "{marks:?}");
    for rule in [Rule::Ambient, Rule::ProcessStatic, Rule::AmbientHeap] {
        assert!(marks.denies(rule), "{} not denied", rule.name());
    }
    assert!(!marks.denies(Rule::SansIoCore) && !marks.denies(Rule::Casts));
    let rules = marks.rules(Step::Strict.rules());
    for rule in [Rule::Ambient, Rule::ProcessStatic, Rule::AmbientHeap, Rule::SansIoCore] {
        assert!(rules.runs(rule), "{} does not run", rule.name());
    }
    assert!(!rules.runs(Rule::NoAsync));
    let levels = marks.levels(Step::Strict.levels());
    assert_eq!(levels.of(Rule::Ambient), Level::Error);
    assert_eq!(levels.of(Rule::Casts), Level::Advice);
    assert!(marks.scope(FILE).sans_io && !RootMarks::NONE.scope(FILE).sans_io);
    let driver = RootMarks::read("#![blaze::driver]\nfn main() {}");
    assert!(driver.driver && !driver.sans_io);
    // Only the top: a mark after an item is none.
    assert_eq!(RootMarks::read("fn f() {}\n#![no_std]"), RootMarks::NONE);
    // An attribute this does not know, or a bare `blaze`, marks nothing and stops nothing.
    assert!(RootMarks::read("#![feature(x)]\n#![blaze]\n#![no_std]").no_std);
    assert!(!RootMarks::read("#![blaze::sans_io_extra]").sans_io);
    // A module file's own mark joins its crate's.
    assert!(RootMarks::NONE.join(RootMarks::read("#![blaze::sans_io]")).sans_io);
    assert!(!RootMarks::NONE.join(RootMarks::read("#![no_std]")).no_std);
    for (lint, rule) in [
        ("std_use", Rule::Ambient),
        ("implicit_allocator", Rule::AmbientHeap),
        ("process_static", Rule::ProcessStatic),
        ("sans_io_core", Rule::SansIoCore),
        ("lock_without_reason", Rule::LockWithoutReason),
    ] {
        assert_eq!(Rule::from_lint(lint), Some(rule));
        assert_eq!(rule.lint(), Some(lint));
    }
    assert_eq!(Rule::from_lint("sans-io-core"), None);
    assert_eq!(Rule::from_name("sans-io-core"), Some(Rule::SansIoCore));
    assert_eq!(Template::SansIoPrint.rule(), Rule::SansIoCore);
    assert_eq!(Template::StaticState.rule(), Rule::ProcessStatic);
    assert_eq!(Template::VariantCountMany.rule(), Rule::VariantCount);
    assert_eq!(Template::LockNoReason.rule(), Rule::LockWithoutReason);
    assert_eq!(Template::LockBadReason.rule(), Rule::LockWithoutReason);
    assert_eq!(Rule::from_name("lock-without-reason"), Some(Rule::LockWithoutReason));
    let denies = RootMarks::read("#![deny(blaze::lock_without_reason)]\n");
    assert!(denies.denies(Rule::LockWithoutReason) && denies.rules(Step::Rust.rules()).runs(Rule::LockWithoutReason));
}

/// Expected: "test tests::lock_without_reason ... ok".
#[test]
fn lock_without_reason() {
    let rule = Rule::LockWithoutReason;
    let only = RuleSet::only(&[rule]);
    let nothing = StdVec::<(&str, &str)>::new();
    // Each of D179's constructors, with no reason, is one finding over its path.
    assert_eq!(found(&only, "fn f() { let l = Lock::new(\"jobs\", 0); }", FILE), [("no-reason", "Lock::new")]);
    assert_eq!(kinds(rule, "fn f() { let a = Lock::unrecorded(0); let b = Lock::probed(p, 0); }"), ["no-reason", "no-reason"]);
    assert_eq!(kinds(rule, "fn f() { let a = ReadLock::new(n, 0); let b = ReadLock::unrecorded(0); }"), ["no-reason", "no-reason"]);
    assert_eq!(kinds(rule, "fn f() { let a = RwLock::new(0); let b = RankedLock::new(r, 0); }"), ["no-reason", "no-reason"]);
    assert_eq!(found(&only, "fn f(a: A) { let m = SharedMap::new_in(a); }", FILE), [("no-reason", "SharedMap::new_in")]);
    assert_eq!(found(&only, "fn f() { let l = bstd::sync::Lock::<u8>::new(0); }", FILE), [("no-reason", "Lock::<u8>::new")]);
    assert_eq!(found(&only, "static L: Lock<u8> = Lock::new(\"l\", 0);", FILE), [("no-reason", "Lock::new")]);
    assert_eq!(kinds(rule, "fn f() { let v = xs.map(RwLock::new); }"), ["no-reason"]);
    // Not a constructor of D179's list, not the type, a method, a `use`, a type's own path, a
    // string, a comment, test code: none.
    none(rule, &[
        "fn f() { let l = Lock::default(); let m = SharedMap::new(); let r = ReadLock::probed(p); }",
        "fn f() { let l = Mutex::new(0); let o = OnceLock::new(); }",
        "fn f() { let g = l.lock(); let n = x.Lock::new(); }",
        "use bstd::sync::Lock::new;",
        "fn f() { let l = Outer::Lock::new(0); }",
        "fn f(l: &Lock<u8>) -> &Lock<u8> { l }",
        "fn f() { let s = \"Lock::new(0)\"; }",
        "fn f() {\n    // Lock::new(0)\n}",
        "#[test]\nfn t() { let l = Lock::new(\"t\", 0); }",
        "#[cfg(test)]\nmod t {\n    fn t() { let l = RwLock::new(0); }\n}",
    ]);
    // The reason on the statement, the `static`, a struct literal's field, or the expression
    // statement: that construction has it, and the next one does not.
    let reason = "#[blaze::lock_exception(tried = \"published\", why = \"the queue is written by every worker\")]";
    let stmt = format!("fn f() {{\n    {reason}\n    let l = Lock::new(\"q\", 0);\n    let m = Lock::new(\"m\", 0);\n}}");
    assert_eq!(found(&only, &stmt, FILE), [("no-reason", "Lock::new")]);
    assert_eq!(found(&only, &stmt, FILE).len(), 1);
    let stat = format!("{reason}\nstatic L: Lock<u8> = Lock::new(\"l\", 0);");
    assert_eq!(found(&only, &stat, FILE), nothing);
    let field = format!("fn f() -> S {{\n    S {{\n        {reason}\n        jobs: RwLock::new(0),\n        done: RwLock::new(0),\n    }}\n}}");
    assert_eq!(kinds(rule, &field), ["no-reason"]);
    let branch = format!("fn f() {{\n    {reason}\n    if c {{ a = Lock::new(\"a\", 0); }} else {{ a = Lock::new(\"b\", 0); }}\n    b = Lock::new(\"c\", 0);\n}}");
    assert_eq!(kinds(rule, &branch), ["no-reason"]);
    // Each shape, either order, a trailing comma, a raw string: well formed.
    for attr in [
        "#[blaze::lock_exception(tried = \"gathered\", why = \"x\")]",
        "#[blaze::lock_exception(why = \"x\", tried = \"claimed\",)]",
        "#[blaze::lock_exception(tried = \"published\", why = r#\"the \"why\"\"#)]",
    ] {
        let text = format!("fn f() {{\n    {attr}\n    let l = Lock::new(\"l\", 0);\n}}");
        assert_eq!(found(&only, &text, FILE), nothing, "{attr}");
    }
    // A shape that is not one of the three, a `why` missing or empty, a key twice or another
    // key, no arguments: one finding over the attribute, and the construction is covered.
    for attr in [
        "#[blaze::lock_exception(tried = \"locked\", why = \"x\")]",
        "#[blaze::lock_exception(tried = \"published\")]",
        "#[blaze::lock_exception(tried = \"published\", why = \"  \")]",
        "#[blaze::lock_exception(tried = \"published\", tried = \"claimed\", why = \"x\")]",
        "#[blaze::lock_exception(tried = \"published\", why = \"x\", owner = \"me\")]",
        "#[blaze::lock_exception(tried = published, why = \"x\")]",
        "#[blaze::lock_exception]",
    ] {
        let text = format!("fn f() {{\n    {attr}\n    let l = Lock::new(\"l\", 0);\n}}");
        assert_eq!(found(&only, &text, FILE), [("bad-reason", attr)], "{attr}");
    }
    // Another `blaze::` attribute, or the same name under another tool, is no reason.
    let other = "fn f() {\n    #[blaze::lock_checked]\n    let l = Lock::new(\"l\", 0);\n}";
    assert_eq!(kinds(rule, other), ["no-reason"]);
    // On an item: its child items have the reason too (the scope's `lock_reason`, in the key),
    // and a sibling after it does not.
    let text = format!("{reason}\nfn new() {{}}\nfn other() {{}}");
    let first = text.find("fn new").map_or(0, |at| at as u32);
    let second = text.find("fn other").map_or(0, |at| at as u32);
    let alloc = blaze_own::testing::heap().alloc();
    let mut out = Findings::new_in(alloc);
    let holes = [Span::new(first, first + "fn new() {}".len() as u32), Span::new(second, second + "fn other() {}".len() as u32)];
    let unit = Unit { text: &text, holes: &holes, scope: FILE };
    match lint(&only, &unit, alloc, &mut out) {
        Ok(linted) => {
            assert_eq!(linted.children.len(), 2);
            assert!(linted.children[0].lock_reason && !linted.children[1].lock_reason, "the reason's reach");
        }
        Err(error) => panic!("lint refused {text:?}: {error:?}"),
    }
    let covered = Scope { lock_reason: true, ..FILE };
    assert_eq!(found(&only, "fn new() { let l = Lock::new(\"l\", 0); }", covered), nothing);
    assert_ne!(covered.key(), FILE.key());
    // The message names the lock by its first argument's string, else by the path, and D179.
    let text = "fn f() { let l = Lock::new(\"jobs\", 0); let m = RwLock::new(0); }";
    let mut out = Findings::new_in(alloc);
    let unit = Unit { text, holes: &[], scope: FILE };
    assert!(lint(&only, &unit, alloc, &mut out).is_ok());
    let mut said = StdVec::new();
    for finding in out.list.iter() {
        let mut line = OwnString::new_in(alloc);
        show_message(&only, text, &out, finding, &mut line);
        said.push(line.as_str().to_string());
    }
    assert!(
        said.len() == 2
            && said[0].starts_with("`jobs`: a lock with no reason: say which D179 shape it could not be")
            && said[1].starts_with("`RwLock::new`: a lock with no reason"),
        "{said:?}"
    );
}

/// What `rules` finds in `text` with `holes` left out, as kinds and range texts.
fn found_holed<'t>(rules: &RuleSet, text: &'t str, holes: &[Span]) -> StdVec<(&'static str, &'t str)> {
    let alloc = blaze_own::testing::heap().alloc();
    let mut out = Findings::new_in(alloc);
    let unit = Unit { text, holes, scope: FILE };
    match lint(rules, &unit, alloc, &mut out) {
        Ok(_) => out.list.iter().map(|f| (f.template.kind(), f.range.of(text))).collect(),
        Err(error) => panic!("lint refused {text:?}: {error:?}"),
    }
}

#[test]
fn sequences_skip_comments_but_not_holes() {
    let read = RuleSet::only(&[Rule::AmbientRead]);
    // Comments and whitespace are not tokens: a sequence matches across them.
    let split = "fn f() {\n    let t = Instant /* the clock */ :: // now\n        now();\n}";
    assert_eq!(found(&read, split, FILE), [("clock", "Instant /* the clock */ :: // now\n        now")]);
    // A child item is a token no sequence has: `todo` and `!` on either side of one are not
    // `todo !`. With the same bytes as spaces instead of a hole, they are.
    let undone = RuleSet::only(&[Rule::LeftUndone]);
    let text = "fn f() {\n    todo\n    fn g() {}\n    !()\n}";
    let start = text.find("fn g").map_or(0, |at| at as u32);
    let hole = Span::new(start, start + "fn g() {}".len() as u32);
    assert_eq!(found_holed(&undone, text, &[hole]), StdVec::<(&str, &str)>::new());
    let blank = text.replace("fn g() {}", "         ");
    assert_eq!(found(&undone, &blank, FILE).len(), 1);
}

// ---- the matcher against a scan per sequence ----------------------------------------------

/// Overlapping sequences over the symbols 0 to 3 (4 is in none, so it ends every match): one a
/// prefix of another, one starting inside another, two equal, a run of one symbol, one as long
/// as a sequence may be.
const TEST_PATTERNS: [Pattern; 8] = [
    Pattern::of(&[0, 1]),
    Pattern::of(&[0, 0, 1]),
    Pattern::of(&[0, 1, 2]),
    Pattern::of(&[1, 0]),
    Pattern::of(&[2]),
    Pattern::of(&[0, 1]),
    Pattern::of(&[3, 3, 3]),
    Pattern::of(&[0, 1, 2, 0, 1, 2, 0, 1]),
];
static TEST_TRIE: Trie<{ nodes_for(&TEST_PATTERNS) }, 5, 8> = Trie::build(&TEST_PATTERNS);

/// A bug the matcher's control plants.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MatchPlant {
    None,
    /// The live states dropped before each symbol: only one-symbol sequences are found.
    RootOnly,
}

/// Every (sequence, first symbol, last symbol) the matcher finds in `syms`, sorted.
fn matched<const N: usize, const S: usize, const Q: usize>(
    trie: &Trie<N, S, Q>,
    syms: &[u8],
    plant: MatchPlant,
) -> StdVec<(u16, u32, u32)> {
    let mut live = Live::new();
    let mut got = StdVec::new();
    for (at, sym) in syms.iter().enumerate() {
        if plant == MatchPlant::RootOnly {
            live = Live::new();
        }
        live.step(trie, *sym, at as u32);
        for state in 0..live.len() {
            let (node, start) = live.state(state);
            let mut seq = trie.first_end(node);
            while seq != NO_SEQ {
                got.push((seq, start, at as u32));
                seq = trie.next_end(seq);
            }
        }
    }
    got.sort();
    got
}

/// The oracle: each sequence compared at each position on its own.
fn scanned(patterns: &[Pattern], syms: &[u8]) -> StdVec<(u16, u32, u32)> {
    let mut got = StdVec::new();
    for (row, pattern) in patterns.iter().enumerate() {
        let pattern = pattern.as_slice();
        for start in 0..syms.len() {
            if syms[start..].starts_with(pattern) {
                got.push((row as u16, start as u32, (start + pattern.len() - 1) as u32));
            }
        }
    }
    got.sort();
    got
}

fn match_case(tape: &Tape, plant: MatchPlant) -> Result<(), Broke> {
    let syms: StdVec<u8> = tape.vec(0..=48, "symbols", |t| t.int_in(0u8..=4, "symbol"));
    let (want, got) = (scanned(&TEST_PATTERNS, &syms), matched(&TEST_TRIE, &syms, plant));
    if want != got {
        return Err(Broke::Oracle(format!("the matcher differs from the scan over {syms:?}:\nscan    {want:?}\nmatcher {got:?}")));
    }
    Ok(())
}

/// Passes printing nothing; a failure prints the symbols and both lists.
#[test]
fn matcher_equals_a_scan_per_sequence() {
    blaze_sim::check(Config::cases(500), |tape| match_case(tape, MatchPlant::None));
}

/// Passes when the scan catches a matcher that keeps no live state ("plant RootOnly: caught at
/// case k of n" on standard error).
#[test]
fn control_matcher_root_only() {
    blaze_sim::control(Config::cases(500), MatchPlant::None, MatchPlant::RootOnly, "the matcher differs", match_case);
}

/// The rules' own table: streams made of whole sequences, their prefixes and `OTHER`, so the
/// long ones and the shared prefixes are reached.
#[test]
fn rule_trie_equals_a_scan_per_sequence() {
    blaze_sim::check(Config::cases(500), |tape| {
        let pieces = tape.vec(0..=12, "pieces", |t| {
            (t.below(PATTERNS.len() as u64, "pattern") as usize, t.below(LIVE as u64 + 1, "cut") as usize, t.coin("other"))
        });
        let mut syms = StdVec::new();
        for (row, cut, other) in pieces {
            let pattern = PATTERNS[row].as_slice();
            syms.extend_from_slice(&pattern[..cut.min(pattern.len())]);
            if other {
                syms.push(OTHER);
            }
        }
        let (want, got) = (scanned(&PATTERNS, &syms), matched(&TRIE, &syms, MatchPlant::None));
        if want != got {
            return Err(Broke::Oracle(format!("the rules' trie differs from the scan over {syms:?}:\nscan    {want:?}\ntrie    {got:?}")));
        }
        Ok(())
    });
}

/// The fixes of `text`'s findings applied.
fn fixed(text: &str) -> StdString {
    fixed_rules(&RuleSet::ALL, text)
}

fn fixed_rules(rules: &RuleSet, text: &str) -> StdString {
    let alloc = blaze_own::testing::heap().alloc();
    let mut out = Findings::new_in(alloc);
    let unit = Unit { text, holes: &[], scope: FILE };
    assert!(lint(rules, &unit, alloc, &mut out).is_ok());
    match apply_all(text, &out, alloc) {
        Ok(new) => new.as_str().to_string(),
        Err(error) => panic!("fixes refused on {text:?}: {error:?}"),
    }
}

#[test]
fn renamed() {
    let rule = Rule::Renamed;
    let only = RuleSet::only(&[rule]);
    let typed = "fn f(al: A) {\n    let mut v: AVec<'_, u8> = AVec::new_in(al);\n    v.insert(0, 1);\n}";
    assert_eq!(kinds(rule, typed), ["fixed"]);
    assert_eq!(fixed(typed), "fn f(al: A) {\n    let mut v: AVec<'_, u8> = AVec::new_in(al);\n    v.insert_shift(0, 1);\n}");
    let param = "fn f(v: &mut AVec<'_, u8>) -> bool {\n    v.contains(&1)\n}";
    assert_eq!(fixed_rules(&only, param), "fn f(v: &mut AVec<'_, u8>) -> bool {\n    v.scan_for(&1).is_some()\n}");
    assert_eq!(fixed(param), "fn f(v: &uniq AVec<'_, u8>) -> bool {\n    v.scan_for(&1).is_some()\n}");
    let init = "fn f(al: A) {\n    let mut d = ADeque::new_in(al);\n    d.remove(0);\n}";
    assert_eq!(fixed(init), "fn f(al: A) {\n    let mut d = ADeque::new_in(al);\n    d.remove_shift(0);\n}");
    assert_eq!(fixed_rules(&only, "fn f() { AVec::swap_remove(&mut v, 0); }"), "fn f() { AVec::remove_swap(&mut v, 0); }");
    assert_eq!(fixed("fn f() { AVec::swap_remove(&mut v, 0); }"), "fn f() { AVec::remove_swap(&uniq v, 0); }");
    assert_eq!(kinds(rule, "fn f(x: &mut X) {\n    x.insert_many(0, &[1]);\n}"), ["for-a-person"]);
    none(rule, &[
        "fn f(m: &mut HashMap<u8, u8>) {\n    m.insert(1, 2);\n}",
        "fn f(al: A) {\n    let v = AVec::new_in(al);\n    let v = HashMap::new();\n    v.insert(1, 2);\n}",
        "fn f(v: &mut AVec<'_, u8>) {\n    v.items.insert(1);\n}",
        "fn f(v: &mut AVec<'_, u8>) {\n    let s = \"v.insert(0)\";\n}",
        "fn f(v: &mut AVec<'_, u8>) {\n    v.insert_shift(0, 1);\n}",
    ]);
}

#[test]
fn apply_refuses_overlaps() {
    let alloc = blaze_own::testing::heap().alloc();
    let text = "abcdef";
    let first = Span::new(1, 3);
    let second = Span::new(2, 4);
    assert_eq!(apply(text, &[(second, "y"), (first, "x")], alloc).map(|s| s.as_str().to_string()), Err(FixError::Overlap {
        first,
        second
    }));
    let point = Span::new(3, 3);
    assert_eq!(apply(text, &[(point, "x"), (point, "y")], alloc).map(|_| ()), Err(FixError::Overlap {
        first: point,
        second: point
    }));
    assert_eq!(apply(text, &[(Span::new(4, 9), "x")], alloc).map(|_| ()), Err(FixError::OutOfRange {
        range: Span::new(4, 9)
    }));
    assert_eq!(apply("é", &[(Span::new(1, 2), "x")], alloc).map(|_| ()), Err(FixError::NotCharBoundary { at: 1 }));
    let made = apply(text, &[(Span::new(3, 3), "-"), (Span::new(0, 1), "A"), (Span::new(5, 6), "")], alloc);
    assert_eq!(made.map(|s| s.as_str().to_string()), Ok("Abc-de".to_string()));
}

#[test]
fn rule_set_identity() {
    assert_eq!(RuleSet::ALL.id(), RuleSet::ALL.id());
    assert_ne!(RuleSet::ALL.id(), RuleSet::only(&[Rule::Casts]).id());
    assert_ne!(RuleSet::only(&[Rule::Casts]).id(), RuleSet::only(&[Rule::Clones]).id());
    let mut scope = FILE;
    let key = scope.key();
    scope.context.recursion_above = 2;
    assert_ne!(scope.key(), key);
    // The core bit is its own: no other field's bits reach it (recursion_above's top bit is 34).
    let mut deep = FILE;
    deep.context.recursion_above = u8::MAX;
    assert_ne!(deep.in_core().key(), deep.key());
    assert_eq!(deep.in_core().key() ^ deep.key(), 1 << 35);
}

#[test]
fn messages() {
    let alloc = blaze_own::testing::heap().alloc();
    let text = "fn f(v: &mut AVec<'_, u8>) -> u32 {\n    v.contains(&1) as u32\n}";
    let mut out = Findings::new_in(alloc);
    let unit = Unit { text, holes: &[], scope: FILE };
    assert!(lint(&RuleSet::ALL, &unit, alloc, &mut out).is_ok());
    let mut said = StdVec::new();
    for finding in out.list.iter() {
        let mut line = OwnString::new_in(alloc);
        show_message(&RuleSet::ALL, text, &out, finding, &mut line);
        said.push(line.as_str().to_string());
    }
    assert!(said.iter().any(|s| s.contains("`contains` was renamed `scan_for` (then `.is_some()`)")), "{said:?}");
    assert!(said.iter().any(|s| s.contains("a bare cast to `u32`")), "{said:?}");
}

// ---- the item-alone property ------------------------------------------------------------

/// A finding at a file-absolute range.
type Abs = (Template, u32, u32);

/// The file linted as one unit.
fn whole(text: &str, scope: Scope) -> StdVec<Abs> {
    let alloc = blaze_own::testing::heap().alloc();
    let mut out = Findings::new_in(alloc);
    let unit = Unit { text, holes: &[], scope };
    match lint(&RuleSet::ALL, &unit, alloc, &mut out) {
        Ok(_) => out.list.iter().map(|f| (f.template, f.range.start, f.range.end)).collect(),
        Err(error) => panic!("lint refused the file: {error:?}"),
    }
}

/// The file linted item by item, as a driver does: the root with its items as holes, then each
/// item in its parent's scope for it, its own children as holes. Ranges made absolute.
fn split(text: &str, scope: Scope) -> StdVec<Abs> {
    let alloc = blaze_own::testing::heap().alloc();
    let lines = outline(text, Edition::E2024, alloc);
    let items = lines.items();
    let mut scopes: StdVec<Option<Scope>> = vec![None; items.len()];
    let mut got = StdVec::new();
    let mut out = Findings::new_in(alloc);
    // The file's one fact, gathered once, as a driver does.
    let variants = match Variants::gather(text, alloc) {
        Ok(variants) => variants,
        Err(error) => panic!("the file's variants were refused: {error:?}"),
    };
    // `None`: the root; then every entry in pre-order, its parent before it.
    let mut units: StdVec<Option<usize>> = vec![None];
    units.extend((0..items.len()).map(Some));
    for at in units {
        let (base, end, unit_scope, parent) = match at {
            None => (0u32, text.len() as u32, scope, None),
            Some(at) => match scopes[at] {
                Some(s) => (items[at].range.start, items[at].range.end, s, Some(blaze_surface::ItemIndex(at as u32))),
                None => panic!("entry {at} was not reached from its parent"),
            },
        };
        let holes: StdVec<Span> =
            lines.children(parent).map(|(_, it)| Span::new(it.range.start - base, it.range.end - base)).collect();
        let unit = Unit { text: &text[base as usize..end as usize], holes: &holes, scope: unit_scope };
        out.clear();
        let linted = match lint_in(&RuleSet::ALL, &unit, &variants, base, alloc, &mut out) {
            Ok(linted) => linted,
            Err(error) => panic!("lint refused unit {at:?}: {error:?}"),
        };
        for ((child, _), child_scope) in lines.children(parent).zip(linted.children.iter()) {
            scopes[child.at()] = Some(*child_scope);
        }
        got.extend(out.list.iter().map(|f| (f.template, base + f.range.start, base + f.range.end)));
    }
    got
}

const STATEMENTS: [&str; 40] = [
    "let r = [0u8; 3];",
    "for i in 0..3 {\n    g(i);\n}",
    "let d = dbg!(g(a));",
    "dbg!(x);",
    "let t = Instant /* now */ ::now(); let v = Vec::new();",
    "loop {\n    rx.recv();\n}",
    "let a = x.clone();",
    "// clone: a refcount\nlet b = h.clone();",
    "let c = y.clone(); // clone: deep copy",
    "let n = y as u32;",
    "let m = u32::MAX;",
    "let k = a.len() as usize;",
    "let s = v[i];",
    "let t = v[0];",
    "let u = v[1..2];",
    "let o = x.unwrap();",
    "let p = x.expect(\"why\");",
    "if a != b {\n    panic!(\"no\");\n}",
    "let q = \"x.clone() as u32 panic!() v[i]\";",
    "// x.unwrap() as u8 v[i] format!()",
    "let r = r#\"todo!() format!() unsafe {}\"#;",
    "unsafe { g() };",
    "// SAFETY: g is sound here\nunsafe { g() };",
    "let f = format!(\"{}\", 1);",
    "write!(out, \"x\");",
    "let w = Global;",
    "let x = 1; // alloc: the root's",
    "let z = std::mem::take(&mut a);",
    "let b: std::vec::Vec<u8> = make();",
    "let mut v: AVec<'_, u8> = AVec::new_in(al);\nv.insert(0, 1);",
    "let mut d = ADeque::new_in(al);\nd.remove(0);",
    "let mut e: AVec<'_, u8> = vec_in![al; 1];\nlet has = e.contains(&1);",
    "items.insert_many(0, &[1]);",
    "map.insert(1, 2);",
    "NAME(1);",
    "self.NAME();",
    "Self::NAME();",
    "let unwrapped = my_clone;",
    "let g = Clone::clone(&a);",
    "let mut w = g(\n    h.clone(),\n);",
];
const SIGNATURES: [&str; 8] = [
    "(x: u32) -> u32",
    "(&self) -> Result<u8, String>",
    "(a: &str) -> Result<(), &'static str>",
    "() -> Vec<u8>",
    "(v: Vec<u8, Global>)",
    "() -> anyhow::Result<u8>",
    "<T>(t: T) -> Box<dyn std::error::Error>",
    "(&mut self, al: A)",
];
const BEFORE: [&str; 5] = ["", "", "// clone: arc", "// SAFETY: x", "// recursion: calls another impl"];
const ATTRS: [&str; 5] = ["", "", "#[test]", "#[cfg(test)]", "#[inline]"];

fn put_lines(out: &mut StdString, indent: &str, text: &str) {
    for line in text.split('\n') {
        out.push_str(indent);
        out.push_str(line);
        out.push('\n');
    }
}

fn gen_fn(tape: &Tape, indent: &str, names: &mut u32, out: &mut StdString) {
    let name = format!("f{names}");
    *names += 1;
    let before = *tape.one_of(&BEFORE, "before");
    if !before.is_empty() {
        put_lines(out, indent, before);
    }
    let attr = *tape.one_of(&ATTRS, "attr");
    if !attr.is_empty() {
        put_lines(out, indent, attr);
    }
    let signature = *tape.one_of(&SIGNATURES, "signature");
    put_lines(out, indent, &format!("fn {name}{signature} {{"));
    let body = format!("{indent}    ");
    let statements = tape.vec(0..=4, "statements", |t| *t.one_of(&STATEMENTS, "statement"));
    for statement in statements {
        put_lines(out, &body, &statement.replace("NAME", &name));
    }
    put_lines(out, indent, "}");
}

fn gen_fns(tape: &Tape, indent: &str, names: &mut u32, out: &mut StdString) {
    let count = tape.below(3, "fns") + 1;
    for _ in 0..count {
        gen_fn(tape, indent, names, out);
    }
}

/// A file of items, each starting its own line, two or three levels deep.
fn gen_file(tape: &Tape) -> StdString {
    let mut out = StdString::new();
    let mut names = 0u32;
    let count = tape.below(4, "items") + 1;
    for _ in 0..count {
        match tape.below(11, "item") {
            9 => put_lines(&mut out, "", "enum E3 {\n    A,\n    B(u8),\n    C { x: u8 },\n}"),
            10 => put_lines(&mut out, "", "const N: usize = 3;"),
            0 => gen_fn(tape, "", &mut names, &mut out),
            1 | 2 => {
                let before = *tape.one_of(&BEFORE, "before");
                if !before.is_empty() {
                    put_lines(&mut out, "", before);
                }
                put_lines(&mut out, "", if tape.coin("trait") { "impl Tr for S {" } else { "impl S {" });
                gen_fns(tape, "    ", &mut names, &mut out);
                put_lines(&mut out, "", "}");
            }
            3 => {
                put_lines(&mut out, "", "mod m {");
                gen_fns(tape, "    ", &mut names, &mut out);
                if tape.coin("nested impl") {
                    put_lines(&mut out, "    ", "impl S {");
                    gen_fns(tape, "        ", &mut names, &mut out);
                    put_lines(&mut out, "    ", "}");
                }
                put_lines(&mut out, "", "}");
            }
            4 => {
                put_lines(&mut out, "", "#[cfg(test)]\nmod tests {");
                gen_fns(tape, "    ", &mut names, &mut out);
                put_lines(&mut out, "", "}");
            }
            5 => put_lines(&mut out, "", "type R = Result<u8, String>;"),
            6 => put_lines(&mut out, "", "use std::collections::HashMap;"),
            7 => put_lines(&mut out, "", "struct S {\n    a: u32,\n    b: std::vec::Vec<u8>,\n}"),
            _ => put_lines(&mut out, "", "const C: u32 = 1 as u32;"),
        }
    }
    out
}

/// A bug a control plants where the checks must see it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Plant {
    None,
    /// Every finding's range one byte later.
    RangeOffByOne,
    /// Every edit's range one byte longer: it eats the next token's first byte.
    FixEatsNeighbour,
}

/// The token boundaries of `text`: every token's start and end.
fn boundaries(text: &str) -> BTreeSet<u32> {
    let mut at = 0u32;
    let mut set = BTreeSet::new();
    set.insert(0);
    for token in tokenize(text, FrontmatterAllowed::No, Cpu::BASELINE) {
        at += token.len;
        set.insert(at);
    }
    set
}

/// The significant tokens of `text` (no whitespace or comment): ranges, and whether any is an
/// error token.
fn significant(text: &str) -> (StdVec<(u32, u32)>, bool) {
    let mut at = 0u32;
    let mut out = StdVec::new();
    let mut bad = false;
    for token in tokenize(text, FrontmatterAllowed::No, Cpu::BASELINE) {
        let (start, end) = (at, at + token.len);
        at = end;
        match token.kind {
            TokenKind::Whitespace | TokenKind::LineComment { .. } | TokenKind::BlockComment { .. } | TokenKind::Eof => {}
            TokenKind::Unknown | TokenKind::InvalidIdent | TokenKind::UnknownPrefix => {
                bad = true;
                out.push((start, end));
            }
            _ => out.push((start, end)),
        }
    }
    (out, bad)
}

/// The oracle over one generated file.
fn check_file(text: &str, scope: Scope, plant: Plant) -> Result<(), Broke> {
    let shift = |mut list: StdVec<Abs>| {
        if plant == Plant::RangeOffByOne {
            for f in &mut list {
                f.1 += 1;
                f.2 += 1;
            }
        }
        list.sort();
        list
    };
    let all = shift(whole(text, scope));
    let parts = shift(split(text, scope));
    let cut = boundaries(text);
    for (template, start, end) in &all {
        if !cut.contains(start) || !cut.contains(end) {
            return Err(Broke::Oracle(format!(
                "a {template:?} range {start}..{end} is off a token boundary in\n{text}"
            )));
        }
    }
    if all != parts {
        return Err(Broke::Oracle(format!("the items alone differ from their file:\nfile  {all:?}\nitems {parts:?}\nin\n{text}")));
    }
    check_fixes(text, scope, plant)
}

/// Every first fix applied: each edit is one whole token (or a point after one), the result's
/// tokens are the old ones with those replaced, it lexes, and the fixed rule reports nothing.
fn check_fixes(text: &str, scope: Scope, plant: Plant) -> Result<(), Broke> {
    let alloc = blaze_own::testing::heap().alloc();
    let mut out = Findings::new_in(alloc);
    let unit = Unit { text, holes: &[], scope };
    if let Err(error) = lint(&RuleSet::ALL, &unit, alloc, &mut out) {
        return Err(Broke::Harness(format!("lint refused: {error:?}")));
    }
    let mut edits: StdVec<(Span, &str)> = StdVec::new();
    for finding in out.list.iter() {
        if let Some(fix) = out.fixes_of(finding).first() {
            for edit in out.edits_of(fix) {
                let mut range = edit.range;
                if plant == Plant::FixEatsNeighbour && (range.end as usize) < text.len() {
                    range.end += 1;
                }
                edits.push((range, out.text_of(edit)));
            }
        }
    }
    let new = match apply(text, &edits, alloc) {
        Ok(new) => new.as_str().to_string(),
        Err(error) => return Err(Broke::Oracle(format!("the fixes were refused: {error:?} in\n{text}"))),
    };
    let (old_tokens, _) = significant(text);
    let mut expected: StdVec<StdString> = StdVec::new();
    let mut used = vec![false; edits.len()];
    // Every non-empty edit must cover one or more whole significant tokens without splitting
    // any neighbouring token (trailing/leading whitespace inside the edit is allowed so fixes
    // like removing `mut ` or a whole `use` line leave clean formatting).
    for (at, (r, _)) in edits.iter().enumerate() {
        if r.is_empty() {
            continue;
        }
        for &(ts, te) in &old_tokens {
            let splits_start = r.start > ts && r.start < te;
            let splits_end = r.end > ts && r.end < te;
            if splits_start || splits_end {
                return Err(Broke::Oracle(format!(
                    "a fix changes a neighbouring token: edit {:?} splits token {ts}..{te} in\n{text}",
                    edits[at].0
                )));
            }
        }
        // Also ensure the edit covers at least one token and ends no further than the start of
        // the next token (so eating into a 1-byte neighbour like `(` is rejected).
        if let Some(first_covered) = old_tokens.iter().position(|&(ts, te)| ts >= r.start && te <= r.end) {
            let last_covered = old_tokens
                .iter()
                .rposition(|&(ts, te)| ts >= r.start && te <= r.end)
                .unwrap_or(first_covered);
            let covered = last_covered - first_covered + 1;
            // Single-token rename/strip edits must cover exactly 1 token; multi-token fixes
            // (unused-import line/brace rewrites) start on `use`, `::`, or `{`.
            let multi_ok = matches!(
                text.get(old_tokens[first_covered].0 as usize..old_tokens[first_covered].1 as usize),
                Some("use" | "{" | ":")
            );
            if covered > 1 && !multi_ok {
                return Err(Broke::Oracle(format!(
                    "a fix changes a neighbouring token: edit {:?} covers {covered} tokens in\n{text}",
                    edits[at].0
                )));
            }
        }
    }
    let mut tok_idx = 0usize;
    while tok_idx < old_tokens.len() {
        let (start, end) = old_tokens[tok_idx];
        match edits.iter().position(|(r, _)| !r.is_empty() && r.start <= start && end <= r.end) {
            Some(at) => {
                if !used[at] {
                    used[at] = true;
                    let (put, _) = significant(edits[at].1);
                    expected.extend(put.iter().map(|(a, b)| edits[at].1[*a as usize..*b as usize].to_string()));
                }
            }
            None => expected.push(text[start as usize..end as usize].to_string()),
        }
        for (at, (r, put)) in edits.iter().enumerate() {
            if r.is_empty() && r.start == end {
                used[at] = true;
                let (tokens, _) = significant(put);
                expected.extend(tokens.iter().map(|(a, b)| put[*a as usize..*b as usize].to_string()));
            }
        }
        tok_idx += 1;
    }
    if let Some(at) = used.iter().position(|u| !u) {
        return Err(Broke::Oracle(format!(
            "a fix changes a neighbouring token: edit {:?} is not one whole token in\n{text}",
            edits[at].0
        )));
    }
    let (new_tokens, bad) = significant(&new);
    if bad {
        return Err(Broke::Oracle(format!("the fixed text does not lex:\n{new}")));
    }
    let got: StdVec<&str> = new_tokens.iter().map(|(a, b)| &new[*a as usize..*b as usize]).collect();
    if got != expected {
        return Err(Broke::Oracle(format!("a fix changes a neighbouring token:\n{text}\nbecame\n{new}")));
    }
    let again = whole(&new, scope);
    if again.iter().any(|f| f.0 == Template::RenamedFixed) {
        return Err(Broke::Oracle(format!("the fixed rule still reports after its fixes:\n{new}")));
    }
    Ok(())
}

fn run_case(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let text = gen_file(tape);
    let scope = Scope::file(TestBits::NONE, tape.coin("bin root"), false);
    check_file(&text, scope, plant)
}

#[test]
fn item_alone_equals_item_in_file() {
    blaze_sim::check(Config::cases(300), |tape| run_case(tape, Plant::None));
}

/// Which place map the own-text property plants (rule 23).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OwnPlant {
    None,
    /// An own range taken as the unit's: no child before it given its length back.
    Unshifted,
}

/// The file linted unit by unit over each unit's OWN text (each child one `HOLE_MARK`), as the
/// Lints stage does (blaze-cli lints.rs), each range given its children's lengths back by
/// `real_range`; and every place of each unit outside its children through `own_place` and
/// `real_place` and back to itself.
fn split_own(text: &str, scope: Scope, plant: OwnPlant) -> Result<StdVec<Abs>, Broke> {
    let alloc = blaze_own::testing::heap().alloc();
    let lines = outline(text, Edition::E2024, alloc);
    let units = Units::plan(lines.items(), text.len(), alloc).map_err(|e| Broke::Harness(format!("the units were refused: {e:?}")))?;
    let variants = Variants::gather(text, alloc).map_err(|e| Broke::Harness(format!("the file's variants were refused: {e:?}")))?;
    let mut scopes = vec![scope; units.list.len()];
    let mut got = StdVec::new();
    let mut out = Findings::new_in(alloc);
    for at in 0..units.list.len() {
        let plan = units.list[at];
        let holes = units.holes_of(at);
        for real in 0..=(plan.end - plan.base) {
            if holes.iter().any(|hole| real > hole.start && real < hole.end) {
                continue;
            }
            let back = crate::real_place(holes.iter().copied(), crate::own_place(holes, real));
            if back != Some(real) {
                return Err(Broke::Oracle(format!("unit {at}: place {real} came back as {back:?} through its own text\n{text}")));
            }
        }
        let own = units.own(at, text, alloc).map_err(|e| Broke::Harness(format!("unit {at}'s own text was refused: {e:?}")))?;
        out.clear();
        let geometry = crate::Geometry { base: plan.base, len: plan.end - plan.base, holes };
        let linted = crate::lint_own_in(&RuleSet::ALL, &own, scopes[at], geometry, Some(variants.view()), alloc, &mut out)
            .map_err(|e| Broke::Harness(format!("lint refused unit {at}'s own text: {e:?}")))?;
        for (child, given) in units.children_of(at).iter().zip(linted.children.iter()) {
            scopes[*child as usize] = *given;
        }
        for finding in out.list.iter() {
            let range = match plant {
                OwnPlant::None => crate::real_range(holes, finding.range)
                    .ok_or_else(|| Broke::Oracle(format!("unit {at}: own range {:?} has no place in the unit\n{text}", finding.range)))?,
                OwnPlant::Unshifted => finding.range,
            };
            got.push((finding.template, plan.base + range.start, plan.base + range.end));
        }
    }
    got.sort();
    Ok(got)
}

/// An impl whose own text has a finding AFTER a child: the `// alloc:` hatch (allocs' `comment`,
/// a plain comment, so no item's) behind a method's hole. `gen_file`'s findings are nearly all in
/// leaf units (bodies, consts), where no hole is before them, so without this the own-text
/// property seldom sees a child given its length back, and its control's plant (a range not
/// shifted) was missed in 300 cases (c111, 2026-11-21).
const HATCH_AFTER_CHILD: &str = "impl S {\n    fn hatch(&self) -> u8 {\n        1\n    }\n    // alloc: after a child\n}";

fn run_own_case(tape: &Tape, plant: OwnPlant) -> Result<(), Broke> {
    let mut text = gen_file(tape);
    if tape.coin("a finding after a child") {
        put_lines(&mut text, "", HATCH_AFTER_CHILD);
    }
    let scope = Scope::file(TestBits::NONE, tape.coin("bin root"), false);
    let mut all = whole(&text, scope);
    all.sort();
    let own = split_own(&text, scope, plant)?;
    if all != own {
        return Err(Broke::Oracle(format!("the units' own texts differ from their file:\nfile {all:?}\nown  {own:?}\nin\n{text}")));
    }
    Ok(())
}

/// Each unit linted over its own text, each child one mark, finds what the file finds, at the
/// same places once each child before a finding is given its length back (blaze-cli lints.rs
/// keys a unit by that text alone, so a child that grows moves no other unit's key).
#[test]
fn own_texts_find_what_the_file_finds() {
    blaze_sim::check(Config::cases(300), |tape| run_own_case(tape, OwnPlant::None));
}

#[test]
fn control_own_range_unshifted() {
    blaze_sim::control(Config::cases(300), OwnPlant::None, OwnPlant::Unshifted, "own texts differ from their file", run_own_case);
}

/// `Units::plan` gives each unit the holes and children the outline's own `children` walk gives.
#[test]
fn units_plan_matches_outline_children() {
    blaze_sim::check(Config::cases(200), |tape| {
        let text = gen_file(tape);
        let alloc = blaze_own::testing::heap().alloc();
        let made = outline(&text, Edition::E2024, alloc);
        let units = crate::Units::plan(made.items(), text.len(), alloc).map_err(|e| Broke::Oracle(format!("{e:?}")))?;
        for unit in 0..units.list.len() {
            let parent = unit.checked_sub(1).map(|at| blaze_surface::ItemIndex(at as u32));
            let base = units.list[unit].base;
            let want: StdVec<(Span, u32)> = made
                .children(parent)
                .map(|(index, child)| (Span::new(child.range.start - base, child.range.end - base), index.at() as u32 + 1))
                .collect();
            let got: StdVec<(Span, u32)> =
                units.holes_of(unit).iter().copied().zip(units.children_of(unit).iter().copied()).collect();
            if got != want {
                return Err(Broke::Oracle(format!("unit {unit}: planned {got:?}, the outline says {want:?}\n{text}")));
            }
        }
        Ok(())
    });
}

#[test]
fn control_range_off_by_one() {
    blaze_sim::control(Config::cases(100), Plant::None, Plant::RangeOffByOne, "off a token boundary", run_case);
}

#[test]
fn control_fix_eats_neighbour() {
    blaze_sim::control(Config::cases(200), Plant::None, Plant::FixEatsNeighbour, "neighbouring token", run_case);
}

#[test]
fn sota_new_rules_and_wired_lints() {
    // 1. Newly wired Rule::lint() names opt in via `#![deny(blaze::..)]`.
    for (rule, name) in [
        (Rule::AmbientHasher, "default_hasher"),
        (Rule::NoAsync, "async_fn"),
        (Rule::Casts, "as_cast"),
        (Rule::Untyped, "untyped_error"),
        (Rule::UnboundedWait, "unbounded_wait"),
        (Rule::WorkspaceUnsafe, "workspace_unsafe"),
        (Rule::SeqCstUnexplained, "seqcst_unexplained"),
        (Rule::RawSync, "raw_sync"),
        (Rule::BlockingOnPool, "blocking_on_pool"),
        (Rule::Transmute, "transmute"),
    ] {
        assert_eq!(rule.lint(), Some(name), "rule {:?} lint name", rule);
        let root = format!("#![deny(blaze::{name})]\n");
        let marks = RootMarks::read(&root);
        assert!(marks.denies(rule), "RootMarks did not enable {name}");
    }

    // 2. WorkspaceUnsafe: refused without trust attr, allowed with `#![blaze::trust(unsafe, reason = "...")]`.
    let wu = RuleSet::only(&[Rule::WorkspaceUnsafe]);
    assert!(!found(&wu, "fn f() { unsafe { foo() } }", FILE).is_empty());
    assert!(!found(&wu, "unsafe fn g() {}", FILE).is_empty());
    let trusted_marks = RootMarks::read("#![blaze::trust(unsafe, reason = \"FFI syscall wrapper\")]\n");
    let trusted_scope = trusted_marks.scope(FILE);
    assert!(found(&wu, "fn f() { unsafe { foo() } }", trusted_scope).is_empty());
    let empty_reason = RootMarks::read("#![blaze::trust(unsafe, reason = \"\")]\n");
    assert!(!found(&wu, "fn f() { unsafe { foo() } }", empty_reason.scope(FILE)).is_empty());

    // 3. SeqCstUnexplained: refused bare, accepted with `// seqcst: ...`.
    let sc = RuleSet::only(&[Rule::SeqCstUnexplained]);
    assert!(!found(&sc, "fn f(a: &AtomicU64) { a.load(Ordering::SeqCst); }", FILE).is_empty());
    assert!(found(&sc, "fn f(a: &AtomicU64) {\n    // seqcst: total order across producer and consumer\n    a.load(Ordering::SeqCst);\n}", FILE).is_empty());

    // 4. RawSync, BlockingOnPool, Transmute.
    assert!(!kinds(Rule::RawSync, "fn f() { let _ = sync::Mutex::new(0); }").is_empty());
    assert!(!kinds(Rule::RawSync, "fn f() { let _ = mpsc::channel(); }").is_empty());
    assert!(!kinds(Rule::BlockingOnPool, "fn f() { thread::sleep(d); }").is_empty());
    assert!(!kinds(Rule::BlockingOnPool, "fn f() { hint::spin_loop(); }").is_empty());
    assert!(!kinds(Rule::Transmute, "fn f(x: u32) -> f32 { unsafe { mem::transmute(x) } }").is_empty());

    // 5. Strengthened AmbientHasher & AmbientHeap.
    assert!(!kinds(Rule::AmbientHasher, "fn f() { let _ = HashMap::new(); }").is_empty());
    assert!(!kinds(Rule::AmbientHasher, "fn f() { let _ = rustc_hash::FxHashMap::default(); }").is_empty());
    assert!(!kinds(Rule::AmbientHeap, "fn f() { let _ = BTreeMap::new(); }").is_empty());
    assert!(!kinds(Rule::AmbientHeap, "fn f() { let _ = VecDeque::new(); }").is_empty());
}

#[test]
fn newtype_discipline_rules() {
    for (rule, name) in [
        (Rule::BoolParam, "bool_param"),
        (Rule::SwappableParameters, "swappable_parameters"),
        (Rule::PrimitiveMeaning, "primitive_meaning"),
    ] {
        assert_eq!(rule.lint(), Some(name));
        let root = format!("#![deny(blaze::{name})]\n");
        assert!(RootMarks::read(&root).denies(rule));
    }

    // BoolParam: catches `pub fn` bool flags, ignores private fns and trivial `set_*` setters.
    assert_eq!(kinds(Rule::BoolParam, "pub fn open(path: &Path, create: bool) {}"), vec!["bool-param"]);
    assert!(kinds(Rule::BoolParam, "fn helper(create: bool) {}").is_empty());
    assert!(kinds(Rule::BoolParam, "pub fn set_enabled(&mut self, enabled: bool) {}").is_empty());

    // SwappableParameters: catches adjacent same-primitive params, ignores symmetric `(x, y)` / `(min, max)`.
    assert_eq!(
        kinds(Rule::SwappableParameters, "pub fn connect(host: &str, user: &str) {}"),
        vec!["swappable-parameters"]
    );
    assert_eq!(
        kinds(Rule::SwappableParameters, "pub fn copy_range(src: u32, dst: u32) {}"),
        vec!["swappable-parameters"]
    );
    assert!(kinds(Rule::SwappableParameters, "pub fn point(x: f32, y: f32, z: f32) {}").is_empty());
    assert!(kinds(Rule::SwappableParameters, "pub fn clamp(min: u64, max: u64) {}").is_empty());
    assert!(kinds(Rule::SwappableParameters, "pub fn typed(src: SrcId, dst: DstId) {}").is_empty());

    // PrimitiveMeaning: catches bare numeric unit/ID params, ignores newtypes and generic counts.
    assert_eq!(
        kinds(Rule::PrimitiveMeaning, "pub fn wait_for(timeout_ms: u64, node_id: u32) {}"),
        vec!["primitive-meaning", "primitive-meaning"]
    );
    assert_eq!(
        kinds(Rule::PrimitiveMeaning, "pub fn bind(port: u16) {}"),
        vec!["primitive-meaning"]
    );
    assert!(kinds(Rule::PrimitiveMeaning, "pub fn wait_for(timeout: Duration, node: NodeId) {}").is_empty());
    assert!(kinds(Rule::PrimitiveMeaning, "pub fn reserve(count: usize) {}").is_empty());

    // AnonTupleReturn: denies `(Option<T>, bool)`, `(T, bool)`, `Option<(A, B, C)>`, `Result<(T, bool), E>`,
    // while allowing `()`, `(u32, u32)`, and named structs.
    assert_eq!(Rule::AnonTupleReturn.lint(), Some("anon_tuple_return"));
    assert!(RootMarks::read("#![deny(blaze::anon_tuple_return)]\n").denies(Rule::AnonTupleReturn));
    assert_eq!(
        kinds(Rule::AnonTupleReturn, "fn classify(s: &str) -> (Option<PrimKind>, bool) { todo!() }"),
        vec!["anon-tuple-return"]
    );
    assert_eq!(
        kinds(Rule::AnonTupleReturn, "fn parse(x: usize) -> Option<(Span, Span, Option<PrimKind>, bool)> { None }"),
        vec!["anon-tuple-return"]
    );
    assert_eq!(
        kinds(Rule::AnonTupleReturn, "pub fn step() -> Result<(u32, bool), Err> { todo!() }"),
        vec!["anon-tuple-return"]
    );
    assert_eq!(
        kinds(Rule::AnonTupleReturn, "fn triple() -> (u32, u32, u32) { (1, 2, 3) }"),
        vec!["anon-tuple-return"]
    );
    assert!(kinds(Rule::AnonTupleReturn, "fn ok() -> () {}").is_empty());
    assert!(kinds(Rule::AnonTupleReturn, "fn pair() -> (u32, u32) { (1, 2) }").is_empty());
    assert!(kinds(Rule::AnonTupleReturn, "fn named() -> PrimClass { todo!() }").is_empty());
}

#[test]
fn profile_guided_linting_and_advisor() {
    use blaze_own::Alloc;
    use crate::Levels;
    use crate::pgo::{
        AdviceKind, EffectiveLevel, FnSpan, Heat, HotSite, PgoHints, PgoPolicy, rank_findings,
        suggest_from_pgo,
    };

    let src = concat!(
        "fn cold_fn() { let _ = x.clone(); }\n",
        "fn hot_fn() {\n",
        "    let v = Vec::new();\n",
        "    for _ in 0..10 { map.clear(); }\n",
        "    let _ = slice.binary_search(&k);\n",
        "    let _ = y.clone();\n",
        "}\n",
        "struct Bad { count: AtomicU64 }\n",
        "struct Good { count: CachePadded<AtomicU64> }\n",
    );

    let cold_start = src.find("fn cold_fn").unwrap() as u32;
    let hot_start = src.find("fn hot_fn").unwrap() as u32;
    let struct_start = src.find("struct Bad").unwrap() as u32;
    let fn_spans = [
        FnSpan::for_symbol(Span::new(cold_start, hot_start), "cold_fn"),
        FnSpan::for_symbol(Span::new(hot_start, struct_start), "hot_fn"),
    ];
    let hints = PgoHints::from_sites(
        &[
            HotSite::for_symbol("hot_fn", 90_000, 90_000),
            HotSite::for_symbol("cold_fn", 10, 100),
        ],
        Alloc::global(),
    );

    let rules = RuleSet::only(&[Rule::Clones]);
    let mut findings = Findings::new_in(Alloc::global());
    lint(&rules, &Unit { text: src, holes: &[], scope: FILE }, Alloc::global(), &mut findings).unwrap();
    assert_eq!(findings.list.len(), 2);

    // Default policy: hot_fn's clone is promoted to Error and ranked first; cold_fn stays Advice.
    let ranked = rank_findings(&findings, &Levels::ADVICE, &fn_spans, &hints, PgoPolicy::DEFAULT, Alloc::global());
    assert_eq!(ranked.len(), 2);
    assert_eq!(ranked[0].heat, Heat::Critical);
    assert_eq!(ranked[0].level, EffectiveLevel::Error);
    assert_eq!(ranked[1].heat, Heat::Cold);
    assert_eq!(ranked[1].level, EffectiveLevel::Advice);

    // HOT_ONLY policy suppresses cold perf noise while keeping hot errors.
    let hot_only = rank_findings(&findings, &Levels::ADVICE, &fn_spans, &hints, PgoPolicy::HOT_ONLY, Alloc::global());
    assert_eq!(hot_only.len(), 1);
    assert_eq!(hot_only[0].heat, Heat::Critical);

    // PGO-guided advisor fires on hot Vec::new, loop .clear(), .binary_search, and bare AtomicU64 —
    // and does NOT fire on CachePadded<AtomicU64>.
    let advice = suggest_from_pgo(src, &fn_spans, &hints, Heat::Hot, Alloc::global());
    let mut kinds_found: StdVec<AdviceKind> = advice.iter().map(|a| a.kind).collect();
    kinds_found.sort();
    assert_eq!(
        kinds_found,
        vec![
            AdviceKind::UseGenMapForHotClear,
            AdviceKind::UseSmallVecForHotLocal,
            AdviceKind::UseCachePadded,
            AdviceKind::UseInterpSearch,
        ]
    );
}

#[test]
fn dup_and_health_scanner() {
    use blaze_own::Alloc;
    use crate::dup::{
        CloneKind, HealthKind, HealthLimits, analyze_health, find_duplicates, fingerprint_file,
    };

    // Two alpha-equivalent functions with renamed variables -> ExactClone!
    let src = concat!(
        "fn compute_alpha(a: u64, b: u64) -> u64 {\n",
        "    let mut acc = a.wrapping_add(b);\n",
        "    for i in 0..16 {\n",
        "        acc = acc.wrapping_mul(31).wrapping_add(i);\n",
        "        if acc > 1000 { acc ^= b; }\n",
        "    }\n",
        "    acc\n",
        "}\n",
        "fn compute_beta(x: u64, y: u64) -> u64 {\n",
        "    let mut sum = x.wrapping_add(y);\n",
        "    for k in 0..16 {\n",
        "        sum = sum.wrapping_mul(31).wrapping_add(k);\n",
        "        if sum > 1000 { sum ^= y; }\n",
        "    }\n",
        "    sum\n",
        "}\n",
        "fn nested_mess(flag: bool) -> bool {\n",
        "    if flag { if flag { if flag { if flag { if flag { return true; } } } } }\n",
        "    if flag { true } else { false }\n",
        "}\n",
    );

    let fps = fingerprint_file(0, src, 12, Alloc::global());
    assert!(fps.len() >= 2);
    let dups = find_duplicates(&fps, Alloc::global());
    assert_eq!(dups.len(), 1);
    assert_eq!(dups[0].kind, CloneKind::ExactClone);
    assert_eq!(dups[0].similarity_pct, 100);

    let health = analyze_health(src, HealthLimits::DEFAULT, Alloc::global());
    assert!(health.iter().any(|h| h.kind == HealthKind::DeepNesting));
    assert!(health.iter().any(|h| h.kind == HealthKind::RedundantBoolIf));

    // HandWrittenDerivable catches >= 8-arm integer discriminant tables; ProcMacroDef catches #[proc_macro*].
    let hand_table = concat!(
        "fn tag(k: K) -> u8 { match k {\n",
        "    K::A => 1, K::B => 2, K::C => 3, K::D => 4,\n",
        "    K::E => 5, K::F => 6, K::G => 7, K::H => 8, _ => 9,\n",
        "} }\n",
    );
    assert_eq!(kinds(Rule::HandWrittenDerivable, hand_table), vec!["hand-written-discriminant"]);
    assert_eq!(
        kinds(Rule::ProcMacroDef, "#[proc_macro_derive(Foo)] pub fn foo(ts: TokenStream) -> TokenStream { ts }"),
        vec!["proc-macro-def"]
    );
}

#[test]
fn double_indirection_arc() {
    let rule = Rule::DoubleIndirectionArc;
    let only = RuleSet::only(&[rule]);

    assert_eq!(rule.name(), "double-indirection-arc");
    assert_eq!(Rule::from_name("double-indirection-arc"), Some(rule));
    assert_eq!(rule.lint(), Some("double_indirection_arc"));
    assert_eq!(Rule::from_lint("double_indirection_arc"), Some(rule));
    assert_eq!(Template::DoubleIndirectionArc.rule(), rule);
    assert!(crate::pgo::is_perf_rule(rule));

    let marks = RootMarks::read("#![deny(blaze::double_indirection_arc)]\n");
    assert!(marks.denies(rule));
    assert!(marks.rules(Step::Rust.rules()).runs(rule));
    assert_ne!(marks.key(), RootMarks::read("#![blaze::driver]\n").key());

    // Positive detections: bare, path-qualified, allocator-parameterized, nested types.
    assert_eq!(
        found(&only, "struct S { data: Arc<Vec<u8>> }", FILE),
        [("double-indirection-arc", "Arc<Vec<u8>>")]
    );
    assert_eq!(
        found(&only, "struct S { name: Arc<String> }", FILE),
        [("double-indirection-arc", "Arc<String>")]
    );
    assert_eq!(
        found(&only, "type T = Arc<Vec<Held>, Global>;", FILE),
        [("double-indirection-arc", "Arc<Vec<Held>, Global>")]
    );
    assert_eq!(
        found(&only, "type T = Arc<Vec<(String, Vec<u8>), Global>, Global>;", FILE),
        [("double-indirection-arc", "Arc<Vec<(String, Vec<u8>), Global>, Global>")]
    );
    assert_eq!(
        found(&only, "fn f(x: &std::sync::Arc<std::vec::Vec<u32>>) -> alloc::sync::Arc<alloc::string::String> { todo!() }", FILE),
        [
            ("double-indirection-arc", "Arc<std::vec::Vec<u32>>"),
            ("double-indirection-arc", "Arc<alloc::string::String>"),
        ]
    );
    assert_eq!(
        found(&only, "fn f() -> Option<Arc<String, A>> { None }", FILE),
        [("double-indirection-arc", "Arc<String, A>")]
    );
    assert_eq!(
        found(&only, "fn f(v: Arc<blaze_own::String<A>, A>) {}", FILE),
        [("double-indirection-arc", "Arc<blaze_own::String<A>, A>")]
    );
    assert_eq!(
        found(&only, "fn f(v: Arc<Vec<fn() -> u8>>) {}", FILE),
        [("double-indirection-arc", "Arc<Vec<fn() -> u8>>")]
    );
    assert_eq!(
        found(&only, "fn f(v: Arc<Vec<[u8; 4]>>) {}", FILE),
        [("double-indirection-arc", "Arc<Vec<[u8; 4]>>")]
    );

    // Messages advise ThinArc for Arc<Vec<...>> and ThinStr for Arc<String>.
    let alloc = blaze_own::testing::heap().alloc();
    let msg_src = "struct S { a: Arc<Vec<u8>>, b: Arc<String> }";
    let mut out = Findings::new_in(alloc);
    assert!(lint(&only, &Unit { text: msg_src, holes: &[], scope: FILE }, alloc, &mut out).is_ok());
    let mut said = StdVec::new();
    for f in out.list.iter() {
        let mut line = OwnString::new_in(alloc);
        show_message(&only, msg_src, &out, f, &mut line);
        said.push(line.as_str().to_string());
    }
    assert_eq!(said.len(), 2);
    assert!(said[0].contains("blaze_own::ThinArc") && said[0].contains("thin pointer"), "{said:?}");
    assert!(said[1].contains("blaze_own::ThinStr") && said[1].contains("thin pointer"), "{said:?}");

    // Negative clean controls.
    none(rule, &[
        "struct S { data: Arc<[u8]>, name: Arc<str> }",
        "struct S { data: blaze_own::ThinArc<u8>, name: blaze_own::ThinStr }",
        "struct S { inner: Arc<Mutex<Vec<u8>>>, lock: Arc<RwLock<String>> }",
        "struct S { item: Arc<MyStruct>, list: Arc<MyVec<u8>> }",
        "struct Arc<T>(T);",
        "use std::sync::Arc;",
        "fn f() { let a = Arc::new(vec![1]); let b = Arc::new(String::new()); }",
        "fn f(it: Arc<String::IntoIter>) {}",
        "fn f() { let s = \"Arc<Vec<u8>> Arc<String>\"; }",
        "fn f() {\n    // Arc<Vec<u8>> Arc<String>\n}",
        "#[test]\nfn t() { let x: Arc<Vec<u8>> = g(); }",
        "#[cfg(test)]\nmod t {\n    type X = Arc<String>;\n}",
    ]);
}

#[test]
fn raw_sandbox_exec() {
    let rule = Rule::RawSandboxExec;
    let only = RuleSet::only(&[rule]);

    assert_eq!(rule.name(), "raw-sandbox-exec");
    assert_eq!(Rule::from_name("raw-sandbox-exec"), Some(rule));
    assert_eq!(rule.lint(), Some("raw_sandbox_exec"));
    assert_eq!(Rule::from_lint("raw_sandbox_exec"), Some(rule));
    assert_eq!(Template::RawSandboxExec.rule(), rule);
    assert!(!crate::pgo::is_perf_rule(rule));

    let marks = RootMarks::read("#![deny(blaze::raw_sandbox_exec)]\n");
    assert!(marks.denies(rule));
    assert!(marks.rules(Step::Rust.rules()).runs(rule));
    assert_ne!(marks.key(), RootMarks::read("#![blaze::trust(unsafe, reason = \"x\")]\n").key());

    // Positive detections outside bstd::procs::hermetic.
    assert_eq!(
        found(&only, "fn f() { Command::new(\"sandbox-exec\"); }", FILE),
        [("raw-sandbox-exec", "\"sandbox-exec\"")]
    );
    assert_eq!(
        found(&only, "fn f() { Command::new(\"/usr/bin/sandbox-exec\"); }", FILE),
        [("raw-sandbox-exec", "\"/usr/bin/sandbox-exec\"")]
    );
    assert_eq!(
        found(&only, "const SANDBOX_EXEC: &str = \"/usr/bin/sandbox-exec\";", FILE),
        [("raw-sandbox-exec", "\"/usr/bin/sandbox-exec\"")]
    );
    assert_eq!(
        kinds(rule, "fn f() { let _ = r#\"/usr/bin/sandbox-exec\"#; let _ = b\"sandbox-exec\"; let _ = c\"/usr/bin/sandbox-exec\"; }"),
        ["raw-sandbox-exec", "raw-sandbox-exec", "raw-sandbox-exec"]
    );

    // Message advises bstd::procs::hermetic::Hermetic and Plan.
    let alloc = blaze_own::testing::heap().alloc();
    let msg_src = "fn f() { Command::new(\"/usr/bin/sandbox-exec\"); }";
    let mut out = Findings::new_in(alloc);
    assert!(lint(&only, &Unit { text: msg_src, holes: &[], scope: FILE }, alloc, &mut out).is_ok());
    assert_eq!(out.list.len(), 1);
    let mut line = OwnString::new_in(alloc);
    show_message(&only, msg_src, &out, &out.list[0], &mut line);
    assert!(
        line.as_str().contains("bstd::procs::hermetic::Hermetic") && line.as_str().contains("Plan"),
        "{}",
        line.as_str()
    );

    // Inside bstd::procs::hermetic: allowed, and child items inherit the scope.
    let hermetic = FILE.in_hermetic();
    assert_ne!(hermetic.key(), FILE.key());
    assert!(found(&only, "const SANDBOX_EXEC: &str = \"/usr/bin/sandbox-exec\";", hermetic).is_empty());
    assert!(found(&only, "fn f() { Command::new(\"sandbox-exec\"); }", hermetic).is_empty());

    let mod_text = "mod m {\n    fn g() {}\n}";
    let start = mod_text.find("fn g").unwrap() as u32;
    let holes = [Span::new(start, start + "fn g() {}".len() as u32)];
    out.clear();
    let linted = lint(&only, &Unit { text: mod_text, holes: &holes, scope: hermetic }, alloc, &mut out).unwrap();
    assert_eq!(linted.children.len(), 1);
    assert!(linted.children[0].in_hermetic);

    // Path helper recognizes bstd::procs::hermetic and rejects other paths.
    assert!(is_hermetic_path("crates/blaze-std/src/procs/hermetic.rs"));
    assert!(is_hermetic_path("crates/blaze-std/src/procs/hermetic/seatbelt.rs"));
    assert!(is_hermetic_path("crates/bstd/src/procs/hermetic/native.rs"));
    assert!(!is_hermetic_path("crates/blaze-std/src/procs/hermetic_extra.rs"));
    assert!(!is_hermetic_path("crates/blaze-cli/src/test_drive.rs"));
    assert!(!is_hermetic_path("crates/other/src/procs/hermetic/seatbelt.rs"));

    // Negative clean controls outside hermetic.
    none(rule, &[
        "fn f() { bstd::procs::hermetic::Hermetic::run(plan); }",
        "fn f() { Command::new(\"/usr/bin/env\"); }",
        "fn f() { let msg = \"failed to run sandbox-exec\"; }",
        "fn f() { let bin = \"sandbox-exec-helper\"; }",
        "fn f() {\n    // /usr/bin/sandbox-exec\n    /* sandbox-exec */\n}",
        "#[test]\nfn t() { Command::new(\"/usr/bin/sandbox-exec\"); }",
        "#[cfg(test)]\nmod t {\n    const S: &str = \"sandbox-exec\";\n}",
    ]);
}

#[test]
fn mut_ref_deprecated_with_autofix() {
    let rule = Rule::MutRef;
    let only = RuleSet::only(&[rule]);
    assert_eq!(Rule::from_name("mut-ref"), Some(rule));
    assert_eq!(Rule::from_lint("mut_ref"), Some(rule));
    assert_eq!(Template::MutRef.rule(), rule);
    assert_eq!(Template::RefMut.rule(), rule);
    assert!(Step::Edition.rules().runs(rule), "Step::Edition runs mut-ref deprecation");
    assert!(!Step::Rust.rules().runs(rule));

    // RootMarks turns on mut-ref via `#![blaze::uniq]` or `#![deny(blaze::mut_ref)]`.
    let uniq_marks = RootMarks::read("#![blaze::uniq]\nfn f() {}");
    assert!(uniq_marks.uniq);
    assert!(uniq_marks.rules(Step::Rust.rules()).runs(rule));
    assert_ne!(uniq_marks.key(), RootMarks::NONE.key());

    let deny_marks = RootMarks::read("#![deny(blaze::mut_ref)]\nfn f() {}");
    assert!(deny_marks.denies(rule));
    assert!(deny_marks.rules(Step::Rust.rules()).runs(rule));
    assert_eq!(deny_marks.levels(Step::Rust.levels()).of(rule), Level::Error);

    // Detection across `&mut`, `&'a mut`, `&&mut`, and `ref mut`.
    let src = "fn obj_row<'a>(&'a mut self, ui: &mut UiWorld, x: &&mut i32) { let mut n = 0; let r = &mut n; let ref mut y = n; let &mut z = r; }";
    let alloc = blaze_own::testing::heap().alloc();
    let mut out = Findings::new_in(alloc);
    assert!(lint(&only, &Unit { text: src, holes: &[], scope: FILE }, alloc, &mut out).is_ok());
    let matched: StdVec<(&str, &str)> =
        out.list.iter().map(|f| (f.template.kind(), f.range.of(src))).collect();
    assert_eq!(
        matched,
        [
            ("mut-ref", "&'a mut"),
            ("mut-ref", "&mut"),
            ("mut-ref", "&mut"),
            ("mut-ref", "&mut"),
            ("ref-mut", "ref mut"),
            ("mut-ref", "&mut"),
        ]
    );

    // Mechanical auto-fix converts every `&mut` / `ref mut` to `&uniq` / `ref uniq`,
    // leaving `let mut n = 0;` untouched.
    let fixed = apply_all(src, &out, alloc).expect("autofix applies cleanly");
    assert_eq!(
        fixed.as_str(),
        "fn obj_row<'a>(&'a uniq self, ui: &uniq UiWorld, x: &&uniq i32) { let mut n = 0; let r = &uniq n; let ref uniq y = n; let &uniq z = r; }"
    );

    // Re-linting the fixed output produces zero findings!
    out.clear();
    assert!(lint(&only, &Unit { text: fixed.as_str(), holes: &[], scope: FILE }, alloc, &mut out).is_ok());
    assert!(out.list.is_empty(), "fixed code must have zero mut-ref findings");

    // Deprecation message mentions `&uniq`.
    out.clear();
    let msg_src = "fn f(x: &mut i32) {}";
    assert!(lint(&only, &Unit { text: msg_src, holes: &[], scope: FILE }, alloc, &mut out).is_ok());
    let mut msg = OwnString::new_in(alloc);
    show_message(&only, msg_src, &out, &out.list[0], &mut msg);
    assert!(msg.as_str().contains("deprecated") && msg.as_str().contains("&uniq"), "{}", msg.as_str());

    // Negative controls: `let mut`, `*mut`, `&raw mut`, `mut` bindings, raw idents, strings, comments.
    none(rule, &[
        "fn f(mut x: i32) { let mut y = x; y += 1; }",
        "fn f(p: *mut u8) -> *mut u8 { p }",
        "fn f(mut x: i32) { let _ = &raw mut x; }",
        "fn f(ui: &uniq UiWorld) { let ref uniq y = *ui; }",
        "fn f() { let s = \"&mut ref mut\"; /* &mut */ // ref mut\n }",
        "fn f(r#mut: i32) { let _ = &r#mut; }",
    ]);
}

#[test]
fn clone_handle_autofixes_rc_to_local_share_and_arc_to_share() {
    let alloc = blaze_own::testing::heap().alloc();
    let only = RuleSet::only(&[Rule::Clones]);
    let src = "fn f(a: &Arc<u8>, r: &Rc<u8>) {\n    let a2 = a.clone();\n    let r2 = r.clone();\n    let r3 = Rc::new_in(1, alloc);\n    let r4 = r3.clone();\n}\n";
    let mut out = Findings::new_in(alloc);
    assert!(lint(&only, &Unit { text: src, holes: &[], scope: FILE }, alloc, &mut out).is_ok());
    assert_eq!(out.list.len(), 3);
    let fixed = apply_all(src, &out, alloc).expect("autofix applies cleanly");
    assert_eq!(
        fixed.as_str(),
        "fn f(a: &Arc<u8>, r: &Rc<u8>) {\n    let a2 = a.share();\n    let r2 = r.local_share();\n    let r3 = Rc::new_in(1, alloc);\n    let r4 = r3.local_share();\n}\n"
    );
    // Re-linting the fixed code produces zero CloneHandle findings!
    out.clear();
    assert!(lint(&only, &Unit { text: fixed.as_str(), holes: &[], scope: FILE }, alloc, &mut out).is_ok());
    assert!(out.list.is_empty(), "fixed code must have zero clone findings");
}

#[test]
fn magic_slice_len_detects_and_autofixes() {
    let rule = Rule::MagicSliceLen;
    let only = RuleSet::only(&[rule]);
    assert_eq!(Rule::from_name("magic-slice-len"), Some(rule));
    assert_eq!(Rule::from_lint("magic_slice_len"), Some(rule));
    assert_eq!(Template::MagicSlicePrefix.rule(), rule);
    assert_eq!(Template::MagicSlicePrefixMismatch.rule(), rule);
    assert_eq!(Template::MagicSliceAdvance.rule(), rule);
    assert_eq!(Template::MagicSliceBytes.rule(), rule);
    assert_eq!(Template::MagicSliceRange.rule(), rule);
    assert!(Step::Edition.rules().runs(rule), "Step::Edition runs magic-slice-len");
    assert!(!Step::Rust.rules().runs(rule));

    let alloc = blaze_own::testing::heap().alloc();
    let mut out = Findings::new_in(alloc);

    // 1. Auto-fixable prefix comparisons (`0..3`, `..4`, `&x[..7] == b".."`, `.get(..4).is_some_and`).
    let autofix_src = "fn f(bytes: &[u8], sect: &[u8]) -> bool { bytes.get(0..3) == Some(&[0xEF, 0xBB, 0xBF]) && bytes.get(..4) == Some(b\"RIFF\") && &sect[..7] == b\"__text\\0\" && bytes.get(..4).is_some_and(|m| m == b\"WEBP\") }";
    assert!(lint(&only, &Unit { text: autofix_src, holes: &[], scope: FILE }, alloc, &mut out).is_ok());
    assert_eq!(out.list.len(), 4, "expected 4 prefix findings");
    assert!(out.list.iter().all(|f| f.template == Template::MagicSlicePrefix));
    let mut msg = OwnString::new_in(alloc);
    show_message(&only, autofix_src, &out, &out.list[0], &mut msg);
    assert!(msg.as_str().contains("strip_prefix(&[0xEF, 0xBB, 0xBF])"), "{}", msg.as_str());
    let fixed = apply_all(autofix_src, &out, alloc).expect("autofix applies cleanly");
    assert_eq!(
        fixed.as_str(),
        "fn f(bytes: &[u8], sect: &[u8]) -> bool { bytes.starts_with(&[0xEF, 0xBB, 0xBF]) && bytes.starts_with(b\"RIFF\") && sect.starts_with(b\"__text\\0\") && bytes.starts_with(b\"WEBP\") }"
    );
    // Re-linting fixed output produces zero findings!
    out.clear();
    assert!(lint(&only, &Unit { text: fixed.as_str(), holes: &[], scope: FILE }, alloc, &mut out).is_ok());
    assert!(out.list.is_empty(), "fixed code must have zero magic-slice-len findings");

    // 2. Cursor prefix (`at..at + 2 == Some(b"//")`), `&sect[..7] == b"__text\0"`, and `.is_some_and`.
    let cursor_src = r#"fn f(bytes: &[u8], at: usize, entry: &[u8]) -> bool {
        if bytes.get(at..at + 2) == Some(b"//") { return true; }
        if &bytes[..7] == b"__text\0" { return true; }
        entry.get(..8).is_some_and(|m| m == MAGIC)
    }"#;
    out.clear();
    assert!(lint(&only, &Unit { text: cursor_src, holes: &[], scope: FILE }, alloc, &mut out).is_ok());
    assert_eq!(out.list.len(), 3, "expected 3 prefix findings");
    assert!(out.list.iter().all(|f| f.template == Template::MagicSlicePrefix));

    // 3. Bug detector: hardcoded length mismatches literal length (`0..4` vs `b"//"`).
    let mismatch_src = "fn f(bytes: &[u8]) -> bool { bytes.get(0..4) == Some(b\"//\") }";
    out.clear();
    assert!(lint(&only, &Unit { text: mismatch_src, holes: &[], scope: FILE }, alloc, &mut out).is_ok());
    assert_eq!(out.list.len(), 1);
    assert_eq!(out.list[0].template, Template::MagicSlicePrefixMismatch);
    assert!(out.list[0].fixes.is_empty(), "mismatched length bug must not silently autofix");

    // 4. `starts_with` followed by hardcoded `[N..]` or `+= N` advance.
    let adv_src = r#"fn f(s: &str, mut at: usize) -> &str {
        if s.starts_with("0x") { return &s[2..]; }
        if s.starts_with("//") { at += 2; }
        let _ = at;
        s
    }"#;
    out.clear();
    assert!(lint(&only, &Unit { text: adv_src, holes: &[], scope: FILE }, alloc, &mut out).is_ok());
    assert_eq!(out.list.len(), 2);
    assert!(out.list.iter().all(|f| f.template == Template::MagicSliceAdvance));

    // 5. Standalone byte-width `.get(at..at + 4)?.try_into()` vs general `.get(0..3)`.
    let standalone_src = r#"fn f(bytes: &[u8], at: usize) -> Option<&[u8]> {
        let _w: [u8; 4] = bytes.get(at..at + 4)?.try_into().ok()?;
        bytes.get(0..3)
    }"#;
    out.clear();
    assert!(lint(&only, &Unit { text: standalone_src, holes: &[], scope: FILE }, alloc, &mut out).is_ok());
    assert_eq!(out.list.len(), 2);
    assert_eq!(out.list[0].template, Template::MagicSliceBytes);
    assert_eq!(out.list[1].template, Template::MagicSliceRange);

    // 6. Negative controls: clean derived code & tests must produce zero findings.
    none(rule, &[
        "fn f(text: &str) -> Option<&str> { text.strip_prefix(\"//\") }",
        "fn f(bytes: &[u8]) -> bool { bytes.starts_with(&[0xEF, 0xBB, 0xBF]) }",
        "fn f(bytes: &[u8]) -> Option<&[u8]> { bytes.get(..MAGIC.len()) }",
        "fn f(bytes: &[u8], at: usize) -> Option<&[u8]> { bytes.get(at..at + HEADER_LEN) }",
        "fn f(bytes: &[u8], at: usize) -> Option<&[u8]> { bytes.get(at..at + size_of::<u32>()) }",
        "fn f(bytes: &[u8], start: usize, end: usize) -> Option<&[u8]> { bytes.get(start..end) }",
        "fn f(bytes: &[u8]) -> Option<&u8> { bytes.get(0) }",
        "#[test]\nfn t() { let b = b\"abc\"; assert!(b.get(0..3) == Some(b\"abc\")); }",
    ]);
}

#[test]
fn bool_as_int_detects_and_autofixes() {
    let rule = Rule::BoolAsInt;
    let only = RuleSet::only(&[rule]);
    assert_eq!(Rule::from_name("bool-as-int"), Some(rule));
    assert_eq!(Rule::from_lint("bool_as_int"), Some(rule));
    assert_eq!(Template::BoolAsInt.rule(), rule);

    let alloc = blaze_own::testing::heap().alloc();
    let mut out = Findings::new_in(alloc);
    let src = "fn f(a: u32, b: u32, opt: Option<u8>) -> usize { (a == b) as usize + opt.is_some() as usize + (a < b) as usize + true as usize }";
    assert!(lint(&only, &Unit { text: src, holes: &[], scope: FILE }, alloc, &mut out).is_ok());
    assert_eq!(out.list.len(), 4);
    let fixed = apply_all(src, &out, alloc).expect("autofix applies cleanly");
    assert_eq!(
        fixed.as_str(),
        "fn f(a: u32, b: u32, opt: Option<u8>) -> usize { usize::from(a == b) + usize::from(opt.is_some()) + usize::from(a < b) + usize::from(true) }"
    );
    out.clear();
    assert!(lint(&only, &Unit { text: fixed.as_str(), holes: &[], scope: FILE }, alloc, &mut out).is_ok());
    assert!(out.list.is_empty());

    none(rule, &[
        "fn f(x: u32) -> u64 { x as u64 }",
        "fn f(x: usize) -> u32 { (x + 1) as u32 }",
        "fn f(x: u32) -> usize { (x << 2) as usize }",
        "fn f(mask: u32) -> u64 { (!mask) as u64 }",
        "fn f() -> u32 { (size_of::<u64>() / size_of::<u32>()) as u32 }",
        "const fn f(a: u32, b: u32) -> u32 { (a == b) as u32 }",
        "fn f(b: bool) -> usize { usize::from(b) }",
    ]);
}

#[test]
fn manual_slice_idioms_detects_and_autofixes() {
    let rule = Rule::ManualSliceIdioms;
    let only = RuleSet::only(&[rule]);
    assert_eq!(Rule::from_name("manual-slice-idioms"), Some(rule));
    assert_eq!(Rule::from_lint("manual_slice_idioms"), Some(rule));

    let alloc = blaze_own::testing::heap().alloc();
    let mut out = Findings::new_in(alloc);

    // 1. `.len() == 0` / `.len() > 0` / `.len() != 0` / `.len() >= 1` -> `is_empty()` / `!is_empty()`.
    let len_src = "fn f(xs: &[u8], ys: &[u8]) -> bool { xs.len() == 0 && ys.len() > 0 && xs.len() != 0 && ys.len() >= 1 }";
    assert!(lint(&only, &Unit { text: len_src, holes: &[], scope: FILE }, alloc, &mut out).is_ok());
    assert_eq!(out.list.len(), 4);
    assert!(out.list.iter().all(|f| f.template == Template::LenZeroCheck));
    let fixed = apply_all(len_src, &out, alloc).expect("autofix applies cleanly");
    assert_eq!(
        fixed.as_str(),
        "fn f(xs: &[u8], ys: &[u8]) -> bool { xs.is_empty() && !ys.is_empty() && !xs.is_empty() && !ys.is_empty() }"
    );
    out.clear();
    assert!(lint(&only, &Unit { text: fixed.as_str(), holes: &[], scope: FILE }, alloc, &mut out).is_ok());
    assert!(out.list.is_empty());

    // 2. Index loop `for i in 0..xs.len() { let _ = xs[i]; }`.
    let loop_src = "fn f(xs: &[u8]) -> u8 { let mut s = 0; for i in 0..xs.len() { s += xs[i]; } s }";
    out.clear();
    assert!(lint(&only, &Unit { text: loop_src, holes: &[], scope: FILE }, alloc, &mut out).is_ok());
    assert_eq!(out.list.len(), 1);
    assert_eq!(out.list[0].template, Template::IndexLoop);

    // 3. `.find('\n')` followed by `[..pos]` / `[pos + 1..]`.
    let find_src = "fn f(s: &str) -> (&str, &str) { if let Some(pos) = s.find(':') { (&s[..pos], &s[pos + 1..]) } else { (s, \"\") } }";
    out.clear();
    assert!(lint(&only, &Unit { text: find_src, holes: &[], scope: FILE }, alloc, &mut out).is_ok());
    assert_eq!(out.list.len(), 1);
    assert_eq!(out.list[0].template, Template::ManualFindSplit);

    none(rule, &[
        "fn f(xs: &[u8], ys: &[u8]) -> bool { xs.is_empty() || xs.len() == 2 || xs.len() >= 1 << 20 || xs.len() + ys.len() >= 1 }",
        "impl S { pub fn is_empty(&self) -> bool { self.len() == 0 } }",
        "fn f(xs: &[u8]) -> u8 { let mut s = 0; for &x in xs { s += x; } s }",
        "fn f(s: &str) -> Option<(&str, &str)> { s.split_once(':') }",
    ]);
}

#[test]
fn test_ambient_leak_catches_test_global_alloc_and_extern_std() {
    let rule = Rule::TestAmbientLeak;
    let only = RuleSet::only(&[rule]);
    assert_eq!(Rule::from_name("test-ambient-leak"), Some(rule));
    assert_eq!(Rule::from_lint("test_ambient_leak"), Some(rule));

    let alloc = blaze_own::testing::heap().alloc();
    let mut out = Findings::new_in(alloc);
    let src = "#[cfg(test)]\nextern crate std;\n#[test]\nfn t() { let _a = Alloc::global(); }\n";
    assert!(lint(&only, &Unit { text: src, holes: &[], scope: FILE }, alloc, &mut out).is_ok());
    assert_eq!(out.list.len(), 2);
    assert_eq!(out.list[0].template, Template::TestExternStd);
    assert_eq!(out.list[1].template, Template::TestGlobalAlloc);

    // Outside tests, `test-ambient-leak` stays silent (other rules cover production code).
    none(rule, &[
        "extern crate std;\nfn prod() { let _a = Alloc::global(); }",
        "#[test]\nfn clean() { let heap = blaze_own::testing::heap(); let _a = heap.alloc(); }",
    ]);
}

#[test]
fn mut_for_uniq_borrow_autofixes_non_rebound_let_mut_and_keeps_rebindings() {
    let rule = Rule::MutForUniqBorrow;
    let only = RuleSet::only(&[rule]);
    assert_eq!(Rule::from_name("mut-for-uniq-borrow"), Some(rule));
    assert_eq!(Rule::from_lint("mut_for_uniq_borrow"), Some(rule));

    let alloc = blaze_own::testing::heap().alloc();
    let mut out = Findings::new_in(alloc);
    let src = "fn demo() -> i32 { let mut n = 41; bump(&uniq n); n }";
    assert!(lint(&only, &Unit { text: src, holes: &[], scope: FILE }, alloc, &mut out).is_ok());
    assert_eq!(out.list.len(), 1);
    assert_eq!(out.list[0].template, Template::MutForUniqBorrow);
    let fixed = apply_all(src, &out, alloc).expect("autofix applies cleanly");
    assert_eq!(fixed.as_str(), "fn demo() -> i32 { let n = 41; bump(&uniq n); n }");

    // Actual local rebinding (`n = ..`, `n += ..`) keeps `let mut`.
    none(rule, &[
        "fn rebound() -> i32 { let mut n = 0; bump(&uniq n); n = 5; n }",
        "fn compound() -> i32 { let mut n = 0; bump(&uniq n); n += 1; n }",
        "fn no_borrow() -> i32 { let mut n = 0; n }",
    ]);
}

#[test]
fn io_ref_denies_general_io_bundle() {
    let rule = Rule::IoRef;
    let only = RuleSet::only(&[rule]);
    assert_eq!(rule.name(), "io-ref");
    assert_eq!(Rule::from_name("io-ref"), Some(rule));
    assert_eq!(rule.lint(), Some("io_ref"));
    assert_eq!(Rule::from_lint("io_ref"), Some(rule));
    assert_eq!(Template::IoRef.rule(), rule);

    assert_eq!(found(&only, "fn f(io: &Io) {}", FILE), [("io-ref", "&Io")]);
    assert_eq!(found(&only, "struct S<'a> { io: &'a Io }", FILE), [("io-ref", "&'a Io")]);
    assert_eq!(found(&only, "fn f(io: &mut Io) {}", FILE), [("io-ref", "&mut Io")]);
    assert_eq!(found(&only, "fn f(io: &bstd::io::Io) {}", FILE), [("io-ref", "&bstd::io::Io")]);
    assert_eq!(found(&only, "fn f(io: &bstd::Io) {}", FILE), [("io-ref", "&bstd::Io")]);

    // Local private `struct Io` exempts unqualified `&Io`, but still flags `&bstd::io::Io`.
    assert!(found(&only, "struct Io;\nfn f(io: &Io) {}", FILE).is_empty());
    assert_eq!(
        found(&only, "struct Io;\nfn f(io: &bstd::io::Io) {}", FILE),
        [("io-ref", "&bstd::io::Io")]
    );

    // Narrow capabilities, owned `Io`, and tests are clean.
    none(rule, &[
        "fn f(out: &Out, fs: &Fs) {}",
        "fn main(io: Io) { drop(io); }",
        "fn f() { let _ = mask & Io; }",
        "#[test]\nfn t(io: &Io) {}",
        "#[cfg(test)]\nmod t { fn f(io: &Io) {} }",
    ]);
}

#[test]
fn unused_import_minimizes_and_autofixes() {
    let rule = Rule::UnusedImport;
    let only = RuleSet::only(&[rule]);
    let alloc = blaze_own::testing::heap().alloc();

    let fix = |src: &str| {
        let mut out = Findings::new_in(alloc);
        crate::lint_file_imports(src, FILE, alloc, &mut out).unwrap();
        lint(&only, &Unit { text: src, holes: &[], scope: FILE }, alloc, &mut out).unwrap();
        apply_all(src, &out, alloc).unwrap()
    };

    // Collapse braced import down to minimal single import (`{Io, Missing}` -> `Missing`).
    assert_eq!(
        fix("use bstd::io::{Io, Missing};\nfn f() -> Missing { todo!() }\n").as_str(),
        "use bstd::io::Missing;\nfn f() -> Missing { todo!() }\n"
    );

    // Collapse `{self, Io}` down to bare module import `use bstd::io;`.
    assert_eq!(
        fix("use bstd::io::{self, Io};\nfn f() { io::init(); }\n").as_str(),
        "use bstd::io;\nfn f() { io::init(); }\n"
    );

    // Prune middle entry from 3-item braced group.
    assert_eq!(
        fix("use bstd::io::{Fs, Io, Out};\nfn f(fs: &Fs, out: &Out) {}\n").as_str(),
        "use bstd::io::{Fs, Out};\nfn f(fs: &Fs, out: &Out) {}\n"
    );

    // Remove redundant braces on single-item braced import.
    assert_eq!(
        fix("use bstd::io::{Missing};\nfn f() -> Missing { todo!() }\n").as_str(),
        "use bstd::io::Missing;\nfn f() -> Missing { todo!() }\n"
    );

    // Delete completely unused single or braced `use` lines cleanly.
    assert_eq!(
        fix("use bstd::io::Io;\nuse bstd::io::Missing;\nfn f() -> Missing { todo!() }\n").as_str(),
        "use bstd::io::Missing;\nfn f() -> Missing { todo!() }\n"
    );
    assert_eq!(
        fix("use bstd::io::{Io, Foo};\nfn f() {}\n").as_str(),
        "fn f() {}\n"
    );

    // Renamed import checks alias; `as _`, `pub use`, globs, and `#[cfg]`/`#[allow]` are untouched.
    assert_eq!(
        fix("use bstd::io::{Io as Unused, Missing as M};\nfn f() -> M { todo!() }\n").as_str(),
        "use bstd::io::Missing as M;\nfn f() -> M { todo!() }\n"
    );
    for clean in [
        "pub use bstd::io::Io;\nfn f() {}\n",
        "pub(crate) use bstd::io::Io;\nfn f() {}\n",
        "use core::fmt::Write as _;\nfn f() {}\n",
        "use bstd::io::*;\nfn f() {}\n",
        "#[cfg(unix)]\nuse bstd::io::Io;\nfn f() {}\n",
        "#[allow(unused_imports)]\nuse bstd::io::Io;\nfn f() {}\n",
    ] {
        assert_eq!(fix(clean).as_str(), clean, "should leave untouched: {clean}");
    }
}

#[test]
fn unused_mut_autofixes_and_preserves_mutations() {
    let rule = Rule::UnusedMut;
    let only = RuleSet::only(&[rule]);
    let alloc = blaze_own::testing::heap().alloc();

    let fix = |src: &str| {
        let mut out = Findings::new_in(alloc);
        lint(&only, &Unit { text: src, holes: &[], scope: FILE }, alloc, &mut out).unwrap();
        apply_all(src, &out, alloc).unwrap()
    };

    // Unnecessary `mut` on `let mut` and `fn` parameter is removed cleanly.
    assert_eq!(
        fix("fn f(mut n: u32) -> u32 {\n    let mut x = n + 1;\n    let mut y: u32 = x + 2;\n    y\n}\n").as_str(),
        "fn f(n: u32) -> u32 {\n    let x = n + 1;\n    let y: u32 = x + 2;\n    y\n}\n"
    );

    // Format string read (`"{x}"`) does not mutate `x`.
    assert_eq!(
        fix("fn f() {\n    let mut x = 42;\n    println!(\"{x}\");\n}\n").as_str(),
        "fn f() {\n    let x = 42;\n    println!(\"{x}\");\n}\n"
    );

    // Every genuinely mutating pattern is preserved untouched.
    for mutated in [
        "fn f() { let mut x = 0; x = 1; }",
        "fn f() { let mut x = 0; x += 1; }",
        "fn f() { let mut x = 0; take(&mut x); }",
        "fn f() { let mut v = Vec::new(); v.push(1); }",
        "fn f() { let mut a = [0; 4]; a[0] = 1; }",
        "fn f() { let mut b = Box::new(0); *b = 1; }",
        "fn f() { let mut x = 0; inc!(x); }",
        "fn f(mut x: u32) -> u32 { x += 1; x }",
        "fn f() { let mut _x = 0; }",
        "fn f() { #[allow(unused_mut)] let mut x = 0; let _ = x; }",
        "fn f() { let mut x = 0; let x = bump(&mut x); let _ = x; }",
    ] {
        assert_eq!(fix(mutated).as_str(), mutated, "should keep mut: {mutated}");
    }
}

#[test]
fn heap_leak_flags_leak_and_allows_root_and_bump() {
    let rule = Rule::HeapLeak;
    let only = RuleSet::only(&[rule]);
    assert_eq!(rule.name(), "heap-leak");
    assert_eq!(Rule::from_name("heap-leak"), Some(rule));
    assert_eq!(rule.lint(), Some("heap_leak"));
    assert_eq!(Rule::from_lint("heap_leak"), Some(rule));
    assert_eq!(Template::HeapLeak.rule(), rule);

    assert_eq!(found(&only, "fn f() { let _ = v.leak(); }", FILE), [("heap-leak", ".leak")]);
    assert_eq!(found(&only, "fn f() { let _ = Box::leak(b); }", FILE), [("heap-leak", "Box::leak")]);
    assert_eq!(found(&only, "fn f() { let _ = Vec::leak(v); }", FILE), [("heap-leak", "Vec::leak")]);

    none(rule, &[
        "fn f(frame: &Frame) { let _ = v.into_bump_slice(); }",
        "fn f(frame: &Frame) { let _ = s.into_bump_str(); }",
        "fn f(root: Root) { let _ = Root::leak(root); }",
        "#[test]\nfn t() { let _ = v.leak(); }",
    ]);
}

#[test]
fn jit_telemetry_driven_perf_lints_rank_soa_and_hot_paths() {
    use crate::pgo::{AdviceKind, Heat, HotSite, JitObservation, JitTelemetryKind, PgoHints, suggest_from_jit};

    let alloc = blaze_own::testing::heap().alloc();
    let hints = PgoHints::from_sites(
        &[
            HotSite::for_symbol("sim::step_particles", 900_000, 120_000), // Critical (12%)
            HotSite::for_symbol("rpc::decode_frame", 200_000, 25_000),    // Hot (2.5%)
            HotSite::for_symbol("cli::rare_help", 10, 50),                // Cold (0.005%)
        ],
        alloc,
    );

    let obs = [
        // Critical 64B AoS loop touching only 8B (87.5% cache-line waste -> UseSoaLayout!)
        JitObservation::for_symbol(
            "sim::step_particles",
            Span::new(10, 60),
            JitTelemetryKind::AosStrideLoop { stride_bytes: 64, accessed_bytes: 8, trip_count: 500_000 },
        ),
        // Critical non-escaping heap alloc in hot loop
        JitObservation::for_symbol(
            "sim::step_particles",
            Span::new(70, 90),
            JitTelemetryKind::NonEscapingHeapAlloc { alloc_bytes: 256, executions: 100_000 },
        ),
        // Hot 99.9% skewed error branch -> UseColdErrorPath / Conv::BlazeResult
        JitObservation::for_symbol(
            "rpc::decode_frame",
            Span::new(100, 140),
            JitTelemetryKind::SkewedBranch { taken_ppm: 1_000, executions: 250_000 },
        ),
        // Hot oversized async frame (512 B -> graph-colored 128 B)
        JitObservation::for_symbol(
            "rpc::decode_frame",
            Span::new(150, 200),
            JitTelemetryKind::OversizedAsyncFrame { frame_bytes: 512, colored_bytes: 128, polls: 80_000 },
        ),
        // Hot skewed integer divisor (95% divisor == 10)
        JitObservation::for_symbol(
            "rpc::decode_frame",
            Span::new(210, 240),
            JitTelemetryKind::SkewedDivisor { dominant_divisor: 10, dominant_ppm: 950_000, executions: 100_000 },
        ),
        // Hot indirect gather loop with 25% cache miss rate
        JitObservation::for_symbol(
            "rpc::decode_frame",
            Span::new(250, 300),
            JitTelemetryKind::IndirectGatherLoop { miss_rate_ppm: 250_000, trip_count: 50_000 },
        ),
        // Negative controls: cold AoS loop (filtered by min_heat), and dense AoS (48/64 = 75% accessed)
        JitObservation::for_symbol(
            "cli::rare_help",
            Span::new(400, 440),
            JitTelemetryKind::AosStrideLoop { stride_bytes: 64, accessed_bytes: 8, trip_count: 500_000 },
        ),
        JitObservation::for_symbol(
            "sim::step_particles",
            Span::new(450, 490),
            JitTelemetryKind::AosStrideLoop { stride_bytes: 64, accessed_bytes: 48, trip_count: 500_000 },
        ),
    ];

    let adv = suggest_from_jit(&obs, &hints, Heat::Hot, alloc);
    assert_eq!(adv.len(), 6, "expected exactly 6 hot JIT findings, got {adv:?}");
    // Critical findings come strictly before Hot findings.
    assert_eq!(adv[0].heat, Heat::Critical);
    assert_eq!(adv[1].heat, Heat::Critical);
    assert!(adv[0].impact_score >= adv[1].impact_score);
    let soa = adv.iter().find(|a| a.kind == AdviceKind::UseSoaLayout).expect("missing UseSoaLayout");
    assert_eq!(soa.waste_pct, 87);
    assert_eq!(soa.est_speedup_x10, 80); // 64/8 = 8.0x capped at 80
    assert!(soa.kind.message().contains("Struct-of-Arrays"));

    assert!(adv.iter().any(|a| a.kind == AdviceKind::PromoteHotHeapAlloc));
    assert!(adv.iter().any(|a| a.kind == AdviceKind::UseColdErrorPath));
    assert!(adv.iter().any(|a| a.kind == AdviceKind::ShrinkAsyncFrame && a.waste_pct == 75));
    assert!(adv.iter().any(|a| a.kind == AdviceKind::SpecializeHotDivisor));
    assert!(adv.iter().any(|a| a.kind == AdviceKind::PrefetchHotGather));
}
