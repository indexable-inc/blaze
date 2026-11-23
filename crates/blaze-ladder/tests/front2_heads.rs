//! The census of HEADS: every item of ONE real crate of the toolchain that has a head (structs,
//! enums, unions, traits, trait aliases, impls, free fns, constants, statics, type aliases, and
//! each trait's and impl's member fns, associated types and constants), its head made through
//! the entry the checker's world reads (`heads::Store::ensure` over `check::Records`, an array
//! length naming a constant evaluated by `check::eval_len` and the head made again, as
//! tests/front2_census.rs and reach.rs do), then filed under exactly one class: "built", or its
//! refusal by the typed reason's name and text.
//!
//! What counts as built: a struct, enum or union: `Want::Adt`; a trait: `Want::Head` then
//! `Want::Members`; an impl: `Want::Impl`; a free fn or constant: `Want::Fn`; a type alias:
//! `Want::Head` with no row in the store's `alias_gaps`; a member fn: its container built and no
//! row in `fn_gaps`; a member constant: its container built and its key filed; an impl's
//! associated type: its container built (it is part of `ImplHead`); a trait's associated type:
//! its container's members built. A member the container skipped for its cfg is counted apart,
//! not as a head. A member of a container whose head is refused is filed under "its container's
//! head is refused" (the container carries the reason). A panic inside one item is caught and is
//! its own class, by where it was raised.
//!
//! Order: by crate, then file index, offset in the file, offset in a macro expansion, then key;
//! members after their container in their written order. Every table is sorted by count, then
//! by name: the same table on every run. Each class names the FILE of front2 that raises it
//! (found by the refusal's literal text, or `Why::Variant(`, in front2's sources) and three
//! examples: item path, file and line, and the refused source text cut to 80 characters read
//! from the text of the item the refusal names (the range is relative to it, rule 71).
//!
//! Environment: `FRONT2_HEADS_CRATE` (default `core`; a comma-separated list is taken in order),
//! `FRONT2_HEADS_OUT` (a file the report is also written to). It needs the installed toolchain
//! under `$HOME/.blaze/v2/toolchain`, as tests/front2_main.rs does.

use std::collections::{BTreeMap, BTreeSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Mutex;
use std::time::Instant;

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::check::{BuildOrder, Records, eval_len};
use blaze_ladder::front2::consts::Consts;
use blaze_ladder::front2::heads::{HeadError, Store, Want};
use blaze_ladder::front2::keys::{MemberKind, checker_of};
use blaze_ladder::front2::types::Why;
use blaze_ladder::refusal::SourceFile;
use blaze_lower::scoped::Prepares;
use blaze_scope::resolve::{At, Names};
use blaze_surface::{ItemDetail, Range};

const EDITION: &str = "blaze-2026.11";
const MAIN: &str = "fn main(cx: &Cx, io: Io) -> Result<(), Error> {
    Ok(())
}
";
const MOST_LENS: u32 = 64;
const EXAMPLES: usize = 3;
const CONTAINER_REFUSED: &str = "its container's head is refused";

/// front2's sources, to name the file that raises a refusal.
const SOURCES: &[(&str, &str)] = &[
    ("body.rs", include_str!("../src/front2/body.rs")),
    ("buckets.rs", include_str!("../src/front2/buckets.rs")),
    ("check.rs", include_str!("../src/front2/check.rs")),
    ("consts.rs", include_str!("../src/front2/consts.rs")),
    ("dyn_ty.rs", include_str!("../src/front2/dyn_ty.rs")),
    ("fill.rs", include_str!("../src/front2/fill.rs")),
    ("heads.rs", include_str!("../src/front2/heads.rs")),
    ("index.rs", include_str!("../src/front2/index.rs")),
    ("keys.rs", include_str!("../src/front2/keys.rs")),
    ("langs.rs", include_str!("../src/front2/langs.rs")),
    ("mod.rs", include_str!("../src/front2/mod.rs")),
    ("pattern_ty.rs", include_str!("../src/front2/pattern_ty.rs")),
    ("preds.rs", include_str!("../src/front2/preds.rs")),
    ("program.rs", include_str!("../src/front2/program.rs")),
    ("reach.rs", include_str!("../src/front2/reach.rs")),
    ("types.rs", include_str!("../src/front2/types.rs")),
    ("world_items.rs", include_str!("../src/front2/world_items.rs")),
    ("world_paths.rs", include_str!("../src/front2/world_paths.rs")),
    ("front.rs", include_str!("../src/front.rs")),
];

