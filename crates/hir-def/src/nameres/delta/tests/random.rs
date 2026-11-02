//! Maintained = fresh after random edit sequences (design §12): Hegel edit scripts over the
//! fixtures of the name resolution and macro expansion tests, and over generated crates. After
//! every write, each crate's maintained map equals a fresh maintainer's exactly and
//! `facts::collect_crate`'s up to §6 ([`super::check_db`]).
//!
//! A fixture is edited as text, at the places an editor would: an item inserted between items,
//! a line holding whole items removed, an identifier renamed. A generated crate is edited on its
//! model (`facts/tests/generated.rs`), rendered again and written file by file, with derives,
//! impls and traits added on top of what the model writes.
//!
//! Each script test runs a fixed number of cases; `HEGEL_TEST_CASES=<n>` runs `n` instead, for a
//! long run. A failure is shrunk to the shortest script that still breaks.

use std::{
    panic::{self, AssertUnwindSafe},
    sync::OnceLock,
};

use blaze_ids::rng::SplitMix64;
use hegel::generators as gs;
use hegel::{Hegel, Settings, TestCase};
use hir_expand::fixture;
use span::FileId;
use test_utils::FixtureWithProjectMeta;

use super::{Loaded, check_db, facts_exempt, summary};
use crate::nameres::facts::tests::{
    Exempt, extra_proc_macros,
    differential::{MACRO_EXPANSION, NAMERES, fixtures},
    generated::{Gen, MODULE_NAMES, MacroBody, Model, Module, TYPE_NAMES, UseTail, VALUE_NAMES},
    with_fast_path,
};

/// Runs `property` over `cases` scripts, or `HEGEL_TEST_CASES` of them where that is set.
fn run_scripts(cases: u64, property: impl FnMut(TestCase)) {
    let cases = std::env::var("HEGEL_TEST_CASES").ok().and_then(|it| it.parse().ok()).unwrap_or(cases);
    Hegel::new(property).settings(Settings::new().test_cases(cases).database(None)).run();
}

/// Checks `loaded`'s database, failing the case with `what` and the step.
fn check(loaded: &Loaded, what: &str, exempt: &Exempt) {
    let mut failures = String::new();
    if let Err(payload) =
        panic::catch_unwind(AssertUnwindSafe(|| check_db(&loaded.db, what, exempt, true, &mut failures)))
    {
        panic!("{what}: panicked: {}", message(&*payload));
    }
    assert!(failures.is_empty(), "{failures}");
}

// dyn: a panic's payload, on a failing case only
fn message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|it| (*it).to_owned()))
        .unwrap_or_else(|| "(no message)".to_owned())
}

/// Writes every file of `texts` whose text differs from the database's, in one batch: the
/// maintainer runs once, over all of them, as for a save of several files.
fn write(loaded: &mut Loaded, texts: &[(FileId, String)]) -> bool {
    let changed: Vec<&(FileId, String)> =
        texts.iter().filter(|(file, text)| loaded.text(*file) != *text).collect();
    if changed.is_empty() {
        return false;
    }
    loaded.db.batch(|db| {
        for (file, text) in changed {
            db.set_file_text(*file, text);
        }
    });
    true
}

// ---------------------------------------------------------------------------------------------
// Text edits of a fixture.

/// Items an edit inserts: every kind §12 names (items, uses, globs, `mod`s, `macro_rules!`,
/// derives, impls), with names the fixtures use, so they meet what is there.
const SNIPPETS: &[&str] = &[
    "pub struct S;",
    "struct T;",
    "pub fn foo() {}",
    "pub fn S() {}",
    "pub enum E { A, B }",
    "pub use E::*;",
    "pub use crate::*;",
    "use self::*;",
    "pub use super::*;",
    "use crate::foo;",
    "pub use crate::S as Renamed;",
    "use crate::m::*;",
    "pub(crate) use crate::m::S as MS;",
    "pub mod m { pub struct S; pub use super::*; }",
    "mod n { pub fn foo() {} pub use crate::m::*; }",
    "macro_rules! mac { () => { pub struct FromMac; } }",
    "mac!();",
    "#[macro_export] macro_rules! exported { () => { pub mod made { pub struct G; pub use $crate::*; } } }",
    "crate::exported!();",
    "#[macro_use] mod mu { macro_rules! inner { () => { pub fn from_inner() {} } } }",
    "inner!();",
    "#[derive(Clone, Copy)] pub struct D;",
    "#[derive(Debug)] enum DE { X }",
    "impl S {}",
    "impl Clone for T { fn clone(&self) -> Self { T } }",
    "pub trait Tr { fn method(&self); }",
    "impl Tr for S { fn method(&self) {} }",
    "extern crate self as me;",
    "use me::S as ViaSelf;",
    "#[cfg(never)] pub struct Gone;",
    "#[cfg(never)] use crate::m::*;",
];

