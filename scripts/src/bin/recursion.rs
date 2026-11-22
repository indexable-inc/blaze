//! recursion: the functions that call themselves only go down, per crate, and the new item
//! layer's crates have none (AGENTS.md bounded-progress, rule 18). `DOC` below is what its usage
//! error prints; it says what a recursion is, what syntax cannot see, and the rule.
//!
//!     blaze run -q -p scripts --bin recursion -- '{"command": "check", "base": "origin/main"}'
//!     blaze run -q -p scripts --bin recursion -- '{"command": "write"}'
//!     blaze run -q -p scripts --bin recursion -- '{"command": "findings"}'
//!     blaze run -q -p scripts --bin recursion -- selftest
//!
//! Why (2026-11-15): a compiler that recurses on its input overflows the stack on deep input,
//! and the stack the OS gives is no bound a test can hold: that night a JSON reader did exactly
//! that under its own "bounded" test. A walk over input is a loop over an explicit worklist, or
//! a recursion with a depth bound that returns a typed error at the bound. Test code, the
//! scanner and the list are `blaze_script::ratchet`'s.

/// What the usage error prints.
const DOC: &str = r#"recursion: the functions that call themselves only go down, per crate, and the new layer's crates have none (AGENTS.md bounded-progress, rule 18).

    blaze run -q -p scripts --bin recursion -- '{"command": "check", "base": "<rev>"}'
    blaze run -q -p scripts --bin recursion -- '{"command": "write"}'
    blaze run -q -p scripts --bin recursion -- '{"command": "findings"}'
    blaze run -q -p scripts --bin recursion -- selftest

    command   check     each crate's count equals its row's in the list: one over its row is
                        refused (its new functions named given `base`), one under it must be
                        lowered with `write`; a crate with no row is held to 0; a crate of the
                        new layer (below) is held to 0 whatever the list says, each function
                        named; with `base`, no row is above the base's (CI)
              write     write the list from the tree as it is
              findings  print every function as a `blaze_script::finding` document (`advise`)
    root      the checkout (".")
    base      the commit the change is against, or null

The list's row is `<crate><TAB><functions>`. The new layer, held at 0: blaze-surface,
blaze-file-items, blaze-scope, blaze-impls, blaze-expand, blaze-typeck, blaze-drive, blaze-run,
blaze-data, blaze-json, blaze-cbor, blaze-toml, blaze-own, blaze-kit, blaze-text.

A function counts once, at the line of its `fn`, when its own body (not a function item nested
in it; a closure in it is its body) calls it by syntax:
  - a free function (not in an `impl` or `trait` block): `name(..)` or `self::name(..)`,
    `name::<T>(..)` too;
  - a function of an `impl` or `trait` block: `Self::name(..)`, or `self.name(..)` when it takes
    `self`.
Comments, strings and characters are skipped. Syntax resolves no names, so it misses:
  - mutual recursion (`a` calls `b` calls `a`): out of reach of syntax, which sees one body at
    a time;
  - a call through another receiver of the same type (`child.walk()`, `self.left.walk()`),
    through a path naming the type (`Tree::walk(..)`, `crate::m::f(..)`), a function pointer,
    a trait object or another impl of the trait;
  - a function a macro makes (`fn $name`);
and over-counts a free function whose body calls a local closure or variable of its own name,
and a method that calls another impl's method of its name (`Extend<&T>::extend` calling
`Extend<T>`'s): the comment `// recursion: calls another impl`, within 3 lines above the `fn`,
says so and drops it.
blaze's lint (`blaze::recursion`, from the call graph) is the definition once blaze checks its
own crates; this is the count the gate can hold before then.

Test code is `blaze_script::ratchet`'s: files under `tests` or `benches`, `tests.rs`, `test.rs`,
`#![cfg(test)]` files, the items after `#[cfg(test)]` and `#[test]`, and modules test code
declares. A crate is the nearest directory under crates/ with a Cargo.toml.
"#;

use std::{collections::BTreeMap, path::PathBuf};

use blaze_script::{
    TempDir, fail,
    finding::{self, Finding},
    input::{self, Fields, Input, InputError},
    ratchet::{self, Hit, Policy, Run, Tests, close_of, comment_end, ident_len, is_ident, quote_end, string_end, word_end},
};

