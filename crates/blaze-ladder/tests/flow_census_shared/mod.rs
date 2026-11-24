//! The flow census, ONE PATH (rule 30), shared by tests/flow_census.rs (bstd) and
//! tests/flow_census_core.rs (the toolchain's `core`, `alloc` or `std`): every fn body of one
//! crate is checked by THE ENGINE'S OWN move and borrow check, `ProjectWorld::check_borrows`
//! (front2/borrows.rs), the function `blaze check`'s body tasks call (blaze-cli body_types.rs,
//! through body_types/covered.rs `each_covered`): the body's types, then, when they were checked
//! clean, its facts read from the heads (borrows.rs `gather_facts`), its own flow and each
//! closure's and async block's body as a flow of its own (borrows_nested.rs), each move and
//! borrow checked (blaze-flow `check`). Nothing here makes a fact: what the engine's path lacks
//! in this setting shows as a refusal or a finding, never patched over by a stand-in.
//!
//! The report, in the text form of the census before it so runs compare: the bodies listed;
//! those whose types were not checked clean, by why; those borrow checked WHOLE (the body and
//! every nested body: `BorrowVerdict::refused` is none); those REFUSED, by form, largest first,
//! three examples each (the owner's path, the item-relative range, the text there, the closure
//! or async block the refusal is in, the refusal's own words); the accepted (whole, no finding);
//! every FINDING with its body, range, code, the text there and its words, then the findings by
//! code. Its last line is the gate, for a script:
//!
//! ```text
//! gate: bodies B, whole W, refused R, false findings F
//! ```
//!
//! B is the bodies whose types were checked clean (W + R, a body whose check panicked counted
//! refused), F every finding (both corpora compile under rustc, so each one is false). With
//! blaze-cli's `BORROWS_ARE_ERRORS` true, R + F > 0 fails the test, after the report is printed
//! and written; while it is false the test passes with a note.
//!
//! What the engine's entry does not give, and the census before it printed: the split of a
//! body's borrow check into flow lowering, the move check, tier one and tier two (blaze-flow
//! `check` runs them in one call: here one `borrows` part of the engine's cost meter), and which
//! tier accepted a body.
//!
//! Bodies are taken in key order. Everything a body's check makes is in one frame, reset after
//! each body; the report, the listing and the world are in the test's frame.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::{Duration, Instant};

use blaze_ladder::front2::borrows::{BorrowRefused, BorrowVerdict};
use blaze_ladder::front2::keys::MemberKind;
use blaze_ladder::front2::project::{BodyRef, BodyVerdict, ProjectWorld, Refused};
use blaze_ladder::front2::tables::cost::{Cost, Gauge, PARTS, Spent};
use blaze_mem::Frame;
use blaze_own::{ABTreeMap, AString, AVec, Alloc, Erased, format_in, format_into};
use blaze_scope::resolve::Names;
use blaze_surface::ItemDetail;

/// Examples kept per form.
const EXAMPLES: usize = 3;
/// The address space of the frame one body's check is made in (committed only as used).
const BODY_RESERVE: usize = 8 << 30;
/// How many characters of a body's text a range shows.
const SNIPPET: usize = 80;
/// How many characters of a refusal's own words an example shows.
const DETAIL: usize = 240;
/// How many of the slowest bodies are listed.
const SLOWEST: usize = 5;

/// The switch the gate reads: blaze-cli body_types.rs, whose `BORROWS_ARE_ERRORS` line is read
/// from its text (blaze-ladder cannot depend on blaze-cli: the dependency runs the other way).
/// Included, so the test is built again when that file changes.
const BODY_TYPES: &str = include_str!("../../../blaze-cli/src/body_types.rs");
const SWITCH: &str = "pub const BORROWS_ARE_ERRORS: bool = ";

/// Why the switch could not be read from blaze-cli body_types.rs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitchUnread {
    /// No line begins `pub const BORROWS_ARE_ERRORS: bool = `.
    Missing,
    /// More than one does.
    Twice,
    /// Its value is not `true;` or `false;`.
    NotABool,
}

