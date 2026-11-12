//! The parts rule against a model, in process: a crate of functions and impls, units above
//! it that call, select and instantiate, and edits of the crate. The model says what each
//! unit's result is; the oracle holds the check of its recorded parts to it.

use blaze_ids::{ContentId, Hasher};
use blaze_items::{ItemKey, kinds};
use hir_ty::next_solver::goal_store::{FactObject, FactRef};
use hegel::TestCase;
use hegel::generators as gs;

use super::{Cover, NARROW_PLANTED, Part, Parts, Quick, Read, Refused, Verdict};

const MODEL: &str = "blaze test parts model 1";

fn id(what: &str, of: &[u64]) -> ContentId {
    let mut hasher = Hasher::new(MODEL);
    hasher.part(what.as_bytes());
    for it in of {
        hasher.update(&it.to_le_bytes());
    }
    hasher.finish()
}

/// A function of the crate below: which name it has, its signature, its body, how many
/// lines it takes, and whether a unit that uses it compiles a copy.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Function {
    name: u8,
    signature: u8,
    body: u8,
    lines: u8,
    generic: bool,
}

/// The crate below: its functions in the order they are written, and its impls (a trait, a
/// type, which impl).
#[derive(Clone, PartialEq, Eq, Debug, Default)]
struct Below {
    functions: Vec<Function>,
    impls: Vec<(u8, u8, u8)>,
}

impl Below {
    fn krate() -> ContentId {
        id("crate", &[])
    }

    fn item(name: u8) -> ItemKey {
        ItemKey(id("item", &[u64::from(name)]))
    }

    fn named(&self, name: u8) -> Option<&Function> {
        self.functions.iter().find(|it| it.name == name)
    }

    /// The line `name`'s function starts on.
    fn line(&self, name: u8) -> u64 {
        self.functions.iter().take_while(|it| it.name != name).map(|it| u64::from(it.lines)).sum()
    }

    /// Everything a front end above reads: names, signatures, impls. No body, no line.
    fn surface(&self) -> ContentId {
        let mut of = Vec::new();
        for it in &self.functions {
            of.extend([u64::from(it.name), u64::from(it.signature), u64::from(it.generic)]);
        }
        of.push(u64::MAX);
        for &(trait_, ty, which) in &self.impls {
            of.extend([u64::from(trait_), u64::from(ty), u64::from(which)]);
        }
        id("surface", &of)
    }

    /// Everything of it.
    fn code(&self) -> ContentId {
        let mut of = Vec::new();
        for it in &self.functions {
            of.extend([u64::from(it.body), u64::from(it.lines)]);
        }
        let mut hasher = Hasher::new(MODEL);
        hasher.id(&self.surface()).id(&id("code", &of));
        hasher.finish()
    }

    fn quick(&self, cover: Cover) -> ContentId {
        match cover {
            Cover::Surface => self.surface(),
            Cover::Code => self.code(),
        }
    }

    /// A part's value now.
    fn value(&self, part: &Part) -> Result<ContentId, String> {
        let of_item = |key: &ItemKey| {
            self.functions
                .iter()
                .find(|it| Below::item(it.name) == *key)
                .ok_or_else(|| format!("no item {key:?}"))
        };
        let fact = match part {
            Part::Spans(key) => {
                let function = of_item(key)?;
                return Ok(id("spans", &[self.line(function.name), u64::from(function.lines)]));
            }
            Part::Fact(fact) => fact,
        };
        let item = ItemKey(fact.subject);
        Ok(match (fact.kind, fact.object) {
            (kinds::RESOLVES, FactObject::Id(name)) => {
                let found = self.functions.iter().find(|it| Below::name(it.name) == name);
                id("resolves", &[found.map_or(u64::MAX, |it| u64::from(it.name))])
            }
            (kinds::SIGNATURE, FactObject::None) => {
                id("signature", &[u64::from(of_item(&item)?.signature)])
            }
            (kinds::CODE, FactObject::None) => id("code of", &[u64::from(of_item(&item)?.body)]),
            (kinds::IMPLEMENTS, FactObject::Id(self_ty)) => {
                let mut which: Vec<u64> = self
                    .impls
                    .iter()
                    .filter(|it| Below::trait_(it.0) == fact.subject && Below::ty(it.1) == self_ty)
                    .map(|it| u64::from(it.2))
                    .collect();
                which.sort_unstable();
                id("implements", &which)
            }
            _ => return Err(format!("the model has no {fact:?}")),
        })
    }

