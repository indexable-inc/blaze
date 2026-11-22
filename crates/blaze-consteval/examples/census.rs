//! The evaluator on real code (rule 82): every constant of the given source trees that the
//! evaluator can reach without naming generics, through the driver's path: `lower_file` (macros
//! expanded by name), `to_typeck`, the checker, then `value_of` in a `Settle` loop.
//!
//! `cargo run --release -p blaze-consteval --example census -- <dir>...`, each `<dir>` a
//! crate (its `src` is walked) or a directory of `.rs` files.
//!
//! Each file is its own world: its non-generic structs, enums, fns, consts and statics,
//! declared by their bare names, with no `use`, no module paths, no prelude and no impl
//! selection (the cross-crate resolver, blaze-scope, is not driven here yet). A constant that
//! needs any of those stops at the check and is counted by its cause, so the table separates
//! what the evaluator refuses from what the world in front of it lacks.
//!
//! Prints: sites by kind, evaluated and refused, the refusals by stage and cause with counts
//! and one location each, and the time.

#![feature(allocator_api)]

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::Instant;

use blaze_consteval::fixture::{Skip, Value, World};
use blaze_expand::{
    DefAt, DefSite, ExpandCx, Export, HostAsk, HostError, HostWhy, IncludeAs, Included, LocalInner, MacroDefinition,
    NestedKind, Place, PlaceOf, Prepared, ProcCall, Produced, TokenStream, find_calls, lex, prepare,
};
use blaze_id::CrateItemKey;
use blaze_ids::ContentId;
use blaze_lower::by_name::ByName;
use blaze_lower::{Cfg, CfgAnswer, Edition, ItemRef, Outcome, Sink, Source, What, head, lower_file, open};
use blaze_own::Alloc;
use blaze_surface::{ItemDetail, Range};
use blaze_typeck::{ItemKey, Mutbl, PNode, UintTy};

/// A 64-bit little-endian Unix host's cfg (as `blaze-lower`'s `lower_std` census answers it).
struct Host;

impl Cfg for Host {
    fn atom(&mut self, name: &str, value: Option<&str>) -> CfgAnswer {
        let yes = match (name, value) {
            ("unix" | "target_thread_local", None) => true,
            ("target_os", Some(os)) => os == "macos",
            ("target_family", Some(f)) => f == "unix",
            ("target_arch", Some(a)) => a == "aarch64",
            ("target_pointer_width", Some(w)) => w == "64",
            ("target_endian", Some(e)) => e == "little",
            ("panic", Some(p)) => p == "unwind",
            ("target_has_atomic" | "target_has_atomic_load_store" | "target_has_atomic_equal_alignment", _) => true,
            _ => false,
        };
        if yes { CfgAnswer::Yes } else { CfgAnswer::No }
    }
}

// The expander's host and the definitions found by name: as blaze-lower's `lower_std` census
// (to become one library call of blaze-lower's `by_name`; asked of the expansion lane).
struct NoHost;

impl ExpandCx for NoHost {
    fn env(&self, _: &str) -> Result<Option<&str>, HostError> {
        Ok(None)
    }
    fn cfg(&self, _: &TokenStream<'_>) -> Result<bool, HostError> {
        Err(HostError { ask: HostAsk::Cfg, why: HostWhy::Refused })
    }
    fn include(&self, _: &str, _: IncludeAs) -> Result<Included<'_>, HostError> {
        Ok(Included::Missing)
    }
    fn place(&self, _: PlaceOf) -> Result<Place<'_>, HostError> {
        Err(HostError { ask: HostAsk::Place, why: HostWhy::Refused })
    }
    fn run_proc_macro<'a>(&self, _: &MacroDefinition<'_>, _: ProcCall<'_>, _: Alloc<'a>) -> Result<Produced<'a>, HostError> {
        Err(HostError { ask: HostAsk::ProcMacro, why: HostWhy::Refused })
    }
}