impl core::fmt::Display for SwitchUnread {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let what = match self {
            SwitchUnread::Missing => "no line declares it",
            SwitchUnread::Twice => "two lines declare it",
            SwitchUnread::NotABool => "its value is not `true;` or `false;`",
        };
        write!(f, "`BORROWS_ARE_ERRORS` in crates/blaze-cli/src/body_types.rs: {what} (the census reads the line `{SWITCH}<bool>;`)")
    }
}

/// The value of blaze-cli's `BORROWS_ARE_ERRORS`, from its one line.
pub fn borrows_are_errors() -> Result<bool, SwitchUnread> {
    let mut found = None;
    for line in BODY_TYPES.lines() {
        let Some(value) = line.trim_start().strip_prefix(SWITCH) else { continue };
        if found.is_some() {
            return Err(SwitchUnread::Twice);
        }
        found = Some(match value.trim_end() {
            "true;" => true,
            "false;" => false,
            _ => return Err(SwitchUnread::NotABool),
        });
    }
    found.ok_or(SwitchUnread::Missing)
}

/// The counts the gate line says.
#[derive(Debug, Clone, Copy)]
pub struct Gate {
    /// Bodies whose types were checked clean: `whole + refused`.
    pub bodies: usize,
    pub whole: usize,
    pub refused: usize,
    /// Every finding, of every body (whole or refused).
    pub findings: usize,
    /// The cap stopped the census before the last body.
    pub capped: bool,
}

/// The census's text and its gate.
pub struct Report {
    pub text: AString<'static>,
    pub gate: Gate,
}

/// One body to check: what the engine is asked, the owner's path as the report names it, and
/// the text its ranges are relative to (a free fn's item text, a member's own text).
struct Entry<'n> {
    body: BodyRef<'n>,
    path: AString<'static>,
    text: &'n str,
}

/// The test's clock for the engine's cost meter (front2 tables/cost.rs `Gauge`): no byte counter.
struct Clock {
    base: Instant,
}

impl Gauge for Clock {
    fn now_ns(&self) -> u64 {
        u64::try_from(self.base.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }
    fn bytes(&self) -> u64 {
        0
    }
}

/// Counts by name, each with its first [`EXAMPLES`] examples; printed largest first.
struct Groups {
    by: ABTreeMap<'static, AString<'static>, (usize, AVec<'static, AString<'static>>)>,
    a: Alloc<'static>,
}

impl Groups {
    fn new_in(a: Alloc<'static>) -> Groups {
        Groups { by: ABTreeMap::new_in(a), a }
    }

    fn file(&mut self, name: AString<'static>, example: impl FnOnce() -> AString<'static>) {
        let a = self.a;
        let slot = self.by.entry(name).or_insert_with(|| (0, AVec::new_in(a)));
        slot.0 += 1;
        if slot.1.len() < EXAMPLES {
            slot.1.push(example());
        }
    }

    fn total(&self) -> usize {
        self.by.values().map(|(n, _)| *n).sum()
    }

