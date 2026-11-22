//! The read list's answer digests ([`crate::asked`], version 2) are canonical: each answer's
//! fields are fed to the hasher in a fixed order with a tag byte per choice, never its `Debug`
//! text. Two answers that differ in one field, or in their variant, have different digests, for
//! every answer type and every variant of it (a variant or field the digest forgot to feed makes
//! two of these equal); a set held in another order or with a repeat digests the same.

use blaze_id::{ContentId, CrateItemKey, ItemKey};
use blaze_own::Alloc;
use blaze_surface::{
    AmbiguityKind, Binding, BindingState, CallShape, Def, DefKind, Edition, MacroShapes, Origin, ProcKind, Visibility,
};

use crate::asked::{CrateFacts, facts_digest, lookup_digest, macro_digest, name_digest, shadow_digest};
use crate::read::NameAnswer;
use crate::scope_record::Reaches;
use crate::task::{MacroBody, MacroSrc};

fn k(n: u8) -> ItemKey {
    ItemKey(ContentId([n; 32]))
}

fn ck(n: u8) -> CrateItemKey {
    CrateItemKey(ContentId([n; 32]))
}

fn attr_shapes() -> MacroShapes {
    MacroShapes::of(CallShape::Attr)
}

/// A `Found` answer with one field replaced by `f`.
fn found(f: impl FnOnce(&mut Def, &mut Visibility, &mut BindingState, &mut Origin)) -> NameAnswer {
    let (mut def, mut vis, mut state, mut origin) = (Def::module(k(1)), Visibility::Public, BindingState::Determined, Origin::Declared);
    f(&mut def, &mut vis, &mut state, &mut origin);
    NameAnswer::Found { def, vis, state, origin }
}

fn binding(f: impl FnOnce(&mut Binding)) -> Option<Binding> {
    let mut b = Binding { def: Def::module(k(1)), vis: Visibility::Public, origin: Origin::Declared, expansion: 0, state: BindingState::Determined };
    f(&mut b);
    Some(b)
}

