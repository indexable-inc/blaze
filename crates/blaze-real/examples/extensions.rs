//! What the new layer does today with each of blaze's language extensions (the ledger in
//! docs/v2/plan/design/extensions.md), measured, not read from a doc (rule 61).
//!
//! Two modes:
//!
//! - `extensions <fixtures dir>`: each `*.rs` there is one feature's smallest program (a
//!   `#![no_core]` one: this example puts the ladder's prelude after the fixture's own inner
//!   attributes). Each goes through the new layer's stages in order, with the calls the
//!   real-code harness (`stages`) and the ladder use, and the first stage that refuses it is
//!   printed with the refusal's exact text:
//!   - outline: blaze-file-items' `outline` (syntax errors, unhandled item forms), and whether the
//!     outline kept the fixture's `blaze::` attributes;
//!   - expansion: blaze-real's macro census, which says whether an attribute or call is one a
//!     macro must answer (a `blaze::` attribute is, unless the driver lists `blaze` as a tool);
//!   - bodies: blaze-lower's `lower_body` and `to_typeck`, every fn, const and static body;
//!   - names, check and build: blaze-ladder's `refusals` (its front stands in for blaze-scope;
//!     then blaze-typeck's checker, blaze-build and blaze-emit, every stage before placing).
//!
//!   A fixture whose header says `expect: refuse` holds a violation of the feature's rule: when
//!   every stage passes it, nothing in the new layer reads the feature.
//! - `extensions --tree <checkout>`: every `.rs` file outlined and every body lowered, the
//!   outline's syntax errors and the bodies' first refusals counted by cause and by which
//!   extension's syntax is on the line they point at.
//!
//! Run with `examplerun.sh blaze-real extensions <args>`. Nothing is written; nothing of a
//! checkout is run.

#![feature(allocator_api)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use blaze_file_items::outline;
use blaze_ladder::refusal::SourceFile;
use blaze_lower::typeck::{Refused, Types, to_typeck};
use blaze_lower::{Cfg, CfgAnswer, ExprId, TypeId, View, lower_body};
use blaze_own::Alloc;
use blaze_real::census::{self, Event};
use blaze_surface::{Edition, ItemDetail};
use blaze_typeck::{ItemKey, PNode, PTy, PortableTys};

/// What every fixture is put after its own inner attributes: the ladder's `#![no_core]` frame.
const PRELUDE: &str = "#![feature(no_core, lang_items)]\n#![no_core]\n#![no_main]\n#[lang = \"sized\"]\ntrait Sized {}\n#[lang = \"copy\"]\ntrait Copy {}\n";

/// A cfg like this host's (macOS on arm64), as `stages` answers it.
struct HostCfg;

impl Cfg for HostCfg {
    fn atom(&mut self, name: &str, value: Option<&str>) -> CfgAnswer {
        let yes = match (name, value) {
            ("unix" | "debug_assertions" | "test" | "blaze", None) => true,
            ("target_os", Some(os)) => os == "macos",
            ("target_vendor", Some(vendor)) => vendor == "apple",
            ("target_family", Some(family)) => family == "unix",
            ("target_arch", Some(arch)) => arch == "aarch64",
            ("target_pointer_width", Some(width)) => width == "64",
            ("target_endian", Some(endian)) => endian == "little",
            ("panic", Some(panic)) => panic == "unwind",
            ("target_has_atomic", _) => true,
            _ => false,
        };
        if yes { CfgAnswer::Yes } else { CfgAnswer::No }
    }
}

/// Written types left to inference, as `stages` does: what is counted is the body's forms.
struct Unresolved;

impl Types for Unresolved {
    fn ty(&mut self, _: &View<'_>, _: TypeId, tys: &mut PortableTys<'_>) -> Result<PTy, Refused> {
        tys.intern(PNode::Infer).map_err(Refused::Check)
    }

    fn repeat_len(&mut self, _: &View<'_>, _: ExprId) -> Result<u64, Refused> {
        Ok(0)
    }

    fn const_arg(&mut self, view: &View<'_>, c: blaze_lower::ConstIdx, _: &mut PortableTys<'_>) -> Result<PTy, Refused> {
        Err(blaze_lower::typeck::no_const_arg(view, c))
    }
}