const RENAMES: &[&str] = &["S", "T", "foo", "m", "Renamed", "a", "b", "E", "mac"];

const KEYWORDS: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern",
    "false", "fn", "for", "if", "impl", "in", "let", "loop", "macro_rules", "match", "mod", "move",
    "mut", "pub", "ref", "return", "self", "Self", "static", "struct", "super", "trait", "true",
    "type", "union", "unsafe", "use", "where", "while",
];

#[derive(Debug, Clone, Copy)]
enum TextEdit {
    /// `snippet` inserted at an item boundary of a file, both picked by `at`.
    Insert { at: u64, snippet: u64 },
    /// A line holding whole items removed.
    Remove { at: u64 },
    /// An identifier renamed to one of [`RENAMES`].
    Rename { at: u64, to: u64 },
}

/// Inserts, removes and renames, 3 : 2 : 2.
#[hegel::composite]
fn text_edit(tc: &TestCase) -> TextEdit {
    let kind: u8 = tc.draw(gs::integers().max_value(6));
    let at: u64 = tc.draw(gs::integers());
    match kind {
        0..=2 => TextEdit::Insert { at, snippet: tc.draw(gs::integers()) },
        3 | 4 => TextEdit::Remove { at },
        _ => TextEdit::Rename { at, to: tc.draw(gs::integers()) },
    }
}

/// The brace depth before each line of `lines`, counting outside strings and line comments.
fn depths(lines: &[&str]) -> Vec<i64> {
    let mut out = Vec::with_capacity(lines.len() + 1);
    let mut depth = 0i64;
    for line in lines {
        out.push(depth);
        let mut in_str = false;
        let mut chars = line.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\\' if in_str => _ = chars.next(),
                '"' => in_str = !in_str,
                '/' if !in_str && chars.peek() == Some(&'/') => break,
                '{' if !in_str => depth += 1,
                '}' if !in_str => depth -= 1,
                _ => {}
            }
        }
    }
    out.push(depth);
    out
}

/// Whether a line opens a module body: an item inserted after it is in that module.
fn opens_module(line: &str) -> bool {
    let line = line.trim();
    line.ends_with('{') && (line.starts_with("mod ") || line.starts_with("pub mod ") || line.contains(" mod "))
}

fn insert(text: &str, at: u64, snippet: &str) -> Option<String> {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let depth = depths(&lines);
    let places: Vec<usize> = (0..=lines.len())
        .filter(|&it| depth[it] == 0 || (it > 0 && opens_module(lines[it - 1])))
        .collect();
    let place = *places.get((at % places.len().max(1) as u64) as usize)?;
    let mut out = String::with_capacity(text.len() + snippet.len() + 2);
    for line in &lines[..place] {
        out.push_str(line);
    }
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(snippet);
    out.push('\n');
    for line in &lines[place..] {
        out.push_str(line);
    }
    Some(out)
}

fn remove(text: &str, at: u64) -> Option<String> {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let balanced = |line: &str| {
        let trimmed = line.trim();
        !trimmed.is_empty()
            && !trimmed.starts_with("//")
            && trimmed.matches('{').count() == trimmed.matches('}').count()
            && trimmed.matches('(').count() == trimmed.matches(')').count()
            && trimmed.matches('[').count() == trimmed.matches(']').count()
    };
    let candidates: Vec<usize> = (0..lines.len()).filter(|&it| balanced(lines[it])).collect();
    let line = *candidates.get((at % candidates.len().max(1) as u64) as usize)?;
    Some(lines.iter().enumerate().filter(|&(it, _)| it != line).map(|(_, it)| *it).collect())
}

