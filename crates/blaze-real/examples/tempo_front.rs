//! The new front end (blaze-ladder's front2 over blaze-typeck) on ONE member crate of a real
//! workspace (tempo): `tempo_front <checkout> <fetch-store> <module-store> <modules-out>
//! <script-records> <crate> [--cap N] [--reach M] [--secs S] [--std-census F] [--std-heads F]`.
//!
//! The names are loaded as `stages` loads them (examples/names_load: the unit graph, the fetch
//! store, the proc-macro modules, the build scripts' records), over the whole graph; the front
//! end is made over those names (`Front2::for_program` with no program files, a library-only
//! survey: its roots are the library crates, so no binary's, test's or example's lang rows are
//! filed; each crate with the package, edition and cfg its unit gave). Then, for the named crate:
//! 1. EVERY HEAD (tests/front2_heads.rs's wants and classes): built, or refused by reason, with
//!    the front2 file whose text raises it.
//! 2. EVERY BODY (tests/front2_census.rs's lowering, check and classes): one set of records
//!    shared by every body, as the census's default; a body counts as clean only with zero
//!    diagnostics and zero gaps.
//! 3. REACH: the first `--reach` bodies (default 200) again, each from EMPTY records: the
//!    crates whose heads its check made (p50, max): how far one body's check reaches.
//! Order: the crate's file index, offset in the file, offset in a macro expansion, then key;
//! members after their container. At most `--cap` bodies (default 20000) and heads (the same
//! cap); a safety stop after `--secs` (default 1500) is printed as such. A panic inside one item
//! is caught and filed as its own class with the item named. Every table is sorted by count,
//! then by name: no map order reaches the output. One thread.
//!
//! A class absent from the standard libraries' censuses (`--std-census`, `--std-heads`: their
//! report files) is marked NEW.

