//! The flow census (docs/v2/plan/design/flow.md, "The census"): every fn body of bstd that the
//! type checker checks clean is lowered to its flow (crates/blaze-flow-lower) and move and borrow
//! checked (crates/blaze-flow): lowered or refused (by form, largest first, three examples each);
//! of the lowered, accepted by tier one, sent to tier two and accepted, or FINDINGS. bstd compiles
//! under the pinned compiler, so every finding is a bug of the lowering or the analyses: each is
//! listed with its body, its item-relative range, its kind and the text there.
//!
//! The bodies are listed and checked as tests/front2_census.rs does (shared records). The facts
//! the record lacks (flow.md section 7) come from the heads store and the impl index: `Copy` and
//! `Drop` by an impl for the ADT's shape; an ADT's or a callee's lifetimes by a `'` in its head's
//! text before its body (the heads' lifetime part is not built: a stand-in, named here).
//!
//! `FLOW_CENSUS_OUT` names the output file; `FLOW_CENSUS_CAP` caps the bodies.

use std::collections::{BTreeMap, BTreeSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::{Duration, Instant};

use blaze_flow::flow::Flow;
use blaze_flow::loans::{BorrowFinding, TierOne, facts, tier_one, tier_two};
use blaze_flow::moves::check_moves_planted;
use blaze_flow::record::Plant as FlowPlant;
use blaze_flow_lower::{AdtFacts, CalleeFacts, Facts, lower};
use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::body::{BodyError, lower_fn, lower_member};
use blaze_ladder::front2::check::{BuildOrder, Records, answer_impls, check_body, eval_len};
use blaze_ladder::front2::consts::Consts;
use blaze_ladder::front2::heads::{HeadError, Store, Want};
use blaze_ladder::front2::keys::{MemberKind, checker_of};
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::front2::types::Why;
use blaze_ladder::refusal::SourceFile;
use blaze_lower::scoped::Prepares;
use blaze_own::Alloc;
use blaze_scope::resolve::Names;
use blaze_surface::ItemDetail;
use blaze_typeck as tc;

const EDITION: &str = "blaze-2026.11";
const MAIN: &str = "fn main(cx: &Cx, io: Io) -> Result<(), Error> {
    io.need_out()?.line(cx, \"Hello, world!\")?;
    Ok(())
}
";
const MOST_LENS: u32 = 64;
const EXAMPLES: usize = 3;

#[derive(Clone, Copy)]
enum Target<'n> {
    Free(blaze_id::ItemKey),
    Member { container: blaze_id::ItemKey, name: &'n str },
}

/// The facts of one body, gathered before it is lowered.
#[derive(Default)]
struct Gathered {
    adts: BTreeMap<u128, AdtFacts>,
    callees: BTreeMap<u128, CalleeFacts>,
    fields: BTreeMap<(u128, u32), Vec<tc::Name>>,
}

impl Facts for Gathered {
    fn adt(&self, adt: tc::ItemKey) -> Option<AdtFacts> {
        self.adts.get(&adt.0).copied()
    }
    fn callee(&self, item: tc::ItemKey) -> Option<CalleeFacts> {
        self.callees.get(&item.0).copied()
    }
    fn field(&self, adt: tc::ItemKey, variant: u32, name: tc::Name) -> Option<u32> {
        self.fields.get(&(adt.0, variant))?.iter().position(|n| *n == name).map(|i| i as u32)
    }
    fn field_count(&self, adt: tc::ItemKey, variant: u32) -> Option<u32> {
        self.fields.get(&(adt.0, variant)).map(|v| v.len() as u32)
    }
    fn param_copy(&self, _: u32) -> bool {
        false
    }
}

/// Whether a head's text writes a lifetime before its body: a `'` followed by a letter or `_`
/// before the first `{` (a stand-in for the heads' lifetime part; `'_` counts: it is written).
fn writes_lifetime(text: &str) -> bool {
    let head = text.split('{').next().unwrap_or(text);
    let b = head.as_bytes();
    (0..b.len()).any(|i| b[i] == b'\'' && b.get(i + 1).is_some_and(|c| c.is_ascii_alphabetic() || *c == b'_') && b.get(i + 2) != Some(&b'\''))
}

fn gather(r: &mut Records<'_, '_>, owner: tc::ItemKey, c: &tc::Checked<'_>, copy: Option<tc::ItemKey>, drop: Option<tc::ItemKey>, boxk: Option<tc::ItemKey>) -> Gathered {
    let mut g = Gathered::default();
    let mut adts: BTreeSet<u128> = BTreeSet::new();
    for n in c.types.view().nodes {
        if let tc::PNode::Adt(k, _) = n {
            adts.insert(k.0);
        }
    }
    for k in adts {
        let key = tc::ItemKey(k);
        let Some(row) = r.store.adts.get(&key) else { continue };
        let (kind, variants) = (row.kind, row.variants.clone());
        let fields = row.fields.clone();
        for (vi, v) in variants.iter().enumerate() {
            let names: Vec<tc::Name> = fields.get(v.fields.range()).unwrap_or(&[]).iter().map(|f| f.name).collect();
            g.fields.insert((k, vi as u32), names);
        }
        let has = |r: &mut Records<'_, '_>, t: Option<tc::ItemKey>| t.and_then(|t| answer_impls(r, t, tc::Shape::Adt(key))).is_some_and(|v| !v.is_empty());
        let is_copy = has(r, copy);
        let drop_impl = has(r, drop);
        let lifetimes = r.keys.library(key).and_then(|lib| r.names.item_text(lib)).is_some_and(|t| {
            let head = t.text.split(['{', '(', ';']).next().unwrap_or("");
            head.contains('\'')
        });
        g.adts.insert(k, AdtFacts {
            copy: is_copy,
            drop_impl,
            needs_drop: !is_copy,
            lifetimes,
            union: kind == tc::AdtKind::Union,
            is_enum: kind == tc::AdtKind::Enum,
            is_box: Some(key) == boxk,
        });
    }
    let mut items: BTreeSet<u128> = c.callees.column::<1>().iter().map(|k| k.0).collect();
    items.insert(owner.0);
    for k in items {
        let Some(f) = r.store.fns.get(&tc::ItemKey(k)) else { continue };
        let self_ref = f.self_param && f.inputs.first().is_some_and(|&t| matches!(r.store.tys.view().node(t), Ok(tc::PNode::Ref(..))));
        g.callees.insert(k, CalleeFacts { writes_lifetimes: writes_lifetime(f.text), self_ref });
    }
    g
}

/// One body's outcome.
enum Got {
    NoBody,
    NotClean,
    Refused { form: String, range: tc::Range, text: String },
    Lowered(Box<Done>),
}

struct Done {
    lower_t: Duration,
    moves_t: Duration,
    tier1_t: Duration,
    tier2_t: Option<Duration>,
    tier1_accepts: bool,
    findings: Vec<String>,
    calls_elided: u32,
    calls_unified: u32,
    sig_lifetimes: bool,
}

fn snippet(text: &str, lo: u32, hi: u32) -> String {
    let s = text.get(lo as usize..(hi as usize).min(text.len())).unwrap_or("?");
    let s: String = s.chars().take(80).collect();
    s.replace('\n', " ")
}

fn analyse(flow: &Flow<Alloc<'static>>, text: &str) -> (Duration, Duration, Option<Duration>, bool, Vec<String>) {
    let a = Alloc::global();
    let t0 = Instant::now();
    let moves = check_moves_planted(flow, FlowPlant::None, a).expect("moves bounded");
    let moves_t = t0.elapsed();
    let t1 = Instant::now();
    let f = facts(flow, a).expect("facts bounded");
    let one = tier_one(flow, &f, FlowPlant::None, a).expect("tier one bounded");
    let tier1_t = t1.elapsed();
    let mut out: Vec<String> = moves.iter().map(|m| format!("move {:?} {}..{} `{}`", m.kind, m.span.lo, m.span.hi, snippet(text, m.span.lo, m.span.hi))).collect();
    let (tier2_t, accepts) = match one {
        TierOne::Accepted => (None, true),
        TierOne::Refused => {
            let t2 = Instant::now();
            let found = tier_two(flow, &f, FlowPlant::None, a).expect("tier two bounded");
            let d = t2.elapsed();
            for b in found.iter() {
                let (kind, span) = match *b {
                    BorrowFinding::Invalidated { span, .. } => ("borrow Invalidated", span),
                    BorrowFinding::UnprovenOutlives { span, .. } => ("borrow UnprovenOutlives", span),
                };
                out.push(format!("{kind} {}..{} `{}`", span.lo, span.hi, snippet(text, span.lo, span.hi)));
            }
            (Some(d), false)
        }
    };
    (moves_t, tier1_t, tier2_t, accepts, out)
}

fn take<'n>(r: &mut Records<'_, 'n>, prepared: &Prepares<'n>, t: Target<'_>, langs: (Option<tc::ItemKey>, Option<tc::ItemKey>, Option<tc::ItemKey>)) -> Got {
    let mut lens = 0;
    let lowered = loop {
        let lowered = match t {
            Target::Free(lib) => lower_fn(r.names, prepared, r.keys, r.store, lib),
            Target::Member { container, name } => match r.keys.filed_member(checker_of(container), MemberKind::Fn, name) {
                Some(k) if !r.store.fn_gaps.contains_key(&k) => lower_member(r.names, prepared, r.keys, r.store, k),
                _ => return Got::NotClean,
            },
        };
        let c = match &lowered {
            Err(BodyError::Convert { why: Some(Why::NeedConst(c, _)), .. }) | Err(BodyError::Head { error: HeadError::Item(_, Why::NeedConst(c, _)), .. }) if lens < MOST_LENS => *c,
            _ => break lowered,
        };
        lens += 1;
        if eval_len(r, c).is_err() {
            return Got::NotClean;
        }
    };
    let body = match lowered {
        Ok(b) => b,
        Err(BodyError::NoBody(_)) => return Got::NoBody,
        Err(_) => return Got::NotClean,
    };
    let view = body.lowered.view();
    let Ok(spell) = Spelling::of(&view) else { return Got::NotClean };
    let scope = body.scope(&spell);
    let done = match check_body(r, &body, &scope) {
        Ok(d) if d.diags.is_empty() => d,
        _ => return Got::NotClean,
    };
    let text = r.store.fns.get(&body.owner).map(|f| f.text).or_else(|| r.names.item_text(body.item).map(|t| t.text)).unwrap_or("").to_string();
    let g = gather(r, body.owner, &done, langs.0, langs.1, langs.2);
    let tview = body.typeck.view(body.tys.view());
    let t0 = Instant::now();
    let lowered = lower(&tview, &done, &g, Alloc::global());
    let lower_t = t0.elapsed();
    match lowered {
        Err(refusal) => Got::Refused { form: format!("{:?}", refusal.form), range: refusal.range, text: snippet(&text, refusal.range.start, refusal.range.end) },
        Ok(l) => {
            let (moves_t, tier1_t, tier2_t, tier1_accepts, findings) = analyse(&l.flow, &text);
            Got::Lowered(Box::new(Done {
                lower_t,
                moves_t,
                tier1_t,
                tier2_t,
                tier1_accepts,
                findings,
                calls_elided: l.stats.calls_elided,
                calls_unified: l.stats.calls_unified,
                sig_lifetimes: l.stats.sig_writes_lifetimes,
            }))
        }
    }
}

