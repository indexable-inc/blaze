//! The recorder's promises, over a test world that is a table of answers. Its `ask` is the
//! shape every world takes: one `match` with an arm for every question, no wildcard, so a new
//! question does not compile here until the table answers it.

use blaze_own::Alloc;
use blaze_typeck::{ItemKey, Mutbl, Name, PNode, PTy, PortableTys, UintTy};

use crate::{Answer, Fact, FactError, FactKey, FactKind, Facts, Reads, Seal, Term, Unknown, Vocab};

fn heap() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

/// The test world's vocabulary: every back-end type a plain number or bytes.
struct Plain;

impl Vocab for Plain {
    type Repr = u32;
    type ValidRange = u32;
    type Abi = u32;
    type Intrinsic = u32;
    type StaticItem = u32;
    type FnKind = u32;
    type ObjectTrait<'w> = u32;
    type Body<'w> = &'w str;
    type FmtPiece<'w> = u32;
    type CallerSite<'w> = u32;
    type Closure<'w> = u32;
    type Coroutine<'w> = u32;
    type Recipe<'w> = u32;
    type CoroutineState = u32;
    type Data<'w> = &'w [u8];
    type Profile = u32;
}

/// A table of answers: lang items and C-variadic counts; every value not evaluated yet.
struct Table<'t> {
    lang: &'t [(Name, ItemKey)],
    variadic: &'t [(ItemKey, u32)],
}

const NO_ROW: Unknown = Unknown { why: "the test table holds no such row" };

impl Facts for Table<'_> {
    type Vocab = Plain;

    fn ask<'w>(&'w self, _: Seal, fact: Fact<'_>) -> Answer<'w, Plain> {
        match fact {
            Fact::Lang(n) => Answer::Lang(self.lang.iter().find(|r| r.0 == n).map(|r| r.1)),
            Fact::CVariadic(k) => Answer::CVariadic(self.variadic.iter().find(|r| r.0 == k).map(|r| r.1)),
            Fact::Value { .. } => Answer::Missing,
            Fact::Adt(_)
            | Fact::Repr(_)
            | Fact::ValidRange(_)
            | Fact::Discriminant { .. }
            | Fact::DiscriminantBody { .. }
            | Fact::Sig(_)
            | Fact::Abi(_)
            | Fact::Nounwind(_)
            | Fact::LegacyConstGenerics(_)
            | Fact::Intrinsic(_)
            | Fact::Static(_)
            | Fact::FnKind(_)
            | Fact::CodeId(_)
            | Fact::CtorOf(_)
            | Fact::OpaqueOf(_)
            | Fact::ObjectTrait(_)
            | Fact::ItemPath(_)
            | Fact::NameText(_)
            | Fact::ConstBody(_)
            | Fact::LangMember { .. }
            | Fact::Literal { .. }
            | Fact::FormatTemplate { .. }
            | Fact::CallerSite { .. }
            | Fact::Closure { .. }
            | Fact::Coroutine { .. }
            | Fact::CoroutineRecipe { .. }
            | Fact::Normalize { .. }
            | Fact::OpaqueHidden { .. }
            | Fact::DropImpl(_)
            | Fact::Selects { .. }
            | Fact::Resolve(_)
            | Fact::TrackCaller(_)
            | Fact::InstanceKey(_)
            | Fact::KeyedInstance(_)
            | Fact::CoroutineState { .. }
            | Fact::Profile => Answer::Unknown(NO_ROW),
        }
    }
}

/// A world that answers every question as if it were `Nounwind`: the planted slip.
struct Liar;

impl Facts for Liar {
    type Vocab = Plain;

    fn ask<'w>(&'w self, _: Seal, _: Fact<'_>) -> Answer<'w, Plain> {
        Answer::Nounwind(true)
    }
}

const DROP: Name = Name::of("drop");
const EMPTY: Table<'static> = Table { lang: &[], variadic: &[] };

/// The key of `f` as `Reads` records it, read off the `Unknown` the empty table answers.
fn key_of(f: Fact<'_>) -> FactKey {
    let mut r = Reads::new_in(&EMPTY, heap());
    match r.ask(f) {
        Err(FactError::Unknown { fact, .. }) => fact,
        _ => panic!("the empty table answered {:?}", f.kind()),
    }
}

/// The digest of the read list of a run that asks `Lang(DROP)` of `t`.
fn lang_digest(t: &Table<'_>) -> u64 {
    let mut r = Reads::new_in(t, heap());
    assert!(matches!(r.ask(Fact::Lang(DROP)), Ok(Answer::Lang(_))));
    r.finish().digest()
}