fn def_at() -> DefAt {
    DefAt {
        site: DefSite { krate: CrateItemKey(ContentId([1; 32])), module: None },
        edition: Edition::E2024,
        export: Export::Exported,
        local_inner: LocalInner::No,
        transparency: None,
    }
}

fn definitions(texts: &[String]) -> &'static [(&'static str, Prepared<'static>)] {
    let mut defs = Vec::new();
    for text in texts {
        let Ok(stream) = lex(text, Edition::E2024, Alloc::global()) else { continue };
        let stream: &'static TokenStream<'static> = Box::leak(Box::new(stream));
        let Ok(found) = find_calls(stream, Alloc::global()) else { continue };
        for call in found.iter() {
            let Some(name_at) = call.name else { continue };
            let inner = call.inner();
            let Ok(body) = stream.copy_in(inner.start as usize..inner.end as usize, Alloc::global()) else { continue };
            let def = match call.kind {
                NestedKind::MacroRules => MacroDefinition::rules(body, def_at()),
                NestedKind::Macro2 => {
                    let params = match call.params {
                        None => None,
                        Some(p) => match stream.copy_in(p.start as usize + 1..p.end as usize - 1, Alloc::global()) {
                            Ok(params) => Some(params),
                            Err(_) => continue,
                        },
                    };
                    MacroDefinition::macro2(params, body, def_at())
                }
                _ => continue,
            };
            let def: &'static MacroDefinition<'static> = Box::leak(Box::new(def));
            if let Ok(prepared) = prepare(def) {
                defs.push((stream.text_at(name_at as usize), prepared));
            }
        }
    }
    Box::leak(defs.into_boxed_slice())
}

/// One record of a file's lowering, kept after the callback.
struct Rec {
    /// The item's outline entry, by the text it is of and its index.
    id: (usize, u32),
    parent: Option<(usize, u32)>,
    from_file: bool,
    /// The item's start in the file (a file item), for the printed location.
    start: u32,
    detail: ItemDetail,
    name: String,
    what: What,
    /// `None`: the lowering met a cfg atom not answered.
    bytes: Option<Vec<u8>>,
    text: String,
    /// The first unhandled form's kind, if any.
    unhandled: Option<String>,
}

#[derive(Default)]
struct Keep {
    recs: Vec<Rec>,
}

impl Sink for Keep {
    fn refused(&mut self, item: &ItemRef<'_>, refusal: blaze_lower::Refusal<'_>) {
        println!("macro call refused: {} on {}: {}", refusal.path, item.item_text().lines().next().unwrap_or(""), refusal.bound);
    }

    fn record(&mut self, item: &ItemRef<'_>, what: What, _key: ContentId, outcome: Outcome<'_, '_>) {
        // The text the record's ranges are of: the item's own, or, for an anonymous constant
        // lowered from an expansion, the item's text with the expansion grown onto it.
        let (bytes, unhandled, text) = match outcome {
            Outcome::Record { bytes, view } => {
                let u = view.unhandled().next().map(|u| format!("{:?} ({:?})", blaze_file_items_kind(u.syntax), u.why));
                (Some(bytes.to_vec()), u, view.item())
            }
            Outcome::Need(_) => (None, None, item.item_text()),
        };
        let base = item.text.as_ptr() as usize;
        self.recs.push(Rec {
            id: (base, item.index.0),
            parent: item.item.parent.map(|p| (base, p.0)),
            from_file: matches!(item.source, Source::File),
            start: item.item.range.start,
            detail: item.item.detail,
            name: item.item.name.map(|r| String::from(item.item.text(item.text, r))).unwrap_or_default(),
            what,
            bytes,
            text: String::from(text),
            unhandled,
        });
    }
}

fn blaze_file_items_kind(syntax: u16) -> blaze_file_items::SyntaxKind {
    blaze_file_items::syntax_kind(syntax)
}

/// A site's outcome: `Ok` evaluated, or the stage and cause.
type Verdict = Result<(), (&'static str, String, &'static str)>;