fn container_head(r: &mut Records<'_, '_>, lib: blaze_id::ItemKey, is_trait: bool) -> bool {
    let wants: &[Want] = if is_trait { &[Want::Head(lib), Want::Members(lib)] } else { &[Want::Impl(lib)] };
    for &w in wants {
        let mut lens = 0;
        loop {
            match r.store.ensure(r.names, r.keys, w) {
                Ok(()) => break,
                Err(HeadError::Item(_, Why::NeedConst(c, _))) if lens < MOST_LENS => {
                    lens += 1;
                    if eval_len(r, c).is_err() {
                        return false;
                    }
                }
                Err(_) => return false,
            }
        }
    }
    true
}

#[test]
fn flow_census() {
    let start = Instant::now();
    let cap: usize = std::env::var("FLOW_CENSUS_CAP").ok().and_then(|s| s.parse().ok()).unwrap_or(100_000);
    let out_path = std::env::var("FLOW_CENSUS_OUT").ok();
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc_ = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/hello-world/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
    let user = UserCrate { name: "hello_world", dir: "/hello-world/", root: "/hello-world/src/main.rs", files: &files, edition };
    let inputs = Inputs::new(&user, &tc_).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names: Names<'_> = inputs.resolve(&crates).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let crate_names: Vec<&str> = crates.iter().map(|c| c.name).collect();
    let cname = if crate_names.contains(&"bstd") { "bstd" } else { "blaze_std" };
    let krate = crate_names.iter().position(|n| *n == cname).expect("bstd is a crate");
    let sources = [SourceFile { path: "src/main.rs", text: MAIN }];
    let mut front = Front2::new(&names, &sources).unwrap_or_else(|e| panic!("front2: {e}"));
    let resolver = front.resolver;
    let lang = |n: &str| front.index.lang(n).map(checker_of);
    let langs = (lang("copy"), lang("drop"), lang("owned_box"));

    // The bodies: free fns and members of impls and traits, in key order.
    let mut items: Vec<(blaze_id::ItemKey, ItemDetail)> = names.items(krate).filter_map(|(k, _, _)| resolver.detail(k).map(|d| (k, d))).collect();
    items.sort_by_key(|(k, _)| format!("{k:x}"));
    let mut members: BTreeSet<String> = BTreeSet::new();
    for (k, d) in &items {
        if matches!(d, ItemDetail::Impl | ItemDetail::Trait) {
            for m in names.members(*k).unwrap_or(&[]) {
                members.insert(format!("{:x}", m.key));
            }
        }
    }
    let mut entries: Vec<(Target<'_>, String, bool)> = Vec::new();
    for (k, d) in &items {
        let Some(it) = names.item_text(*k) else { continue };
        match d {
            ItemDetail::Fn { .. } if !members.contains(&format!("{k:x}")) => {
                if !it.text.trim_end().ends_with(';') {
                    entries.push((Target::Free(*k), format!("{}::{}", names.krate(krate).path_of(it.module).unwrap_or("?"), it.name.unwrap_or("?")), false));
                }
            }
            ItemDetail::Impl | ItemDetail::Trait => {
                let head = it.impl_header.map(|h| h.chars().take(60).collect::<String>()).unwrap_or_else(|| format!("trait {}", it.name.unwrap_or("?")));
                for m in names.members(*k).unwrap_or(&[]) {
                    let (ItemDetail::Fn { .. }, Some(name)) = (m.detail, m.name) else { continue };
                    entries.push((Target::Member { container: *k, name }, format!("<{head}>::{name}"), matches!(d, ItemDetail::Trait)));
                }
            }
            _ => {}
        }
    }

    let prepared = Prepares::new_in(Alloc::global());
    let sized = front.index.lang("sized");
    let mut store = Store::new(sized);
    let mut buckets = None;
    let mut consts = Consts::default();
    let mut heads: BTreeMap<String, bool> = BTreeMap::new();
    let (mut clean, mut lowered, mut refused, mut t1, mut t2, mut found_bodies) = (0usize, 0usize, 0usize, 0usize, 0usize, 0usize);
    let (mut not_clean, mut panics) = (0usize, 0usize);
    let mut refusals: BTreeMap<String, (usize, Vec<String>)> = BTreeMap::new();
    let mut finding_lines: Vec<String> = Vec::new();
    let mut finding_kinds: BTreeMap<String, usize> = BTreeMap::new();
    let (mut lower_t, mut moves_t, mut tier1_t, mut tier2_t) = (Duration::ZERO, Duration::ZERO, Duration::ZERO, Duration::ZERO);
    let (mut elided, mut unified, mut sig_lt) = (0u64, 0u64, 0usize);
    let mut slowest: Vec<(Duration, String)> = Vec::new();
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    for (i, (t, path, is_trait)) in entries.iter().enumerate() {
        if i == cap {
            break;
        }
        let mut r = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: Some((&mut consts, &prepared)) };
        let got = catch_unwind(AssertUnwindSafe(|| {
            if let Target::Member { container, .. } = *t {
                let key = format!("{container:x}");
                let ok = match heads.get(&key) {
                    Some(&ok) => ok,
                    None => {
                        let ok = container_head(&mut r, container, *is_trait);
                        heads.insert(key, ok);
                        ok
                    }
                };
                if !ok {
                    return Got::NotClean;
                }
            }
            take(&mut r, &prepared, *t, langs)
        }));
        let got = match got {
            Ok(g) => g,
            Err(_) => {
                panics += 1;
                continue;
            }
        };
        match got {
            Got::NoBody => {}
            Got::NotClean => not_clean += 1,
            Got::Refused { form, range, text } => {
                clean += 1;
                refused += 1;
                let slot = refusals.entry(form).or_insert((0, Vec::new()));
                slot.0 += 1;
                if slot.1.len() < EXAMPLES {
                    slot.1.push(format!("{path} at {}..{}: `{text}`", range.start, range.end));
                }
            }
            Got::Lowered(d) => {
                clean += 1;
                lowered += 1;
                lower_t += d.lower_t;
                moves_t += d.moves_t;
                tier1_t += d.tier1_t;
                elided += u64::from(d.calls_elided);
                unified += u64::from(d.calls_unified);
                sig_lt += usize::from(d.sig_lifetimes);
                let all = d.lower_t + d.moves_t + d.tier1_t + d.tier2_t.unwrap_or_default();
                slowest.push((all, path.clone()));
                if let Some(x) = d.tier2_t {
                    tier2_t += x;
                }
                if d.tier1_accepts {
                    t1 += 1;
                } else if d.findings.iter().all(|f| f.starts_with("move")) {
                    t2 += 1;
                }
                if !d.findings.is_empty() {
                    found_bodies += 1;
                    for f in &d.findings {
                        let kind: String = f.split(' ').take(2).collect::<Vec<_>>().join(" ");
                        *finding_kinds.entry(kind).or_insert(0) += 1;
                        finding_lines.push(format!("{path}: {f}"));
                    }
                }
            }
        }
    }
    std::panic::set_hook(prev_hook);
    let per = |d: Duration, n: usize| if n == 0 { Duration::ZERO } else { d / n as u32 };
    let mut out = format!(
        "{cname}: clean bodies {clean}; lowered {lowered}; refused {refused}; tier one accepts {t1}; tier two accepts {t2}; findings {} (in {found_bodies} bodies)\n",
        finding_lines.len()
    );
    out.push_str(&format!("bodies listed {}; not clean in the type checker {not_clean}; panics {panics}; a debug build; {:?} in all\n", entries.len(), start.elapsed()));
    out.push_str(&format!(
        "time per lowered body: lowering {:?}, move check {:?}, tier one {:?}; tier two {:?} per body sent ({} sent)\n",
        per(lower_t, lowered),
        per(moves_t, lowered),
        per(tier1_t, lowered),
        per(tier2_t, lowered - t1),
        lowered - t1
    ));
    out.push_str(&format!("calls related by elision {elided}; calls unified (generic origins or unknown callee) {unified}; bodies whose signature writes lifetimes (placeholders taken as related) {sig_lt}\n"));
    let mut sorted: Vec<(&String, &(usize, Vec<String>))> = refusals.iter().collect();
    sorted.sort_by(|a, b| b.1.0.cmp(&a.1.0).then_with(|| a.0.cmp(b.0)));
    out.push_str(&format!("REFUSED by form ({}), largest first:\n", sorted.len()));
    for (form, (n, ex)) in sorted {
        out.push_str(&format!("{n:6} {form}\n"));
        for x in ex {
            out.push_str(&format!("         e.g. {x}\n"));
        }
    }
    out.push_str(&format!("FINDINGS by kind: {finding_kinds:?}\n"));
    for f in &finding_lines {
        out.push_str(&format!("  {f}\n"));
    }
    slowest.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    out.push_str("slowest lowered bodies (lowering + checks):\n");
    for (d, p) in slowest.iter().take(5) {
        out.push_str(&format!("  {d:?} {p}\n"));
    }
    print!("{out}");
    if let Some(p) = out_path {
        std::fs::write(&p, &out).unwrap_or_else(|e| panic!("writing {p}: {e}"));
    }
}