static PANIC_AT: Mutex<String> = Mutex::new(String::new());

/// A refusal: its class, the front2 file(s) raising it, the item whose text its range is
/// relative to, and the range.
struct Refusal {
    class: String,
    file: String,
    base: Option<blaze_id::ItemKey>,
    range: Option<Range>,
    /// For a member's refusal: its container (the range is relative to the member's text).
    owner: Option<blaze_id::ItemKey>,
}

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

/// The files of front2 whose source holds `needle`, joined by `+`.
fn files_with(needle: &str) -> String {
    let hits: Vec<&str> = SOURCES.iter().filter(|(_, s)| s.contains(needle)).map(|(n, _)| *n).collect();
    if hits.is_empty() { "(not found by its text)".to_string() } else { hits.join("+") }
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

/// The class of a `Why`: its variant and, for a stated form, the form (`Unsupported "a negative
/// impl"`); the text of a `Program` or `PatternType` cut to 60 characters.
fn why_refusal(base: blaze_id::ItemKey, why: &Why) -> Refusal {
    let debug = format!("{why:?}");
    let variant = cls(&debug).to_string();
    let (class, file) = match why {
        Why::Unsupported(s, _) => (format!("Unsupported \"{s}\""), files_with(&format!("\"{s}\""))),
        // A formatted text: found by its longest part outside backticks (the names put in).
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

fn head_refusal(e: &HeadError) -> Refusal {
    match e {
        HeadError::Item(k, why) => why_refusal(*k, why),
        other => {
            let d = format!("{other:?}");
            let detail = match other {
                HeadError::NotAHead(_, what) => format!(" {what}"),
                _ => String::new(),
            };
            Refusal { class: format!("HeadError::{}{detail}", cls(&d)), file: "heads.rs".to_string(), base: None, range: None, owner: None }
        }
    }
}

/// Makes `w`, evaluating an array length naming a constant as needed.
fn make(r: &mut Records<'_, '_>, w: Want) -> Result<(), Refusal> {
    let mut lens = 0;
    loop {
        match r.store.ensure(r.names, r.keys, w) {
            Ok(()) => return Ok(()),
            Err(HeadError::Item(_, Why::NeedConst(c, _))) if lens < MOST_LENS => {
                lens += 1;
                if let Err(e) = eval_len(r, c) {
                    return Err(Refusal { class: format!("const length: {}", cls(&format!("{e:?}"))), file: "check.rs".to_string(), base: None, range: None, owner: None });
                }
            }
            Err(e) => return Err(head_refusal(&e)),
        }
    }
}

/// The trait path of an impl header as written (`impl<T> const Clone for &T` -> `Clone`), or
/// `None` for an inherent impl.
fn header_trait(header: &str) -> Option<String> {
    let at = header.find("impl")?;
    let mut s = header[at + 4..].trim_start();
    if s.starts_with('<') {
        let mut depth = 0i32;
        let mut end = s.len();
        let b = s.as_bytes();
        for (i, c) in s.char_indices() {
            match c {
                '<' => depth += 1,
                '>' if i > 0 && b[i - 1] == b'-' => {}
                '>' => {
                    depth -= 1;
                    if depth == 0 {
                        end = i + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        s = s[end..].trim_start();
    }
    s = s.strip_prefix("const ").unwrap_or(s).trim_start();
    let neg = s.starts_with('!');
    s = s.trim_start_matches('!');
    let b = s.as_bytes();
    let mut depth = 0i32;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'<' | b'(' | b'[' => depth += 1,
            b'>' if i > 0 && b[i - 1] == b'-' => {}
            b'>' | b')' | b']' => depth -= 1,
            b' ' if depth == 0 && s[i..].starts_with(" for ") => {
                let t = &s[..i];
                let end = t.find(['<', '(']).unwrap_or(t.len());
                return Some(format!("{}{}", if neg { "!" } else { "" }, t[..end].trim()));
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// One head to file.
struct Row {
    kind: &'static str,
    path: String,
    at: String,
    got: Result<(), Refusal>,
    /// For an impl: its trait as written.
    trait_: Option<String>,
}

#[test]
fn head_census() {
    let start = Instant::now();
    let want = std::env::var("FRONT2_HEADS_CRATE").unwrap_or_else(|_| "core".to_string());
    let out_path = std::env::var("FRONT2_HEADS_OUT").ok();
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
    let mut out = format!("head census: names resolved in {:?}\n", start.elapsed());
    let sources = [SourceFile { path: "src/main.rs", text: MAIN }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
        let mut front = Front2::new(&names, &frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|info| {
        if let Ok(mut at) = PANIC_AT.lock() {
            *at = info.location().map_or_else(|| "?".to_string(), |l| format!("{}:{}", l.file(), l.line()));
        }
    }));
    for w in want.split(',') {
        let Some(krate) = crate_names.iter().position(|n| *n == w) else {
            std::panic::set_hook(prev_hook);
            panic!("no crate named {w:?} among {crate_names:?}");
        };
        census_crate(&names, &mut front, krate, w, &mut out);
    }
    std::panic::set_hook(prev_hook);
    out.push_str(&format!("head census: {:?} in all\n", start.elapsed()));
    print!("{out}");
    if let Some(p) = &out_path {
        std::fs::write(p, &out).unwrap_or_else(|e| panic!("writing {p}: {e}"));
    }
}

fn census_crate<'n>(names: &'n Names<'n>, front: &mut Front2<'n>, krate: usize, cname: &str, out: &mut String) {
    let start = Instant::now();
    let resolver = front.resolver;
    let cn = names.krate(krate);
    // (file index, offset, expansion offset + 1, shown place), as tests/front2_census.rs.
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
            let (f, o, s, at) = pos(*k, it.text);
            Some(((f, o, s, *k), at, *k, *d))
        })
        .collect();
    ordered.sort_by(|a, b| a.0.cmp(&b.0));

    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut store = Store::new(front.index.lang("sized"), front.frame, blaze_own::Alloc::global());
    let mut buckets = None;
    let mut consts = Consts::default();
    let mut rows: Vec<Row> = Vec::new();
    let mut cfg_skipped = 0usize;
    for (_, at, k, d) in &ordered {
        let Some(it) = names.item_text(*k) else { continue };
        let module = cn.path_of(it.module).unwrap_or("?");
        let path = match d {
            ItemDetail::Impl => format!("{module}::<{}>", short(it.impl_header.unwrap_or("impl"), 60)),
            _ => format!("{module}::{}", it.name.unwrap_or("?")),
        };
        let mut r = Records { names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: Some((&mut consts, &prepared)) };
        let got = catch_unwind(AssertUnwindSafe(|| -> Vec<Row> {
            let mut rs = Vec::new();
            let ck = checker_of(*k);
            let (kind, got, trait_): (&'static str, Result<(), Refusal>, Option<String>) = match d {
                ItemDetail::Struct => ("struct", make(&mut r, Want::Adt(*k)), None),
                ItemDetail::Enum => ("enum", make(&mut r, Want::Adt(*k)), None),
                ItemDetail::Union => ("union", make(&mut r, Want::Adt(*k)), None),
                ItemDetail::Trait => ("trait", make(&mut r, Want::Head(*k)).and_then(|()| make(&mut r, Want::Members(*k))), None),
                ItemDetail::TraitAlias => ("trait alias", make(&mut r, Want::Head(*k)), None),
                ItemDetail::Impl => {
                    let t = it.impl_header.and_then(header_trait);
                    (if t.is_some() { "impl (trait)" } else { "impl (inherent)" }, make(&mut r, Want::Impl(*k)), t)
                }
                ItemDetail::Fn { .. } => ("fn", make(&mut r, Want::Fn(*k)), None),
                ItemDetail::Const { .. } => ("const", make(&mut r, Want::Fn(*k)), None),
                ItemDetail::Static { .. } => ("static", make(&mut r, Want::Fn(*k)), None),
                ItemDetail::TypeAlias => {
                    let got = make(&mut r, Want::Head(*k)).and_then(|()| match r.store.maps.alias_gaps.get(k) {
                        // Lowered here, never filed from a record: the builder's `Why` is kept.
                        Some(gap) => Err(why_refusal(*k, gap.why.as_ref().expect("a refusal lowered here keeps its Why"))),
                        None => Ok(()),
                    });
                    ("type alias", got, None)
                }
                _ => return rs,
            };
            let built = got.is_ok();
            rs.push(Row { kind, path: path.clone(), at: at.clone(), got, trait_ });
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
                let (_, _, _, mat) = pos(*k, m.text);
                let mpath = format!("{path}::{name}");
                let got = if !built {
                    Err(Refusal { class: CONTAINER_REFUSED.to_string(), file: "-".to_string(), base: None, range: None, owner: None })
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
                            // Its range is relative to the member's own text.
                            (Ok(()), Some(why)) => {
                                let mut x = why_refusal(m.key, why);
                                x.owner = Some(*k);
                                Err(x)
                            }
                            (Ok(()), None) if r.store.fns.contains_key(&fk) => Ok(()),
                            (Ok(()), None) => Err(Refusal { class: "member fn filed but no head in the store".to_string(), file: "heads.rs".to_string(), base: None, range: None, owner: None }),
                        },
                        (MemberKind::Const, Some(_)) => Ok(()),
                        (MemberKind::Const, None) if is_trait => Err(Refusal { class: "trait assoc const: no head made (trait_members files none)".to_string(), file: "heads.rs".to_string(), base: None, range: None, owner: None }),
                        (MemberKind::Type, _) if !is_trait => {
                            let has = r.store.impls.get(&ck).is_some_and(|h| h.assoc.iter().any(|(n, _)| *n == blaze_typeck::Name::of(name)));
                            if has { Ok(()) } else { Err(Refusal { class: "cfg".to_string(), file: String::new(), base: None, range: None, owner: None }) }
                        }
                        (_, Some(_)) => Ok(()),
                        // The container skipped it for its cfg.
                        (_, None) => Err(Refusal { class: "cfg".to_string(), file: String::new(), base: None, range: None, owner: None }),
                    }
                };
                rs.push(Row { kind, path: mpath, at: mat, got, trait_: None });
            }
            rs
        }));
        match got {
            Ok(rs) => {
                for row in rs {
                    if matches!(&row.got, Err(x) if x.class == "cfg") {
                        cfg_skipped += 1;
                    } else {
                        rows.push(row);
                    }
                }
            }
            Err(p) => {
                let at_p = PANIC_AT.lock().map(|s| s.clone()).unwrap_or_default();
                let file = at_p.rsplit_once(':').map_or(at_p.as_str(), |x| x.0).to_string();
                let text = p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_default();
                rows.push(Row { kind: "panicked item", path, at: at.clone(), got: Err(Refusal { class: format!("panic at {file}: {}", short(&text, 60)), file: at_p, base: None, range: None, owner: None }), trait_: None });
            }
        }
    }

    // The tables.
    let all = rows.len();
    let built = rows.iter().filter(|r| r.got.is_ok()).count();
    let pct = |a: usize, b: usize| if b == 0 { 0.0 } else { 100.0 * a as f64 / b as f64 };
    out.push_str(&format!("HEADS crate {cname}: heads {all}, built {built} ({:.1}%); {cfg_skipped} members skipped by their container's cfg (not counted); {:?}\n", pct(built, all), start.elapsed()));
    let mut kinds: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    for r in &rows {
        let e = kinds.entry(r.kind).or_insert((0, 0));
        e.0 += 1;
        if r.got.is_ok() {
            e.1 += 1;
        }
    }
    let mut ks: Vec<_> = kinds.into_iter().collect();
    ks.sort_by(|a, b| b.1.0.cmp(&a.1.0).then_with(|| a.0.cmp(b.0)));
    out.push_str("BY KIND (heads, built, share):\n");
    for (k, (n, b)) in &ks {
        out.push_str(&format!("{n:7} {b:7} {:5.1}%  {k}\n", pct(*b, *n)));
    }
    // class -> (count, file, examples)
    let mut classes: BTreeMap<String, (usize, String, Vec<String>)> = BTreeMap::new();
    for r in &rows {
        let Err(x) = &r.got else { continue };
        let slot = classes.entry(x.class.clone()).or_insert((0, x.file.clone(), Vec::new()));
        slot.0 += 1;
        if slot.2.len() < EXAMPLES {
            let text = match (base_text(names, x), x.range) {
                (Some(t), Some(rg)) if rg.end == 0 => format!("(no range; item: {})", short(t, 60)),
                (Some(t), Some(rg)) => t.get(rg.start as usize..rg.end as usize).map_or_else(|| format!("(range {}..{} past the item's text)", rg.start, rg.end), |s| format!("`{}`", short(s, 80))),
                _ => "(no range)".to_string(),
            };
            slot.2.push(format!("{} [{}] at {}: {text}", r.path, r.kind, r.at));
        }
    }
    let mut cs: Vec<_> = classes.into_iter().collect();
    cs.sort_by(|a, b| b.1.0.cmp(&a.1.0).then_with(|| a.0.cmp(&b.0)));
    out.push_str(&format!("REFUSALS ({} classes), largest first:\n", cs.len()));
    for (c, (n, file, ex)) in &cs {
        out.push_str(&format!("{n:7} {c}  [raised in: {file}]\n"));
        for e in ex {
            out.push_str(&format!("          e.g. {e}\n"));
        }
    }
    let mut traits: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    for r in &rows {
        if let Some(t) = &r.trait_ {
            let e = traits.entry(t.as_str()).or_insert((0, 0));
            e.0 += 1;
            if r.got.is_err() {
                e.1 += 1;
            }
        }
    }
    let mut ts: Vec<_> = traits.into_iter().filter(|(_, (_, refused))| *refused > 0).collect();
    ts.sort_by(|a, b| b.1.1.cmp(&a.1.1).then_with(|| a.0.cmp(b.0)));
    out.push_str(&format!("IMPLS REFUSED BY TRAIT (as written; {} traits with a refused impl), first 20 (refused of impls):\n", ts.len()));
    for (t, (n, refused)) in ts.iter().take(20) {
        out.push_str(&format!("{refused:6} of {n:5}  {t}\n"));
    }
}

/// The text a refusal's range is relative to: a member's own text, else its item's.
fn base_text<'n>(names: &'n Names<'n>, x: &Refusal) -> Option<&'n str> {
    let key = x.base?;
    match x.owner {
        Some(owner) => names.members(owner)?.iter().find(|m| m.key == key).map(|m| m.text),
        None => names.item_text(key).map(|t| t.text),
    }
}