fn skip(s: &Skip) -> (&'static str, String, &'static str) {
    match s {
        Skip::Open => ("lower", String::from("a record that does not open"), "expansion lane"),
        Skip::Generic => ("world", String::from("a generic item (no instantiation named)"), "census world"),
        Skip::Signature(what) => ("world", format!("signature: {what}"), "census world"),
        Skip::Convert(blaze_lower::typeck::Refused::Form { what, .. }) => ("convert", String::from(*what), "names (blaze-scope)"),
        Skip::Convert(other) => ("convert", format!("{other:?}"), "expansion lane"),
        Skip::Duplicate => ("world", String::from("a name declared twice in the file"), "census world"),
        Skip::Check(e) => ("check", format!("{e}"), "checker"),
        Skip::WorldMissed => ("check", String::from("the world could not answer the checker"), "names (blaze-scope)"),
        Skip::Diag(code, _) => ("check", format!("{code:?}"), "names (blaze-scope)"),
    }
}

fn refusal_name(r: &blaze_consteval::Refusal<'_>) -> String {
    if let blaze_consteval::Refusal::Missing { what, .. } | blaze_consteval::Refusal::Unsupported { what } | blaze_consteval::Refusal::Malformed { what } = r {
        let s = format!("{r:?}");
        let end = s.find([' ', '{', '(']).unwrap_or(s.len());
        return format!("{}: {what}", &s[..end]);
    }
    let s = format!("{r:?}");
    let end = s.find([' ', '{', '(']).unwrap_or(s.len());
    String::from(&s[..end])
}

#[derive(Default)]
struct Row {
    count: u64,
    owner: &'static str,
    example: String,
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let root = if dir.join("src").is_dir() { dir.join("src") } else { dir.to_path_buf() };
    let mut stack = vec![root];
    while let Some(d) = stack.pop() {
        let entries = std::fs::read_dir(&d).unwrap_or_else(|e| panic!("{}: {e}", d.display()));
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|e| e == "rs") {
                out.push(p);
            }
        }
    }
}

fn line_of(text: &str, at: u32) -> usize {
    text.as_bytes()[..at as usize].iter().filter(|&&b| b == b'\n').count() + 1
}

