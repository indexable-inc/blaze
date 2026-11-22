//! The distance to the finish line, as a number: `census <crate dir or .rs file>...` runs the
//! new front end's per-item stages over every item of every file, without stopping at a
//! refusal and without building, and prints a Markdown table per input and one for all:
//! items, items that pass every stage, and each cause by the items it refuses first and by
//! every place it occurs, with one example location.
//!
//! The stages, in order, each the one `blaze run` uses (crates/blaze-ladder/src/front.rs):
//! 1. outline: blaze-file-items' outline of the file (a syntax error refuses the file's items);
//! 2. signature: blaze-lower's `lower_signature` of the item's text, clean (nothing unhandled,
//!    no diagnostic);
//! 3. body: `lower_body` of a fn's, const's or static's text, clean. Macro calls are not
//!    expanded here: each is a refusal of the expansion lane's.
//!
//! Not counted yet: names, types and checking (`to_typeck`, the checker): they need names
//! across crates, which the census gets with blaze-scope (the README's step 3). So the counts
//! are a lower bound on the distance.
//!
//! `cfg` is answered for the host the ladder runs on (macOS on arm64, debug, not `test`); every
//! `feature` is off, so a crate is read as with no features. An item `cfg` turns off is not
//! counted.
//!
//! Run with `examplerun.sh blaze-ladder census <dirs>`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use blaze_lower::{Cfg, CfgAnswer, Lowered};
use blaze_own::Alloc;
use blaze_surface::{Edition, ItemDetail};

const EDITION: Edition = Edition::E2024;

/// The host's cfg set (see the module doc).
struct HostCfg;

impl Cfg for HostCfg {
    fn atom(&mut self, name: &str, value: Option<&str>) -> CfgAnswer {
        let yes = match (name, value) {
            ("unix" | "debug_assertions", None) => true,
            ("target_os", Some("macos")) => true,
            ("target_family", Some("unix")) => true,
            ("target_arch", Some("aarch64")) => true,
            ("target_pointer_width", Some("64")) => true,
            ("target_endian", Some("little")) => true,
            ("target_vendor", Some("apple")) => true,
            ("target_env", Some("")) => true,
            ("target_has_atomic", Some("8" | "16" | "32" | "64" | "ptr")) => true,
            ("panic", Some("unwind")) => true,
            _ => false,
        };
        if yes { CfgAnswer::Yes } else { CfgAnswer::No }
    }
}

/// The stage an item was first refused at.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Stage {
    Outline,
    Signature,
    Body,
}

impl Stage {
    fn name(self) -> &'static str {
        match self {
            Stage::Outline => "outline",
            Stage::Signature => "signature",
            Stage::Body => "body",
        }
    }
}

/// One cause: the items it refuses first, every place it occurs, and its first location.
#[derive(Default, Clone)]
struct Cause {
    first: usize,
    all: usize,
    at: String,
}

/// One input's tally.
#[derive(Default)]
struct Tally {
    files: usize,
    unparsed: usize,
    items: usize,
    clean: usize,
    fns: usize,
    clean_fns: usize,
    cfg_out: usize,
    /// By (stage, cause).
    causes: BTreeMap<(Stage, String), Cause>,
}

impl Tally {
    fn add(&mut self, other: &Tally) {
        self.files += other.files;
        self.unparsed += other.unparsed;
        self.items += other.items;
        self.clean += other.clean;
        self.fns += other.fns;
        self.clean_fns += other.clean_fns;
        self.cfg_out += other.cfg_out;
        for (k, c) in &other.causes {
            let e = self.causes.entry(k.clone()).or_insert_with(|| Cause { at: c.at.clone(), ..Cause::default() });
            e.first += c.first;
            e.all += c.all;
        }
    }

    fn print(&self, title: &str) {
        println!("### {title}\n");
        println!(
            "{} files ({} the outline refuses), {} items ({} turned off by `cfg`, not counted), {} pass every stage ({}%); {} fns, {} pass ({}%)\n",
            self.files,
            self.unparsed,
            self.items,
            self.cfg_out,
            self.clean,
            (self.clean * 100).checked_div(self.items).unwrap_or(0),
            self.fns,
            self.clean_fns,
            (self.clean_fns * 100).checked_div(self.fns).unwrap_or(0),
        );
        let mut rows: Vec<(&(Stage, String), &Cause)> = self.causes.iter().collect();
        rows.sort_by(|a, b| b.1.first.cmp(&a.1.first).then(b.1.all.cmp(&a.1.all)).then(a.0.cmp(b.0)));
        println!("| stage | cause | items it refuses first | occurrences | first at |");
        println!("|---|---|---|---|---|");
        for ((stage, cause), c) in rows {
            println!("| {} | {cause} | {} | {} | {} |", stage.name(), c.first, c.all, c.at);
        }
        println!();
    }
}