/// A question the world has no answer for is the typed `Unknown` naming it, never a default
/// "none", and it is no read. Prints `test tests::an_unanswered_question_is_unknown ... ok`.
#[test]
fn an_unanswered_question_is_unknown() {
    let mut r = Reads::new_in(&EMPTY, heap());
    let got = r.ask(Fact::Abi(ItemKey(7)));
    assert!(
        matches!(got, Err(FactError::Unknown { fact: FactKey { kind: FactKind::Abi, .. }, why }) if why == NO_ROW.why),
        "an unanswered `Abi` was not the typed `Unknown`"
    );
    assert!(r.reads().is_empty(), "an unanswered question entered the read list");
}

/// A record not made yet is a hole, kept once however often it is asked, and no read.
/// Prints `test tests::a_missing_record_is_a_hole_not_a_read ... ok`.
#[test]
fn a_missing_record_is_a_hole_not_a_read() {
    let mut r = Reads::new_in(&EMPTY, heap());
    let value = Fact::Value { of: crate::ConstOf::Item(ItemKey(5)), args: crate::Ground::NONE };
    for _ in 0..2 {
        assert!(matches!(r.ask(value), Err(FactError::Missing { .. })));
    }
    assert_eq!(r.holes().len(), 1, "one hole asked twice is not one hole");
    assert!(r.reads().is_empty());
}

/// Two asks of one question record one read. Prints
/// `test tests::two_asks_of_one_question_record_one_read ... ok`.
#[test]
fn two_asks_of_one_question_record_one_read() {
    let rows = [(DROP, ItemKey(3))];
    let t = Table { lang: &rows, variadic: &[] };
    let mut r = Reads::new_in(&t, heap());
    for _ in 0..2 {
        assert!(matches!(r.ask(Fact::Lang(DROP)), Ok(Answer::Lang(Some(ItemKey(3))))));
    }
    assert_eq!(r.reads().len(), 1, "two asks of one question made two rows");
}

/// The read list's digest changes when an answer read changes, and not when a fact no ask
/// reached changes. Prints `test tests::the_read_digest_follows_only_the_answers_read ... ok`.
#[test]
fn the_read_digest_follows_only_the_answers_read() {
    let drop_3 = [(DROP, ItemKey(3))];
    let drop_4 = [(DROP, ItemKey(4))];
    let one = [(ItemKey(9), 1)];
    let two = [(ItemKey(9), 2)];
    let base = lang_digest(&Table { lang: &drop_3, variadic: &one });
    assert_eq!(base, lang_digest(&Table { lang: &drop_3, variadic: &two }), "a fact no ask reached changed the digest");
    assert_ne!(base, lang_digest(&Table { lang: &drop_4, variadic: &one }), "the answer read changed and the digest did not");
}

/// A question at a ground type is keyed by the term, not by its index: `&u8` at index 1 of one
/// table and at index 2 of another give one key; the control, `&bool`, gives another. Prints
/// `test tests::a_ground_question_is_keyed_by_content ... ok`.
#[test]
fn a_ground_question_is_keyed_by_content() {
    let mut first = PortableTys::new_in(heap());
    let u8_1 = first.intern(PNode::Uint(UintTy::U8)).expect("interned");
    let ref_1 = first.intern(PNode::Ref(Mutbl::Not, u8_1)).expect("interned");
    let mut second = PortableTys::new_in(heap());
    let bool_2 = second.intern(PNode::Bool).expect("interned");
    let u8_2 = second.intern(PNode::Uint(UintTy::U8)).expect("interned");
    let ref_2 = second.intern(PNode::Ref(Mutbl::Not, u8_2)).expect("interned");
    let ref_bool = second.intern(PNode::Ref(Mutbl::Not, bool_2)).expect("interned");
    assert_ne!(ref_1, ref_2, "the test needs the term at two indices");
    let at = |ty: PTy, types| Fact::DropImpl(Term { ty, types });
    assert_eq!(key_of(at(ref_1, first.view())), key_of(at(ref_2, second.view())), "one term at two indices gave two keys");
    assert_ne!(key_of(at(ref_2, second.view())), key_of(at(ref_bool, second.view())), "two terms gave one key");
}

/// An answer to another question is refused at its first ask, never read as this one's.
/// Prints `test tests::an_answer_to_another_question_is_refused ... ok`.
#[test]
fn an_answer_to_another_question_is_refused() {
    let mut r = Reads::new_in(&Liar, heap());
    assert!(matches!(
        r.ask(Fact::Abi(ItemKey(1))),
        Err(FactError::WrongAnswer { asked: FactKind::Abi, got: FactKind::Nounwind })
    ));
    assert!(matches!(r.ask(Fact::Nounwind(ItemKey(1))), Ok(Answer::Nounwind(true))));
    assert_eq!(r.reads().len(), 1);
}