    fn name(name: u8) -> ContentId {
        id("name", &[u64::from(name)])
    }

    fn trait_(trait_: u8) -> ContentId {
        id("trait", &[u64::from(trait_)])
    }

    fn ty(ty: u8) -> ContentId {
        id("type", &[u64::from(ty)])
    }

    /// What `name` is in the crate's root module: found or not, one fact.
    fn resolves(name: u8) -> Part {
        Part::Fact(FactRef {
            kind: kinds::RESOLVES,
            subject: id("root module", &[]),
            object: FactObject::Id(Below::name(name)),
        })
    }

    fn signature(name: u8) -> Part {
        Part::Fact(FactRef {
            kind: kinds::SIGNATURE,
            subject: Below::item(name).0,
            object: FactObject::None,
        })
    }

    fn code_of(name: u8) -> Part {
        Part::Fact(FactRef { kind: kinds::CODE, subject: Below::item(name).0, object: FactObject::None })
    }

    /// Which impls `trait_` has for `ty`: none is an answer.
    fn implements(trait_: u8, ty: u8) -> Part {
        Part::Fact(FactRef {
            kind: kinds::IMPLEMENTS,
            subject: Below::trait_(trait_),
            object: FactObject::Id(Below::ty(ty)),
        })
    }
}

/// A unit above: the names it calls (a name the crate below does not have is its own
/// function of that name, as under a glob import), the impls it selects through, and the
/// names it compiles copies of where they are generic.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
struct Unit {
    calls: Vec<u8>,
    selects: Vec<(u8, u8)>,
}

/// A bug a test plants in the unit's trace.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Plant {
    None,
    /// A read that found nothing is not recorded: a name the crate lacks, a trait with no
    /// impl for a type.
    NoNegative,
    /// The crate's quick digest is of its surface whatever was read of it.
    NarrowQuick,
}

impl Unit {
    /// The unit made over `below`: what it makes (the model's stand-in for its record), and
    /// what it read.
    fn made(&self, below: &Below, plant: Plant) -> (Vec<u64>, Result<Parts, Refused>) {
        let krate = Below::krate();
        let mut out = Vec::new();
        let mut reads = Vec::new();
        let mut read = |part: Part| {
            let value = below.value(&part).expect("a part the model just read has a value");
            reads.push(Read { krate, part, value });
        };
        for &name in &self.calls {
            let resolves = Below::resolves(name);
            match below.named(name) {
                None => {
                    if plant != Plant::NoNegative {
                        read(resolves);
                    }
                    out.extend([0, u64::from(name)]);
                }
                Some(function) => {
                    read(resolves);
                    read(Below::signature(name));
                    out.extend([1, u64::from(function.signature)]);
                    if function.generic {
                        read(Below::code_of(name));
                        read(Part::Spans(Below::item(name)));
                        out.extend([
                            u64::from(function.body),
                            below.line(name),
                            u64::from(function.lines),
                        ]);
                    }
                }
            }
        }
        for &(trait_, ty) in &self.selects {
            let chosen =
                below.impls.iter().filter(|it| it.0 == trait_ && it.1 == ty).map(|it| it.2).max();
            if chosen.is_some() || plant != Plant::NoNegative {
                read(Below::implements(trait_, ty));
            }
            out.extend([2, chosen.map_or(u64::MAX, u64::from)]);
        }
        let cover = match reads.iter().all(|it| it.part.in_surface()) || plant == Plant::NarrowQuick {
            true => Cover::Surface,
            false => Cover::Code,
        };
        let quick = vec![Quick { krate, cover, digest: below.quick(cover) }];
        NARROW_PLANTED.with(|it| it.set(plant == Plant::NarrowQuick));
        let parts = Parts::new(quick, reads);
        NARROW_PLANTED.with(|it| it.set(false));
        (out, parts)
    }
}