fn mac(f: impl FnOnce(&mut MacroSrc<'static>)) -> Option<MacroSrc<'static>> {
    let mut m = MacroSrc { name: "m", body: MacroBody::Rules("a"), module: k(1), export: false, local_inner: false };
    f(&mut m);
    Some(m)
}

fn facts(f: impl FnOnce(&mut CrateFacts<'static>)) -> CrateFacts<'static> {
    let mut c = CrateFacts { key: ck(1), root: k(1), name: "c", edition: Edition::E2021, proc_key: None, reaches: Reaches::Keys(&[]) };
    f(&mut c);
    c
}

/// 54 answers, pairwise distinct; prints (no `std` line added, so libtest's own line):
/// `test tests::asked_digest::answer_digests_differ_in_every_field_and_every_variant ... ok`.
#[test]
fn answer_digests_differ_in_every_field_and_every_variant() {
    let g = Alloc::global();
    let base_reaches: &'static [CrateItemKey] = &[CrateItemKey(ContentId([2; 32])), CrateItemKey(ContentId([3; 32]))];
    let digests: [ContentId; 54] = [
        // NameAnswer: both variants, and each field of `Found` (each choice of each enum field).
        name_digest(&NameAnswer::Absent),
        name_digest(&found(|_, _, _, _| {})),
        name_digest(&found(|d, _, _, _| d.key = k(2))),
        name_digest(&found(|d, _, _, _| d.kind = DefKind::Enum)),
        name_digest(&found(|d, _, _, _| d.kind = DefKind::Trait)),
        name_digest(&found(|d, _, _, _| d.kind = DefKind::Macro(MacroShapes::FN_LIKE))),
        name_digest(&found(|d, _, _, _| d.kind = DefKind::Macro(attr_shapes()))),
        name_digest(&found(|d, _, _, _| d.kind = DefKind::Other)),
        name_digest(&found(|_, v, _, _| *v = Visibility::Crate(ck(1)))),
        name_digest(&found(|_, v, _, _| *v = Visibility::Crate(ck(2)))),
        name_digest(&found(|_, v, _, _| *v = Visibility::Restricted(k(1)))),
        name_digest(&found(|_, _, s, _| *s = BindingState::Ambiguous(AmbiguityKind::GlobVsGlob))),
        name_digest(&found(|_, _, s, _| *s = BindingState::Ambiguous(AmbiguityKind::GlobVsExpanded))),
        name_digest(&found(|_, _, _, o| *o = Origin::Import)),
        // A lookup's `Option<Binding>`: none, and each field of the binding.
        lookup_digest(None),
        lookup_digest(binding(|_| {})),
        lookup_digest(binding(|b| b.def.key = k(2))),
        lookup_digest(binding(|b| b.def.kind = DefKind::Enum)),
        lookup_digest(binding(|b| b.vis = Visibility::Crate(ck(1)))),
        lookup_digest(binding(|b| b.vis = Visibility::Restricted(k(1)))),
        lookup_digest(binding(|b| b.origin = Origin::Glob)),
        lookup_digest(binding(|b| b.expansion = 1)),
        lookup_digest(binding(|b| b.expansion = 256)),
        lookup_digest(binding(|b| b.state = BindingState::Ambiguous(AmbiguityKind::GlobVsGlob))),
        // A macro's source: none, each field, each body variant and each field of it.
        macro_digest(None),
        macro_digest(mac(|_| {})),
        macro_digest(mac(|m| m.name = "n")),
        macro_digest(mac(|m| m.body = MacroBody::Rules("b"))),
        macro_digest(mac(|m| m.body = MacroBody::Macro2 { params: None, body: "a" })),
        macro_digest(mac(|m| m.body = MacroBody::Macro2 { params: Some(""), body: "a" })),
        macro_digest(mac(|m| m.body = MacroBody::Macro2 { params: Some("a"), body: "" })),
        macro_digest(mac(|m| m.body = MacroBody::BuiltinFn("a"))),
        macro_digest(mac(|m| m.body = MacroBody::BuiltinOther(CallShape::FnLike))),
        macro_digest(mac(|m| m.body = MacroBody::BuiltinOther(CallShape::Attr))),
        macro_digest(mac(|m| m.body = MacroBody::Proc { kind: ProcKind::FnLike, helpers: "a" })),
        macro_digest(mac(|m| m.body = MacroBody::Proc { kind: ProcKind::Attr, helpers: "a" })),
        macro_digest(mac(|m| m.body = MacroBody::Proc { kind: ProcKind::FnLike, helpers: "h" })),
        macro_digest(mac(|m| m.body = MacroBody::BuiltinDerive { helpers: "" })),
        macro_digest(mac(|m| m.body = MacroBody::BuiltinDerive { helpers: "h" })),
        macro_digest(mac(|m| m.module = k(2))),
        macro_digest(mac(|m| m.export = true)),
        macro_digest(mac(|m| m.local_inner = true)),
        // Whether a glob source may shadow a name.
        shadow_digest(false),
        shadow_digest(true),
        // A handed crate's facts: each field, each choice of the optional proc key, the set.
        facts_digest(facts(|_| {}), g),
        facts_digest(facts(|c| c.key = ck(2)), g),
        facts_digest(facts(|c| c.root = k(2)), g),
        facts_digest(facts(|c| c.name = "d"), g),
        facts_digest(facts(|c| c.edition = Edition::E2024), g),
        facts_digest(facts(|c| c.proc_key = Some(ck(1))), g),
        facts_digest(facts(|c| c.proc_key = Some(ck(2))), g),
        facts_digest(facts(|c| c.reaches = Reaches::Keys(base_reaches)), g),
        facts_digest(facts(|c| c.reaches = Reaches::Keys(&base_reaches[..1])), g),
        facts_digest(facts(|c| c.reaches = Reaches::Keys(&base_reaches[1..])), g),
    ];
    for (i, a) in digests.iter().enumerate() {
        for (j, b) in digests.iter().enumerate().skip(i + 1) {
            assert_ne!(a, b, "answer digests {i} and {j} are equal: a field or variant is not fed");
        }
    }

    // A set held in another order, or with a repeat, digests as the sorted set.
    let shuffled: &'static [CrateItemKey] = &[CrateItemKey(ContentId([3; 32])), CrateItemKey(ContentId([2; 32]))];
    let repeated: &'static [CrateItemKey] =
        &[CrateItemKey(ContentId([3; 32])), CrateItemKey(ContentId([2; 32])), CrateItemKey(ContentId([3; 32]))];
    let sorted = facts_digest(facts(|c| c.reaches = Reaches::Keys(base_reaches)), g);
    assert_eq!(facts_digest(facts(|c| c.reaches = Reaches::Keys(shuffled)), g), sorted, "reaches in another order digests otherwise");
    assert_eq!(facts_digest(facts(|c| c.reaches = Reaches::Keys(repeated)), g), sorted, "reaches with a repeat digests otherwise");
    // The record's form (32-byte words read in place) digests as the working form's keys.
    let words: &'static [[u8; 32]] = &[[3; 32], [2; 32]];
    assert_eq!(facts_digest(facts(|c| c.reaches = Reaches::Words(words)), g), sorted, "reaches read in place digest otherwise");
    // Equal answers built apart digest equal (no address or arena enters the digest).
    assert_eq!(name_digest(&found(|_, _, _, o| *o = Origin::Import)), digests[13]);
    assert_eq!(macro_digest(mac(|m| m.body = MacroBody::Rules(core::str::from_utf8(b"b").unwrap_or("")))), digests[27]);
}
