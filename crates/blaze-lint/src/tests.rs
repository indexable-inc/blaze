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

use crate::{
    FixError, Findings, Rule, RuleSet, Scope, Span, Template, TestBits, Unit, apply, apply_all, lint, show_message,
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

/// The fixes of `text`'s findings applied.
fn fixed(text: &str) -> StdString {
    let alloc = blaze_own::testing::heap().alloc();
    let mut out = Findings::new_in(alloc);
    let unit = Unit { text, holes: &[], scope: FILE };
    assert!(lint(&RuleSet::ALL, &unit, alloc, &mut out).is_ok());
    match apply_all(text, &out, alloc) {
        Ok(new) => new.as_str().to_string(),
        Err(error) => panic!("fixes refused on {text:?}: {error:?}"),
    }
}

#[test]
fn renamed() {
    let rule = Rule::Renamed;
    let typed = "fn f(al: A) {\n    let mut v: AVec<'_, u8> = AVec::new_in(al);\n    v.insert(0, 1);\n}";
    assert_eq!(kinds(rule, typed), ["fixed"]);
    assert_eq!(fixed(typed), "fn f(al: A) {\n    let mut v: AVec<'_, u8> = AVec::new_in(al);\n    v.insert_shift(0, 1);\n}");
    let param = "fn f(v: &mut AVec<'_, u8>) -> bool {\n    v.contains(&1)\n}";
    assert_eq!(fixed(param), "fn f(v: &mut AVec<'_, u8>) -> bool {\n    v.scan_for(&1).is_some()\n}");
    let init = "fn f(al: A) {\n    let mut d = ADeque::new_in(al);\n    d.remove(0);\n}";
    assert_eq!(fixed(init), "fn f(al: A) {\n    let mut d = ADeque::new_in(al);\n    d.remove_shift(0);\n}");
    assert_eq!(fixed("fn f() { AVec::swap_remove(&mut v, 0); }"), "fn f() { AVec::remove_swap(&mut v, 0); }");
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
        let linted = match lint(&RuleSet::ALL, &unit, alloc, &mut out) {
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

const STATEMENTS: [&str; 34] = [
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
        match tape.below(9, "item") {
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
    for (start, end) in &old_tokens {
        match edits.iter().position(|(r, _)| !r.is_empty() && (r.start, r.end) == (*start, *end)) {
            Some(at) => {
                used[at] = true;
                let (put, _) = significant(edits[at].1);
                expected.extend(put.iter().map(|(a, b)| edits[at].1[*a as usize..*b as usize].to_string()));
            }
            None => expected.push(text[*start as usize..*end as usize].to_string()),
        }
        for (at, (r, put)) in edits.iter().enumerate() {
            if r.is_empty() && r.start == *end {
                used[at] = true;
                let (tokens, _) = significant(put);
                expected.extend(tokens.iter().map(|(a, b)| put[*a as usize..*b as usize].to_string()));
            }
        }
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