const SCRIPT: &str = "recursion";
/// The list, from the root.
const LIST: &str = "tools/ratchets/recursion.txt";
/// The new item layer's crates: held at 0.
const NEW_LAYER: [&str; 15] = [
    "blaze-surface",
    "blaze-file-items",
    "blaze-scope",
    "blaze-impls",
    "blaze-expand",
    "blaze-typeck",
    "blaze-drive",
    "blaze-run",
    "blaze-data",
    "blaze-json",
    "blaze-cbor",
    "blaze-toml",
    "blaze-own",
    "blaze-kit",
    "blaze-text",
];
const RULE: &str = "AGENTS.md bounded-progress (rule 18): a recursion on its input's depth has no bound but the stack, \
                    and overflows it on deep input";
const INSTEAD: &str = "walk with an explicit worklist (a `Vec` stack in the task's frame, pushed and popped in a loop), \
                       or bound the depth and return a typed error at the bound";
const KIND: finding::Kind = finding::Kind {
    name: "direct",
    label: "direct recursions",
    rule: "18 bounded-progress",
    instead: "an explicit worklist in the task's frame, or a depth bound with a typed error",
    mechanical: false,
};
/// The list's first lines.
const HEADER: &str = "# The functions that call themselves directly, outside test code, by crate (its directory\n\
                      # under crates/), read from the syntax (mutual recursion is out of its reach). Only shrinks; a\n\
                      # crate with no row is held to 0, and the new layer's crates are held to 0 whatever it says\n\
                      # (AGENTS.md bounded-progress). Written by scripts/src/bin/recursion.rs (`{\"command\":\n\
                      # \"write\"}`), checked in CI.\n\
                      # crate\tfunctions\n";

struct Args {
    command: String,
    root: String,
    base: Option<String>,
}

impl Input for Args {
    const FIELDS: &'static [&'static str] = &["command", "root", "base"];

    fn parse(f: &Fields<'_>) -> Result<Args, InputError> {
        Ok(Args {
            command: f.one_of("command", &["check", "write", "findings"], None)?,
            root: f.str("root", Some("."))?,
            base: f.str_or_null("base")?,
        })
    }
}

/// A function item: its name, the line of its `fn`, whether it is in an `impl` or `trait`
/// block, whether it takes `self`, and whether its body calls it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FnItem {
    name: String,
    line: usize,
    assoc: bool,
    has_self: bool,
    recursive: bool,
}

/// What a `{` opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Frame {
    /// The body of the function at this index of the file's items.
    Fn(usize),
    /// An `impl` or `trait` block.
    Container,
    /// Any other block.
    Other,
}

/// How a call names its function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Called {
    /// `name(..)`.
    Bare,
    /// `self::name(..)`.
    SelfModule,
    /// `Self::name(..)`.
    SelfType,
    /// `self.name(..)`.
    SelfMethod,
    /// Through any other path or receiver.
    Other,
}

/// A signature being read: its function's index, and its bracket depth.
struct Signature {
    item: usize,
    depth: usize,
}