/// Every `.rs` file under `root`, sorted.
fn rust_files(root: &Path) -> Vec<PathBuf> {
    if root.is_file() {
        return vec![root.to_path_buf()];
    }
    let src = root.join("src");
    let mut todo = vec![if src.is_dir() { src } else { root.to_path_buf() }];
    let mut out = Vec::new();
    while let Some(at) = todo.pop() {
        let Ok(entries) = std::fs::read_dir(&at) else {
            eprintln!("census: cannot read {}", at.display());
            std::process::exit(2);
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                todo.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// `path:line:col` of byte `at` of `text`.
fn place(path: &str, text: &str, at: u32) -> String {
    let before = text.get(..at as usize).unwrap_or(text);
    let line = before.bytes().filter(|&b| b == b'\n').count() + 1;
    let col = before.len() - before.rfind('\n').map_or(0, |i| i + 1) + 1;
    format!("{path}:{line}:{col}")
}

/// What one stage refused of an item: each cause and its byte in the file.
enum Seen {
    Clean,
    CfgOut,
    Refused(Vec<(String, u32)>),
}

/// The causes in one lowering: what it did not lower, and what it diagnosed.
fn seen(lowered: &Lowered<'_, '_>, item_start: u32) -> Seen {
    let view = lowered.view();
    if view.cfg_out() {
        return Seen::CfgOut;
    }
    let mut out = Vec::new();
    for u in view.unhandled() {
        let kind = blaze_file_items::syntax_kind(u.syntax);
        let name = blaze_ladder::refusal::syntax_name(kind).unwrap_or("an unnamed construct");
        let cause = match u.why {
            blaze_lower::form::Why::Kind => name.to_string(),
            why => format!("{name} ({why:?})"),
        };
        let at = view.site(u.range).map_or(0, |r| r.start);
        out.push((cause, item_start + at));
    }
    for d in view.diags() {
        match d {
            Ok(d) => {
                let at = view.site(d.range).map_or(0, |r| r.start);
                out.push((format!("blaze-lower's diagnostic {:?}", d.kind), item_start + at));
            }
            Err(bad) => out.push((format!("a malformed record row in `{}`", bad.column), item_start)),
        }
    }
    if out.is_empty() { Seen::Clean } else { Seen::Refused(out) }
}

fn tally(root: &Path, alloc: Alloc<'_>) -> Tally {
    let mut t = Tally::default();
    for path in rust_files(root) {
        let shown = path.display().to_string();
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) => {
                eprintln!("census: {shown}: {error}");
                std::process::exit(2);
            }
        };
        t.files += 1;
        let outline = blaze_file_items::outline(&text, EDITION, alloc);
        let errors = outline.errors();
        t.unparsed += usize::from(!errors.is_empty());
        for item in outline.items() {
            let (is_fn, has_body) = match item.detail {
                ItemDetail::Fn { .. } => (true, true),
                ItemDetail::Const { .. } | ItemDetail::Static { .. } => (false, true),
                ItemDetail::Struct
                | ItemDetail::Enum
                | ItemDetail::Union
                | ItemDetail::Trait
                | ItemDetail::TraitAlias
                | ItemDetail::Impl
                | ItemDetail::TypeAlias
                | ItemDetail::MacroCall { .. } => (false, false),
                _ => continue,
            };
            let start = item.range.start;
            let mut refused: Vec<(Stage, String, u32)> = Vec::new();
            if !errors.is_empty() {
                refused.push((Stage::Outline, "a syntax error".to_string(), start));
            } else if matches!(item.detail, ItemDetail::MacroCall { .. }) {
                refused.push((Stage::Signature, "a macro call `name!(..)` (an item)".to_string(), start));
            } else {
                let item_text = item.range.of(&text);
                let mut off = false;
                let stages: &[Stage] = if has_body { &[Stage::Signature, Stage::Body] } else { &[Stage::Signature] };
                for &stage in stages {
                    let lowered = match stage {
                        Stage::Body => blaze_lower::lower_body(item_text, EDITION, &mut HostCfg, &mut blaze_lower::NoModule, alloc),
                        _ => blaze_lower::lower_signature(item_text, EDITION, &mut HostCfg, alloc),
                    };
                    let lowered = match lowered {
                        Ok(lowered) => lowered,
                        Err(_) => {
                            refused.push((stage, "a `cfg` this census does not answer".to_string(), start));
                            continue;
                        }
                    };
                    match seen(&lowered, start) {
                        Seen::Clean => {}
                        Seen::CfgOut => off = true,
                        Seen::Refused(causes) => refused.extend(causes.into_iter().map(|(c, at)| (stage, c, at))),
                    }
                    if off {
                        break;
                    }
                }
                if off {
                    t.cfg_out += 1;
                    continue;
                }
            }
            t.items += 1;
            t.fns += usize::from(is_fn);
            if refused.is_empty() {
                t.clean += 1;
                t.clean_fns += usize::from(is_fn);
                continue;
            }
            refused.sort_by_key(|r| (r.0, r.2));
            for (i, (stage, cause, at)) in refused.iter().enumerate() {
                let c = t.causes.entry((*stage, cause.clone())).or_insert_with(|| Cause { at: place(&shown, &text, *at), ..Cause::default() });
                c.all += 1;
                if i == 0 {
                    c.first += 1;
                }
            }
        }
    }
    t
}

/// One function of a measured list (port27/hosted-hello-functions.txt), by its demangled name.
struct Wanted {
    side: &'static str,
    name: String,
    /// The impl's self type, normalized ([`norm`]); `None` for a free fn.
    self_ty: Option<String>,
    /// The impl's trait, its last segment.
    trait_: Option<String>,
    /// For a free fn: the module it is in, its last segment.
    module: Option<String>,
    fn_name: String,
}

/// `text` from its first `<` at depth 0 on, removed.
fn strip_generics(text: &str) -> &str {
    text.find('<').map_or(text, |i| &text[..i])
}

/// A type's key for matching a demangled name to an impl: a pointer's kind, a reference, an
/// array or slice, unit, or the last path segment without generic arguments.
fn norm(ty: &str) -> String {
    let ty = ty.trim();
    for p in ["*const", "*mut"] {
        if ty.starts_with(p) {
            return p.to_string();
        }
    }
    if ty.starts_with('&') {
        return "&".to_string();
    }
    if ty.starts_with('[') {
        return "[".to_string();
    }
    if ty.starts_with('(') {
        return "()".to_string();
    }
    let base = strip_generics(ty);
    base.rsplit("::").next().unwrap_or(base).trim().to_string()
}

/// Splits `text` at the first `sep` outside brackets.
fn split_top<'t>(text: &'t str, sep: &str) -> Option<(&'t str, &'t str)> {
    let mut depth = 0i32;
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'<' | b'[' | b'(' => depth += 1,
            b'>' | b']' | b')' => depth -= 1,
            _ => {}
        }
        if depth == 0 && text[i..].starts_with(sep) {
            return Some((&text[..i], &text[i + sep.len()..]));
        }
        i += 1;
    }
    None
}