fn line_of(text: &str, at: usize) -> usize {
    text.get(..at).unwrap_or(text).bytes().filter(|&b| b == b'\n').count() + 1
}

/// The line `at` is on, trimmed.
fn line_text(text: &str, at: usize) -> &str {
    let at = at.min(text.len());
    let start = text[..at].rfind('\n').map_or(0, |i| i + 1);
    let end = text[at..].find('\n').map_or(text.len(), |i| at + i);
    text[start..end].trim()
}

/// The first refusal of one body: the lowering's unhandled forms and diagnostics, then
/// `to_typeck`'s first refused form. `None` when the body is clean, cfg'd out or has none.
fn body_refusal(text: &str, edition: Edition) -> Option<(String, usize)> {
    let alloc = Alloc::global();
    let lowered = match lower_body(text, edition, &mut HostCfg, &mut blaze_lower::NoModule, alloc) {
        Ok(lowered) => lowered,
        Err(_) => return Some(("lower_body: a cfg atom left unanswered".to_string(), 0)),
    };
    let view = lowered.view();
    if view.cfg_out() {
        return None;
    }
    let offset_of = |piece: &str| (piece.as_ptr() as usize).checked_sub(text.as_ptr() as usize).filter(|o| *o <= text.len()).unwrap_or(0);
    if let Some(u) = view.unhandled().next() {
        let piece = view.text_at(u.range).unwrap_or("");
        let kind = format!("{:?} ({:?})", blaze_file_items::syntax_kind(u.syntax), u.why);
        let cause = if kind.starts_with("MACRO_CALL") { "lower_body: a macro call, not expanded".to_string() } else { format!("lower_body: unhandled {kind}") };
        return Some((cause, offset_of(piece)));
    }
    if let Some(diag) = view.diags().next() {
        return Some(match diag {
            Ok(diag) => (format!("lower_body: {:?}", diag.kind), view.text_at(diag.range).map_or(0, offset_of)),
            Err(bad) => (format!("lower_body: bad row {bad:?}"), 0),
        });
    }
    let mut tys = PortableTys::new_in(alloc);
    match to_typeck(&view, ItemKey(0), &mut tys, &mut Unresolved, alloc) {
        Ok(_) | Err(Refused::NoBody) => None,
        Err(Refused::Form { what, range }) => Some((format!("to_typeck: {what}"), range.start as usize)),
        Err(Refused::Bad(bad)) => Some((format!("to_typeck: bad row {bad:?}"), 0)),
        Err(Refused::Check(error)) => Some((format!("to_typeck: table {error:?}"), 0)),
        Err(Refused::Text(error)) => Some((format!("to_typeck: text {error:?}"), 0)),
    }
}

// ---------------------------------------------------------------------------------------------
// Fixtures.

/// The fixture's text with the prelude after its leading `//!` lines and inner attributes.
fn program(fixture: &str) -> String {
    let mut head = 0;
    for line in fixture.split_inclusive('\n') {
        let t = line.trim_start();
        if t.starts_with("//!") || t.starts_with("#![") || t.trim().is_empty() {
            head += line.len();
        } else {
            break;
        }
    }
    format!("{}{PRELUDE}{}", &fixture[..head], &fixture[head..])
}