/// One edit of the crate below.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Edit {
    Body(u8),
    Line(u8),
    Signature(u8),
    Add { name: u8, generic: bool },
    Remove(u8),
    Impl { trait_: u8, ty: u8, which: u8 },
}

fn apply(below: &mut Below, edit: Edit) {
    let at = |at: u8, below: &Below| match below.functions.len() {
        0 => None,
        len => Some(usize::from(at) % len),
    };
    match edit {
        Edit::Body(it) => {
            if let Some(at) = at(it, below) {
                below.functions[at].body = below.functions[at].body.wrapping_add(1);
            }
        }
        Edit::Line(it) => {
            if let Some(at) = at(it, below) {
                below.functions[at].lines = below.functions[at].lines.wrapping_add(1);
            }
        }
        Edit::Signature(it) => {
            if let Some(at) = at(it, below) {
                below.functions[at].signature = below.functions[at].signature.wrapping_add(1);
            }
        }
        Edit::Add { name, generic } => {
            if below.named(name).is_none() {
                below.functions.push(Function { name, signature: 0, body: 0, lines: 3, generic });
            }
        }
        Edit::Remove(it) => {
            if let Some(at) = at(it, below) {
                below.functions.remove(at);
            }
        }
        Edit::Impl { trait_, ty, which } => {
            if !below.impls.contains(&(trait_, ty, which)) {
                below.impls.push((trait_, ty, which));
            }
        }
    }
}

/// The check of `parts`, recorded over the crate as it was, against the crate as it is.
fn checked(parts: &Parts, now: &Below) -> Verdict {
    parts.check(|quick| Some(now.quick(quick.cover)), |read| now.value(&read.part))
}

/// The oracle. The invariant: a unit whose result over the crate as it is differs from its
/// result over the crate as it was has a part that moved; its record is never found stale.
/// And nothing is worked out for a crate whose quick digest stands.
fn hold(unit: &Unit, was: &Below, now: &Below, plant: Plant) -> Result<Verdict, String> {
    let (made_was, parts) = unit.made(was, plant);
    let parts = parts.map_err(|why| format!("the unit's reads are refused: {why}"))?;
    let (made_now, _) = unit.made(now, Plant::None);
    let verdict = checked(&parts, now);
    let unmoved = matches!(verdict, Verdict::Unmoved { .. });
    if made_was != made_now && unmoved {
        return Err(format!(
            "stale: the unit makes {made_now:?} now and made {made_was:?}, and no part it read moved\n{unit:?}\n{was:?}\n{now:?}"
        ));
    }
    if let Verdict::Unmoved { worked_out } = verdict
        && worked_out != 0
        && was.code() == now.code()
    {
        return Err(format!("{worked_out} parts worked out for a crate that did not move"));
    }
    // The record round-trips, and is found under the same key exactly while its parts stand.
    let again = Parts::decoded(&parts.encoded()?).map_err(|why| why.to_string())?;
    if again != parts {
        return Err("the record read back is another".to_owned());
    }
    let base = id("base", &[]);
    let (_, parts_now) = unit.made(now, Plant::None);
    let parts_now = parts_now.map_err(|why| why.to_string())?;
    if unmoved && parts.reads() == parts_now.reads() && parts.filed(&base) != parts_now.filed(&base) {
        return Err("equal parts are filed under two keys".to_owned());
    }
    Ok(verdict)
}

