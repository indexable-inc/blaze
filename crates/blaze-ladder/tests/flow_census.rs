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
use blaze_flow_lower::{Facts, Region as TextRegion, SigLife, lower};
use blaze_ladder::front2::borrows::{BorrowFacts, BorrowLangs, gather_facts};
use blaze_ladder::front2::lifetimes::{FnLife, Region as FrontRegion};
use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::body::{BodyError, lower_fn, lower_member};
use blaze_ladder::front2::check::{BuildOrder, Records, check_body, eval_len};
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

/// The facts of one body, gathered before it is lowered (front2/borrows.rs `gather_facts`, the
/// one home the command shares), and the census's own counts beside them.
struct Gathered {
    facts: BorrowFacts<'static>,
    /// Why the signatures not read were refused, by the refusal's name.
    sig_refused: BTreeMap<String, usize>,
    /// The front end's lifetime part (front2/lifetimes.rs, `Store::fn_life`) against the text's,
    /// per signature, by outcome: a comparison only, the lowering still reads the text's.
    front: BTreeMap<String, usize>,
    /// Each signature where both answered and the answers differ.
    front_lines: Vec<String>,
}

/// Whether the text's lifetime part and the front end's say the same where both answer: the
/// same positions and ends; position by position the same kind of region, and one renaming of
/// the text's regions onto the front end's (the text numbers a named lifetime by first
/// appearance, the front end by declaration, the impl's first); every bound the text declares
/// among the front end's. A position the text calls `Self`'s is not compared: the front end
/// says the impl's lifetimes there, which the text cannot.
fn sigs_agree(text: SigLife<'_>, front: &FnLife) -> Result<(), String> {
    if text.regions.len() != front.regions.len() {
        return Err(format!("{} positions in the text, {} in the front end", text.regions.len(), front.regions.len()));
    }
    if !text.ends.iter().eq(front.ends.iter()) {
        return Err(format!("the inputs end at {:?} in the text, {:?} in the front end", text.ends, front.ends));
    }
    let mut to_front: BTreeMap<TextRegion, FrontRegion> = BTreeMap::new();
    let mut to_text: BTreeMap<FrontRegion, TextRegion> = BTreeMap::new();
    for (i, (&t, &f)) in text.regions.iter().zip(front.regions.iter()).enumerate() {
        let same_kind = match (t, f) {
            (TextRegion::SelfTy(_), _) => continue,
            (TextRegion::Static, FrontRegion::Static)
            | (TextRegion::Named(_), FrontRegion::Named(_))
            | (TextRegion::Anon(_), FrontRegion::Anon(_))
            | (TextRegion::Hidden(_), FrontRegion::Hidden(_)) => true,
            _ => false,
        };
        let one_renaming = *to_front.entry(t).or_insert(f) == f && *to_text.entry(f).or_insert(t) == t;
        if !same_kind || !one_renaming {
            return Err(format!("position {i}: the text says {t:?}, the front end {f:?} (text {:?}, front end {:?})", text.regions, front.regions));
        }
    }
    for &(a, b) in text.bounds {
        let (Some(&fa), Some(&fb)) = (to_front.get(&a), to_front.get(&b)) else { continue };
        if !front.bounds.iter().any(|x| *x == (fa, fb)) {
            return Err(format!("the text's bound {a:?}: {b:?} is not among the front end's {:?}", front.bounds));
        }
    }
    Ok(())
}