fn probe(name: &str, fixture: &str) {
    let text = program(fixture);
    let expect = if fixture.contains("//! expect: refuse") { "refuse" } else { "accept" };
    let alloc = Alloc::global();
    let edition = Edition::E2024;
    println!("== {name} (expect: {expect})");

    // Outline.
    let made = outline(&text, edition, alloc);
    let first_error = made.errors().first().map(|e| (made.message(e).to_string(), e.range.start as usize));
    let unhandled = made.items().iter().find_map(|item| match item.detail {
        ItemDetail::Unhandled { syntax } => Some((format!("{:?}", blaze_file_items::syntax_kind(syntax)), item.range.start as usize)),
        _ => None,
    });
    println!("  outline: {} items, {} syntax errors", made.items().len(), made.errors().len());
    if let Some((message, at)) = &first_error {
        println!("  REFUSED at outline: line {}: {message:?}  [{}]", line_of(&text, *at), line_text(&text, *at));
        return;
    }
    if let Some((kind, at)) = &unhandled {
        println!("  REFUSED at outline: an unhandled item form {kind}  [{}]", line_text(&text, *at));
        return;
    }

    // Expansion: what the census says a macro must answer.
    let mut asks = Vec::new();
    census::scan(&text, blaze_lex::Cpu::BASELINE, alloc, &mut |event| match event {
        Event::Attr { path, .. } if path.starts_with("blaze") && !census::builtin_attr(path) => {
            asks.push(format!("attribute `{path}`"));
        }
        Event::Call { path, name, .. } if !census::std_macro(path, name) => asks.push(format!("macro call `{path}!`")),
        _ => {}
    });
    if !asks.is_empty() {
        println!("  expansion: {} (an attribute macro to resolve: `blaze` is no tool in census::TOOLS; blaze-scope makes it inert only if the driver lists the tool)", asks.join(", "));
    }

    // Bodies.
    let mut lowered = 0;
    for item in made.items() {
        let has_body = matches!(item.detail, ItemDetail::Fn { .. } | ItemDetail::Const { .. } | ItemDetail::Static { .. });
        if !has_body || item.body.is_none() {
            continue;
        }
        let base = item.range.start as usize;
        let Some(item_text) = text.get(base..item.range.end as usize) else { continue };
        lowered += 1;
        if let Some((cause, at)) = body_refusal(item_text, edition) {
            println!("  REFUSED at bodies: {cause}  [{}]", line_text(&text, base + at));
            return;
        }
    }
    println!("  bodies: {lowered} lowered and converted, clean");

    // Names, check, build (the ladder).
    let files = [SourceFile { path: "src/main.rs", text: &text }];
    // `blaze run`'s setting: the dev profile's overflow checks (D101).
    let refusals = blaze_ladder::run::refusals(&files, "src/main.rs", blaze_ladder::run::OverflowChecks::On, alloc);
    match refusals.first() {
        Some(r) => {
            let at = (r.place + r.start) as usize;
            println!("  REFUSED at names/check/build (ladder, {:?}): {:?}  [{}]", r.rule, &*r.message, line_text(&text, at));
        }
        None => {
            let verdict = if expect == "refuse" { "nothing in the new layer reads the feature: its violation passes" } else { "accepted" };
            println!("  PASSES every stage (outline, expansion census, bodies, names, check, build): {verdict}");
        }
    }
}

fn fixtures(dir: &Path) {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "rs"))
        .collect();
    paths.sort();
    println!("{} fixtures in {}", paths.len(), dir.display());
    for path in &paths {
        let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("?");
        let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        probe(name, &text);
    }
}

// ---------------------------------------------------------------------------------------------
// A tree.

/// The extension whose syntax is on `line`, or `rust` for none.
fn marker(line: &str) -> &'static str {
    let l = line.trim_start();
    if l.starts_with("//") || l.starts_with("*") {
        return "comment";
    }
    if l.contains("&uniq ") || l.contains("ref uniq ") || l.contains(" uniq self") {
        "uniq"
    } else if l.contains("for const ") {
        "comptime: for const"
    } else if l.contains("where const") {
        "comptime: where const"
    } else if l.contains("const static {") {
        "comptime: const static"
    } else if l.contains(".[") && !l.contains("b\".[") {
        "comptime: x.[f]"
    } else if l.contains("type_of!") {
        "comptime: type_of!"
    } else if l.contains("self.{") {
        "views"
    } else if ["requires ", "ensures ", "decreases ", "recommends "].iter().any(|k| l.starts_with(k)) || l.contains(") requires ") {
        "verify clauses"
    } else if l.contains(" is ") && !l.contains('"') {
        "pattern types (` is `)"
    } else if l.contains("become ") {
        "tail calls (`become`)"
    } else if l.contains("#![blaze::") || l.contains("#[blaze::") {
        "a blaze attribute"
    } else {
        "rust"
    }
}

fn testish(rel: &str) -> bool {
    rel.contains("/tests/") || rel.contains("/test_data/") || rel.ends_with("tests.rs") || rel.contains("/benches/") || rel.contains("/fixtures/")
}