use std::alloc::{GlobalAlloc, Layout as MemLayout, System};
use std::collections::{BTreeMap, BTreeSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use blaze_ladder::front2::Front2;
use blaze_ladder::front2::body::{BodyError, BodyFinding};
use blaze_ladder::front2::lower_made::{LowerTarget, lower_made};
use blaze_ladder::front2::buckets::Buckets;
use blaze_ladder::front2::check::{BuildOrder, CheckFail, Records, check_body, eval_len};
use blaze_ladder::front2::consts::Consts;
use blaze_ladder::front2::heads::{HeadError, Store, Want};
use blaze_ladder::front2::keys::{Keys, MemberKind, Origin, checker_of};
use blaze_ladder::front2::receiver::Pointers;
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::front2::types::Why;
use blaze_ladder::front2::world_items::Gap;
use blaze_lower::scoped::Prepares;
use blaze_own::Alloc;
use blaze_scope::resolve::{At, Names};
use blaze_surface::{ItemDetail, Range};

#[expect(dead_code, reason = "stages' loader, shared: this example reads only the names and the members")]
mod names_load;

const MOST_LENS: u32 = 64;
const EXAMPLES: usize = 3;
const CONTAINER_REFUSED: &str = "its container's head is refused";

/// front2's sources, to name the file that raises a refusal (as tests/front2_heads.rs).
const SOURCES: &[(&str, &str)] = &[
    ("body.rs", include_str!("../../blaze-ladder/src/front2/body.rs")),
    ("buckets.rs", include_str!("../../blaze-ladder/src/front2/buckets.rs")),
    ("check.rs", include_str!("../../blaze-ladder/src/front2/check.rs")),
    ("consts.rs", include_str!("../../blaze-ladder/src/front2/consts.rs")),
    ("dyn_ty.rs", include_str!("../../blaze-ladder/src/front2/dyn_ty.rs")),
    ("fill.rs", include_str!("../../blaze-ladder/src/front2/fill.rs")),
    ("heads.rs", include_str!("../../blaze-ladder/src/front2/heads.rs")),
    ("index.rs", include_str!("../../blaze-ladder/src/front2/index.rs")),
    ("keys.rs", include_str!("../../blaze-ladder/src/front2/keys.rs")),
    ("langs.rs", include_str!("../../blaze-ladder/src/front2/langs.rs")),
    ("mod.rs", include_str!("../../blaze-ladder/src/front2/mod.rs")),
    ("pattern_ty.rs", include_str!("../../blaze-ladder/src/front2/pattern_ty.rs")),
    ("preds.rs", include_str!("../../blaze-ladder/src/front2/preds.rs")),
    ("program.rs", include_str!("../../blaze-ladder/src/front2/program.rs")),
    ("reach.rs", include_str!("../../blaze-ladder/src/front2/reach.rs")),
    ("spell.rs", include_str!("../../blaze-ladder/src/front2/spell.rs")),
    ("types.rs", include_str!("../../blaze-ladder/src/front2/types.rs")),
    ("world_items.rs", include_str!("../../blaze-ladder/src/front2/world_items.rs")),
    ("world_paths.rs", include_str!("../../blaze-ladder/src/front2/world_paths.rs")),
    ("front.rs", include_str!("../../blaze-ladder/src/front.rs")),
];

// ---------------------------------------------------------------------------------------------
// The heap's peak over the whole run (as stages' `Counting`, always on).

struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

// SAFETY: every call forwards to `System` with the caller's own layout and pointer, unchanged;
// the counters are plain atomics beside it.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: MemLayout) -> *mut u8 {
        let live = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
        PEAK.fetch_max(live, Ordering::Relaxed);
        // SAFETY: the caller's layout, as `GlobalAlloc::alloc` requires.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: MemLayout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        // SAFETY: `ptr` came from `System` with this layout.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: MemLayout, new_size: usize) -> *mut u8 {
        if new_size >= layout.size() {
            let live = LIVE.fetch_add(new_size - layout.size(), Ordering::Relaxed) + new_size - layout.size();
            PEAK.fetch_max(live, Ordering::Relaxed);
        } else {
            LIVE.fetch_sub(layout.size() - new_size, Ordering::Relaxed);
        }
        // SAFETY: `ptr` came from `System` with `layout`; the new size is the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static HEAP: Counting = Counting;

fn mem() -> String {
    format!("heap live {:.0} MB, peak {:.0} MB", LIVE.load(Ordering::Relaxed) as f64 / 1e6, PEAK.load(Ordering::Relaxed) as f64 / 1e6)
}

/// The machine's load average, as `sysctl` prints it.
fn load() -> String {
    std::process::Command::new("sysctl")
        .args(["-n", "vm.loadavg"])
        .output()
        .map_or_else(|e| format!("(sysctl: {e})"), |o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// Where the last panic was raised: the hook writes it, the census reads it after a catch.
static PANIC_AT: Mutex<String> = Mutex::new(String::new());

fn panic_text(p: &(dyn std::any::Any + Send)) -> String {
    p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_else(|| "(no message)".into())
}

fn panic_at() -> (String, String) {
    let at = PANIC_AT.lock().map(|s| s.clone()).unwrap_or_default();
    let file = at.rsplit_once(':').map_or(at.as_str(), |x| x.0).to_string();
    (file, at)
}

// ---------------------------------------------------------------------------------------------
// Classes, as tests/front2_census.rs and tests/front2_heads.rs name them.

fn cls(debug: &str) -> &str {
    let end = debug.find(['(', ' ', '{']).unwrap_or(debug.len());
    &debug[..end]
}

fn short(s: &str, n: usize) -> String {
    let one: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    match one.char_indices().nth(n) {
        Some((i, _)) => format!("{}..", &one[..i]),
        None => one,
    }
}

fn nest(debug: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    let mut rest = debug;
    for _ in 0..4 {
        let end = rest.find(['(', ' ', '{', ',', ')']).unwrap_or(rest.len());
        let word = &rest[..end];
        if word.is_empty() || !word.starts_with(|c: char| c.is_ascii_uppercase()) || matches!(word, "ItemKey" | "LibKey" | "Some" | "None" | "Range" | "Name" | "PatId" | "ExprId") {
            break;
        }
        parts.push(word);
        if !rest[end..].starts_with('(') {
            break;
        }
        rest = &rest[end + 1..];
    }
    let mut out = parts.join("/");
    if let Some(q) = rest.strip_prefix('"').and_then(|r| r.split_once('"')).map(|x| x.0) {
        out.push_str(&format!(" \"{}\"", short(q, 70)));
    }
    if out.is_empty() { cls(debug).to_string() } else { out }
}

fn why_class(why: &Why) -> String {
    nest(&format!("{why:?}"))
}

fn head_class(e: &HeadError) -> String {
    match e {
        HeadError::Item(_, why) => format!("Item/{}", why_class(why)),
        other => nest(&format!("{other:?}")),
    }
}

fn body_class(e: &BodyError) -> String {
    match e {
        BodyError::NoText(_) => "lower NoText".into(),
        BodyError::Cfg(_) => "lower Cfg".into(),
        BodyError::Generics { why, .. } => format!("lower Generics: {}", why_class(why)),
        BodyError::Record { why, .. } => format!("lower Record: {}", why_class(why)),
        BodyError::Lowered { findings, .. } => match findings.first() {
            Some(BodyFinding::Macro { path, .. }) => format!("lower Lowered: Macro `{path}`"),
            Some(BodyFinding::Unhandled { syntax, .. }) => format!("lower Lowered: Unhandled {syntax:?}"),
            Some(BodyFinding::Diag { kind, .. }) => format!("lower Lowered: Diag {kind:?}"),
            Some(BodyFinding::CfgOut) => "lower Lowered: CfgOut".into(),
            None => "lower Lowered: (no finding)".into(),
        },
        BodyError::Calls { errors, .. } => match errors.first() {
            Some(e) => format!("lower Calls: `{}`", e.path),
            None => "lower Calls: (no call)".into(),
        },
        BodyError::Convert { refused, why, .. } => {
            format!("lower Convert: {}{}", cls(&format!("{refused:?}")), why.as_ref().map_or(String::new(), |w| format!(" / {}", why_class(w))))
        }
        BodyError::Head { error, .. } => format!("lower Head: {}", head_class(error)),
        BodyError::Rounds { .. } => "lower Rounds".into(),
        BodyError::Made { need, .. } => format!("lower Made: {}", cls(&format!("{need:?}"))),
        BodyError::Table { error, .. } => format!("lower Table: {}", cls(&format!("{error:?}"))),
        BodyError::Traits { error, .. } => format!("lower Traits: {}", cls(&format!("{error:?}"))),
        BodyError::NoHead(_) => "lower NoHead".into(),
        BodyError::NoBody(_) => "lower NoBody".into(),
        BodyError::Const { error, .. } => format!("lower Const: {}", cls(&format!("{error:?}"))),
        BodyError::PatternName { error, .. } => format!("lower PatternName: {}", cls(&format!("{error:?}"))),
    }
}

fn impl_trait(text: &str) -> String {
    let Some(at) = text.find("impl") else { return "(no impl text)".into() };
    let rest = &text[at + 4..];
    let (mut depth, mut i) = (0i32, 0usize);
    let b = rest.as_bytes();
    let rest = if rest.trim_start().starts_with('<') {
        while i < b.len() {
            match b[i] {
                b'<' => depth += 1,
                b'>' if i > 0 && b[i - 1] == b'-' => {}
                b'>' => {
                    depth -= 1;
                    if depth == 0 {
                        i += 1;
                        break;
                    }
                }
                _ => {}
            }
            i += 1;
        }
        &rest[i..]
    } else {
        rest
    };
    let Some(f) = rest.find(" for ") else { return "(inherent)".into() };
    let path = rest[..f].trim().trim_start_matches(['!', '?']).trim_start_matches("const ");
    let path = path.split('<').next().unwrap_or(path);
    path.rsplit("::").next().unwrap_or(path).trim().to_string()
}

fn refusal_reason(why: &str) -> String {
    let after = why.split_once("): ").map_or(why, |x| x.1);
    let n = nest(after);
    match n.split_once(" \"") {
        Some((v, q)) => format!("{v} \"{}", short(q, 40)),
        None => n,
    }
}

fn gap_class(g: &Gap, names: &Names<'_>) -> String {
    match g {
        Gap::Lang(_, s) => format!("gap Lang: {s}"),
        Gap::BuiltinTypePath(s) => format!("gap BuiltinTypePath: {s}"),
        // The reason without the impl's key: one class per reason, the key in the examples.
        Gap::UnreadImpl(_, s) => match blaze_ladder::front2::header_written::split_written(s) {
            (reason, Some(path)) => format!("gap UnreadImpl: {} {path}", refusal_reason(reason)),
            (reason, None) => format!("gap UnreadImpl: {}", refusal_reason(reason)),
        },
        Gap::RefusedImpl { impl_, why, .. } => {
            let tr = names.item_text(*impl_).map_or("(no text)".to_string(), |t| t.impl_header.map_or_else(|| impl_trait(t.text), impl_trait));
            format!("gap RefusedImpl [{tr}]: {}", refusal_reason(why))
        }
        Gap::FnHead(_, s) => format!("gap FnHead: {}", nest(s)),
        Gap::Unspelled(u) => format!("gap Unspelled: {}", nest(&format!("{u:?}"))),
        Gap::Local(g) => format!("gap Local: {}", g.form),
        Gap::Resolve(e) => format!("gap Resolve: {}", nest(&format!("{e:?}"))),
        Gap::Fill(e) => format!("gap Fill: {}", nest(&format!("{e:?}"))),
        Gap::BodyRecord(q) => format!("gap BodyRecord: {}", nest(&format!("{q:?}"))),
        other => format!("gap {}", cls(&format!("{other:?}"))),
    }
}

fn bound_trait(about: &str) -> String {
    let after = about.rsplit_once(": ").map_or(about, |(_, t)| t);
    let end = after.find(['<', ' ', '`', ',']).unwrap_or(after.len());
    after[..end].to_string()
}

fn source_at(text: &str, start: u32, end: u32) -> String {
    let at = text.get(start as usize..end as usize).unwrap_or("(past the item's text)");
    short(at, 80)
}

/// The impls a failure's gaps name (unread or refused candidates), each with its gap's text in
/// full; `true` when every gap of the failure is such an impl.
fn blocking_impls(e: &CheckFail, into: &mut Vec<(blaze_id::ItemKey, String)>) -> bool {
    let CheckFail::Gap { gaps, .. } = e else { return false };
    let mut only = !gaps.is_empty();
    for g in gaps.iter() {
        match g {
            Gap::UnreadImpl(k, _) => into.push((*k, format!("{g}"))),
            Gap::RefusedImpl { impl_, .. } => into.push((*impl_, format!("{g}"))),
            _ => only = false,
        }
    }
    only
}

fn fail_class(e: &CheckFail, text: &str, names: &Names<'_>) -> (String, String) {
    match e {
        CheckFail::Gap { gaps, .. } => {
            let first = gaps.first().map_or("gap (empty list)".to_string(), |g| gap_class(g, names));
            (first, format!("{} gaps; the first: {}", gaps.len(), gaps.first().map(|g| short(&format!("{g}"), 200)).unwrap_or_default()))
        }
        CheckFail::Diags { diags, .. } => {
            let (code, _, about) = &diags[0];
            let c = code.code();
            let class = if c == "E0277" {
                format!("diag E0277: {}", bound_trait(about))
            } else if let Some(lang) = about.strip_prefix("no lang item ") {
                format!("diag {c}: no lang item {}", lang.split_whitespace().next().unwrap_or("?"))
            } else {
                format!("diag {c}")
            };
            let d = &diags[0];
            (class, format!("{} diagnostics; the first at `{}`: {}", diags.len(), source_at(text, d.1.start, d.1.end), short(about, 200)))
        }
        CheckFail::Check { error, .. } => (format!("internal Check: {}", nest(&format!("{error:?}"))), short(&format!("{e}"), 200)),
        CheckFail::Resolve { error, .. } => (format!("check Resolve: {}", nest(&format!("{error:?}"))), short(&format!("{e}"), 200)),
        CheckFail::Head { error, .. } => (format!("check Head: {}", head_class(error)), short(&format!("{e}"), 200)),
        other => (format!("check {}", cls(&format!("{other:?}"))), short(&format!("{e}"), 200)),
    }
}

/// Who a body's class belongs to, as far as its name tells.
fn body_owner(class: &str) -> &'static str {
    if class.starts_with("lower Lowered: Macro") {
        "expansion (front2 body.rs's host refuses proc macros, env!, include! in bodies; else the names library's macro definition)"
    } else if class.contains("NotFound") || class.contains("Unresolved") || class.starts_with("lower Traits") || class.starts_with("member not filed") {
        "names library (a name or member not found; or the build-script wall), else front2"
    } else if class.starts_with("lower Lowered") {
        "lowering (blaze-lower)"
    } else if class.starts_with("internal") {
        "type checker (internal error)"
    } else if class.starts_with("diag") {
        "type checker, or the front2 records it reads (tempo compiles: ours)"
    } else if class.starts_with("check Resolve") || class.starts_with("gap Resolve") {
        "type checker's instance resolution over front2's impl records"
    } else if class.starts_with("panic") {
        "the crate the panic location names"
    } else if class.starts_with("container head") || class.starts_with("member head") || class.starts_with("lower Head") || class.starts_with("check Head") || class.starts_with("gap RefusedImpl") || class.starts_with("gap FnHead") {
        "front2 heads"
    } else if class.starts_with("lower Convert") || class.starts_with("lower Generics") || class.starts_with("lower Record") {
        "front2 types-and-bounds (to_typeck, Builder)"
    } else {
        "front2"
    }
}

/// Who a head's class belongs to, by the file that raises it.
fn head_owner(class: &str, file: &str) -> &'static str {
    if class.starts_with("panic") {
        "the crate the panic location names"
    } else if class == CONTAINER_REFUSED {
        "front2 heads (the container's own reason)"
    } else if class.starts_with("Resolve") {
        "names library (a path not resolved; or the build-script wall), else front2"
    } else if file.contains("types.rs") || file.contains("preds.rs") || file.contains("world_paths.rs") || file.contains("dyn_ty.rs") || file.contains("pattern_ty.rs") {
        "front2 types-and-bounds"
    } else {
        "front2 items"
    }
}

fn files_with(needle: &str) -> String {
    let hits: Vec<&str> = SOURCES.iter().filter(|(_, s)| s.contains(needle)).map(|(n, _)| *n).collect();
    if hits.is_empty() { "(not found by its text)".to_string() } else { hits.join("+") }
}

struct Refusal {
    class: String,
    file: String,
    base: Option<blaze_id::ItemKey>,
    range: Option<Range>,
    owner: Option<blaze_id::ItemKey>,
}

fn why_range(why: &Why) -> Option<Range> {
    match why {
        Why::Need(_) | Why::Regions(_) => None,
        Why::Unsupported(_, r)
        | Why::UnsupportedIn(_, _, r)
        | Why::Program(_, r)
        | Why::Resolve(_, r)
        | Why::NoCheckerType(_, r)
        | Why::AssocNotFound(_, r)
        | Why::AssocAmbiguous(_, _, r)
        | Why::TwoPrincipals(_, _, r)
        | Why::TypeMacro(_, r)
        | Why::PatternType(_, r)
        | Why::ConstPath(_, r)
        | Why::NeedConst(_, r) => Some(*r),
    }
}

fn why_refusal(base: blaze_id::ItemKey, why: &Why) -> Refusal {
    let debug = format!("{why:?}");
    let variant = cls(&debug).to_string();
    let (class, file) = match why {
        Why::Unsupported(s, _) => (format!("Unsupported \"{s}\""), files_with(&format!("\"{s}\""))),
        Why::Program(s, _) => {
            let fixed = s.split('`').step_by(2).max_by_key(|p| p.len()).unwrap_or("");
            (format!("{variant} \"{}\"", short(s, 60)), if fixed.trim().len() >= 8 { files_with(fixed) } else { files_with("Why::Program(") })
        }
        Why::PatternType(s, _) | Why::TypeMacro(s, _) => (format!("{variant} \"{}\"", short(s, 60)), files_with(&format!("Why::{variant}("))),
        Why::Resolve(e, _) => {
            let e = format!("{e:?}");
            (format!("Resolve {}", cls(&e)), files_with("Why::Resolve("))
        }
        _ => (variant.clone(), files_with(&format!("Why::{variant}("))),
    };
    Refusal { class, file, base: Some(base), range: why_range(why), owner: None }
}

fn plain_refusal(class: String, file: &str) -> Refusal {
    Refusal { class, file: file.to_string(), base: None, range: None, owner: None }
}

fn head_refusal(e: &HeadError) -> Refusal {
    match e {
        HeadError::Item(k, why) => why_refusal(*k, why),
        other => {
            let d = format!("{other:?}");
            let detail = match other {
                HeadError::NotAHead(_, what) => format!(" {what}"),
                _ => String::new(),
            };
            plain_refusal(format!("HeadError::{}{detail}", cls(&d)), "heads.rs")
        }
    }
}

fn make(r: &mut Records<'_, '_>, w: Want) -> Result<(), Refusal> {
    let mut lens = 0;
    loop {
        match r.store.ensure(r.names, r.keys, w) {
            Ok(()) => return Ok(()),
            Err(HeadError::Item(_, Why::NeedConst(c, _))) if lens < MOST_LENS => {
                lens += 1;
                if let Err(e) = eval_len(r, c) {
                    return Err(plain_refusal(format!("const length: {}", cls(&format!("{e:?}"))), "check.rs"));
                }
            }
            Err(e) => return Err(head_refusal(&e)),
        }
    }
}

fn base_text<'n>(names: &'n Names<'n>, x: &Refusal) -> Option<&'n str> {
    let key = x.base?;
    match x.owner {
        Some(owner) => names.members(owner)?.iter().find(|m| m.key == key).map(|m| m.text),
        None => names.item_text(key).map(|t| t.text),
    }
}

// ---------------------------------------------------------------------------------------------
// Order and places.

/// (file index, offset, expansion offset + 1, shown place) of `text` of item `key` in `krate`.
fn place_of(names: &Names<'_>, krate: usize, key: blaze_id::ItemKey, text: &str) -> (u32, u32, u32, String) {
    let cn = names.krate(krate);
    let mut k = key;
    let mut inner: Option<u32> = None;
    let mut sub_text = text.as_ptr() as usize;
    for _ in 0..16 {
        let Some(p) = names.place(k) else { break };
        let Some((base, _)) = cn.outline_at(p.at) else { break };
        let b = base.as_ptr() as usize;
        let off = if sub_text >= b && sub_text <= b + base.len() { (sub_text - b) as u32 } else { 0 };
        match p.at {
            At::File(f) => {
                let line = base.as_bytes()[..off as usize].iter().filter(|c| **c == b'\n').count() + 1;
                let path = cn.files.get(f as usize).map_or("?", |s| s.path);
                let shown = match inner {
                    Some(i) => format!("{path}:{line} (in a macro expansion, offset {i})"),
                    None => format!("{path}:{line}"),
                };
                return (f, off, inner.map_or(0, |i| i + 1), shown);
            }
            At::Expansion(call) => {
                inner.get_or_insert(off);
                k = call;
                sub_text = names.item_text(call).map_or(0, |t| t.text.as_ptr() as usize);
            }
        }
    }
    (u32::MAX, 0, inner.map_or(0, |i| i + 1), "(no file place)".to_string())
}

/// A class table: class -> (count, raising file or owner, examples).
type Table = BTreeMap<String, (usize, String, Vec<String>)>;

fn print_table(out: &mut String, title: &str, t: &Table, std_text: &str, owner: &dyn Fn(&str, &str) -> String) {
    let mut cs: Vec<_> = t.iter().collect();
    cs.sort_by(|a, b| b.1.0.cmp(&a.1.0).then_with(|| a.0.cmp(b.0)));
    out.push_str(&format!("{title} ({} classes), largest first:\n", cs.len()));
    for (c, (n, file, ex)) in cs {
        let new = if !std_text.is_empty() && !std_text.contains(c.as_str()) { "NEW " } else { "" };
        out.push_str(&format!("{n:7} {new}{c}  [{}] [owner: {}]\n", file, owner(c, file)));
        for e in ex {
            out.push_str(&format!("          e.g. {e}\n"));
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Heads.

fn heads<'n>(names: &'n Names<'n>, front: &mut Front2<'n>, krate: usize, cname: &str, cap: usize, std_heads: &str, out: &mut String) -> f64 {
    let start = Instant::now();
    let resolver = front.resolver;
    let cn = names.krate(krate);
    let mut items: Vec<(blaze_id::ItemKey, ItemDetail)> = names.items(krate).filter_map(|(k, _, _)| resolver.detail(k).map(|d| (k, d))).collect();
    let mut member_keys: BTreeSet<blaze_id::ItemKey> = BTreeSet::new();
    for (k, d) in &items {
        if matches!(d, ItemDetail::Impl | ItemDetail::Trait) {
            for m in names.members(*k).unwrap_or(&[]) {
                member_keys.insert(m.key);
            }
        }
    }
    items.retain(|(k, d)| {
        !member_keys.contains(k)
            && matches!(
                d,
                ItemDetail::Struct | ItemDetail::Enum | ItemDetail::Union | ItemDetail::Trait | ItemDetail::TraitAlias | ItemDetail::Impl | ItemDetail::Fn { .. } | ItemDetail::Const { .. } | ItemDetail::Static { .. } | ItemDetail::TypeAlias
            )
    });
    let mut ordered: Vec<((u32, u32, u32, blaze_id::ItemKey), String, blaze_id::ItemKey, ItemDetail)> = items
        .iter()
        .filter_map(|(k, d)| {
            let it = names.item_text(*k)?;
            let (f, o, s, at) = place_of(names, krate, *k, it.text);
            Some(((f, o, s, *k), at, *k, *d))
        })
        .collect();
    ordered.sort_by(|a, b| a.0.cmp(&b.0));
    let listed = ordered.len();
    ordered.truncate(cap);

    let bump = front.frame.alloc(blaze_own::Erased::new(front.frame)).bump();
    let prepared = Prepares::new_in(bump);
    let mut store = Store::new(front.index.lang("sized"), front.frame, Alloc::global());
    store.pointers = Pointers::find(&front.index, front.resolver);
    let mut buckets = None;
    let mut consts = Consts::default();
    // (kind, path, place, outcome)
    let mut rows: Vec<(&'static str, String, String, Result<(), Refusal>)> = Vec::new();
    let mut cfg_skipped = 0usize;
    for (_, at, k, d) in &ordered {
        let Some(it) = names.item_text(*k) else { continue };
        let module = cn.path_of(it.module).unwrap_or("?");
        let path = match d {
            ItemDetail::Impl => format!("{module}::<{}>", short(it.impl_header.unwrap_or("impl"), 60)),
            _ => format!("{module}::{}", it.name.unwrap_or("?")),
        };
        let mut r = Records { names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: Some((&mut consts, &prepared)) };
        let got = catch_unwind(AssertUnwindSafe(|| {
            let mut rs: Vec<(&'static str, String, String, Result<(), Refusal>)> = Vec::new();
            let ck = checker_of(*k);
            let (kind, got): (&'static str, Result<(), Refusal>) = match d {
                ItemDetail::Struct => ("struct", make(&mut r, Want::Adt(*k))),
                ItemDetail::Enum => ("enum", make(&mut r, Want::Adt(*k))),
                ItemDetail::Union => ("union", make(&mut r, Want::Adt(*k))),
                ItemDetail::Trait => ("trait", make(&mut r, Want::Head(*k)).and_then(|()| make(&mut r, Want::Members(*k)))),
                ItemDetail::TraitAlias => ("trait alias", make(&mut r, Want::Head(*k))),
                ItemDetail::Impl => ("impl", make(&mut r, Want::Impl(*k))),
                ItemDetail::Fn { .. } => ("fn", make(&mut r, Want::Fn(*k))),
                ItemDetail::Const { .. } => ("const", make(&mut r, Want::Fn(*k))),
                ItemDetail::Static { .. } => ("static", make(&mut r, Want::Fn(*k))),
                ItemDetail::TypeAlias => {
                    let got = make(&mut r, Want::Head(*k)).and_then(|()| match r.store.maps.alias_gaps.get(k) {
                        // Lowered here, never filed from a record: the builder's `Why` is kept.
                        Some(gap) => Err(why_refusal(*k, gap.why.as_ref().expect("a refusal lowered here keeps its Why"))),
                        None => Ok(()),
                    });
                    ("type alias", got)
                }
                _ => return rs,
            };
            let built = got.is_ok();
            rs.push((kind, path.clone(), at.clone(), got));
            let is_trait = matches!(d, ItemDetail::Trait);
            if !matches!(d, ItemDetail::Trait | ItemDetail::Impl) {
                return rs;
            }
            for m in names.members(*k).unwrap_or(&[]) {
                let Some(name) = m.name else { continue };
                let (mk, kind) = match (m.detail, is_trait) {
                    (ItemDetail::Fn { .. }, true) => (MemberKind::Fn, "trait fn"),
                    (ItemDetail::Fn { .. }, false) => (MemberKind::Fn, "impl fn"),
                    (ItemDetail::Const { .. }, true) => (MemberKind::Const, "trait assoc const"),
                    (ItemDetail::Const { .. }, false) => (MemberKind::Const, "impl assoc const"),
                    (ItemDetail::TypeAlias, true) => (MemberKind::Type, "trait assoc type"),
                    (ItemDetail::TypeAlias, false) => (MemberKind::Type, "impl assoc type"),
                    _ => continue,
                };
                let (_, _, _, mat) = place_of(names, krate, *k, m.text);
                let mpath = format!("{path}::{name}");
                let got = if !built {
                    Err(plain_refusal(CONTAINER_REFUSED.to_string(), "-"))
                } else {
                    let filed = r.keys.filed_member(ck, mk, name);
                    // An impl's fn's signature is its own want (heads.rs `Want::MemberFn`),
                    // never made with the impl's head: asked here. Its own refusal is its
                    // `fn_gaps` row; a want that does not settle is the made want's refusal.
                    let member_made = match (mk, filed) {
                        (MemberKind::Fn, Some(fk)) if !is_trait => make(&mut r, Want::MemberFn(*k, fk)),
                        _ => Ok(()),
                    };
                    match (mk, filed) {
                        (MemberKind::Fn, Some(fk)) => match (member_made, r.store.fn_gaps.get(&fk)) {
                            (Err(x), _) => Err(x),
                            (Ok(()), Some(why)) => {
                                let mut x = why_refusal(m.key, why);
                                x.owner = Some(*k);
                                Err(x)
                            }
                            (Ok(()), None) if r.store.fns.contains_key(&fk) => Ok(()),
                            (Ok(()), None) => Err(plain_refusal("member fn filed but no head in the store".to_string(), "heads.rs")),
                        },
                        (MemberKind::Const, Some(_)) => Ok(()),
                        (MemberKind::Const, None) if is_trait => Err(plain_refusal("trait assoc const: no head made (trait_members files none)".to_string(), "heads.rs")),
                        (MemberKind::Type, _) if !is_trait => {
                            let has = r.store.impls.get(&ck).is_some_and(|h| h.assoc.iter().any(|(n, _)| *n == blaze_typeck::Name::of(name)));
                            if has { Ok(()) } else { Err(plain_refusal("cfg".to_string(), "")) }
                        }
                        (_, Some(_)) => Ok(()),
                        (_, None) => Err(plain_refusal("cfg".to_string(), "")),
                    }
                };
                rs.push((kind, mpath, mat, got));
            }
            rs
        }));
        match got {
            Ok(rs) => {
                for row in rs {
                    if matches!(&row.3, Err(x) if x.class == "cfg") {
                        cfg_skipped += 1;
                    } else {
                        rows.push(row);
                    }
                }
            }
            Err(p) => {
                let (file, at_p) = panic_at();
                rows.push(("panicked item", path, at.clone(), Err(plain_refusal(format!("panic at {file}: {}", short(&panic_text(&*p), 60)), &at_p))));
            }
        }
    }
    let all = rows.len();
    let built = rows.iter().filter(|r| r.3.is_ok()).count();
    let pct = if all == 0 { 0.0 } else { 100.0 * built as f64 / all as f64 };
    out.push_str(&format!(
        "HEADS {cname}: heads {all}, built {built} ({pct:.1}%); {cfg_skipped} members skipped by their container's cfg (not counted); {listed} top-level items listed, cap {cap}; {:.1}s; {}\n",
        start.elapsed().as_secs_f64(),
        mem()
    ));
    let mut kinds: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    for r in &rows {
        let e = kinds.entry(r.0).or_insert((0, 0));
        e.0 += 1;
        if r.3.is_ok() {
            e.1 += 1;
        }
    }
    let mut ks: Vec<_> = kinds.into_iter().collect();
    ks.sort_by(|a, b| b.1.0.cmp(&a.1.0).then_with(|| a.0.cmp(b.0)));
    out.push_str("HEADS BY KIND (heads, built):\n");
    for (k, (n, b)) in &ks {
        out.push_str(&format!("{n:7} {b:7}  {k}\n"));
    }
    let mut classes: Table = BTreeMap::new();
    for (kind, path, at, got) in &rows {
        let Err(x) = got else { continue };
        let slot = classes.entry(x.class.clone()).or_insert((0, x.file.clone(), Vec::new()));
        slot.0 += 1;
        if slot.2.len() < EXAMPLES {
            let text = match (base_text(names, x), x.range) {
                (Some(t), Some(rg)) if rg.end == 0 => format!("(no range; item: {})", short(t, 60)),
                (Some(t), Some(rg)) => t.get(rg.start as usize..rg.end as usize).map_or_else(|| format!("(range {}..{} past the item's text)", rg.start, rg.end), |s| format!("`{}`", short(s, 80))),
                _ => "(no range)".to_string(),
            };
            slot.2.push(format!("{path} [{kind}] at {at}: {text}"));
        }
    }
    print_table(out, "HEAD REFUSALS", &classes, std_heads, &|c, f| format!("raised in {f}; {}", head_owner(c, f)));
    start.elapsed().as_secs_f64()
}

// ---------------------------------------------------------------------------------------------
// Bodies.

#[derive(Clone, Copy)]
enum Target<'n> {
    Free(blaze_id::ItemKey),
    Member { container: blaze_id::ItemKey, name: &'n str },
}

struct Entry<'n> {
    order: (u32, u32, u32, blaze_id::ItemKey),
    target: Target<'n>,
    path: String,
    at: String,
}

fn take<'n>(r: &mut Records<'_, 'n>, prepared: &Prepares<'n>, t: Target<'_>, blocked: &mut (Vec<(blaze_id::ItemKey, String)>, bool)) -> Result<Option<()>, (String, String)> {
    let target = match t {
        Target::Free(lib) => LowerTarget::Free(lib),
        Target::Member { container, name } => match r.keys.filed_member(checker_of(container), MemberKind::Fn, name) {
            Some(k) => {
                if let Some(why) = r.store.fn_gaps.get(&k) {
                    return Err((format!("member head refused: {}", why_class(why)), short(&format!("{why:?}"), 200)));
                }
                LowerTarget::Member(k)
            }
            None => return Err(("member not filed (no key after the container's head)".into(), format!("member `{name}`"))),
        },
    };
    // Every head and constant the lowering needs is made first (front2/lower_made.rs); an
    // evaluator's refusal is `BodyError::Const`, classed "lower Const: .." by `body_class` as
    // this loop classed it before.
    let lowered = lower_made(r, prepared, target);
    let body = match lowered {
        Ok(b) => b,
        Err(BodyError::NoBody(_)) => return Ok(None),
        Err(e) => return Err((body_class(&e), short(&format!("{e}"), 200))),
    };
    let view = body.lowered.view();
    let spell = Spelling::of(&view).map_err(|e| ("spelling".to_string(), short(&format!("{e:?}"), 200)))?;
    let scope = body.scope(&spell);
    match check_body(r, &body, &scope) {
        Ok(done) if done.diags.is_empty() => Ok(Some(())),
        Ok(done) => Err(("internal Ok with diagnostics".into(), format!("{} diagnostics", done.diags.len()))),
        Err(e) => {
            let text = r.store.fns.get(&body.owner).map(|f| f.text).or_else(|| r.names.item_text(body.item).map(|t| t.text)).unwrap_or("");
            blocked.1 = blocking_impls(&e, &mut blocked.0);
            Err(fail_class(&e, text, r.names))
        }
    }
}

fn container_head(r: &mut Records<'_, '_>, lib: blaze_id::ItemKey, is_trait: bool) -> Result<(), (String, String)> {
    let wants: &[Want] = if is_trait { &[Want::Head(lib), Want::Members(lib)] } else { &[Want::Impl(lib)] };
    for &w in wants {
        if let Err(x) = make(r, w) {
            return Err((format!("container head: {}", x.class), format!("raised in {}", x.file)));
        }
    }
    Ok(())
}

/// The records one body is checked over.
struct Recs<'n> {
    store: Store<'n>,
    buckets: Option<Buckets>,
    consts: Consts<'n>,
    heads: BTreeMap<blaze_id::ItemKey, Result<(), (String, String)>>,
    /// The last body's blocking impls (with each gap's text), and whether they were its only gaps.
    blocked: (Vec<(blaze_id::ItemKey, String)>, bool),
}

impl<'n> Recs<'n> {
    fn new(sized: Option<blaze_id::ItemKey>, pointers: Pointers, frame: &'n blaze_mem::Frame) -> Recs<'n> {
        let mut store = Store::new(sized, frame, Alloc::global());
        store.pointers = pointers;
        Recs { store, buckets: None, consts: Consts::default(), heads: BTreeMap::new(), blocked: (Vec::new(), false) }
    }
}

fn one<'n>(names: &'n Names<'n>, front: &mut Front2<'n>, recs: &mut Recs<'n>, prepared: &Prepares<'n>, t: Target<'_>) -> Result<Option<()>, (String, String)> {
    let resolver = front.resolver;
    let Recs { store, buckets, consts, heads, blocked } = recs;
    blocked.0.clear();
    blocked.1 = false;
    let mut r = Records { names, keys: &mut front.keys, index: &front.index, store, buckets, order: BuildOrder::AsAsked, consts: Some((consts, prepared)) };
    let got = catch_unwind(AssertUnwindSafe(|| {
        if let Target::Member { container, .. } = t {
            if !heads.contains_key(&container) {
                let is_trait = matches!(resolver.detail(container), Some(ItemDetail::Trait));
                let h = container_head(&mut r, container, is_trait);
                heads.insert(container, h);
            }
            if let Some(Err((c, d))) = heads.get(&container) {
                return Err((c.clone(), d.clone()));
            }
        }
        take(&mut r, prepared, t, blocked)
    }));
    got.unwrap_or_else(|p| {
        let (file, at) = panic_at();
        Err((format!("panic at {file}"), format!("{at}: {}", short(&panic_text(&*p), 200))))
    })
}

/// The crates whose heads `store` holds (each head's library key's crate).
fn crates_in(names: &Names<'_>, keys: &Keys, store: &Store<'_>) -> BTreeSet<usize> {
    let crate_of_checker = |k: &blaze_typeck::ItemKey| -> Option<usize> {
        let lib = match keys.origin(*k)? {
            Origin::Item(lib) => *lib,
            Origin::Member { container, .. } => keys.library_in(names, *container)?,
        };
        names.crate_of(lib)
    };
    let mut out = BTreeSet::new();
    out.extend(store.adts.keys().filter_map(crate_of_checker));
    out.extend(store.impls.keys().filter_map(crate_of_checker));
    out.extend(store.fns.keys().filter_map(crate_of_checker));
    out.extend(store.maps.traits.keys().filter_map(|k| names.crate_of(*k)));
    out
}

#[expect(clippy::too_many_arguments, reason = "the run's settings")]
fn bodies<'n>(
    names: &'n Names<'n>,
    sources: &'n dyn blaze_ladder::front2::body_host::BodySources,
    front: &mut Front2<'n>,
    krate: usize,
    cname: &str,
    cap: usize,
    reach: usize,
    secs: u64,
    std_census: &str,
    blockers: &mut BTreeMap<blaze_id::ItemKey, Blocker>,
    out: &mut String,
) -> (f64, f64) {
    let start = Instant::now();
    let resolver = front.resolver;
    let cn = names.krate(krate);
    let items: Vec<(blaze_id::ItemKey, ItemDetail)> = names.items(krate).filter_map(|(k, _, _)| resolver.detail(k).map(|d| (k, d))).collect();
    let mut member_keys: BTreeSet<blaze_id::ItemKey> = BTreeSet::new();
    for (k, d) in &items {
        if matches!(d, ItemDetail::Impl | ItemDetail::Trait) {
            for m in names.members(*k).unwrap_or(&[]) {
                member_keys.insert(m.key);
            }
        }
    }
    let mut entries: Vec<Entry<'_>> = Vec::new();
    let mut declarations = 0usize;
    for (k, d) in &items {
        let Some(it) = names.item_text(*k) else { continue };
        let module = cn.path_of(it.module).unwrap_or("?");
        match d {
            ItemDetail::Fn { .. } if !member_keys.contains(k) => {
                if it.text.trim_end().ends_with(';') {
                    declarations += 1;
                    continue;
                }
                let (f, o, s, at) = place_of(names, krate, *k, it.text);
                entries.push(Entry { order: (f, o, s, *k), target: Target::Free(*k), path: format!("{module}::{}", it.name.unwrap_or("?")), at });
            }
            ItemDetail::Impl | ItemDetail::Trait => {
                let head = it.impl_header.map(|h| short(h, 80)).unwrap_or_else(|| format!("trait {}", it.name.unwrap_or("?")));
                for m in names.members(*k).unwrap_or(&[]) {
                    let (ItemDetail::Fn { .. }, Some(name)) = (m.detail, m.name) else { continue };
                    let (f, o, s, at) = place_of(names, krate, *k, m.text);
                    entries.push(Entry { order: (f, o, s, m.key), target: Target::Member { container: *k, name }, path: format!("{module}::<{head}>::{name}"), at });
                }
            }
            _ => {}
        }
    }
    entries.sort_by(|a, b| a.order.cmp(&b.order).then_with(|| a.path.cmp(&b.path)));
    let listed = entries.len();
    out.push_str(&format!("BODIES {cname}: {listed} fns with a body listed ({declarations} bodiless free declarations skipped); records shared by every body; cap {cap}\n"));

    let bump = front.frame.alloc(blaze_own::Erased::new(front.frame)).bump();
    let prepared = Prepares::new_in(bump);
    let sized = front.index.lang("sized");
    let pointers = Pointers::find(&front.index, front.resolver);
    let mut shared = Recs::new(sized, pointers, front.frame);
    // Bodies expand with the names' own sources: env, includes, proc macros (brief 13).
    shared.store.sources = Some(sources);
    let mut classes: Table = BTreeMap::new();
    let (mut clean, mut checked, mut no_body, mut impl_only) = (0usize, 0usize, 0usize, 0usize);
    let mut stopped = None;
    for (i, e) in entries.iter().enumerate() {
        if i == cap {
            stopped = Some(format!("capped at {cap} bodies of {listed}"));
            break;
        }
        if start.elapsed().as_secs() > secs {
            stopped = Some(format!("STOPPED BY TIME ({secs}s) at {i} of {listed}: not complete"));
            break;
        }
        match one(names, front, &mut shared, &prepared, e.target) {
            Ok(None) => no_body += 1,
            Ok(Some(())) => {
                checked += 1;
                clean += 1;
            }
            Err((class, detail)) => {
                checked += 1;
                let (list, only) = &shared.blocked;
                let mut seen: BTreeSet<blaze_id::ItemKey> = BTreeSet::new();
                for (k, text) in list {
                    if !seen.insert(*k) {
                        continue;
                    }
                    let b = blockers.entry(*k).or_insert_with(|| Blocker { bodies: 0, only: 0, text: text.clone(), crates: BTreeSet::new(), first: format!("{} at {}", e.path, e.at) });
                    b.bodies += 1;
                    if *only {
                        b.only += 1;
                    }
                    b.crates.insert(cname.to_string());
                }
                if *only {
                    impl_only += 1;
                }
                let slot = classes.entry(class.clone()).or_insert((0, String::new(), Vec::new()));
                slot.0 += 1;
                if slot.2.len() < EXAMPLES {
                    slot.2.push(format!("{} at {}: {}", e.path, e.at, short(&detail, 240)));
                }
            }
        }
    }
    let pass_s = start.elapsed().as_secs_f64();
    let pct = if checked == 0 { 0.0 } else { 100.0 * clean as f64 / checked as f64 };
    out.push_str(&format!(
        "BODIES {cname}: bodies {checked}, clean {clean} ({pct:.1}%); {no_body} listed members had no body; {}; {pass_s:.1}s; {}\n",
        stopped.as_deref().unwrap_or("all listed bodies taken"),
        mem()
    ));
    for slot in classes.values_mut() {
        slot.1 = String::from("body");
    }
    print_table(out, "BODY CLASSES", &classes, std_census, &|c, _| body_owner(c).to_string());
    out.push_str(&format!(
        "BLOCKED BY IMPLS ONLY {cname}: {impl_only} of {checked} bodies have no gap but unread or refused candidate impls (whether each is clean without them is not measured: that needs a front2 option this program does not have)\n"
    ));

    // Reach: each of the first `reach` bodies from empty records.
    let r0 = Instant::now();
    let mut touched: Vec<usize> = Vec::new();
    let mut heads_made: Vec<usize> = Vec::new();
    let mut widest: Vec<(usize, String)> = Vec::new();
    for e in entries.iter().take(reach) {
        if start.elapsed().as_secs() > secs {
            break;
        }
        let mut fresh = Recs::new(sized, pointers, front.frame);
        fresh.store.sources = Some(sources);
        let got = one(names, front, &mut fresh, &prepared, e.target);
        if matches!(got, Ok(None)) {
            continue;
        }
        let cs = crates_in(names, &front.keys, &fresh.store);
        touched.push(cs.len());
        heads_made.push(fresh.store.adts.len() + fresh.store.impls.len() + fresh.store.fns.len() + fresh.store.maps.traits.len());
        widest.push((cs.len(), e.path.clone()));
    }
    touched.sort_unstable();
    heads_made.sort_unstable();
    widest.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let p = |v: &[usize], q: f64| -> usize {
        if v.is_empty() {
            0
        } else {
            v[(((q / 100.0) * v.len() as f64).ceil() as usize).clamp(1, v.len()) - 1]
        }
    };
    out.push_str(&format!(
        "REACH {cname}: {} bodies each from empty records (first {reach} in order): crates touched p50 {}, p90 {}, max {}; heads made p50 {}, p90 {}, max {}; widest: {:?}; {:.1}s\n",
        touched.len(),
        p(&touched, 50.0),
        p(&touched, 90.0),
        p(&touched, 100.0),
        p(&heads_made, 50.0),
        p(&heads_made, 90.0),
        p(&heads_made, 100.0),
        widest.iter().take(3).collect::<Vec<_>>(),
        r0.elapsed().as_secs_f64()
    ));
    (pass_s, r0.elapsed().as_secs_f64())
}

/// An impl that stops bodies as an unread or refused candidate.
struct Blocker {
    bodies: usize,
    /// Bodies whose only gaps are such impls.
    only: usize,
    /// Its gap's text in full.
    text: String,
    crates: BTreeSet<String>,
    first: String,
}

/// The blocking impls, most bodies first, each with its crate, place and refusal in full.
fn print_blockers(names: &Names<'_>, title: &str, blockers: &BTreeMap<blaze_id::ItemKey, Blocker>) -> String {
    let mut bs: Vec<_> = blockers.iter().collect();
    bs.sort_by(|a, b| b.1.bodies.cmp(&a.1.bodies).then_with(|| a.0.cmp(b.0)));
    let mut out = format!("{title}: {} impls block bodies, most first:\n", bs.len());
    for (k, b) in bs {
        let (cr, place) = match (names.crate_of(*k), names.item_text(*k)) {
            (Some(c), Some(t)) => (names.krate(c).name.to_string(), place_of(names, c, *k, t.text).3),
            (Some(c), None) => (names.krate(c).name.to_string(), "(no text)".to_string()),
            _ => ("?".to_string(), "?".to_string()),
        };
        let header = names.item_text(*k).and_then(|t| t.impl_header).map_or_else(|| "(no header text)".to_string(), |h| short(h, 120));
        out.push_str(&format!(
            "{:6} bodies ({} only by impls) in {:?}: impl {k:x} of crate {cr} at {place}: `{header}`\n         first blocked: {}\n         gap: {}\n",
            b.bodies, b.only, b.crates, b.first, b.text
        ));
    }
    out
}

/// The proc-macro runner's findings, one line each (a call it refused or that failed is never
/// a silent empty expansion: blaze-proc-macro's `Finding`, as stages.rs prints them), then
/// every error an expansion emitted (`compile_error!`, a proc macro's error or panic), naming
/// the crate it was written in, the macro, the call and the message (blaze-scope's
/// `ExpandNote::Failed` keeps the expansion's words: `blaze_expand::Failed`).
fn print_proc_findings(loaded: &names_load::Loaded) {
    use blaze_expand::{Diagnostic, ProcLevel};
    use blaze_scope::resolve::{ExpandNote, NamesNote};
    if let Some(procs) = loaded.procs {
        for finding in procs.procs.take_findings() {
            println!("proc-macro finding: {finding}");
        }
    }
    for (k, c) in loaded.names.crates().iter().enumerate() {
        for note in loaded.names.notes(k) {
            let NamesNote::Expand { call, at, note: ExpandNote::Failed { path, failed, .. } } = note else { continue };
            let what = match failed.diagnostic {
                Diagnostic::CompileError { .. } => "compile_error!",
                Diagnostic::ProcMacro { level: ProcLevel::Error, .. } => "a proc macro's error",
                Diagnostic::ProcMacroPanicked { .. } => "a proc macro's panic",
                _ => continue,
            };
            println!("expansion error: crate {}: {what} from the call of `{path}` ({call:?} at {at:?}): {failed}", c.name);
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 7 {
        eprintln!("usage: tempo_front <checkout> <fetch-store> <module-store> <modules-out> <script-records> <crate[,crate..]> [--cap N] [--reach M] [--secs S] [--std-census F] [--std-heads F]");
        std::process::exit(2);
    }
    let flag = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let cap: usize = flag("--cap").and_then(|s| s.parse().ok()).unwrap_or(20_000);
    let reach: usize = flag("--reach").and_then(|s| s.parse().ok()).unwrap_or(200);
    let secs: u64 = flag("--secs").and_then(|s| s.parse().ok()).unwrap_or(1500);
    let read = |f: Option<String>| f.map(|p| std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{p}: {e}"))).unwrap_or_default();
    let std_census = read(flag("--std-census"));
    let std_heads = read(flag("--std-heads"));
    let (root, store, modules, modules_out, scripts) = (PathBuf::from(&args[1]), PathBuf::from(&args[2]), PathBuf::from(&args[3]), PathBuf::from(&args[4]), PathBuf::from(&args[5]));
    println!("tempo_front on {} for {}; load average before: {}; one thread", root.display(), args[6], load());
    std::panic::set_hook(Box::new(|info| {
        if let Ok(mut at) = PANIC_AT.lock() {
            *at = info.location().map_or_else(|| "?".to_string(), |l| format!("{}:{}", l.file(), l.line()));
        }
    }));
    let t0 = Instant::now();
    let loaded = match names_load::load(&root, &store, Some((modules.as_path(), modules_out.as_path())), Some(scripts.as_path()), false, blaze_own::Heap::process()) {
        Ok(l) => l,
        Err(e) => {
            println!("stopped: the names: {e}");
            std::process::exit(1);
        }
    };
    let names: &'static Names<'static> = loaded.names;
    let names_s = t0.elapsed().as_secs_f64();
    let names_mb = LIVE.load(Ordering::Relaxed) as f64 / 1e6;
    println!("names: {} crates in {names_s:.1}s (resolve {:.1}s); {}", names.crates().len(), loaded.resolve_ms / 1e3, mem());
    print_proc_findings(&loaded);
    let mut blockers: BTreeMap<blaze_id::ItemKey, Blocker> = BTreeMap::new();
    let t1 = Instant::now();
    // The command's frame, alive as long as the names (the whole run).
    let frame: &'static blaze_mem::Frame = Box::leak(Box::new(blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command's frame")));
    // A library-only survey: the roots are the library crates (a toolchain crate, or a unit of
    // kind `lib`); a binary, test or example unit is its own program with its own
    // `#[global_allocator]`, and its lang rows are not this survey's (front2 linked.rs).
    let libs: Vec<usize> = (0..names.crates().len())
        .filter(|&k| match loaded.crates.crates[k].origin {
            blaze_scope::units::Origin::Toolchain { .. } => true,
            blaze_scope::units::Origin::Unit(u) => loaded.graph.units[u.index()].kind == blaze_unit_graph::UnitKind::Lib,
        })
        .collect();
    let linked = match blaze_ladder::front2::linked::Linked::of(names, &libs, blaze_own::Heap::process().alloc()) {
        Ok(l) => l,
        Err(e) => {
            println!("stopped: the library roots: {e}");
            std::process::exit(1);
        }
    };
    let mut front = match Front2::for_program(names, frame, &[], &linked) {
        Ok(f) => f,
        Err(e) => {
            println!("stopped: Front2::for_program: {e}");
            std::process::exit(1);
        }
    };
    println!("front2: Front2::for_program over {} library roots ({} crates linked) in {:.1}s; {} lang items; {}", linked.roots().len(), linked.crates().count(), t1.elapsed().as_secs_f64(), front.index.lang_count(), mem());
    for want in args[6].split(',') {
        let ks: Vec<usize> = names.crates().iter().enumerate().filter(|(k, c)| c.name == want && loaded.member.get(*k).copied().unwrap_or(false)).map(|(k, _)| k).collect();
        if ks.is_empty() {
            println!("no member crate named {want:?}");
            continue;
        }
        for k in ks {
            let c0 = Instant::now();
            let mut out = String::new();
            let heads_s = heads(names, &mut front, k, want, cap, &std_heads, &mut out);
            let mut mine: BTreeMap<blaze_id::ItemKey, Blocker> = BTreeMap::new();
            let (bodies_s, reach_s) = bodies(names, loaded.body_sources(), &mut front, k, want, cap, reach, secs, &std_census, &mut mine, &mut out);
            out.push_str(&print_blockers(names, &format!("BLOCKING IMPLS {want}"), &mine));
            for (key, b) in mine {
                let all = blockers.entry(key).or_insert_with(|| Blocker { bodies: 0, only: 0, text: b.text.clone(), crates: BTreeSet::new(), first: b.first.clone() });
                all.bodies += b.bodies;
                all.only += b.only;
                all.crates.extend(b.crates);
            }
            out.push_str(&format!(
                "TIME {want}: the graph's names once {names_s:.1}s (heap {names_mb:.0} MB after them); heads {heads_s:.1}s; bodies {bodies_s:.1}s; reach pass {reach_s:.1}s; {}; load {}\n",
                mem(),
                load()
            ));
            let head = out.lines().find(|l| l.starts_with("HEADS ")).unwrap_or("").to_string();
            let body = out.lines().find(|l| l.starts_with("BODIES ") && l.contains("clean")).unwrap_or("").to_string();
            let h = head.split(':').nth(1).and_then(|s| s.split(';').next()).unwrap_or("").trim().to_string();
            let b = body.split(':').nth(1).and_then(|s| s.split(';').next()).unwrap_or("").trim().to_string();
            println!("TEMPO {want} (crate {k}, edition {:?}): {h}; {b}; {:.1}s; {}", names.edition(k), c0.elapsed().as_secs_f64(), mem());
            print!("{out}");
        }
    }
    print!("{}", print_blockers(names, "BLOCKING IMPLS ACROSS THE RUN", &blockers));
    println!("load average after: {}; {:.1}s in all", load(), t0.elapsed().as_secs_f64());
}