    /// `{title} ({forms}), largest first:` then each name with its count and examples; ties by name.
    fn print(&self, title: &str, out: &mut AString<'static>) {
        let mut sorted: AVec<'static, (&AString<'static>, usize, &[AString<'static>])> = AVec::new_in(self.a);
        for (name, (n, ex)) in self.by.iter() {
            sorted.push((name, *n, ex.as_slice()));
        }
        sorted.sort_unstable_by(|x, y| y.1.cmp(&x.1).then_with(|| x.0.cmp(y.0)));
        format_into!(out, "{title} ({}), largest first:\n", sorted.len());
        for (name, n, ex) in sorted.iter() {
            format_into!(out, "{n:6} {name}\n");
            for x in ex.iter() {
                format_into!(out, "         e.g. {x}\n");
            }
        }
    }
}

/// The text of `text` at the item-relative `lo..hi`, its first [`SNIPPET`] characters on one
/// line; `?` when the range is not in the text.
fn snippet(text: &str, lo: u32, hi: u32, out: &mut AString<'static>) {
    let Some(s) = text.get(lo as usize..(hi as usize).min(text.len())) else {
        out.push('?');
        return;
    };
    for c in s.chars().take(SNIPPET) {
        out.push(if c == '\n' { ' ' } else { c });
    }
}

/// `words` on one line, its first `most` characters, ` ..` after them when cut.
fn clipped(words: &str, most: usize, out: &mut AString<'static>) {
    for (i, c) in words.chars().enumerate() {
        if i == most {
            out.push_str(" ..");
            return;
        }
        out.push(if c == '\n' { ' ' } else { c });
    }
}

/// The variant's name at the head of a `Debug` text: `Closure`, `Unaligned(..)` -> `Unaligned`.
fn first_word(debug: &str) -> &str {
    debug.split([' ', '{', '(']).next().unwrap_or(debug)
}

/// The form a refusal is counted under: the lowering's form by its name (as the census before
/// this one named it), every other refusal by its variant, a call's or a fact's with what it
/// lacked. The callee, the ADT and the range are in the example, not the name.
fn form_name(r: &BorrowRefused, a: Alloc<'static>) -> AString<'static> {
    let word = |debug: AString<'static>| format_in!(a, "{}", first_word(debug.as_str()));
    match r {
        BorrowRefused::Lower(x) => match x.form {
            blaze_flow_lower::Form::OneRule { why, .. } => match why {
                blaze_flow_lower::OneRuleWhy::Unselected { why: u, .. } => format_in!(a, "OneRule Unselected/{u:?}"),
                blaze_flow_lower::OneRuleWhy::Count { why: c, at, .. } => format_in!(a, "OneRule Count/{at:?}/{c:?}"),
                blaze_flow_lower::OneRuleWhy::Malformed { why: m, .. } => format_in!(a, "OneRule Malformed/{m:?}"),
                blaze_flow_lower::OneRuleWhy::Shape { why: s, .. } => format_in!(a, "OneRule Shape/{}", first_word(format_in!(a, "{s:?}").as_str())),
                other => format_in!(a, "OneRule {}", word(format_in!(a, "{other:?}"))),
            },
            _ => word(format_in!(a, "{:?}", x.form)),
        },
        BorrowRefused::Stricter(x) => format_in!(a, "Stricter {}", word(format_in!(a, "{:?}", x.form))),
        BorrowRefused::CallUnrelated { lacks, .. } => format_in!(a, "CallUnrelated {}", word(format_in!(a, "{lacks:?}"))),
        BorrowRefused::RegionsUnread { why, .. } => format_in!(a, "RegionsUnread {}", word(format_in!(a, "{why:?}"))),
        BorrowRefused::FactUnknown { fact, .. } => format_in!(a, "FactUnknown {fact}"),
        BorrowRefused::AdtNotMade { why, .. } => format_in!(a, "AdtNotMade {}", word(format_in!(a, "{why:?}"))),
        other => word(format_in!(a, "{other:?}")),
    }
}

/// Why a body's types were not checked clean, by the class its verdict names.
fn why_untyped(types: &BodyVerdict<'_>) -> &'static str {
    match &types.refused {
        Some(r) => r.text(),
        None if !types.diagnostics.is_empty() => "type errors",
        None if !types.gaps.is_empty() => "questions the world could not answer (gaps)",
        None => "types not checked, no refusal named",
    }
}

/// Every fn body of crate `krate` with a body text, in key order: free fns (a declaration
/// ending in `;` has none), then, at each impl's or trait's key, its fn members.
fn list<'n>(names: &'n Names<'n>, world: &ProjectWorld<'n>, krate: usize, a: Alloc<'static>) -> AVec<'static, Entry<'n>> {
    let resolver = world.front.resolver;
    let mut items: AVec<'static, (blaze_id::ItemKey, ItemDetail)> = AVec::new_in(a);
    for (k, _, _) in names.items(krate) {
        if let Some(d) = resolver.detail(k) {
            items.push((k, d));
        }
    }
    items.sort_unstable_by(|x, y| x.0.cmp(&y.0));
    let mut members: AVec<'static, blaze_id::ItemKey> = AVec::new_in(a);
    for (k, d) in items.iter() {
        if matches!(d, ItemDetail::Impl | ItemDetail::Trait) {
            for m in names.members(*k).unwrap_or(&[]) {
                members.push(m.key);
            }
        }
    }
    members.sort_unstable();
    let mut entries: AVec<'static, Entry<'n>> = AVec::new_in(a);
    for &(k, d) in items.iter() {
        let Some(it) = names.item_text(k) else { continue };
        match d {
            ItemDetail::Fn { .. } if members.binary_search(&k).is_err() => {
                if !it.text.trim_end().ends_with(';') {
                    let module = names.krate(krate).path_of(it.module).unwrap_or("?");
                    entries.push(Entry { body: BodyRef::Free(k), path: format_in!(a, "{module}::{}", it.name.unwrap_or("?")), text: it.text });
                }
            }
            ItemDetail::Impl | ItemDetail::Trait => {
                let is_trait = matches!(d, ItemDetail::Trait);
                let mut head = AString::new_in(a);
                match it.impl_header {
                    Some(h) => head.extend(h.chars().take(60)),
                    None => format_into!(&mut head, "trait {}", it.name.unwrap_or("?")),
                }
                for m in names.members(k).unwrap_or(&[]) {
                    let (ItemDetail::Fn { .. }, Some(name)) = (m.detail, m.name) else { continue };
                    let body = BodyRef::Member { container: k, is_trait, kind: MemberKind::Fn, name };
                    entries.push(Entry { body, path: format_in!(a, "<{head}>::{name}"), text: m.text });
                }
            }
            _ => {}
        }
    }
    entries
}

/// What the census counts as it goes.
struct Tally {
    no_body: usize,
    not_clean: Groups,
    panics: AVec<'static, AString<'static>>,
    whole: usize,
    accepted: usize,
    refused: Groups,
    findings: AVec<'static, AString<'static>>,
    finding_bodies: usize,
    kinds: ABTreeMap<'static, &'static str, usize>,
    /// The cost meter's parts over every body checked, and the borrows part over the clean ones.
    spent: Spent,
    checked: usize,
    borrows_ns: u64,
    /// (wall time of the body's check, its borrows part, its index in the listing), clean bodies.
    slowest: AVec<'static, (Duration, u64, usize)>,
}

impl Tally {
    /// One body's verdicts filed.
    #[expect(clippy::too_many_arguments, reason = "one body's verdicts and its measurements")]
    fn file(&mut self, e: &Entry<'_>, at: usize, types: &BodyVerdict<'_>, borrows: &BorrowVerdict<'_>, took: Duration, spent: &Spent, a: Alloc<'static>) {
        if matches!(types.refused, Some(Refused::NoBody)) {
            self.no_body += 1;
            return;
        }
        self.checked += 1;
        self.spent.add(spent);
        if let Some(BorrowRefused::Untyped(_)) = borrows.refused {
            self.not_clean.file(format_in!(a, "{}", why_untyped(types)), || {
                let mut x = format_in!(a, "{}", e.path);
                match (&types.refused, types.diagnostics.first(), types.gaps.first()) {
                    (Some(r), _, _) => {
                        x.push_str(": ");
                        clipped(format_in!(a, "{r:?}").as_str(), DETAIL, &mut x);
                    }
                    (None, Some(d), _) => {
                        format_into!(&mut x, " at {}..{}: {} `", d.start, d.end, d.code);
                        snippet(e.text, d.start, d.end, &mut x);
                        x.push('`');
                    }
                    (None, None, Some(g)) => {
                        x.push_str(": ");
                        clipped(format_in!(a, "{g:?}").as_str(), DETAIL, &mut x);
                    }
                    (None, None, None) => {}
                }
                x
            });
            return;
        }
        let borrows_part = spent.ns[blaze_ladder::front2::tables::cost::Part::Borrows as usize];
        self.borrows_ns += borrows_part;
        self.slowest.push((took, borrows_part, at));
        match &borrows.refused {
            Some(r) => self.refused.file(form_name(r, a), || {
                let mut x = format_in!(a, "{}", e.path);
                match r.range() {
                    Some((s, end)) => {
                        format_into!(&mut x, " at {s}..{end}: `");
                        snippet(e.text, s, end, &mut x);
                        x.push('`');
                    }
                    None => x.push_str(" at -"),
                }
                if let Some((s, end)) = borrows.within {
                    format_into!(&mut x, " in the closure or async block at {s}..{end}");
                }
                x.push_str("; ");
                clipped(format_in!(a, "{r:?}").as_str(), DETAIL, &mut x);
                x
            }),
            None => {
                self.whole += 1;
                if borrows.findings.is_empty() {
                    self.accepted += 1;
                }
            }
        }
        if !borrows.findings.is_empty() {
            self.finding_bodies += 1;
        }
        for d in borrows.findings.iter() {
            *self.kinds.entry(d.code).or_insert(0) += 1;
            let mut x = format_in!(a, "{}: {} {}..{} `", e.path, d.code, d.start, d.end);
            snippet(e.text, d.start, d.end, &mut x);
            format_into!(&mut x, "`: {}", d.text);
            self.findings.push(x);
        }
    }
}

/// The census of crate `krate` (named `cname`) over `world`, at most `cap` bodies: each checked
/// by `world.check_borrows`, its verdicts made in a frame reset after it. `frame` is the test's
/// (the cost meter is placed in it), `a` its handle; `start` when the test began.
#[expect(clippy::too_many_arguments, reason = "the run's settings")]
pub fn census<'n>(names: &'n Names<'n>, world: &mut ProjectWorld<'n>, krate: usize, cname: &str, cap: usize, frame: &'static Frame, a: Alloc<'static>, start: Instant) -> Report {
    let entries = list(names, world, krate, a);
    let clock: &'static Clock = frame.alloc(Clock { base: Instant::now() });
    let cost: &'static Cost<'static> = frame.alloc(Cost::new(clock));
    world.set_cost(Some(cost));
    let mut t = Tally {
        no_body: 0,
        not_clean: Groups::new_in(a),
        panics: AVec::new_in(a),
        whole: 0,
        accepted: 0,
        refused: Groups::new_in(a),
        findings: AVec::new_in(a),
        finding_bodies: 0,
        kinds: ABTreeMap::new_in(a),
        spent: Spent::default(),
        checked: 0,
        borrows_ns: 0,
        slowest: AVec::new_in(a),
    };
    let mut body_frame = Frame::with_reserve(BODY_RESERVE, blaze_mem::tag::Tag::Infer).expect("the frame each body is checked in");
    let (mut taken, mut capped) = (0usize, false);
    let started = Instant::now();
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    for (at, e) in entries.iter().enumerate() {
        if at == cap {
            capped = true;
            break;
        }
        taken += 1;
        {
            let erased = Erased::new(&body_frame);
            let alloc = erased.handle();
            let before = cost.spent();
            let t0 = Instant::now();
            let got = catch_unwind(AssertUnwindSafe(|| world.check_borrows(e.body, alloc)));
            let took = t0.elapsed();
            let spent = cost.spent().since(&before);
            match got {
                Ok((types, borrows)) => t.file(e, at, &types, &borrows, took, &spent, a),
                Err(_) => {
                    // A panic is no verdict: the body counts as refused (never as whole), named.
                    t.checked += 1;
                    t.spent.add(&spent);
                    t.panics.push(format_in!(a, "{}", e.path));
                    t.refused.file(format_in!(a, "Panic (the engine's check panicked)"), || format_in!(a, "{}", e.path));
                }
            }
        }
        body_frame.reset();
    }
    std::panic::set_hook(prev_hook);
    let checking = started.elapsed();
    world.set_cost(None);

    let refused = t.refused.total();
    let clean = t.whole + refused;
    let mut out = AString::new_in(a);
    format_into!(
        &mut out,
        "{cname}: clean bodies {clean}; borrow-checked whole {}; refused {refused}; accepted {}; findings {} (in {} bodies)\n",
        t.whole,
        t.accepted,
        t.findings.len(),
        t.finding_bodies
    );
    let build = if cfg!(debug_assertions) { "a debug build" } else { "a release build" };
    format_into!(
        &mut out,
        "bodies listed {} (taken {taken}, cap {cap}); no body {}; not clean in the type checker {}; panics {}; {build}; {:?} in all ({checking:?} checking)\n",
        entries.len(),
        t.no_body,
        t.not_clean.total(),
        t.panics.len(),
        start.elapsed()
    );
    out.push_str("path: the engine's own, `ProjectWorld::check_borrows` (front2/borrows.rs): types, facts from the heads, the body's flow and each closure's and async block's flow; no body sources set (a body's `env!`, include and proc macro calls are refused by name)\n");
    let per = |ns: u64, n: usize| Duration::from_nanos(if n == 0 { 0 } else { ns / n as u64 });
    format_into!(&mut out, "time per body checked ({} bodies, the cost meter's parts):", t.checked);
    let mut parts_ns = 0u64;
    for (i, p) in PARTS.iter().enumerate() {
        parts_ns += t.spent.ns[*p as usize];
        format_into!(&mut out, "{} {} {:?}", if i == 0 { "" } else { "," }, p.name(), per(t.spent.ns[*p as usize], t.checked));
    }
    let wall = u64::try_from(checking.as_nanos()).unwrap_or(u64::MAX);
    format_into!(&mut out, "; outside every part {:?}\n", per(wall.saturating_sub(parts_ns), t.checked));
    format_into!(
        &mut out,
        "time per clean body in the borrow check (facts, every flow lowered, moves and loans checked): {:?}; not split into flow lowering, move check, tier one and tier two, and which tier accepted not said (blaze-flow `check` runs them in one call)\n",
        per(t.borrows_ns, clean)
    );
    t.not_clean.print("NOT CLEAN in the type checker by why", &mut out);
    t.refused.print("REFUSED by form", &mut out);
    format_into!(&mut out, "FINDINGS by kind: {:?}\n", t.kinds);
    for f in t.findings.iter() {
        format_into!(&mut out, "  {f}\n");
    }
    t.slowest.sort_unstable_by(|x, y| y.0.cmp(&x.0).then_with(|| x.2.cmp(&y.2)));
    out.push_str("slowest clean bodies (types + borrows; the borrows part beside):\n");
    for &(took, borrows_ns, at) in t.slowest.iter().take(SLOWEST) {
        format_into!(&mut out, "  {took:?} (borrows {:?}) {}\n", Duration::from_nanos(borrows_ns), entries[at].path);
    }
    if !t.panics.is_empty() {
        out.push_str("panicked bodies:\n");
        for p in t.panics.iter() {
            format_into!(&mut out, "  {p}\n");
        }
    }
    let gate = Gate { bodies: clean, whole: t.whole, refused, findings: t.findings.len(), capped };
    format_into!(&mut out, "gate: bodies {}, whole {}, refused {}, false findings {}\n", gate.bodies, gate.whole, gate.refused, gate.findings);
    Report { text: out, gate }
}

/// Prints the report, writes it to `out_path` when one is named, then applies the gate: with
/// `BORROWS_ARE_ERRORS` true, any refusal or finding fails the test; while it is false, a note.
pub fn finish(report: &Report, out_path: Option<&str>) {
    print!("{}", report.text);
    if let Some(p) = out_path {
        std::fs::write(p, report.text.as_bytes()).unwrap_or_else(|e| panic!("writing {p}: {e}"));
    }
    let on = borrows_are_errors().unwrap_or_else(|e| panic!("{e}"));
    let g = report.gate;
    let dirty = g.refused + g.findings;
    let capped = if g.capped { " (a capped run: the bodies after the cap were not checked)" } else { "" };
    if on {
        assert!(dirty == 0, "gate: BORROWS_ARE_ERRORS is true (crates/blaze-cli/src/body_types.rs) and the census is dirty: {} refused + {} false findings", g.refused, g.findings);
        println!("gate: BORROWS_ARE_ERRORS is true and the census is clean{capped}");
    } else {
        println!("gate note: BORROWS_ARE_ERRORS is false (crates/blaze-cli/src/body_types.rs), so the census passes; with it true this run would {} (refused + false findings = {dirty}){capped}", if dirty == 0 { "pass" } else { "fail" });
    }
}