fn walk(root: &Path, out: &mut Vec<(String, String)>) {
    let mut todo = vec![root.to_path_buf()];
    while let Some(dir) = todo.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if path.is_dir() {
                if !name.starts_with('.') && name != "target" {
                    todo.push(path);
                }
            } else if name.ends_with(".rs") {
                if let Ok(text) = std::fs::read_to_string(&path) {
                    let rel = path.strip_prefix(root).unwrap_or(&path).to_string_lossy().into_owned();
                    out.push((rel, text));
                }
            }
        }
    }
    out.sort();
}

#[derive(Default)]
struct Tally(BTreeMap<String, (usize, String)>);

impl Tally {
    fn add(&mut self, key: String, place: impl FnOnce() -> String) {
        let slot = self.0.entry(key).or_insert_with(|| (0, place()));
        slot.0 += 1;
    }

    fn print(&self, title: &str, top: usize) {
        let total: usize = self.0.values().map(|v| v.0).sum();
        println!("  {title}: {total} in {} kinds", self.0.len());
        let mut rows: Vec<_> = self.0.iter().collect();
        rows.sort_by(|a, b| b.1.0.cmp(&a.1.0).then(a.0.cmp(b.0)));
        for (key, (n, first)) in rows.into_iter().take(top) {
            println!("    {n:>7}  {key}  (first: {first})");
        }
    }
}

fn tree(root: &Path) {
    let mut files = Vec::new();
    walk(root, &mut files);
    let alloc = Alloc::global();
    let edition = Edition::E2024;
    let (mut code_files, mut lines) = (0usize, 0usize);
    let mut errors_by_marker = Tally::default();
    let mut ext_errors = Tally::default();
    let mut bodies_by_marker = Tally::default();
    let mut ext_bodies = Tally::default();
    let mut rust_bodies = Tally::default();
    let (mut bodies, mut stopped) = (0usize, 0usize);
    for (rel, text) in &files {
        let test = testish(rel);
        if !test {
            code_files += 1;
            lines += text.lines().count();
        }
        let scope = if test { "test" } else { "code" };
        let made = outline(text, edition, alloc);
        for error in made.errors() {
            let at = error.range.start as usize;
            let m = marker(line_text(text, at));
            let place = || format!("{rel}:{}", line_of(text, at));
            errors_by_marker.add(format!("{scope}: {m}"), place);
            if m != "rust" && m != "comment" {
                ext_errors.add(format!("{scope}: {m}: {}", made.message(error)), place);
            }
        }
        for item in made.items() {
            let has_body = matches!(item.detail, ItemDetail::Fn { .. } | ItemDetail::Const { .. } | ItemDetail::Static { .. });
            if test || !has_body || item.body.is_none() {
                continue;
            }
            let base = item.range.start as usize;
            let Some(item_text) = text.get(base..item.range.end as usize) else { continue };
            bodies += 1;
            if let Some((cause, at)) = body_refusal(item_text, edition) {
                stopped += 1;
                let m = marker(line_text(text, base + at));
                let place = || format!("{rel}:{}", line_of(text, base + at));
                bodies_by_marker.add(m.to_string(), place);
                if m == "rust" || m == "comment" {
                    rust_bodies.add(cause, place);
                } else {
                    ext_bodies.add(format!("{m}: {cause}"), place);
                }
            }
        }
    }
    println!("tree {}: {} .rs files ({code_files} outside tests, {lines} lines)", root.display(), files.len());
    errors_by_marker.print("outline syntax errors, by the extension whose syntax is on the error's line", 20);
    ext_errors.print("of them on an extension's line, by message", 20);
    println!("  bodies outside tests: {bodies}, {stopped} stopped ({:.1}% clean)", 100.0 * (bodies - stopped) as f64 / bodies.max(1) as f64);
    bodies_by_marker.print("stopped bodies, by the extension whose syntax is on the line of the first refusal", 20);
    ext_bodies.print("of them on an extension's line, by cause", 20);
    rust_bodies.print("of them on a plain Rust line, by cause", 12);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("--tree") => match args.get(2) {
            Some(root) => tree(Path::new(root)),
            None => eprintln!("usage: extensions --tree <checkout>"),
        },
        Some(dir) => fixtures(Path::new(dir)),
        None => {
            eprintln!("usage: extensions <fixtures dir> | extensions --tree <checkout>");
            std::process::exit(2);
        }
    }
}