fn rename(text: &str, at: u64, to: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut idents = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'/') {
            i += text[i..].find('\n').unwrap_or(text.len() - i);
        } else if bytes[i] == b'"' {
            i += 1;
            while i < bytes.len() && bytes[i] != b'"' {
                i += if bytes[i] == b'\\' { 2 } else { 1 };
            }
            i += 1;
        } else if word(bytes[i]) && !bytes[i].is_ascii_digit() && (i == 0 || !word(bytes[i - 1])) {
            let end = i + bytes[i..].iter().take_while(|it| word(**it)).count();
            let ident = &text[i..end];
            // A `$crate` or `$x` of a macro is not a name to rename.
            if !KEYWORDS.contains(&ident) && (i == 0 || bytes[i - 1] != b'$') {
                idents.push(i..end);
            }
            i = end;
        } else {
            i += 1;
        }
    }
    let range = idents.get((at % idents.len().max(1) as u64) as usize)?.clone();
    if &text[range.clone()] == to {
        return None;
    }
    let mut out = String::with_capacity(text.len() + to.len());
    out.push_str(&text[..range.start]);
    out.push_str(to);
    out.push_str(&text[range.end..]);
    Some(out)
}

/// `edit` applied to one file of `loaded` (picked by its `at`), as the new text of that file;
/// `None` where it does not apply.
fn apply_text_edit(loaded: &Loaded, edit: TextEdit) -> Option<(FileId, String)> {
    let at = match edit {
        TextEdit::Insert { at, .. } | TextEdit::Remove { at } | TextEdit::Rename { at, .. } => at,
    };
    let (_, file) = loaded.files[(at % loaded.files.len() as u64) as usize];
    let text = loaded.text(file);
    let within = at >> 8;
    let new = match edit {
        TextEdit::Insert { snippet, .. } => {
            insert(&text, within, SNIPPETS[(snippet % SNIPPETS.len() as u64) as usize])
        }
        TextEdit::Remove { .. } => remove(&text, within),
        TextEdit::Rename { to, .. } => rename(&text, within, RENAMES[(to % RENAMES.len() as u64) as usize]),
    }?;
    Some((file, new))
}

