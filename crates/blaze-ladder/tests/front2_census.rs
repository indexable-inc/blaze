//! The census WITH the world: every function body of ONE real crate of the toolchain (free fns,
//! inherent and trait impl methods, provided trait methods), each lowered by front2
//! (`body::lower_fn` / `body::lower_member`) and checked by `check::check_body` over the real
//! names of a program on the `blaze-2026.11` edition, then filed under exactly one class.
//!
//! A body counts as clean only with zero diagnostics and zero gaps. Bodies are taken in a stated
//! order (the crate's file index, then the byte offset in that file; an item made by a macro
//! expansion at its call's place, then its offset in the expansion; ties by key), and at most
//! `FRONT2_CENSUS_CAP` of them (default 100000) in that order. A panic in one body's lowering or
//! check is caught and filed as its own class with the body named; the census goes on (the
//! shared records are kept: a body after a panic reads what the panicking one left).
//!
//! Environment: `FRONT2_CENSUS_CRATE` (default `bstd`, else `blaze_std`), `FRONT2_CENSUS_CAP`,
//! `FRONT2_CENSUS_SECS` (a safety stop, default 600: a stop by it is printed as such and makes
//! the run not comparable), `FRONT2_CENSUS_OUT` (a file the report is written to, also written
//! every 250 bodies so a killed run leaves its prefix). It needs the installed toolchain under
//! `$HOME/.blaze/v2/toolchain`, as tests/front2_main.rs does.

use std::collections::{BTreeMap, BTreeSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Mutex;
use std::time::Instant;

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::buckets::Buckets;
use blaze_ladder::front2::body::{BodyError, BodyFinding, lower_fn, lower_member};
use blaze_ladder::front2::check::{BuildOrder, CheckFail, Records, check_body, eval_len};
use blaze_ladder::front2::consts::Consts;
use blaze_ladder::front2::heads::{HeadError, Store, Want};
use blaze_ladder::front2::keys::{MemberKind, checker_of};
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::front2::types::Why;
use blaze_ladder::front2::world_items::Gap;
use blaze_ladder::refusal::SourceFile;
use blaze_lower::scoped::Prepares;
use blaze_scope::resolve::{At, Names};
use blaze_surface::ItemDetail;

const EDITION: &str = "blaze-2026.11";
const MAIN: &str = "fn main(cx: &Cx, io: Io) -> Result<(), Error> {
    io.need_out()?.line(cx, \"Hello, world!\")?;
    Ok(())
}
";
/// How many times a body is lowered again after an array length naming a constant is evaluated
/// (as reach.rs's `MOST_LENS`).
const MOST_LENS: u32 = 64;
const EXAMPLES: usize = 3;

/// Where the last panic was raised: the hook writes it, the census reads it after a catch.
static PANIC_AT: Mutex<String> = Mutex::new(String::new());

/// One body to take: a free fn by its library key, or a member fn of a container.
#[derive(Clone, Copy)]
enum Target<'n> {
    Free(blaze_id::ItemKey),
    Member { container: blaze_id::ItemKey, member: blaze_id::ItemKey, name: &'n str },
}

struct Entry<'n> {
    /// (file index or u32::MAX, offset in the file, offset in an expansion + 1, key): the order.
    order: (u32, u32, u32, u64),
    target: Target<'n>,
    path: String,
    at: String,
}

/// The class's first word up to the first field (`Variant(..)` -> `Variant`).
fn cls(debug: &str) -> &str {
    let end = debug.find(['(', ' ', '{']).unwrap_or(debug.len());
    &debug[..end]
}

/// A `Debug` text's nested variants (`Malformed(BodyTypeInRecord(TraitDef(..)))` ->
/// `Malformed/BodyTypeInRecord/TraitDef`), up to a key or a field, then the first quoted string
/// right inside (`Unsupported("a const generic parameter", ..)`): the form a refusal names.
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

/// `Why`'s variant, and the form or thing it names.
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
        BodyError::PatternName { error, .. } => format!("lower PatternName: {}", cls(&format!("{error:?}"))),
        BodyError::NoHead(_) => "lower NoHead".into(),
        BodyError::NoBody(_) => "lower NoBody".into(),
        BodyError::Const { error, .. } => format!("lower Const: {}", cls(&format!("{error:?}"))),
    }
}

fn short(s: &str, n: usize) -> String {
    let one: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    match one.char_indices().nth(n) {
        Some((i, _)) => format!("{}..", &one[..i]),
        None => one,
    }
}