/// The function items of `text`, each marked when its body calls it.
fn functions(text: &str) -> Vec<FnItem> {
    let bytes = text.as_bytes();
    let mut items: Vec<FnItem> = Vec::new();
    let mut frames: Vec<Frame> = Vec::new();
    // The functions whose bodies are open, innermost last.
    let mut bodies: Vec<usize> = Vec::new();
    let mut signature: Option<Signature> = None;
    // An `impl` or `trait` was read and its block is not yet open.
    let mut container = false;
    let mut line = 1;
    let mut counted = 0;
    let mut at = 0;
    // Bounded: every turn moves `at` forward, to the end of the text at most.
    while at < bytes.len() {
        let byte = bytes[at];
        let next = bytes.get(at + 1).copied();
        at = match byte {
            b'/' if next == Some(b'/') => text[at..].find('\n').map_or(bytes.len(), |n| at + n),
            b'/' if next == Some(b'*') => comment_end(bytes, at),
            b'"' => string_end(bytes, at + 1),
            b'\'' => quote_end(text, at),
            b'(' | b'[' => {
                if let Some(sig) = &mut signature {
                    sig.depth += 1;
                }
                at + 1
            }
            b')' | b']' => {
                if let Some(sig) = &mut signature {
                    sig.depth = sig.depth.saturating_sub(1);
                }
                at + 1
            }
            b'{' => {
                let frame = match signature.take() {
                    Some(sig) if sig.depth == 0 => {
                        bodies.push(sig.item);
                        Frame::Fn(sig.item)
                    }
                    // A brace inside a signature's brackets (a const generic's block): not its body.
                    Some(sig) => {
                        signature = Some(sig);
                        Frame::Other
                    }
                    None if container => Frame::Container,
                    None => Frame::Other,
                };
                container = false;
                frames.push(frame);
                at + 1
            }
            b'}' => {
                if let Some(Frame::Fn(_)) = frames.pop() {
                    bodies.pop();
                }
                at + 1
            }
            b';' => {
                if signature.as_ref().is_some_and(|sig| sig.depth == 0) {
                    // A signature with no body: a trait's required function, or an extern one.
                    signature = None;
                }
                container = false;
                at + 1
            }
            b'_' | b'a'..=b'z' | b'A'..=b'Z' => {
                let end = word_end(text, at);
                let word = &text[at..end];
                match word {
                    "fn" if signature.is_none() => {
                        let rest = text[end..].trim_start();
                        let name_len = ident_len(rest.as_bytes());
                        // `fn(u8)`, a pointer type, or `fn $name`, a macro's: no item.
                        if name_len > 0 && !rest.as_bytes()[0].is_ascii_digit() {
                            line += text[counted..at].matches('\n').count();
                            counted = at;
                            let assoc = frames.iter().rev().find(|it| **it != Frame::Other).is_some_and(|it| *it == Frame::Container);
                            items.push(FnItem { name: rest[..name_len].to_owned(), line, assoc, has_self: false, recursive: false });
                            signature = Some(Signature { item: items.len() - 1, depth: 0 });
                        }
                    }
                    "self" => {
                        if let Some(sig) = &signature {
                            if sig.depth == 1 {
                                items[sig.item].has_self = true;
                            }
                        }
                    }
                    "impl" | "trait" if signature.is_none() => container = true,
                    _ => {}
                }
                if let Some(&inner) = bodies.last() {
                    if signature.is_none() && items[inner].name == word && !items[inner].recursive {
                        let called = call_of(text, at, end);
                        let item = &mut items[inner];
                        item.recursive = match called {
                            Some(Called::Bare | Called::SelfModule) => !item.assoc,
                            Some(Called::SelfType) => item.assoc,
                            Some(Called::SelfMethod) => item.assoc && item.has_self,
                            Some(Called::Other) | None => false,
                        };
                    }
                }
                end
            }
            // A number's digits and suffix (`1u8`, `0x1F`): never the start of a word.
            b'0'..=b'9' => at + ident_len(&bytes[at..]),
            _ => at + 1,
        };
    }
    items
}

/// How the word from byte `at` to `end` is called, or `None` when it is not called (`(` after
/// it, or `::<..>` then `(`): by what is before it.
fn call_of(text: &str, at: usize, end: usize) -> Option<Called> {
    let mut after = text[end..].trim_start();
    if let Some(generic) = after.strip_prefix("::") {
        let generic = generic.trim_start();
        if !generic.starts_with('<') {
            return None;
        }
        // The turbofish's `>`, nested `<..>` counted.
        let mut depth = 0usize;
        let close = generic.bytes().position(|b| {
            match b {
                b'<' => depth += 1,
                b'>' => depth = depth.saturating_sub(1),
                _ => {}
            }
            depth == 0
        })?;
        after = generic[close + 1..].trim_start();
    }
    if !after.starts_with('(') {
        return None;
    }
    let before = text[..at].trim_end();
    if let Some(dot) = before.strip_suffix('.') {
        if dot.ends_with('.') {
            return Some(Called::Other);
        }
        let dot = dot.trim_end();
        let run = dot.bytes().rev().take_while(|b| is_ident(*b)).count();
        let receiver = &dot[dot.len() - run..];
        let chained = dot[..dot.len() - run].trim_end().ends_with('.');
        return Some(if receiver == "self" && !chained { Called::SelfMethod } else { Called::Other });
    }
    if let Some(path) = before.strip_suffix("::") {
        let path = path.trim_end();
        let run = path.bytes().rev().take_while(|b| is_ident(*b)).count();
        let segment = &path[path.len() - run..];
        let longer = path[..path.len() - run].trim_end().ends_with("::");
        return Some(match segment {
            "Self" if !longer => Called::SelfType,
            "self" if !longer => Called::SelfModule,
            _ => Called::Other,
        });
    }
    let run = before.bytes().rev().take_while(|b| is_ident(*b)).count();
    // `fn name(`: the item itself, not a call; `$name(`: a macro's fragment.
    if &before[before.len() - run..] == "fn" || before.ends_with('$') {
        return None;
    }
    Some(Called::Bare)
}

/// The comment that says a function's call of its own name is another impl's function (an
/// `Extend<&T>` that calls `Extend<T>`'s `extend`): syntax over-counts it, and the author says so.
const MARK: &str = "// recursion: calls another impl";
/// How many lines above its `fn` the mark may be (its attributes and docs between).
const LOOK_UP: usize = 3;