/// The fixtures of the test files that load, by name, with what the comparison with `facts`
/// leaves out of each.
fn all_fixtures() -> &'static [(String, &'static str, Exempt)] {
    static ALL: OnceLock<Vec<(String, &'static str, Exempt)>> = OnceLock::new();
    ALL.get_or_init(|| {
        let mut out = Vec::new();
        for &(file, source) in NAMERES.iter().chain(MACRO_EXPANSION) {
            for fixture in fixtures(file, source) {
                // `fixture::db` panics on text it can't read as a fixture; that literal is not one.
                let loads = panic::catch_unwind(|| {
                    drop(fixture::db_with_proc_macros(fixture.text, extra_proc_macros()))
                })
                .is_ok();
                if !loads {
                    continue;
                }
                let exempt = facts_exempt(&fixture.name);
                out.push((fixture.name, fixture.text, exempt));
            }
        }
        out
    })
}

/// Every fixture, maintained as it is loaded (the cold run of a first write), equals a fresh
/// maintainer's map and `facts`'.
#[test]
fn every_fixture_loads_like_a_fresh_run() {
    let all = all_fixtures();
    assert!(all.len() > 100, "only {} fixtures load", all.len());
    let mut failures = String::new();
    for (name, text, exempt) in all {
        let result = panic::catch_unwind(AssertUnwindSafe(|| {
            let loaded = Loaded::new(text);
            let mut out = String::new();
            check_db(&loaded.db, name, exempt, true, &mut out);
            out
        }));
        match result {
            Ok(out) => failures.push_str(&out),
            Err(payload) => failures.push_str(&format!("== {name}: panicked: {}\n{text}\n", message(&*payload))),
        }
    }
    assert!(failures.is_empty(), "{failures}");
}

#[test]
fn edit_scripts_over_the_fixtures() {
    let all = all_fixtures();
    run_scripts(192, |tc| {
        let fixture = tc.draw(gs::integers::<usize>().max_value(all.len() - 1));
        let edits = tc.draw(gs::vecs(text_edit()).min_size(1).max_size(6));
        let fast_path = tc.draw(gs::booleans());
        fixture_script(&all[fixture], &edits, fast_path);
    });
}

/// `fixture` maintained through `edits`, one write each, checked after each.
fn fixture_script((name, text, exempt): &(String, &'static str, Exempt), edits: &[TextEdit], fast_path: bool) {
    with_fast_path(fast_path, || {
        let mut loaded = Loaded::new(text);
        check(&loaded, &format!("{name} (fast path {fast_path}), loaded"), exempt);
        for (step, &edit) in edits.iter().enumerate() {
            let Some(write_) = apply_text_edit(&loaded, edit) else { continue };
            if !write(&mut loaded, &[write_]) {
                continue;
            }
            let what = format!("{name} (fast path {fast_path}), after edit {step} {edit:?}");
            check(&loaded, &what, exempt);
        }
    })
}

/// A script whose edits touch two files at once: one write, one maintenance pass over both.
#[test]
fn batched_edits_over_the_fixtures() {
    let all = all_fixtures();
    run_scripts(64, |tc| {
        let fixture = tc.draw(gs::integers::<usize>().max_value(all.len() - 1));
        let edits = tc.draw(gs::vecs(gs::tuples!(text_edit(), text_edit())).min_size(1).max_size(4));
        fixture_batches(&all[fixture], &edits);
    });
}

/// `fixture` maintained through `batches`, each pair of edits one write, checked after each.
fn fixture_batches((name, text, exempt): &(String, &'static str, Exempt), batches: &[(TextEdit, TextEdit)]) {
    let mut loaded = Loaded::new(text);
    for (step, &(first, second)) in batches.iter().enumerate() {
        let mut texts: Vec<(FileId, String)> = Vec::new();
        for edit in [first, second] {
            let Some((file, new)) = apply_text_edit(&loaded, edit) else { continue };
            // Two edits of one file: the first one's text is written.
            if texts.iter().all(|(it, _)| *it != file) {
                texts.push((file, new));
            }
        }
        if !write(&mut loaded, &texts) {
            continue;
        }
        let what = format!("{name}, after batch {step} {first:?} {second:?}");
        check(&loaded, &what, exempt);
    }
}

fn fixture_named(name: &str) -> &'static (String, &'static str, Exempt) {
    all_fixtures().iter().find(|it| it.0 == name).unwrap_or_else(|| panic!("no fixture `{name}` loads"))
}

/// Scripts the fixture tests above once failed, as they printed them: their draws are fresh
/// each run, so each is kept here.
#[test]
fn the_fixture_scripts_that_once_failed_keep_the_maps_equal() {
    // 2026-10-31: the maintained map kept a module, `company_name::network::v1::ip_address`,
    // the fresh one did not have.
    fixture_script(fixture_named("tests/macros.rs::nested_include#0"), &[TextEdit::Remove { at: 1 }], false);
    // 2026-10-31: the fresh map reported an attribute `S` unresolved, the maintained one did not.
    fixture_batches(
        fixture_named("../macro_expansion_tests/builtin_derive_macro.rs::coerce_pointee_errors#0"),
        &[(TextEdit::Rename { at: 1932032, to: 0 }, TextEdit::Remove { at: 0 })],
    );
    // 2026-11-01: removing `#![feature(import_trait_associated_functions)]` changed no round-0
    // log, and the maintained map kept the glob of a trait's items without its error.
    fixture_script(
        fixture_named("tests/trait_imports.rs::glob_of_a_trait_with_the_feature#0"),
        &[TextEdit::Remove { at: 0 }],
        false,
    );
    // 2026-11-01: `#[path = "models"]` renamed to `#[S = "models"]`, then that line removed:
    // positions after the walk found `bar`'s parent group through `group_of`, as of the last
    // run, and panicked ("a round-0 module its parent does not declare").
    fixture_script(
        fixture_named("tests/mod_resolution.rs::module_resolution_decl_inside_inline_module_with_path_attribute#0"),
        &[TextEdit::Rename { at: 0, to: 0 }, TextEdit::Remove { at: 0 }],
        false,
    );
    // 2026-11-01: renaming the variant `Minus` that `panels` imports by path
    // (`use MinusPlusIndex::Minus as Left`) woke nothing: a path through an enum noted no read
    // of its variants, so the maintained map kept `Left`.
    fixture_script(
        fixture_named("tests/globs.rs::variant_alias_reexported#0"),
        &[TextEdit::Rename { at: 23552, to: 0 }],
        false,
    );
    // 2026-11-01: removing `fn f();` from a trait the same module globs touched the root and ran
    // nothing: its own read of the trait's items was not kept, so the maintained map kept `f`.
    fixture_script(
        fixture_named("tests/trait_imports.rs::glob_of_a_trait_with_the_feature#0"),
        &[TextEdit::Remove { at: 256 }],
        false,
    );
    fixture_batches(
        fixture_named("tests/trait_imports.rs::glob_of_a_trait_without_the_feature#0"),
        &[(TextEdit::Remove { at: 0 }, TextEdit::Remove { at: 0 })],
    );
    // 2026-11-01: `reexport_2::Placeholder` reached by `use reexport_1::*` and `pub use
    // reexport_1::*`: the fresh map credited the `pub use`, the maintained one the `use`, both
    // `pub`. The two globs' shared edge kept the first glob when the second widened it.
    fixture_script(fixture_named("tests/globs.rs::regression_18580#0"), &[TextEdit::Remove { at: 1024 }], false);
    // 2026-11-01: the maintained `crate::a` kept `struct Ok` from `foo!()`, which the fresh map did
    // not have after the edit.
    fixture_script(
        fixture_named("tests/macros.rs::macro_use_prelude_is_eagerly_expanded#0"),
        &[TextEdit::Remove { at: 257 }],
        false,
    );
    // 2026-11-01: removing `#![crate_type="proc-macro"]` left the maintained root censored, its
    // `S` and fn `pub(crate)`.
    fixture_batches(
        fixture_named("tests/uses.rs::a_derive_helper_before_its_derive_resolves_as_rustc_s_compat_helper#0"),
        &[
            (TextEdit::Insert { at: 0, snippet: 0 }, TextEdit::Insert { at: 513, snippet: 0 }),
            (TextEdit::Remove { at: 0 }, TextEdit::Remove { at: 1 }),
        ],
    );
}

// ---------------------------------------------------------------------------------------------
// Edits of a generated crate.

/// What the model of `generated.rs` does not write and a script adds to a file's end: derives
/// (builtin, through minicore), inherent and trait impls, traits.
#[derive(Debug, Clone, PartialEq)]
enum Extra {
    Derive(&'static str),
    Impl(&'static str),
    TraitImpl(&'static str),
    Trait(&'static str),
}

impl Extra {
    fn render(&self) -> String {
        match self {
            Extra::Derive(name) => format!("#[derive(Clone, Copy, Debug)]\npub struct {name};\n"),
            Extra::Impl(name) => format!("impl {name} {{ pub fn new() {{}} }}\n"),
            Extra::TraitImpl(name) => {
                format!("impl Clone for {name} {{ fn clone(&self) -> Self {{ loop {{}} }} }}\n")
            }
            Extra::Trait(name) => format!("pub trait {name} {{ fn method(&self) {{}} }}\n"),
        }
    }
}

#[derive(Debug, Clone)]
struct Generated {
    model: Model,
    /// Each extra with the path of the file it is added to; left out while the model does not
    /// write that file.
    extras: Vec<(String, Extra)>,
}

impl Generated {
    /// The fixture, with minicore for the derives and `Clone`.
    fn fixture(&self) -> String {
        format!("//- minicore: derive, clone, copy, fmt\n{}", self.model.render())
    }

    /// The text of each file of the fixture, by path, extras added.
    fn files(&self) -> Vec<(String, String)> {
        let mut files: Vec<(String, String)> = FixtureWithProjectMeta::parse(&self.fixture())
            .fixture
            .into_iter()
            .map(|it| (it.path, it.text))
            .collect();
        for (path, extra) in &self.extras {
            if let Some((_, text)) = files.iter_mut().find(|(it, _)| it == path) {
                text.push_str(&extra.render());
            }
        }
        files
    }
}

#[derive(Debug, Clone, Copy)]
enum ModelEdit {
    /// An item the model's generator picks (`seed`), inserted into a module.
    AddItem { module: u64, at: u64, seed: u64 },
    RemoveItem { module: u64, at: u64 },
    /// A struct, function, alias or inline module of a module renamed.
    Rename { module: u64, at: u64, to: u64 },
    /// An inline module added.
    AddModule { parent: u64, at: u64, name: u64 },
    /// A `macro_rules!` added (its body picked by `seed`), and a call of it.
    AddMacro { module: u64, seed: u64, call_in: u64 },
    ToggleMacroUse { module: u64 },
    AddExtra { file: u64, kind: u64, name: u64 },
    RemoveExtra { at: u64 },
}

/// The edits weighted 4 : 3 : 2 : 1 : 1 : 1 : 2 : 1, in [`ModelEdit`]'s order.
#[hegel::composite]
fn model_edit(tc: &TestCase) -> ModelEdit {
    let kind: u8 = tc.draw(gs::integers().max_value(14));
    let n = || -> u64 { tc.draw(gs::integers()) };
    match kind {
        0..=3 => ModelEdit::AddItem { module: n(), at: n(), seed: n() },
        4..=6 => ModelEdit::RemoveItem { module: n(), at: n() },
        7 | 8 => ModelEdit::Rename { module: n(), at: n(), to: n() },
        9 => ModelEdit::AddModule { parent: n(), at: n(), name: n() },
        10 => ModelEdit::AddMacro { module: n(), seed: n(), call_in: n() },
        11 => ModelEdit::ToggleMacroUse { module: n() },
        12 | 13 => ModelEdit::AddExtra { file: n(), kind: n(), name: n() },
        _ => ModelEdit::RemoveExtra { at: n() },
    }
}

fn pick<T: Copy>(from: &[T], at: u64) -> T {
    from[(at % from.len() as u64) as usize]
}

/// Whether module `module` of `model` is written out: declared by a chain of `Gen::Mod`s from
/// the root.
fn reachable(model: &Model, module: usize) -> bool {
    let mut at = module;
    while let Some(parent) = model.modules[at].parent {
        let declared = model.modules[parent].items.iter().any(|it| matches!(it, Gen::Mod(child) if *child == at));
        if !declared {
            return false;
        }
        at = parent;
    }
    true
}

/// Whether a file module is `module` or below it: renaming `module` would move its file.
fn holds_a_file(model: &Model, module: usize) -> bool {
    !model.modules[module].inline
        || model.modules[module].items.iter().any(|it| matches!(it, Gen::Mod(child) if holds_a_file(model, *child)))
}

impl ModelEdit {
    /// Applies the edit; `false` where it changes nothing. A file module is never renamed,
    /// moved or made: that would make a file the fixture has not got.
    fn apply(self, generated: &mut Generated, files: &[(String, FileId)]) -> bool {
        let model = &mut generated.model;
        let live: Vec<usize> = (0..model.modules.len()).filter(|&it| reachable(model, it)).collect();
        let module = |at: u64| live[(at % live.len() as u64) as usize];
        match self {
            ModelEdit::AddItem { module: m, at, seed } => {
                let m = module(m);
                let item = model.item(&mut SplitMix64::new(seed), m);
                let items = &mut model.modules[m].items;
                let at = (at % (items.len() as u64 + 1)) as usize;
                items.insert(at, item);
                true
            }
            ModelEdit::RemoveItem { module: m, at } => {
                let items = &mut model.modules[module(m)].items;
                if items.is_empty() {
                    return false;
                }
                items.remove((at % items.len() as u64) as usize);
                true
            }
            ModelEdit::Rename { module: m, at, to } => {
                let m = module(m);
                let len = model.modules[m].items.len();
                if len == 0 {
                    return false;
                }
                let at = (at % len as u64) as usize;
                let child = match &mut model.modules[m].items[at] {
                    Gen::Struct { name, .. } => {
                        *name = pick(TYPE_NAMES, to);
                        return true;
                    }
                    Gen::Fn { name, .. } => {
                        *name = pick(VALUE_NAMES, to);
                        return true;
                    }
                    Gen::Use { tail: UseTail::Alias(_, alias), .. } => {
                        *alias = pick(TYPE_NAMES, to);
                        return true;
                    }
                    Gen::Mod(child) => *child,
                    _ => return false,
                };
                if holds_a_file(model, child) {
                    return false;
                }
                model.modules[child].name = pick(MODULE_NAMES, to);
                true
            }
            ModelEdit::AddModule { parent, at, name } => {
                let parent = module(parent);
                if model.path(parent).len() >= 3 {
                    return false;
                }
                model.modules.push(Module {
                    parent: Some(parent),
                    name: pick(MODULE_NAMES, name),
                    inline: true,
                    macro_use: false,
                    items: Vec::new(),
                });
                let child = model.modules.len() - 1;
                let items = &mut model.modules[parent].items;
                let at = (at % (items.len() as u64 + 1)) as usize;
                items.insert(at, Gen::Mod(child));
                true
            }
            ModelEdit::AddMacro { module: m, seed, call_in } => {
                let mut rng = SplitMix64::new(seed);
                let body = match rng.below(3) {
                    0 => MacroBody::Item(pick(TYPE_NAMES, rng.next_u64())),
                    1 => MacroBody::Module(pick(MODULE_NAMES, rng.next_u64())),
                    _ => {
                        let target = module(rng.next_u64());
                        MacroBody::GlobOf(model.path(target).join("::"))
                    }
                };
                model.macros.push(body);
                let id = model.macros.len() - 1;
                let export = rng.one_in(2);
                let m = module(m);
                model.modules[m].items.insert(0, Gen::MacroRules { id, export });
                let caller = module(call_in);
                let qualified = export && rng.one_in(2);
                model.modules[caller].items.push(Gen::Call { id, qualified });
                true
            }
            ModelEdit::ToggleMacroUse { module: m } => {
                let m = module(m);
                if model.modules[m].parent.is_none() {
                    return false;
                }
                model.modules[m].macro_use = !model.modules[m].macro_use;
                true
            }
            ModelEdit::AddExtra { file, kind, name } => {
                let name = pick(TYPE_NAMES, name);
                let extra = match kind % 4 {
                    0 => Extra::Derive(name),
                    1 => Extra::Impl(name),
                    2 => Extra::TraitImpl(name),
                    _ => Extra::Trait(name),
                };
                let (path, _) = &files[(file % files.len() as u64) as usize];
                generated.extras.push((path.clone(), extra));
                true
            }
            ModelEdit::RemoveExtra { at } => {
                if generated.extras.is_empty() {
                    return false;
                }
                let at = (at % generated.extras.len() as u64) as usize;
                generated.extras.remove(at);
                true
            }
        }
    }
}

/// The text `generated` gives each file `loaded` has. A file it no longer writes (a file
/// module whose declaration was removed) keeps its text; one it newly writes cannot happen, as
/// edits make inline modules only.
fn texts(loaded: &Loaded, generated: &Generated) -> Vec<(FileId, String)> {
    let mut out = Vec::new();
    for (path, text) in generated.files() {
        let Some(&(_, file)) = loaded.files.iter().find(|(it, _)| *it == path) else {
            panic!("the edit made a new file `{path}`");
        };
        out.push((file, text));
    }
    out
}

/// The crate generated from `seed`, maintained through `edits`, checked after each.
fn generated_script(seed: u64, edits: &[ModelEdit], fast_path: bool) {
    with_fast_path(fast_path, || {
        let mut generated = Generated { model: Model::generate(&mut SplitMix64::new(seed)), extras: Vec::new() };
        let mut loaded = Loaded::new(&generated.fixture());
        let what = |step: &str| format!("generated {seed:#x} (fast path {fast_path}), {step}");
        check(&loaded, &what("loaded"), &Exempt::NONE);
        for (step, &edit) in edits.iter().enumerate() {
            if !edit.apply(&mut generated, &loaded.files) {
                continue;
            }
            let texts = texts(&loaded, &generated);
            if !write(&mut loaded, &texts) {
                continue;
            }
            let what = what(&format!("after edit {step} {edit:?}, now\n{}", generated.fixture()));
            check(&loaded, &what, &Exempt::NONE);
        }
    })
}

#[test]
fn edit_scripts_over_generated_crates() {
    run_scripts(160, |tc| {
        let seed: u64 = tc.draw(gs::integers());
        let edits = tc.draw(gs::vecs(model_edit()).min_size(1).max_size(8));
        let fast_path = tc.draw(gs::booleans());
        generated_script(seed, &edits, fast_path);
    });
}

/// Scripts [`edit_scripts_over_generated_crates`] once failed, as it printed them (seed, fast
/// path, the edits up to the failing one): its seeds are fresh each run, so each is kept here.
const PINNED_GENERATED: &[(u64, bool, &[ModelEdit])] = &[
    // 2026-10-31: the maintained map differed from a fresh one after the edit.
    (0x7d5e, false, &[ModelEdit::RemoveItem { module: 4, at: 0 }]),
    // 2026-11-01: removing a `macro_rules!` panicked in `incremental`, which read the log of a
    // group whose key still inherited it ("Can't find ast id MacroRules").
    (0, false, &[ModelEdit::RemoveItem { module: 0, at: 0 }]),
    // 2026-11-01: one item through two globs, `pub(crate)` and private in the root (the same
    // visibility): the fresh map kept `pub(crate)`, the maintained one the private spelling.
    (0x4082715, false, &[ModelEdit::AddItem { module: 0, at: 1, seed: 181 }]),
    // 2026-11-01: `m0` and `m1` reached `crate::a` through two globs, both `pub(crate)`: each map
    // credited the glob that arrived first.
    (0x33a6fd1240da37, false, &[ModelEdit::AddItem { module: 1, at: 0, seed: 1 }]),
    // 2026-11-01: a macro-written `pub use $crate::b::*` gave `U` an item another glob bound:
    // each map recorded the ambiguity at the visibility of the glob's delivery that came last,
    // `pub` in one and `pub(crate)` in the other.
    (0x1598f, false, &[ModelEdit::AddItem { module: 143, at: 0, seed: 1 }]),
];

#[test]
fn the_generated_scripts_that_once_failed_keep_the_maps_equal() {
    for &(seed, fast_path, edits) in PINNED_GENERATED {
        generated_script(seed, edits, fast_path);
    }
}

/// Undoing every edit of a script brings back the map the crate started with: the maintained
/// state keeps nothing of the history. Compared by [`summary`], which holds no id (interned ids
/// are collected and reused between revisions), then in full against a fresh maintainer.
#[test]
fn undoing_a_script_restores_the_first_map() {
    run_scripts(48, |tc| {
        let seed: u64 = tc.draw(gs::integers());
        let edits = tc.draw(gs::vecs(model_edit()).min_size(1).max_size(6));
        let first = Generated { model: Model::generate(&mut SplitMix64::new(seed)), extras: Vec::new() };
        let mut loaded = Loaded::new(&first.fixture());
        let krate = loaded.krate();
        let before = summary(&loaded.db, krate);
        let mut generated = first.clone();
        for &edit in &edits {
            if edit.apply(&mut generated, &loaded.files) {
                let texts = texts(&loaded, &generated);
                write(&mut loaded, &texts);
            }
        }
        let texts = texts(&loaded, &first);
        write(&mut loaded, &texts);
        let after = summary(&loaded.db, krate);
        assert!(before == after, "after undoing {edits:?}:\nbefore {before:#?}\nafter {after:#?}");
        check(&loaded, &format!("generated {seed:#x}, undone"), &Exempt::NONE);
    });
}

#[test]
fn the_text_edits_keep_items_whole() {
    let text = "mod a {\n    pub struct S;\n}\nfn f() {\n    let x = \"{\";\n}\n";
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    assert_eq!(depths(&lines), [0, 1, 1, 0, 1, 1, 0]);
    // Between items of the root, and at the start of `a`'s body; never inside `f`.
    let all: Vec<String> = (0..8).filter_map(|at| insert(text, at, "X")).collect();
    for it in &all {
        assert!(!it.contains("fn f() {\nX"), "{it}");
        assert!(!it.contains("let x = \"{\";\nX"), "{it}");
    }
    assert!(all.iter().any(|it| it.contains("mod a {\nX\n")), "{all:?}");
    // Only lines with whole items: never a lone brace or a line opening a block.
    for at in 0..8 {
        let removed = remove(text, at).expect("a line to remove");
        let lines: Vec<&str> = removed.split_inclusive('\n').collect();
        assert_eq!(depths(&lines).last(), Some(&0), "{removed}");
        assert!(!removed.contains("pub struct S;"), "{removed}");
    }
    // Names, not keywords, strings or `$crate`.
    let renamed: Vec<String> = (0..16).filter_map(|at| rename("pub use $crate::a as b; // c\n\"d\"", at, "Z")).collect();
    assert!(renamed.iter().all(|it| it.contains("$crate") && it.contains("\"d\"") && it.contains("// c")), "{renamed:?}");
    assert!(renamed.iter().any(|it| it.contains("::Z as b")) && renamed.iter().any(|it| it.contains("as Z")), "{renamed:?}");
}