/// The list's program side: each function and the side (crate) it was measured in.
fn wanted(list: &str) -> Vec<Wanted> {
    let mut out = Vec::new();
    let mut side = None;
    for line in list.lines() {
        if let Some(h) = line.strip_prefix("== ") {
            side = match h {
                "program side, bstd" => Some("bstd"),
                "program side, core" => Some("core"),
                "program side, the program (hosted_hello example)" => Some("program"),
                _ => None,
            };
            continue;
        }
        let Some(side) = side else { continue };
        let words: Vec<&str> = line.split_whitespace().collect();
        if words.len() < 2 {
            continue;
        }
        // `count [kinds] name`; the name is the last column, which may hold spaces (`as`).
        let rest = line.trim_start().trim_start_matches(|c: char| c.is_ascii_digit()).trim_start();
        let name = match rest.split_once("  ") {
            Some((kinds, name)) if !kinds.contains("::") && !kinds.starts_with('<') => name.trim(),
            _ => rest.trim(),
        }
        .to_string();
        let mut w = Wanted { side, name: name.clone(), self_ty: None, trait_: None, module: None, fn_name: String::new() };
        if let Some(inner) = name.strip_prefix('<') {
            // `<Self as Trait>::f` or `<Self>::f`: the closing `>` before the last `::`.
            let (head, f) = name.rsplit_once(">::").unwrap_or((&name, ""));
            let head = head.strip_prefix('<').unwrap_or(inner);
            match split_top(head, " as ") {
                Some((s, t)) => {
                    w.self_ty = Some(norm(s));
                    let t = strip_generics(t);
                    w.trait_ = Some(t.rsplit("::").next().unwrap_or(t).to_string());
                }
                None => w.self_ty = Some(norm(head)),
            }
            w.fn_name = strip_generics(f).trim_end_matches("::").to_string();
        } else {
            let base = strip_generics(&name).trim_end_matches("::");
            let mut segs: Vec<&str> = base.split("::").collect();
            w.fn_name = segs.pop().unwrap_or("").to_string();
            w.module = segs.last().map(|s| s.to_string());
        }
        out.push(w);
    }
    out
}