fn gather(r: &mut Records<'_, '_>, owner: tc::ItemKey, c: &tc::Checked<'_>, copy: Option<tc::ItemKey>, drop: Option<tc::ItemKey>, boxk: Option<tc::ItemKey>) -> Gathered {
    let facts = gather_facts(r, owner, c, BorrowLangs { copy, drop, owned_box: boxk }, Alloc::global());
    let mut g = Gathered { facts, sig_refused: BTreeMap::new(), front: BTreeMap::new(), front_lines: Vec::new() };
    for (_, why) in g.facts.sig_refused() {
        let name = format!("{why:?}");
        let name = name.split([' ', '{']).next().unwrap_or("").to_string();
        *g.sig_refused.entry(name).or_default() += 1;
    }
    // The front end's answer for each signature, as a second source: compared only.
    let refusal = |why: &dyn std::fmt::Debug| format!("{why:?}").split([' ', '{']).next().unwrap_or("").to_string();
    for &k in g.facts.items() {
        let key = tc::ItemKey(k);
        let Some(text) = r.store.fns.get(&key).map(|f| f.text) else { continue };
        let head_text = text.split('{').next().unwrap_or("").trim();
        let outcome = match (g.facts.sig(key), r.store.fn_life(r.names, &*r.keys, key)) {
            (Some(t), Ok(front)) => match sigs_agree(t, front) {
                Ok(()) => "agree".to_string(),
                Err(what) => {
                    g.front_lines.push(format!("`{head_text}`: {what}"));
                    "disagree".to_string()
                }
            },
            (None, Ok(_)) => "front end read, text refused".to_string(),
            (Some(_), Err(why)) => format!("text read, front end refused: {}", refusal(&why)),
            (None, Err(why)) => format!("both refused, front end: {}", refusal(&why)),
        };
        *g.front.entry(outcome).or_default() += 1;
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
    /// Calls related through the callee's lifetime part; whether the body's own was read.
    calls_by_signature: u32,
    sig_read: bool,
    /// Signatures (the body's and its callees') whose text was not read, by refusal.
    sig_refused: BTreeMap<String, usize>,
    /// The front end's lifetime part against the text's, by outcome, and each disagreement.
    front: BTreeMap<String, usize>,
    front_lines: Vec<String>,
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
    let tview = body.typeck.view(body.tys.view(&r.store.tys).expect("a body's types over the records' table it was lowered over"));
    let t0 = Instant::now();
    let lowered = lower(&tview, &done, &g.facts, Alloc::global());
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
                calls_by_signature: l.stats.calls_by_signature,
                sig_read: l.stats.sig_read,
                sig_refused: g.sig_refused.clone(),
                front: g.front.clone(),
                front_lines: g.front_lines.clone(),
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
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
        let mut front = Front2::new(&names, &frame, &sources).unwrap_or_else(|e| panic!("front2: {e}"));
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
    let mut store = Store::new(sized, front.frame);
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
    let (mut by_sig, mut sig_read) = (0u64, 0usize);
    let mut sig_refused: BTreeMap<String, usize> = BTreeMap::new();
    let mut front_all: BTreeMap<String, usize> = BTreeMap::new();
    let mut front_lines: BTreeSet<String> = BTreeSet::new();
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
                by_sig += u64::from(d.calls_by_signature);
                sig_read += usize::from(d.sig_read);
                for (name, n) in &d.sig_refused {
                    *sig_refused.entry(name.clone()).or_default() += n;
                }
                for (name, n) in &d.front {
                    *front_all.entry(name.clone()).or_default() += n;
                }
                front_lines.extend(d.front_lines.iter().cloned());
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
    out.push_str(&format!(
        "signatures: bodies whose own lifetime part was read {sig_read}; calls related through the callee's {by_sig}; signature texts not read (of lowered bodies, a callee counted once per body), by refusal: {sig_refused:?}\n"
    ));
    // The front end's lifetime part (front2/lifetimes.rs) beside the text's: where both answer
    // they must agree; each disagreement once, then every outcome's count.
    out.push_str(&format!("signatures: front end and text disagree: {}\n", front_all.get("disagree").copied().unwrap_or(0)));
    out.push_str(&format!("signatures: front end against text (of lowered bodies, a callee counted once per body), by outcome: {front_all:?}\n"));
    for l in front_lines.iter().take(60) {
        out.push_str(&format!("  DISAGREE {l}\n"));
    }
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