fn drawn_below(tc: &TestCase) -> Below {
    let functions: Vec<(u8, u8, bool)> = tc.draw(
        gs::vecs(gs::tuples!(gs::integers().max_value(5), gs::integers().max_value(3), gs::booleans()))
            .max_size(5),
    );
    let mut below = Below::default();
    for (name, signature, generic) in functions {
        if below.named(name).is_none() {
            below.functions.push(Function { name, signature, body: 0, lines: 3, generic });
        }
    }
    let impls: Vec<(u8, u8, u8)> = tc.draw(
        gs::vecs(gs::tuples!(
            gs::integers().max_value(1),
            gs::integers().max_value(1),
            gs::integers().max_value(2)
        ))
        .max_size(3),
    );
    for it in impls {
        if !below.impls.contains(&it) {
            below.impls.push(it);
        }
    }
    below
}

fn drawn_unit(tc: &TestCase) -> Unit {
    Unit {
        calls: tc.draw(gs::vecs(gs::integers().max_value(5)).max_size(4)),
        selects: tc.draw(
            gs::vecs(gs::tuples!(gs::integers().max_value(1), gs::integers().max_value(1)))
                .max_size(2),
        ),
    }
}

fn drawn_edit(tc: &TestCase) -> Edit {
    let kind: u8 = tc.draw(gs::integers().max_value(5));
    let at: u8 = tc.draw(gs::integers().max_value(5));
    let more: u8 = tc.draw(gs::integers().max_value(2));
    match kind {
        0 => Edit::Body(at),
        1 => Edit::Line(at),
        2 => Edit::Signature(at),
        3 => Edit::Add { name: at, generic: more == 0 },
        4 => Edit::Remove(at),
        _ => Edit::Impl { trait_: at % 2, ty: more % 2, which: more },
    }
}

/// The invariant: over any crate, any units above it and any sequence of edits, no unit's
/// record is found after an edit that changes what the unit makes (`hold`), each unit
/// recorded again after every edit as a build does.
#[hegel::test]
fn no_unit_is_found_stale_after_any_edit_of_the_crate_below(tc: TestCase) {
    let mut below = drawn_below(&tc);
    let units: Vec<Unit> = (0..3).map(|_| drawn_unit(&tc)).collect();
    let edits: usize = tc.draw(gs::integers().max_value(6));
    for _ in 0..edits {
        let was = below.clone(); // clone: the crate before the edit
        apply(&mut below, drawn_edit(&tc));
        for unit in &units {
            if let Err(found) = hold(unit, &was, &below, Plant::None) {
                panic!("{found}");
            }
        }
    }
}

fn below() -> Below {
    let function = |name, generic| Function { name, signature: 0, body: 0, lines: 3, generic };
    Below {
        functions: vec![function(0, false), function(1, false), function(2, true)],
        impls: vec![(0, 0, 0)],
    }
}

fn after(edit: Edit) -> (Below, Below) {
    let was = below();
    let mut now = was.clone(); // clone: the crate before the edit
    apply(&mut now, edit);
    (was, now)
}

fn unmoved(unit: &Unit, edit: Edit) -> bool {
    let (was, now) = after(edit);
    matches!(hold(unit, &was, &now, Plant::None), Ok(Verdict::Unmoved { .. }))
}