/// An impl item's self type and trait, read from its signature text.
fn impl_head(sig: &str) -> Option<(String, Option<String>)> {
    let at = sig.find("impl")?;
    let mut rest = sig[at + 4..].trim_start();
    if rest.starts_with('<') {
        let (_, after) = split_top(rest, ">").map(|(a, b)| (a, b)).unwrap_or(("", rest));
        rest = after.trim_start();
    }
    let rest = split_top(rest, "where").map_or(rest, |(a, _)| a);
    match split_top(rest, " for ") {
        Some((t, s)) => {
            let t = strip_generics(t.trim().trim_start_matches('!'));
            Some((norm(s), Some(t.rsplit("::").next().unwrap_or(t).trim().to_string())))
        }
        None => Some((norm(rest), None)),
    }
}

/// Where a wanted function was found, and what refused it first.
struct Found {
    at: String,
    stage: &'static str,
    cause: String,
}

/// The first refusal of one fn item's text, or `None` when it lowers clean.
fn first_refusal(shown: &str, text: &str, item: &blaze_surface::Item, outline_ok: bool, alloc: Alloc<'_>) -> Option<(&'static str, String, String)> {
    let start = item.range.start;
    if !outline_ok {
        return Some(("outline", "a syntax error in the file".to_string(), place(shown, text, start)));
    }
    let item_text = item.range.of(text);
    for stage in [Stage::Signature, Stage::Body] {
        let lowered = match stage {
            Stage::Body => blaze_lower::lower_body(item_text, EDITION, &mut HostCfg, &mut blaze_lower::NoModule, alloc),
            _ => blaze_lower::lower_signature(item_text, EDITION, &mut HostCfg, alloc),
        };
        let Ok(lowered) = lowered else {
            return Some(("lowering", "a `cfg` this census does not answer".to_string(), place(shown, text, start)));
        };
        if let Seen::Refused(mut causes) = seen(&lowered, start) {
            causes.sort_by_key(|c| c.1);
            let (cause, at) = causes.swap_remove(0);
            let stage = if cause.contains("macro call") { "expansion" } else { "lowering" };
            return Some((stage, cause, place(shown, text, at)));
        }
    }
    None
}