/// The recursive functions of a file's text, as (line, name), less those marked with [`MARK`].
fn recursive(text: &str) -> Vec<(usize, String)> {
    let lines: Vec<&str> = text.split('\n').collect();
    let marked = |line: usize| lines[line.saturating_sub(1 + LOOK_UP)..line.saturating_sub(1)].iter().any(|it| it.trim_start().starts_with(MARK));
    functions(text).into_iter().filter(|it| it.recursive && !marked(it.line)).map(|it| (it.line, it.name)).collect()
}

fn named(it: &Hit<String>) -> String {
    format!("{}:{} `{}`", it.path, it.line, it.what)
}

fn summary(counts: &BTreeMap<String, Vec<Hit<String>>>) -> String {
    let total: usize = counts.values().map(Vec::len).sum();
    format!("{total} functions that call themselves outside tests, in {} crates", counts.len())
}

const POLICY: Policy<'static> = Policy {
    list: LIST,
    noun: "functions that call themselves directly",
    instead: INSTEAD,
    rule: RULE,
    at_zero: &NEW_LAYER,
};

fn main_with(tests: &Tests, it: &Args) {
    let root: PathBuf =
        std::fs::canonicalize(&it.root).unwrap_or_else(|_| std::env::current_dir().unwrap_or_default().join(&it.root));
    if !root.join("crates").is_dir() {
        fail(SCRIPT, &format!("{} has no crates/ directory", root.display()));
    }
    let counts = ratchet::walk(SCRIPT, &root, tests, recursive);
    if it.command == "findings" {
        let found = counts.iter().flat_map(|(krate, sites)| {
            sites.iter().map(|it| Finding { krate: krate.clone(), file: it.path.clone(), line: it.line, kind: KIND.name, detail: Some(it.what.clone()) })
        });
        finding::print(SCRIPT, &[KIND], found.collect());
        return;
    }
    let summary = summary(&counts);
    let run = Run { script: SCRIPT, policy: &POLICY, header: HEADER, root: &root, write: it.command == "write", base: it.base.as_deref(), summary: &summary };
    ratchet::write_or_check(&run, &counts, named);
}

fn expect(holds: bool, what: &str) {
    blaze_script::expect(SCRIPT, holds, what);
}

/// The selftest's file. Line by line:
/// - 1, `fact` calls `fact(..)`: recursive;
/// - 2, calls another function: not;
/// - 5, `self.walk(..)` in a method taking `self`: recursive;
/// - 6, `Self::build(..)`: recursive;
/// - 7, `self.items.len()` in `len`: another receiver, not;
/// - 8, the method `fact` calls the free `fact(..)`: not itself;
/// - 10, a trait's provided method calling `self.go()`: recursive;
/// - 11, `outer` called from the function nested in it: a nested function's body is its own, not;
/// - 12, `generic::<T>(..)`: recursive;
/// - 13, `self::path(..)`: recursive;
/// - 14, its name in a string and a comment: not;
/// - 15, a call through a function pointer of another name: not;
/// - 16, `fn $f` a macro makes: no item;
/// - 18 and 20, test code;
/// - 21, two calls: once.
const FIXTURE: &str = r#"fn fact(n: u64) -> u64 { if n == 0 { 1 } else { n * fact(n - 1) } }
fn not_rec(n: u64) -> u64 { other(n) }
struct T;
impl T {
    fn walk(&self, d: u32) { if d > 0 { self.walk(d - 1) } }
    fn build(d: u32) -> T { if d > 0 { Self::build(d - 1) } else { T } }
    fn len(&self) -> usize { self.items.len() }
    fn fact(&self) -> u64 { fact(3) }
}
trait W { fn go(&self) { self.go() } fn need(&self); }
fn outer() { fn inner() { outer() } inner() }
fn generic<T>(x: T) { generic::<T>(x) }
fn path(n: u32) { self::path(n) }
fn s() { let _ = "s()"; /* s() */ }
fn by_fn_ptr(f: fn(u8)) { f(1) }
macro_rules! m { ($f:ident) => { fn $f() { $f() } } }
#[cfg(test)]
mod tests { fn t(n: u32) { t(n) } }
#[test]
fn rec_test() { rec_test() }
fn twice(n: u32) { twice(n); twice(n) }
"#;

/// [`FIXTURE`]'s recursive functions outside test code.
const FIXTURE_SITES: [(usize, &str); 7] = [(1, "fact"), (5, "walk"), (6, "build"), (10, "go"), (12, "generic"), (13, "path"), (21, "twice")];