/// The trait an impl's text names, its last segment without arguments (`impl<T> ops::Deref<X>
/// for Box<T>` -> `Deref`); `(inherent)` without ` for `.
fn impl_trait(text: &str) -> String {
    let Some(at) = text.find("impl") else { return "(no impl text)".into() };
    let rest = &text[at + 4..];
    let (mut depth, mut i) = (0i32, 0usize);
    let b = rest.as_bytes();
    // Skip the impl's own generics.
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

/// A refused header's reason as a class: its `Why` (after the item's key), the named form cut.
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
        Gap::UnreadImpl(_, s) => format!("gap UnreadImpl: {}", short(s, 50)),
        Gap::RefusedImpl { impl_, why, .. } => {
            let tr = names.item_text(*impl_).map_or("(no text)".to_string(), |t| impl_trait(t.text));
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

/// The trait of an unmet bound's text (`T: Trait<A>` -> `Trait`).
fn bound_trait(about: &str) -> String {
    let after = about.rsplit_once(": ").map_or(about, |(_, t)| t);
    let end = after.find(['<', ' ', '`', ',']).unwrap_or(after.len());
    after[..end].to_string()
}

/// The source text at an item-relative range of `text`, on one line, cut to 60 characters.
fn source_at(text: &str, start: u32, end: u32) -> String {
    let at = text.get(start as usize..end as usize).unwrap_or("(past the item's text)");
    short(&at.split_whitespace().collect::<Vec<_>>().join(" "), 60)
}

/// A failure's class and its line; `text` is the body's item text, whose ranges a diagnostic
/// gives.
fn fail_class(e: &CheckFail, text: &str, names: &Names<'_>) -> (String, String) {
    match e {
        CheckFail::Gap { gaps, .. } => {
            let first = gaps.first().map_or("gap (empty list)".to_string(), |g| gap_class(g, names));
            (first, format!("{} gaps; the first: {}", gaps.len(), gaps.first().map(|g| short(&format!("{g}"), 300)).unwrap_or_default()))
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
            let all: Vec<&str> = diags.iter().map(|d| d.0.code()).collect();
            let at = |d: &(_, blaze_typeck::Range, _)| format!("`{}` ({}..{})", source_at(text, d.1.start, d.1.end), d.1.start, d.1.end);
            let rest: Vec<String> = diags.iter().skip(1).map(|d| format!("{} at {}", d.0.code(), at(d))).collect();
            (class, format!("{} diagnostics {all:?}; the first at {}: {}; then {rest:?}", diags.len(), at(&diags[0]), short(about, 300)))
        }
        CheckFail::Check { error, .. } => (format!("internal Check: {}", nest(&format!("{error:?}"))), short(&format!("{e}"), 300)),
        CheckFail::Resolve { error, .. } => (format!("check Resolve: {}", nest(&format!("{error:?}"))), short(&format!("{e}"), 300)),
        CheckFail::Head { error, .. } => (format!("check Head: {}", head_class(error)), short(&format!("{e}"), 300)),
        other => (format!("check {}", cls(&format!("{other:?}"))), short(&format!("{e}"), 300)),
    }
}

/// Who a class belongs to, as far as its name tells.
fn owner(class: &str) -> &'static str {
    if class.contains("NotFound") || class.contains("Unresolved") || class.starts_with("lower Traits") || class.starts_with("member not filed") {
        "names library (a name or member not found), else front2"
    } else if class.starts_with("internal") {
        "type checker (internal error)"
    } else if class.starts_with("diag") {
        "type checker, or the front2 records it reads (bstd compiles: ours)"
    } else if class.starts_with("check Resolve") || class.starts_with("gap Resolve") {
        "type checker's instance resolution over front2's impl records"
    } else if class.starts_with("panic") {
        "the crate the panic location names"
    } else {
        "front2"
    }
}

fn panic_text(p: &(dyn std::any::Any + Send)) -> String {
    p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_else(|| "(no message)".into())
}

/// Lowers (a length naming a constant evaluated and the body lowered again, as reach.rs does),
/// spells and checks one body; `Ok(None)` when it has no body.
fn take<'n>(r: &mut Records<'_, 'n>, prepared: &Prepares<'n>, t: Target<'_>) -> Result<Option<()>, (String, String)> {
    let mut lens = 0;
    let lowered = loop {
        let lowered = match t {
            Target::Free(lib) => lower_fn(r.names, prepared, r.keys, r.store, lib),
            Target::Member { container, name, .. } => match r.keys.filed_member(checker_of(container), MemberKind::Fn, name) {
                Some(k) => {
                    if let Some(why) = r.store.fn_gaps.get(&k) {
                        return Err((format!("member head refused: {}", why_class(why)), short(&format!("{why:?}"), 300)));
                    }
                    lower_member(r.names, prepared, r.keys, r.store, k)
                }
                None => return Err(("member not filed (no key after the container's head)".into(), format!("member `{name}`"))),
            },
        };
        let c = match &lowered {
            Err(BodyError::Convert { why: Some(Why::NeedConst(c, _)), .. }) | Err(BodyError::Head { error: HeadError::Item(_, Why::NeedConst(c, _)), .. }) if lens < MOST_LENS => *c,
            _ => break lowered,
        };
        lens += 1;
        if let Err(e) = eval_len(r, c) {
            return Err((format!("lower Const: {}", cls(&format!("{e:?}"))), short(&format!("{e:?}"), 300)));
        }
    };
    let body = match lowered {
        Ok(b) => b,
        Err(BodyError::NoBody(_)) => return Ok(None),
        Err(e) => return Err((body_class(&e), short(&format!("{e}"), 300))),
    };
    let view = body.lowered.view();
    let spell = Spelling::of(&view).map_err(|e| ("spelling".to_string(), short(&format!("{e:?}"), 300)))?;
    let scope = body.scope(&spell);
    match check_body(r, &body, &scope) {
        Ok(done) if done.diags.is_empty() => Ok(Some(())),
        Ok(done) => Err(("internal Ok with diagnostics".into(), format!("{} diagnostics", done.diags.len()))),
        // The text the body was lowered from (a member's own head text; its ranges are
        // relative to it), else the free item's.
        Err(e) => {
            let text = r.store.fns.get(&body.owner).map(|f| f.text).or_else(|| r.names.item_text(body.item).map(|t| t.text)).unwrap_or("");
            Err(fail_class(&e, text, r.names))
        }
    }
}