/// A line added to a body, and a body's edit, move no unit that only calls: nothing is even
/// worked out, the crate's surface being as it was. A unit that compiles a copy of a generic
/// below the line reads its place, and is made again; one that copies a generic above it is
/// not.
#[test]
fn a_line_or_a_body_below_moves_only_the_units_that_hold_a_copy() {
    let calls = Unit { calls: vec![0, 1], selects: vec![(0, 0)] };
    for edit in [Edit::Line(0), Edit::Body(0), Edit::Body(1), Edit::Body(2), Edit::Line(2)] {
        let (was, now) = after(edit);
        assert_eq!(
            hold(&calls, &was, &now, Plant::None),
            Ok(Verdict::Unmoved { worked_out: 0 }),
            "{edit:?}"
        );
    }
    let copies = Unit { calls: vec![2], selects: Vec::new() };
    assert!(!unmoved(&copies, Edit::Line(0)), "the copy's place moved");
    assert!(!unmoved(&copies, Edit::Body(2)), "the copy's body");
    assert!(unmoved(&copies, Edit::Body(0)), "another function's body");
    let above = Below {
        functions: vec![
            Function { name: 2, signature: 0, body: 0, lines: 3, generic: true },
            Function { name: 0, signature: 0, body: 0, lines: 3, generic: false },
        ],
        impls: Vec::new(),
    };
    let mut lower = above.clone(); // clone: the crate before the edit
    apply(&mut lower, Edit::Line(1));
    assert!(matches!(
        hold(&copies, &above, &lower, Plant::None),
        Ok(Verdict::Unmoved { worked_out: 4 })
    ));
}

/// A public function's signature changed moves exactly the units that call it: of three
/// units, the one that does, and neither the one that calls another nor the one that calls
/// none.
#[test]
fn a_changed_signature_moves_exactly_the_units_that_read_it() {
    let reads = Unit { calls: vec![0], selects: Vec::new() };
    let other = Unit { calls: vec![1], selects: Vec::new() };
    let none = Unit { calls: Vec::new(), selects: vec![(0, 0)] };
    let edit = Edit::Signature(0);
    assert!(!unmoved(&reads, edit));
    assert!(unmoved(&other, edit));
    assert!(unmoved(&none, edit));
    let (was, now) = after(edit);
    assert!(matches!(
        hold(&reads, &was, &now, Plant::None),
        Ok(Verdict::Moved { read: Read { part, .. }, .. }) if part == Below::signature(0)
    ));
}

/// A read that found nothing is a part: a function added under a name a unit resolved to
/// its own, and an impl added where a unit found none or another, move exactly the units
/// that asked.
#[test]
fn an_added_item_or_impl_moves_the_units_that_asked_and_found_otherwise() {
    let falls_back = Unit { calls: vec![4], selects: Vec::new() };
    let other = Unit { calls: vec![0], selects: Vec::new() };
    let shadow = Edit::Add { name: 4, generic: false };
    assert!(!unmoved(&falls_back, shadow), "the name now resolves below");
    assert!(unmoved(&other, shadow));
    let selects_none = Unit { calls: Vec::new(), selects: vec![(1, 1)] };
    let selects_one = Unit { calls: Vec::new(), selects: vec![(0, 0)] };
    let added = Edit::Impl { trait_: 1, ty: 1, which: 0 };
    assert!(!unmoved(&selects_none, added), "an impl where there was none");
    assert!(unmoved(&selects_one, added));
    assert!(!unmoved(&selects_one, Edit::Impl { trait_: 0, ty: 0, which: 2 }), "another impl wins");
}

/// The control of the negative reads (test-the-cause): with a trace that leaves out the
/// reads that found nothing, the oracle finds the stale unit, for the name and for the impl.
#[test]
fn the_oracle_catches_a_trace_without_its_negative_reads() {
    let falls_back = Unit { calls: vec![4], selects: Vec::new() };
    let (was, now) = after(Edit::Add { name: 4, generic: false });
    assert!(hold(&falls_back, &was, &now, Plant::None).is_ok());
    let found = hold(&falls_back, &was, &now, Plant::NoNegative).unwrap_err();
    assert!(found.starts_with("stale: "), "{found}");
    let selects_none = Unit { calls: Vec::new(), selects: vec![(1, 1)] };
    let (was, now) = after(Edit::Impl { trait_: 1, ty: 1, which: 0 });
    assert!(hold(&selects_none, &was, &now, Plant::None).is_ok());
    let found = hold(&selects_none, &was, &now, Plant::NoNegative).unwrap_err();
    assert!(found.starts_with("stale: "), "{found}");
}