fn selftest(tests: &Tests) {
    let all: Vec<(usize, String)> = recursive(FIXTURE);
    let mut want: Vec<(usize, String)> = FIXTURE_SITES.iter().map(|(line, name)| (*line, (*name).to_owned())).collect();
    want.extend([(18, "t".to_owned()), (20, "rec_test".to_owned())]);
    want.sort();
    expect(all == want, &format!("the fixture's recursions, test code in: found {all:?}, wanted {want:?}"));
    let items = functions(FIXTURE);
    let of = |name: &str| items.iter().find(|it| it.name == name).cloned();
    expect(of("walk").is_some_and(|it| it.assoc && it.has_self) && of("build").is_some_and(|it| it.assoc && !it.has_self), "methods and their `self`");
    expect(of("need").is_some_and(|it| it.assoc && !it.recursive) && of("inner").is_some_and(|it| !it.assoc), "a required function, a nested free one");
    // Planted: each cause removed, its effect goes.
    expect(!recursive(&FIXTURE.replace("self.walk(d - 1)", "other.walk(d - 1)")).iter().any(|it| it.0 == 5), "`walk` on another receiver is not seen");
    expect(recursive(&FIXTURE.replace("impl T {", "mod T {")).iter().any(|it| it.0 == 8), "`fact` outside an impl calls the free `fact`: itself");
    let marked = recursive(&FIXTURE.replace("struct T;", MARK));
    // The mark on line 3 covers the `fn`s on lines 4 to 6: `walk` and `build`, not `go` on 10.
    expect(
        !marked.iter().any(|it| it.0 == 5 || it.0 == 6) && marked.iter().any(|it| it.0 == 10),
        &format!("the mark drops `walk` and `build`, 2 and 3 lines below it, and not `go`: {marked:?}"),
    );
    expect(close_of("(a[b])", 0) == Some(5), "a bracket's close");

    // A tree: a crate of the old layer held to its row, one of the new layer held at 0.
    let tmp = TempDir::new("recursion-selftest-").unwrap_or_else(|e| fail(&format!("{SCRIPT} selftest"), &e.to_string()));
    let put = |path: &str, text: &str| {
        let full = tmp.path().join(path);
        let made = full.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| std::fs::write(&full, text));
        made.unwrap_or_else(|e| fail(&format!("{SCRIPT} selftest"), &format!("{path} is not written: {e}")));
    };
    put("Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n");
    put("crates/a/Cargo.toml", "[package]\nname = \"a\"\n");
    put("crates/a/src/lib.rs", FIXTURE);
    put("crates/a/tests/it.rs", "fn r() { r() }\n");
    put("crates/blaze-json/Cargo.toml", "[package]\nname = \"blaze-json\"\n");
    put("crates/blaze-json/src/lib.rs", "pub fn value(depth: u32) { if depth > 0 { value(depth - 1) } }\n");
    let counts = ratchet::walk(SCRIPT, tmp.path(), tests, recursive);
    let lines = |krate: &str| counts.get(krate).map(|it| it.iter().map(|it| it.line).collect::<Vec<usize>>()).unwrap_or_default();
    expect(lines("a") == FIXTURE_SITES.map(|it| it.0) && lines("blaze-json") == [1], &format!("the tree's recursions: {counts:?}"));
    let rows = ratchet::parse_rows(LIST, &ratchet::render(HEADER, &counts, &NEW_LAYER)).unwrap_or_else(|e| fail(SCRIPT, &e.to_string()));
    expect(rows == BTreeMap::from([("a".to_owned(), 7)]), &format!("the rows, the new layer's left out: {rows:?}"));
    let said = ratchet::problems(&POLICY, &counts, &rows, Some(&rows), None, named);
    expect(
        said.len() == 1 && said[0].contains("`blaze-json` is held at 0") && said[0].contains("crates/blaze-json/src/lib.rs:1 `value`") && said[0].contains("worklist"),
        &format!("the new layer's recursion is named: {said:?}"),
    );
    let fewer = BTreeMap::from([("a".to_owned(), 6)]);
    let said = ratchet::problems(&POLICY, &counts, &fewer, None, None, named);
    expect(said.iter().any(|it| it.contains("`a` has 7 functions that call themselves directly outside tests, over its row's 6")), &format!("a rise: {said:?}"));
    println!("{SCRIPT} selftest: ok");
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let tests = Tests::new(SCRIPT);
    match args.as_slice() {
        [one] if one == "selftest" => selftest(&tests),
        [one] => main_with(&tests, &input::parse::<Args>(one, SCRIPT)),
        _ => {
            eprintln!("usage: {SCRIPT} '<json>' | selftest\n\n{DOC}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The script's own cases run under `blaze test` and `cargo test` too.
    #[test]
    fn the_selftest_holds() {
        selftest(&Tests::new(SCRIPT));
    }
}