/// Makes a container's head (an impl's, or a trait's head and members), evaluating a length
/// naming a constant as needed.
fn container_head(r: &mut Records<'_, '_>, lib: blaze_id::ItemKey, is_trait: bool) -> Result<(), (String, String)> {
    let wants: &[Want] = if is_trait { &[Want::Head(lib), Want::Members(lib)] } else { &[Want::Impl(lib)] };
    for &w in wants {
        let mut lens = 0;
        loop {
            match r.store.ensure(r.names, r.keys, w) {
                Ok(()) => break,
                Err(HeadError::Item(_, Why::NeedConst(c, _))) if lens < MOST_LENS => {
                    lens += 1;
                    if let Err(e) = eval_len(r, c) {
                        return Err((format!("container head: const {}", cls(&format!("{e:?}"))), short(&format!("{e:?}"), 300)));
                    }
                }
                Err(e) => return Err((format!("container head: {}", head_class(&e)), short(&format!("{e}"), 300))),
            }
        }
    }
    Ok(())
}

#[test]
fn census_with_the_world() {
    let start = Instant::now();
    let want = std::env::var("FRONT2_CENSUS_CRATE").ok();
    let cap: usize = std::env::var("FRONT2_CENSUS_CAP").ok().and_then(|s| s.parse().ok()).unwrap_or(100_000);
    let secs: u64 = std::env::var("FRONT2_CENSUS_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(600);
    let out_path = std::env::var("FRONT2_CENSUS_OUT").ok();
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/hello-world/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
    let user = UserCrate { name: "hello_world", dir: "/hello-world/", root: "/hello-world/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names: Names<'_> = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let crate_names: Vec<&str> = crates.iter().map(|c| c.name).collect();
    let mut out = format!("census with the world: names resolved in {:?}; crates {crate_names:?}\n", start.elapsed());
    let sources = [SourceFile { path: "src/main.rs", text: MAIN }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
        let mut front = Front2::new(&names, &frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    // A comma-separated list of crates, each its own census and table, in the order given.
    let list = want.unwrap_or_else(|| if crate_names.contains(&"bstd") { "bstd".to_string() } else { "blaze_std".to_string() });
    for w in list.split(',') {
        let Some(krate) = crate_names.iter().position(|n| *n == w) else {
            out.push_str(&format!("no crate named {w:?}\n"));
            write_out(&out_path, &out);
            panic!("{out}");
        };
        census_crate(&names, &mut front, krate, w, cap, secs, out_path.clone(), &mut out);
    }
    out.push_str(&format!("census: {:?} in all\n", start.elapsed()));
    print!("{out}");
    write_out(&out_path, &out);
}

/// Every body of crate `krate`, in the stated order, at most `cap`, the table appended to `out`.
#[expect(clippy::too_many_arguments, reason = "the run's settings")]
fn census_crate<'n>(names: &'n Names<'n>, front: &mut Front2<'n>, krate: usize, cname: &str, cap: usize, secs: u64, out_path: Option<String>, out: &mut String) {
    let start = Instant::now();
    let resolver = front.resolver;
    let cn = names.krate(krate);

    // The bodies, in the stated order.
    let pos = |key: blaze_id::ItemKey, text: &str| -> (u32, u32, u32, String) {
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
    };
    let mut items: Vec<(blaze_id::ItemKey, ItemDetail)> = names.items(krate).filter_map(|(k, _, _)| resolver.detail(k).map(|d| (k, d))).collect();
    items.sort_by_key(|(k, _)| format!("{k:x}"));
    let mut member_keys: BTreeSet<String> = BTreeSet::new();
    for (k, d) in &items {
        if matches!(d, ItemDetail::Impl | ItemDetail::Trait) {
            for m in names.members(*k).unwrap_or(&[]) {
                member_keys.insert(format!("{:x}", m.key));
            }
        }
    }
    let mut entries: Vec<Entry<'_>> = Vec::new();
    let mut declarations = 0usize;
    for (k, d) in &items {
        let Some(it) = names.item_text(*k) else { continue };
        match d {
            ItemDetail::Fn { .. } if !member_keys.contains(&format!("{k:x}")) => {
                if it.text.trim_end().ends_with(';') {
                    declarations += 1;
                    continue;
                }
                let (f, o, s, at) = pos(*k, it.text);
                let module = cn.path_of(it.module).unwrap_or("?");
                entries.push(Entry { order: (f, o, s, 0), target: Target::Free(*k), path: format!("{module}::{}", it.name.unwrap_or("?")), at });
            }
            ItemDetail::Impl | ItemDetail::Trait => {
                let head = it.impl_header.map(|h| short(h, 80)).unwrap_or_else(|| format!("trait {}", it.name.unwrap_or("?")));
                for m in names.members(*k).unwrap_or(&[]) {
                    let (ItemDetail::Fn { .. }, Some(name)) = (m.detail, m.name) else { continue };
                    let (f, o, s, at) = pos(*k, m.text);
                    let module = cn.path_of(it.module).unwrap_or("?");
                    entries.push(Entry { order: (f, o, s, 0), target: Target::Member { container: *k, member: m.key, name }, path: format!("{module}::<{head}>::{name}"), at });
                }
            }
            _ => {}
        }
    }
    for e in entries.iter_mut() {
        let k = match e.target {
            Target::Free(k) => k,
            Target::Member { member, .. } => member,
        };
        e.order.3 = u64::from_str_radix(&format!("{k:x}").chars().rev().take(16).collect::<String>().chars().rev().collect::<String>(), 16).unwrap_or(0);
    }
    entries.sort_by(|a, b| a.order.cmp(&b.order).then_with(|| a.path.cmp(&b.path)));
    let listed = entries.len();
    out.push_str(&format!(
        "crate {cname}: {listed} fns with a body listed ({} free, {} members; {declarations} bodiless free declarations skipped); order: file index, offset, expansion offset, key; cap {cap}\n",
        entries.iter().filter(|e| matches!(e.target, Target::Free(_))).count(),
        entries.iter().filter(|e| matches!(e.target, Target::Member { .. })).count(),
    ));

    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|info| {
        if let Ok(mut at) = PANIC_AT.lock() {
            *at = info.location().map_or_else(|| "?".to_string(), |l| format!("{}:{}", l.file(), l.line()));
        }
    }));

    // `fresh` (the default): each body from empty records (store, impl buckets, constants), so
    // its outcome is a function of the program alone and no body's record poisons another's;
    // `shared` (the default): one set of records for the whole census, as reach.rs keeps them.
    // No body's type reaches a record (a body is lowered into its own copy of the records'
    // table: body.rs, test `shared_records_hold_no_body_type`); a body that still meets one
    // (`Malformed/BodyTypeInRecord`) is counted in RESETS, which must read 0, and named with
    // the body checked just before it.
    let mode = std::env::var("FRONT2_CENSUS_RECORDS").unwrap_or_else(|_| "shared".to_string());
    let fresh = mode == "fresh";
    out.push_str(&format!("records: {}\n", if fresh { "fresh per body" } else { "shared by every body" }));
    let mut resets = 0usize;
    let mut suspects: BTreeMap<String, usize> = BTreeMap::new();
    let mut ex_idx: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let sized = front.index.lang("sized");
    let mut shared_store = Store::new(sized, front.frame, blaze_own::Alloc::global());
    let mut shared_buckets = None;
    let mut shared_consts = Consts::default();
    let mut heads: BTreeMap<String, Result<(), (String, String)>> = BTreeMap::new();
    // class -> (count, examples)
    let mut classes: BTreeMap<String, (usize, Vec<String>)> = BTreeMap::new();
    let (mut clean, mut bodies, mut no_body) = (0usize, 0usize, 0usize);
    let mut stopped = None;
    for (i, e) in entries.iter().enumerate() {
        if i == cap {
            stopped = Some(format!("capped at {cap} bodies of {listed}"));
            break;
        }
        if start.elapsed().as_secs() > secs {
            stopped = Some(format!("STOPPED BY TIME ({secs}s) at {i} of {listed}: not comparable"));
            break;
        }
        let got = if fresh {
            heads.clear();
            let (mut fs, mut fb, mut fc) = (Store::new(sized, front.frame, blaze_own::Alloc::global()), None, Consts::default());
            one(names, front, &mut fs, &mut fb, &mut fc, prepared, &mut heads, e.target)
        } else {
            one(names, front, &mut shared_store, &mut shared_buckets, &mut shared_consts, prepared, &mut heads, e.target)
        };
        if matches!(&got, Err((c, _)) if c.starts_with("internal Check: Malformed/BodyTypeInRecord")) {
            resets += 1;
            let prev = if i == 0 { "(none)".to_string() } else { format!("{} at {}", entries[i - 1].path, entries[i - 1].at) };
            *suspects.entry(prev).or_insert(0) += 1;
        }
        match got {
            Ok(None) => no_body += 1,
            Ok(Some(())) => {
                bodies += 1;
                clean += 1;
            }
            Err((class, detail)) => {
                bodies += 1;
                let slot = classes.entry(class.clone()).or_insert((0, Vec::new()));
                slot.0 += 1;
                if slot.1.len() < EXAMPLES {
                    slot.1.push(format!("{} at {}: {detail}", e.path, e.at));
                    ex_idx.entry(class).or_default().push(i);
                }
            }
        }
        if (i + 1) % 250 == 0 {
            let mut partial = out.clone();
            partial.push_str(&format!("PARTIAL at {} of {listed} ({:?}): bodies {bodies}, clean {clean}\n", i + 1, start.elapsed()));
            write_out(&out_path, &partial);
        }
    }
    // Each class's examples checked again from empty records: a class whose examples change
    // there is an artifact of the shared records (its count is not to be trusted).
    let mut confirm: BTreeMap<String, String> = BTreeMap::new();
    if !fresh {
        for (class, idx) in &ex_idx {
            let mut same = 0;
            let mut other = Vec::new();
            for &i in idx {
                heads.clear();
                let (mut fs, mut fb, mut fc) = (Store::new(sized, front.frame, blaze_own::Alloc::global()), None, Consts::default());
                match one(names, front, &mut fs, &mut fb, &mut fc, prepared, &mut heads, entries[i].target) {
                    Err((c, _)) if &c == class => same += 1,
                    Err((c, _)) => other.push(c),
                    Ok(Some(())) => other.push("clean".to_string()),
                    Ok(None) => other.push("no body".to_string()),
                }
            }
            confirm.insert(class.clone(), format!("from empty records: {same} of {} examples the same{}", idx.len(), if other.is_empty() { String::new() } else { format!("; else {other:?}") }));
        }
    }
    std::panic::set_hook(prev_hook);
    {
        let mut s: Vec<(&String, &usize)> = suspects.iter().collect();
        s.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
        out.push_str(&format!("RESETS: {resets} bodies met a record holding a body's type (no reset: must be 0); the bodies checked just before, most first: {:?}\n", s.iter().take(5).collect::<Vec<_>>()));
    }

    let pct = if bodies == 0 { 0.0 } else { 100.0 * clean as f64 / bodies as f64 };
    out.push_str(&format!(
        "HEAD crate {cname}: bodies {bodies}, clean {clean} ({pct:.1}%); {no_body} listed members had no body (required trait methods); {}\n",
        stopped.as_deref().unwrap_or("all listed bodies taken")
    ));
    // Bodies whose lowering was kept for the command because they hold local items (the
    // shared store's; a fresh store's per-body reruns keep their own).
    out.push_str(&format!("LOCALS: bodies that kept their local items (shared store): {}\n", shared_store.locals.kept()));
    let mut sorted: Vec<(&String, &(usize, Vec<String>))> = classes.iter().collect();
    sorted.sort_by(|a, b| b.1.0.cmp(&a.1.0).then_with(|| a.0.cmp(b.0)));
    out.push_str(&format!("CLASSES ({}), largest first:\n", sorted.len()));
    for (class, (n, ex)) in &sorted {
        out.push_str(&format!("{n:6} {class}  [owner: {}]\n", owner(class)));
        if let Some(c) = confirm.get(*class) {
            out.push_str(&format!("         {c}\n"));
        }
        for x in ex {
            out.push_str(&format!("         e.g. {x}\n"));
        }
    }
    // The refused impls behind "gap RefusedImpl" bodies, by the refusal's reason and by trait:
    // what each heads lane has to build.
    {
        let (mut by_reason, mut by_trait, mut by_both): (BTreeMap<&str, usize>, BTreeMap<&str, usize>, BTreeMap<(&str, &str), usize>) = Default::default();
        for (class, (n, _)) in &classes {
            let Some(rest) = class.strip_prefix("gap RefusedImpl [") else { continue };
            let Some((tr, reason)) = rest.split_once("]: ") else { continue };
            *by_reason.entry(reason).or_insert(0) += n;
            *by_trait.entry(tr).or_insert(0) += n;
            *by_both.entry((reason, tr)).or_insert(0) += n;
        }
        let largest = |m: Vec<(String, usize)>| {
            let mut m = m;
            m.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            m
        };
        out.push_str("REFUSED IMPLS (bodies stopped by a refused candidate impl), by reason:\n");
        for (r, n) in largest(by_reason.iter().map(|(r, n)| (r.to_string(), *n)).collect()) {
            out.push_str(&format!("{n:6} {r}\n"));
        }
        out.push_str("by trait:\n");
        for (t, n) in largest(by_trait.iter().map(|(t, n)| (t.to_string(), *n)).collect()) {
            out.push_str(&format!("{n:6} {t}\n"));
        }
        out.push_str("by reason and trait:\n");
        for (rt, n) in largest(by_both.iter().map(|((r, t), n)| (format!("{t} <- {r}"), *n)).collect()) {
            out.push_str(&format!("{n:6} {rt}\n"));
        }
    }
    out.push_str(&format!("crate {cname}: {:?}\n", start.elapsed()));
    write_out(&out_path, out);
}

/// One body: its container's head (once per container while the records live), then lowered
/// and checked; a panic caught and filed by where it was raised.
#[expect(clippy::too_many_arguments, reason = "the records' parts")]
fn one<'n>(
    names: &'n Names<'n>,
    front: &mut Front2<'n>,
    store: &mut Store<'n>,
    buckets: &mut Option<Buckets>,
    consts: &mut Consts<'n>,
    prepared: &Prepares<'n>,
    heads: &mut BTreeMap<String, Result<(), (String, String)>>,
    t: Target<'_>,
) -> Result<Option<()>, (String, String)> {
    let resolver = front.resolver;
    let mut r = Records { names, keys: &mut front.keys, index: &front.index, store, buckets, order: BuildOrder::AsAsked, consts: Some((consts, prepared)) };
    let got = catch_unwind(AssertUnwindSafe(|| {
        if let Target::Member { container, .. } = t {
            let key = format!("{container:x}");
            if !heads.contains_key(&key) {
                let is_trait = matches!(resolver.detail(container), Some(ItemDetail::Trait));
                let h = container_head(&mut r, container, is_trait);
                heads.insert(key.clone(), h);
            }
            if let Some(Err((c, d))) = heads.get(&key) {
                return Err((c.clone(), d.clone()));
            }
        }
        take(&mut r, prepared, t)
    }));
    got.unwrap_or_else(|p| {
        let at = PANIC_AT.lock().map(|s| s.clone()).unwrap_or_default();
        let file = at.rsplit_once(':').map_or(at.as_str(), |x| x.0).to_string();
        Err((format!("panic at {file}"), format!("{at}: {}", short(&panic_text(&*p), 300))))
    })
}

fn write_out(path: &Option<String>, text: &str) {
    if let Some(p) = path {
        std::fs::write(p, text).unwrap_or_else(|e| panic!("writing the census to {p}: {e}"));
    }
}