/// The control of the quick check: a crate's digest of its surface taken as covering a body
/// and a place is refused when the record is made, and, with the refusal planted away, the
/// oracle finds the unit stale after the body it copied changed.
#[test]
fn the_quick_digest_covers_every_part_or_the_record_is_refused() {
    let krate = Below::krate();
    let below = below();
    let part = Below::code_of(2);
    let read = Read { krate, part, value: below.value(&part).unwrap() };
    let quick = Quick { krate, cover: Cover::Surface, digest: below.surface() };
    assert_eq!(Parts::new(vec![quick], vec![read]), Err(Refused::QuickTooNarrow(part)));
    let copies = Unit { calls: vec![2], selects: Vec::new() };
    let (was, now) = after(Edit::Body(2));
    assert!(hold(&copies, &was, &now, Plant::None).is_ok());
    let found = hold(&copies, &was, &now, Plant::NarrowQuick).unwrap_err();
    assert!(found.starts_with("stale: "), "{found}");
}

/// What a record is refused for, each by name: one part with two values, a part of a crate
/// with no quick digest, and bytes of another layout or order, never read as an empty record.
#[test]
fn a_record_that_is_not_one_is_refused_by_name() {
    let krate = Below::krate();
    let below = below();
    let part = Below::signature(0);
    let value = below.value(&part).unwrap();
    let quick = Quick { krate, cover: Cover::Surface, digest: below.surface() };
    let read = Read { krate, part, value };
    let twice = Read { value: id("another", &[]), ..read };
    assert_eq!(Parts::new(vec![quick], vec![read, twice]), Err(Refused::TwoValues(part)));
    assert_eq!(Parts::new(Vec::new(), vec![read]), Err(Refused::NoQuick(krate)));
    assert_eq!(Parts::new(vec![quick, quick], vec![read]), Err(Refused::NoQuick(krate)));
    let made = Parts::new(vec![quick], vec![read, read]).unwrap();
    assert_eq!(made.reads(), &[read]);
    assert_eq!(Parts::decoded(&made.encoded().unwrap()), Ok(made));
    assert!(matches!(Parts::decoded(&[]), Err(Refused::Bytes(_))));
    // The layout before this one had no record: bytes of any other layout are refused.
    let other = blaze_protocol::cbor::to_vec(&(0u32, Vec::<Quick>::new(), Vec::<Read>::new())).unwrap();
    assert!(matches!(Parts::decoded(&other), Err(Refused::Bytes(_))));
}

/// A part's bytes read back as the part, for a fact with each kind of object and for an
/// item's spans, and a row no part writes is refused: a part's record means one thing.
#[test]
fn a_part_reads_back_from_its_bytes_and_no_other_row_reads() {
    let field = Part::Fact(FactRef {
        kind: kinds::FIELD,
        subject: id("a type", &[]),
        object: FactObject::Field(3),
    });
    for part in [Below::signature(1), Below::resolves(2), field, Part::Spans(Below::item(4))] {
        let bytes = blaze_protocol::cbor::to_vec(&part).unwrap();
        let read: Part = blaze_protocol::cbor::from_slice(&bytes).unwrap();
        assert_eq!(read, part);
    }
    let row = |part: u8, kind: u16, tag: u8, field: u32| {
        let none = ContentId::default();
        blaze_protocol::cbor::to_vec(&(part, kind, none, tag, field, none)).unwrap()
    };
    // The same fields as a struct's map would hold are not what a tuple writes: whichever
    // the encoding, a row with a field set under "no object", or spans with a kind, must not
    // read as a part.
    for bytes in [row(0, kinds::SIGNATURE.0, 0, 7), row(1, kinds::SIGNATURE.0, 0, 0), row(9, 0, 0, 0)] {
        assert!(blaze_protocol::cbor::from_slice::<Part>(&bytes).is_err());
    }
}