/// The census of one measured list: per function, the first stage that refuses it and why.
fn functions(list: &Path, roots: &[String], alloc: Alloc<'_>) {
    let Ok(list) = std::fs::read_to_string(list) else {
        eprintln!("census: cannot read {}", list.display());
        std::process::exit(2);
    };
    let wanted = wanted(&list);
    let mut files = Vec::new();
    for root in roots {
        for path in rust_files(Path::new(root)) {
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            files.push((path.display().to_string(), text));
        }
    }
    let outlines: Vec<_> = files.iter().map(|(_, text)| blaze_file_items::outline(text, EDITION, alloc)).collect();
    let mut found: Vec<Found> = Vec::new();
    for w in &wanted {
        let mut hit = None;
        if w.name.starts_with("core::ptr::drop_glue") {
            hit = Some(Found { at: "-".into(), stage: "builder", cause: "drop glue: made by the builder from the type, no source item".into() });
        }
        for ((shown, text), outline) in files.iter().zip(&outlines) {
            if hit.is_some() {
                break;
            }
            let items = outline.items();
            let ok = outline.errors().is_empty();
            for item in items {
                // An item's name and signature ranges are relative to the item's start.
                let own = |of: &blaze_surface::Item, r: blaze_surface::Range| of.range.of(text).get(r.start as usize..r.end as usize);
                if !matches!(item.detail, ItemDetail::Fn { .. }) || item.name.and_then(|r| own(item, r)) != Some(w.fn_name.as_str()) {
                    continue;
                }
                let parent = item.parent.map(|p| outline.item(p));
                let matches = match (&w.self_ty, parent) {
                    (Some(s), Some(p)) if matches!(p.detail, ItemDetail::Impl) => {
                        own(p, p.signature).and_then(impl_head).is_some_and(|(ps, pt)| &ps == s && pt == w.trait_)
                    }
                    (Some(_), _) => false,
                    (None, Some(p)) if matches!(p.detail, ItemDetail::Mod { .. }) => {
                        w.module.is_none() || p.name.and_then(|r| own(p, r)) == w.module.as_deref()
                    }
                    (None, Some(_)) => false,
                    (None, None) => {
                        let stem = Path::new(shown).file_stem().and_then(|s| s.to_str()).unwrap_or("");
                        w.side == "program" || w.module.is_none() || w.module.as_deref() == Some(stem) || stem == "lib" || stem == "mod"
                    }
                };
                if !matches {
                    continue;
                }
                let at = place(shown, text, item.range.start);
                hit = Some(match first_refusal(shown, text, item, ok, alloc) {
                    Some((stage, cause, at)) => Found { at, stage, cause },
                    None => Found {
                        at,
                        stage: "names",
                        cause: "the runner's names take one crate: no other crate, prelude, glob or `cfg` (blaze_scope::resolve_crates is the step)".into(),
                    },
                });
                break;
            }
        }
        // A trait fn with no written impl: a derive's, or the edition's entry wrapper.
        let hit = hit.unwrap_or_else(|| match (&w.trait_, w.side) {
            (Some(t), _) => Found { at: "-".into(), stage: "expansion", cause: format!("no written impl: `#[derive({t})]` or a blanket impl") },
            (None, "program") => Found { at: "-".into(), stage: "expansion", cause: "made by the edition's entry (no source in his main.rs)".into() },
            _ => Found { at: "-".into(), stage: "outline", cause: "not found in the sources given".into() },
        });
        found.push(hit);
    }
    println!("### The hosted hello's {} program-side functions\n", wanted.len());
    let pass = found.iter().filter(|f| f.stage == "pass").count();
    println!("{pass} of {} pass every stage.\n", wanted.len());
    println!("| # | side | function | at | first refusing stage | refusal |");
    println!("|---|---|---|---|---|---|");
    for (i, (w, f)) in wanted.iter().zip(&found).enumerate() {
        println!("| {} | {} | `{}` | {} | {} | {} |", i + 1, w.side, w.name, f.at, f.stage, f.cause);
    }
    let mut by: BTreeMap<(&str, &str), usize> = BTreeMap::new();
    for f in &found {
        *by.entry((f.stage, f.cause.as_str())).or_default() += 1;
    }
    let mut rows: Vec<_> = by.into_iter().collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    println!("\n| stage | cause | functions |\n|---|---|---|");
    for ((stage, cause), n) in rows {
        println!("| {stage} | {cause} | {n} |");
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: census <crate dir or .rs file>... | census --fns <list> <crate dir or .rs file>...");
        std::process::exit(2);
    }
    // The program's one allocator, made here at its top and handed down: a frame, whose memory
    // comes from the OS (no global heap).
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).unwrap_or_else(|e| panic!("census: the frame: {e}"));
    let erased = blaze_own::Erased::new(&frame);
    let alloc = erased.handle();
    if args[0] == "--fns" && args.len() > 2 {
        functions(Path::new(&args[1]), &args[2..], alloc);
        return;
    }
    let mut all = Tally::default();
    for a in &args {
        let t = tally(Path::new(a), alloc);
        t.print(a);
        all.add(&t);
    }
    if args.len() > 1 {
        all.print("all of the above");
    }
}