fn main() {
    let dirs: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    if dirs.is_empty() {
        eprintln!("usage: census <crate or source dir>...");
        std::process::exit(2);
    }
    let mut files = Vec::new();
    for d in &dirs {
        walk(d, &mut files);
    }
    files.sort();
    let texts: Vec<String> = files.iter().map(|f| std::fs::read_to_string(f).unwrap_or_else(|e| panic!("{}: {e}", f.display()))).collect();
    let show = std::env::var("CENSUS_SHOW").ok();
    let started = Instant::now();
    let rules = definitions(&texts);
    let mut sites: BTreeMap<&'static str, (u64, u64)> = BTreeMap::new();
    let mut causes: BTreeMap<(&'static str, String), Row> = BTreeMap::new();
    let mut eval_ns = 0u128;
    let mut evals = 0u64;
    for (fi, text) in texts.iter().enumerate() {
        let mut keep = Keep::default();
        let mut by_name = ByName::new(rules, def_at(), NoHost, Alloc::global());
        lower_file(text, Edition::E2024, &mut Host, &mut blaze_lower::NoModule, &mut by_name, &mut keep, Alloc::global());
        let recs = keep.recs;
        let place = |r: &Rec| {
            let tree = dirs.iter().find(|d| files[fi].starts_with(d)).unwrap_or(&files[fi]);
            let rel = Path::new(tree.file_name().unwrap_or_default()).join(files[fi].strip_prefix(tree).unwrap_or(&files[fi]));
            if r.from_file { format!("{}:{}", rel.display(), line_of(text, r.start)) } else { format!("{} (in an expansion)", rel.display()) }
        };
        let details: HashMap<(usize, u32), ItemDetail> = recs.iter().map(|r| (r.id, r.detail)).collect();
        // An item inside an impl or a trait is associated: no impl selection here.
        let associated = |r: &Rec| {
            r.parent.and_then(|p| details.get(&p)).is_some_and(|d| matches!(d, ItemDetail::Impl | ItemDetail::Trait))
        };
        let mut w = World::new();
        let mut declared: HashMap<(usize, u32), Result<ItemKey, Skip>> = HashMap::new();
        let mut discs: HashMap<((usize, u32), Range), ItemKey> = HashMap::new();
        // Types first, then values and fns, which name them.
        for pass in 0..2 {
            for r in recs.iter().filter(|r| r.what == What::Signature) {
                let is_type = matches!(r.detail, ItemDetail::Struct | ItemDetail::Enum);
                if is_type != (pass == 0) || associated(r) {
                    continue;
                }
                let Some(Ok(v)) = r.bytes.as_deref().map(|b| open(b, &r.text)) else { continue };
                let got = match r.detail {
                    ItemDetail::Struct | ItemDetail::Enum => {
                        w.declare_adt(&r.name, r.detail == ItemDetail::Enum, &v).map(|ds| {
                            for (range, k) in ds {
                                discs.insert((r.id, range), k);
                            }
                            ItemKey(0)
                        })
                    }
                    ItemDetail::Const { .. } => w.declare_value(Value::Const, &r.name, &v),
                    ItemDetail::Static { .. } => {
                        let m = if v.head[head::FLAGS] & blaze_lower::machine::sig::MUT != 0 { Mutbl::Mut } else { Mutbl::Not };
                        w.declare_value(Value::Static(m), &r.name, &v)
                    }
                    ItemDetail::Fn { .. } => w.declare_fn(&r.name, &v),
                    _ => continue,
                };
                declared.insert(r.id, got);
            }
        }
        // The sites, each with its key or its verdict already.
        let mut todo: Vec<(&'static str, ItemKey, String)> = Vec::new();
        let mut verdict = |kind: &'static str, v: Verdict, at: String, sites: &mut BTreeMap<&'static str, (u64, u64)>| {
            let e = sites.entry(kind).or_default();
            match v {
                Ok(()) => e.0 += 1,
                Err((stage, cause, owner)) => {
                    e.1 += 1;
                    // `CENSUS_SHOW=<text>`: every site whose cause holds the text, to read them.
                    if show.as_deref().is_some_and(|s| cause.contains(s)) {
                        println!("show: {kind} {at}");
                    }
                    let row = causes.entry((stage, cause)).or_default();
                    row.count += 1;
                    row.owner = owner;
                    if row.example.is_empty() {
                        row.example = at;
                    }
                }
            }
        };
        for r in &recs {
            let kind = match (r.what, r.detail) {
                (What::Body, ItemDetail::Const { .. }) => "const item",
                (What::Body, ItemDetail::Static { .. }) => "static",
                (What::AnonConst(range), ItemDetail::Enum) if discs.contains_key(&(r.id, range)) => "discriminant",
                (What::AnonConst(_), ItemDetail::Enum) => "discriminant",
                (What::AnonConst(_), _) => "anon const (array length, const arg, default)",
                _ => continue,
            };
            // The location, and the smallest snippet to hand an owner: the anonymous constant's
            // own text, or the item's first line.
            let snippet = match r.what {
                What::AnonConst(range) => r.text.get(range.start as usize..range.end as usize).unwrap_or("<range past the item's text>"),
                _ => r.text.lines().next().unwrap_or_default(),
            };
            // A doc comment is `#[doc = "..."]`, an attribute, not an anonymous constant: one
            // recorded as one is the lowering's, and no constant site.
            let doc = matches!(r.what, What::AnonConst(_)) && (snippet.starts_with("///") || snippet.starts_with("//!") || snippet.starts_with("/**"));
            let snippet: String = snippet.chars().take(80).map(|c| if c == '\n' { ' ' } else { c }).collect();
            let at = format!("{}  `{snippet}`", place(r));
            if doc {
                verdict(kind, Err(("lower", String::from("a doc comment recorded as an anonymous constant"), "expansion lane")), at, &mut sites);
                continue;
            }
            if r.bytes.is_none() {
                verdict(kind, Err(("lower", String::from("a cfg atom not answered"), "census cfg")), at, &mut sites);
                continue;
            }
            if let Some(u) = &r.unhandled {
                verdict(kind, Err(("lower", format!("unhandled form {u}"), "expansion lane")), at, &mut sites);
                continue;
            }
            let key = match r.what {
                What::AnonConst(range) => match discs.get(&(r.id, range)) {
                    Some(&k) => k,
                    None if r.detail == ItemDetail::Enum => {
                        let why = match declared.get(&r.id) {
                            Some(Err(s)) => skip(s),
                            _ => ("world", String::from("an associated item's enum"), "census world"),
                        };
                        verdict(kind, Err(why), at, &mut sites);
                        continue;
                    }
                    None => w.declare_anon(PNode::Uint(UintTy::Usize)),
                },
                _ if associated(r) => {
                    verdict(kind, Err(("world", String::from("an associated const (impl selection)"), "names (blaze-scope)")), at, &mut sites);
                    continue;
                }
                _ => match declared.get(&r.id) {
                    Some(Ok(k)) => *k,
                    Some(Err(s)) => {
                        verdict(kind, Err(skip(s)), at, &mut sites);
                        continue;
                    }
                    None => {
                        verdict(kind, Err(("world", String::from("a signature that did not lower"), "expansion lane")), at, &mut sites);
                        continue;
                    }
                },
            };
            w.attach(key, r.bytes.as_deref().unwrap_or_default(), &r.text);
            todo.push((kind, key, at));
        }
        let failed: HashMap<ItemKey, Skip> = w.try_build().into_iter().collect();
        let attached: std::collections::HashSet<ItemKey> = todo.iter().map(|t| t.1).collect();
        for (kind, key, at) in todo {
            if let Some(s) = failed.get(&key) {
                verdict(kind, Err(skip(s)), at, &mut sites);
                continue;
            }
            let t = Instant::now();
            let got = w.eval_item(key).map(|_| ()).map_err(|e| match e.refusal {
                // A constant this one reads was refused before evaluation (counted by its own
                // cause): the world holds its signature and no body.
                blaze_consteval::Refusal::Missing { what: "body", item } if !attached.contains(&item) || failed.contains_key(&item) => {
                    ("eval", String::from("reads a constant refused before evaluation"), "its dependency's cause")
                }
                ref r => ("eval", refusal_name(r), "consteval"),
            });
            eval_ns += t.elapsed().as_nanos();
            evals += 1;
            verdict(kind, got, at, &mut sites);
        }
    }
    let took = started.elapsed();
    let (mut ok, mut bad) = (0, 0);
    println!("{} files from {} trees", files.len(), dirs.len());
    println!("{:<48} {:>9} {:>9}", "site", "evaluated", "refused");
    for (kind, (o, b)) in &sites {
        println!("{kind:<48} {o:>9} {b:>9}");
        ok += o;
        bad += b;
    }
    println!("{:<48} {ok:>9} {bad:>9}", "all");
    let mut rows: Vec<_> = causes.iter().collect();
    rows.sort_by(|a, b| b.1.count.cmp(&a.1.count).then(a.0.cmp(b.0)));
    println!("refusals by stage and cause (owner; one location):");
    for ((stage, cause), row) in rows {
        let cause: String = cause.chars().take(70).collect();
        println!("  {:>7}  {stage:<8} {cause}  [{}]  {}", row.count, row.owner, row.example);
    }
    println!(
        "time: {:.2} s in all; evaluation {:.1} ms over {evals} evaluations ({:.0} ns each)",
        took.as_secs_f64(),
        eval_ns as f64 / 1e6,
        eval_ns as f64 / evals.max(1) as f64
    );
}
