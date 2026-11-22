//! A BODY'S KEPT CODE, as a store record (run.md, section R, piece R4; RV3 job 4; D119).
//!
//! The Bodies stage leaves, beside each clean body's facts record (engine_body_facts.rs), its
//! checked generic body in blaze-body's flat form with the portable type table it indexes: the
//! bytes blaze-body kept.rs writes (front2 tables/task.rs `keep_checked` makes them). A run
//! builds a called instance from it (`blaze_body_build::build(kept.verified(), kept.tys(), ..)`)
//! and lowers nothing again. This file is the one home of the record's key, its put and its
//! reader (rule 30).
//!
//! THE KEY IS THE CODE'S, NOT THE VERDICT'S. [`key_of`] hashes the record's bytes under its own
//! form, so it covers every input of the value (rule 21: the body's rows, its table, the
//! lowering's facts as they shaped the rows), and two bodies with one clean verdict (`return 1`,
//! `return 2`) are two keys. The verdict cell (`VERDICT_CELL`) is unchanged: it is what the
//! facts record and the row carry; this key is a separate code input beside it.
//!
//! READ ONCE, IN PLACE. [`read`] takes the record where the store's mapping holds it
//! (`Held::place`, as engine_records.rs reads), checks that its bytes are the key's (a record
//! under another key is refused), and opens it with `blaze_body::kept::open`, whose [`Kept`]
//! carries the body's `Verified` proof: nothing reads it unchecked or checks it twice (rule 74).
//! A record put by this command and not committed yet is not in place, and is refused by name
//! (rule 3: never copied behind the reader's back).

use core::alloc::Allocator;
use core::fmt;

use blaze_body::{Kept, KeptError};
use blaze_drive::{Key, Name};
use bstd::cx::Cx;

use crate::CliError;
use crate::store::Store;

/// The form of a kept body's key: raised with `blaze_body::KEPT_VERSION` or the body's
/// `blaze_body::VERSION`, so a record of another form is never found under a new key.
const KEPT_FORM: &str = "blaze-cli engine kept body key 1";

/// The store key of the kept body record `bytes`: their hash under [`KEPT_FORM`], so equal
/// bytes meet under one key (rule 25) and other bytes never do (rule 21).
pub(crate) fn key_of(bytes: &[u8]) -> Key {
    let mut h = blaze_ids::Hasher::new(KEPT_FORM);
    h.update(&(bytes.len() as u64).to_le_bytes()).update(bytes);
    Key(h.finish())
}

/// Why body `body`'s kept record could not be put or read. Each names the body and the key
/// (rule 19) and says which check refused.
#[derive(Debug)]
pub(crate) enum BodyRecordError {
    /// The store refused the put or the get.
    Store { body: Name, key: Key, why: CliError },
    /// No record is at the key the body's facts name: the store lost a record its answer
    /// names, never a cue to lower the body again (rule 3).
    Missing { body: Name, key: Key },
    /// The record is this command's own put, not yet committed, or not aligned for a read in
    /// place.
    NotInPlace { body: Name, key: Key, why: blaze_cas::plain::NotInPlace },
    /// The bytes at `key` are not the bytes `key` names.
    WrongBytes { body: Name, key: Key, found: Key },
    /// The bytes are not a kept body (blaze-body kept.rs names which check).
    Kept { body: Name, key: Key, why: KeptError },
}

impl fmt::Display for BodyRecordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BodyRecordError::Store { body, key, why } => write!(f, "{body:?}: its kept body {key:?} could not be put or read: {why} (rule: engine_body_record.rs)"),
            BodyRecordError::Missing { body, key } => write!(f, "{body:?}: its kept body {key:?} is not in the store (rule: engine_body_record.rs)"),
            BodyRecordError::NotInPlace { body, key, why } => write!(f, "{body:?}: its kept body {key:?} is not read in place: {why} (rule: engine_body_record.rs)"),
            BodyRecordError::WrongBytes { body, key, found } => write!(f, "{body:?}: the record at {key:?} is another body's ({found:?}) (rule 21)"),
            BodyRecordError::Kept { body, key, why } => write!(f, "{body:?}: the record at {key:?} is not a kept body: {why:?} (rule: blaze-body kept.rs)"),
        }
    }
}

/// Puts body `body`'s kept record `bytes` under [`key_of`] and files the key beside `owner`,
/// the body's facts record (engine_check.rs, the Bodies put), so a root that keeps the facts
/// keeps the code (store.rs `index_beside`). Equal bytes already held are left as they are.
/// The key, for the facts record to name.
pub(crate) fn put(cx: &Cx, store: &mut dyn Store<'_>, body: Name, owner: Key, bytes: &[u8]) -> Result<Key, BodyRecordError> {
    let key = key_of(bytes);
    let cx = cx.span("engine: a body's kept code put");
    store.put(&cx, key, bytes).map_err(|why| BodyRecordError::Store { body, key, why })?;
    store.index_beside(&cx, owner, &[key]).map_err(|why| BodyRecordError::Store { body, key, why })?;
    Ok(key)
}

/// Body `body`'s kept record at `key`, read in place and opened: its table decoded into
/// `alloc`, its body verified once ([`Kept::verified`]).
pub(crate) fn read<'m, A: Allocator + Copy>(cx: &Cx, store: &mut dyn Store<'m>, body: Name, key: Key, alloc: A) -> Result<Kept<'m, A>, BodyRecordError> {
    let cx = cx.span("engine: a body's kept code read");
    let held = store.get(&cx, key).map_err(|why| BodyRecordError::Store { body, key, why })?;
    let Some(held) = held else { return Err(BodyRecordError::Missing { body, key }) };
    let place = held.place.map_err(|why| BodyRecordError::NotInPlace { body, key, why })?;
    open(body, key, place.bytes(), alloc)
}

/// The kept body in `bytes`, found at `key`: refused when the bytes are not `key`'s, then
/// opened by blaze-body's one reader.
pub(crate) fn open<'b, A: Allocator + Copy>(body: Name, key: Key, bytes: &'b [u8], alloc: A) -> Result<Kept<'b, A>, BodyRecordError> {
    let found = key_of(bytes);
    if found != key {
        return Err(BodyRecordError::WrongBytes { body, key, found });
    }
    blaze_body::kept::open(bytes, alloc).map_err(|why| BodyRecordError::Kept { body, key, why })
}

// ---- THE TYPED RECORD: a body's checked columns (D163 part 2, D159 "one body record") ------
//
// What a run reads in place of checking a body again: blaze-typeck checked_record.rs's record
// of the body's clean `Checked` (`encode`, `decode`). THE KEY (rule 21, revised 2026-11-20):
// the record's OWN BYTES, hashed under [`TYPED_FORM`] ([`typed_key`]), so two values can never
// meet under one key. The first key, (the check's name, the row's `out`), missed an input: a
// clean body's `out` is its verdict's digest, which does not move when an edit adds an
// expression and leaves the verdict clean (`fn two() -> u8 { 2 }` to `{ let _ = 0u8; 2 }`),
// while the record's expression column does; the second command's put was refused with
// `TwoValues { differs_at: 24 }`, the record's `ExprTy` row count (blaze-cli tests, wave-1119
// t26). How a reader finds the key: the body's run facts record names it (engine_body_facts.rs
// `BodyFacts::typed`), and the facts record is keyed by its bytes (engine_check.rs, the Bodies
// put), so the facts record moves with the typed record and an unchanged one is found by its key.
//
// WHICH BODIES HAVE ONE (D197 root 1): every workspace root body whose types `check` found
// clean, whatever its borrows say (front2 tables/task.rs `keeps_typed`). A body of a kind with
// no record yet (a toolchain's or a dependency's body, a fn declared in a body, a body whose
// stalled goals were decided after its record) is the run's own check, named and counted by
// kind ([`StoreTyped`]'s `source`), never a refusal and never a second check of a body that
// has a record.

/// The form of a typed record's key: raised with blaze-typeck's `CHECKED_RECORD_MAGIC`, so a
/// record of another form is never found under a new key. Form 2: the key of the bytes (form 1
/// keyed (check, out), which missed the record's columns; see THE KEY above).
const TYPED_FORM: &str = "blaze-cli engine typed body key 2";

/// The store key of the typed record `bytes`: their length and every byte hashed under
/// [`TYPED_FORM`], so equal records meet under one key (rule 25) and other bytes never do (rule
/// 21). The put (engine_bodies.rs, the Bodies stage) calls it; a reader takes the key from the
/// body's run facts ([`StoreTyped`]).
pub(crate) fn typed_key(bytes: &[u8]) -> Key {
    let mut h = blaze_ids::Hasher::new(TYPED_FORM);
    h.update(&(bytes.len() as u64).to_le_bytes()).update(bytes);
    Key(h.finish())
}

/// Why the store did not hand a body's typed record over: names the body's check and the key.
#[derive(Debug)]
pub(crate) enum TypedReadError {
    /// The command's store refused the get of `key` (its facts record or its typed record).
    Store { body: Name, key: Key, why: blaze_cas::plain::LentError },
    /// The body's run facts record, which names the typed record, did not read: what refused it.
    Facts { body: Name, what: &'static str },
    /// The facts name a typed record the store does not hold: a record lost, never a cue to
    /// check the body again (rule 3).
    Missing { body: Name, key: Key },
}

impl fmt::Display for TypedReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TypedReadError::Store { body, key, why } => write!(f, "{body:?}: its record {key:?} could not be read: {why} (rule: engine_body_record.rs)"),
            TypedReadError::Facts { body, what } => write!(f, "{body:?}: its run facts record, which names its typed record, could not be read: {what} (engine_body_facts.rs, engine_body_record.rs)"),
            TypedReadError::Missing { body, key } => write!(f, "{body:?}: its run facts name the typed record {key:?}, which the store does not hold (rule 3: engine_body_record.rs)"),
        }
    }
}

impl core::error::Error for TypedReadError {}

/// What the run's typed reader is made of besides the body ids: the parent hands it the ids at
/// the maker's making (run_parent.rs `CommandFront::maker`), when they exist.
#[derive(Clone, Copy)]
pub(crate) struct TypedFrom<'h> {
    pub(crate) cx: &'h Cx,
    /// The command's store, read through its lent handle: this command's own puts (a check in
    /// the same command) and the committed generation alike (blaze-cas plain/lent.rs `get`).
    pub(crate) lent: &'h crate::store::CommandLent,
    /// Where each row's record is: a body's is its run facts record (engine_check.rs
    /// `Standing::records`).
    pub(crate) records: &'h dyn crate::engine_run_answer::Records,
    /// Where each decoded `Checked` lives: the run's (rule 75).
    pub(crate) alloc: blaze_own::Alloc<'static>,
}

impl<'h> TypedFrom<'h> {
    /// The reader over the run's body ids `ids` (a second view of the parent's table, no row
    /// copied: run_ids.rs `RunIds` is `Copy`).
    pub(crate) fn over(self, ids: crate::run_ids::RunIds<'h>) -> StoreTyped<'h> {
        StoreTyped { from: self, ids }
    }
}

/// THE READER of the typed records, kind by kind (blaze-ladder lazy/typed.rs `KeptBodies`), what
/// the run's maker reads through (run_parent.rs `RunMaker`, blaze-ladder make_one.rs `ByKind`):
///
/// - a toolchain's or a dependency's body (`RunIds::grain`) is the run's own check, by kind:
///   `check` checks none of them;
/// - a fn declared in a body (its key's inner bit, typed.rs `is_local`) likewise: its check is
///   its root's, and the root's record is not its own (reading it under the local's key would be
///   two values under one key, rule 21);
/// - a workspace root body: its row (`RunIds::check_of`), the check's run facts record where the
///   answer names it (`Records::record`), the typed record's key the facts hold
///   (engine_body_facts.rs `BodyFacts::typed`), the record from the store, decoded once
///   (blaze-typeck `checked_record::decode`, which refuses bytes of another form by name). Facts
///   that name no typed record are `RunChecked::Unrecorded`, the run's own check; a row, a facts
///   record or a named record that is not there is a typed gap (rule 3: never a check in its
///   place).
///
/// A body local to another as a closure or an async block is never asked: its columns are its
/// owner's (typed.rs, "Body-local items").
pub(crate) struct StoreTyped<'h> {
    from: TypedFrom<'h>,
    ids: crate::run_ids::RunIds<'h>,
}

impl StoreTyped<'_> {
    /// The record at `at` from the store, for body `check`'s read.
    fn get(&self, check: Name, at: Key) -> Result<Option<&[u8]>, TypedReadError> {
        match self.from.lent.get(&blaze_cas::Key(at.0)) {
            Ok(found) => Ok(found.map(|lend| lend.bytes)),
            Err(why) => Err(TypedReadError::Store { body: check, key: at, why }),
        }
    }
}

impl blaze_ladder::front2::program::lazy::typed::KeptBodies for StoreTyped<'_> {
    fn source(&mut self, key: blaze_typeck::ItemKey) -> Result<blaze_ladder::front2::program::lazy::typed::BodySource, blaze_ladder::front2::program::lazy::typed::TypedGap> {
        use blaze_ladder::front2::program::lazy::typed::{BodySource, RunChecked, TypedGap, is_local};
        use crate::run_ids::Grain;
        match self.ids.grain(key) {
            Some(Grain::Toolchain) => return Ok(BodySource::Run(RunChecked::Toolchain)),
            Some(Grain::Dependency) => return Ok(BodySource::Run(RunChecked::Dependency)),
            Some(Grain::Workspace) => {}
            // No crate row: the run gate refused it before any make (run_ids.rs `runnable`).
            None => return Err(TypedGap::NoRecord { key }),
        }
        if is_local(key) {
            return Ok(BodySource::Run(RunChecked::Local));
        }
        let read = |why: TypedReadError| TypedGap::Read { key, why: Box::new(why) };
        let facts_refused = |check: Name, what: &'static str| read(TypedReadError::Facts { body: check, what });
        let Some(check) = self.ids.check_of(key) else { return Err(TypedGap::NoRecord { key }) };
        let Some(facts_at) = self.from.records.record(check).map_err(|what| facts_refused(check, what))? else { return Err(TypedGap::NoRecord { key }) };
        let _cx = self.from.cx.span("engine: a body's typed record read");
        // The facts record: a plain drive record whose payload is the run facts (engine_check.rs,
        // the Bodies put), which name the typed record by its key.
        let Some(facts) = self.get(check, facts_at).map_err(read)? else { return Err(facts_refused(check, "the answer names a facts record the store does not hold")) };
        let whole = match blaze_drive::record::open(facts) {
            Ok(blaze_drive::record::Stored::Whole(whole)) => whole,
            Ok(blaze_drive::record::Stored::Shape(_)) => return Err(facts_refused(check, "its facts record is a shape record, which holds no payload")),
            Err(_) => return Err(facts_refused(check, "its facts record is not a drive record")),
        };
        let facts = crate::engine_run_answer::facts_of(check, whole.payload).map_err(|what| facts_refused(check, what))?;
        let typed = crate::engine_body_facts::read(facts).map_err(|_| facts_refused(check, "its facts payload does not read (engine_body_facts.rs `read`)"))?.typed;
        // `check` ran and kept no record of it: the run's one check, counted.
        let Some(at) = typed else { return Ok(BodySource::Run(RunChecked::Unrecorded)) };
        let Some(bytes) = self.get(check, at).map_err(read)? else { return Err(read(TypedReadError::Missing { body: check, key: at })) };
        blaze_typeck::checked_record::decode(bytes, self.from.alloc).map(BodySource::Read).map_err(|why| TypedGap::Record { key, why })
    }
}

#[cfg(test)]
mod tests {
    use std::alloc::Global;

    use blaze_body::{BlockKind, Body, BodyBuilder, ConstDecl, ConstKind, LocalDecl, LocalKind, Operand, Span, StmtKind, Value};
    use blaze_drive::Name;
    use blaze_typeck::{Mutbl, PNode, PTy, PTys, UintTy};

    use super::{BodyRecordError, key_of, open};

    const TABLE: [PNode; 2] = [PNode::Bool, PNode::Uint(UintTy::U8)];
    const NO_LISTS: [PTy; 0] = [];

    /// `fn f() -> u8 { bits }`, built against [`TABLE`].
    fn returns(bits: u128) -> Body<Global> {
        let mut b = BodyBuilder::new_with_types_in(PTys::flat(&TABLE, &NO_LISTS), Global);
        let u8_ = b.intern_ty(PNode::Uint(UintTy::U8)).expect("the table's u8");
        let ret = b.new_local(LocalDecl { ty: u8_, kind: LocalKind::Return, mutbl: Mutbl::Not });
        let entry = b.new_block(BlockKind::Normal);
        b.start(entry);
        let c = b.new_const(ConstDecl { kind: ConstKind::Scalar { bits }, ty: u8_ });
        let dest = b.local_place(ret);
        b.assign(dest, Value::Use(Operand::Const(c)), Span::default());
        b.terminate(StmtKind::Return, Span::default());
        b.finish().expect("a whole body")
    }

    /// A body task's name, as an error names it.
    fn a_body() -> Name {
        Name::new(blaze_drive::Kind::BodyCheck, blaze_ids::ContentId::of("engine_body_record test", b"f"))
    }

    fn kept(bits: u128) -> blaze_own::Vec<u8, Global> {
        blaze_body::kept::encode(&returns(bits), PTys::flat(&TABLE, &NO_LISTS), Global).expect("kept")
    }

    /// The typed record's key covers every byte of the record (rule 21): two records of one
    /// check whose verdict is equal (a clean body that gained an expression: the second
    /// command's `TwoValues { differs_at: 24 }`, wave-1119 t26) are two keys, one byte apart or
    /// one byte longer; equal bytes are one key (rule 25). The cause, removed: form 1 keyed
    /// (check, out), which gave these two records one key.
    #[test]
    fn a_typed_key_moves_with_every_byte_of_the_record() {
        let one: &[u8] = b"bzchkd01 one record";
        let mut moved = one.to_vec();
        moved[24 % one.len()] ^= 1;
        let mut longer = one.to_vec();
        longer.push(0);
        assert_ne!(super::typed_key(one), super::typed_key(&moved), "two records one byte apart share a key");
        assert_ne!(super::typed_key(one), super::typed_key(&longer), "two records one byte longer share a key");
        assert_eq!(super::typed_key(one), super::typed_key(&one.to_vec()), "control: equal bytes, one key");
        assert_ne!(super::typed_key(one), super::key_of(one), "a typed record and a kept body record of equal bytes share a key: their forms are one");
    }

    /// The record read back at its key is the body put: the same table, the same rows, and the
    /// same bytes when written again (rule 26).
    #[test]
    fn a_kept_body_reads_back_equal() {
        let bytes = kept(1);
        let key = key_of(&bytes);
        let body = a_body();
        let back = open(body, key, &bytes, Global).expect("opens");
        let again = blaze_body::kept::encode_ref(&back, Global).expect("re-kept");
        assert!(again[..] == bytes[..]);
        assert_eq!(back.table(), blaze_body::TableId::of(PTys::flat(&TABLE, &NO_LISTS)));
    }

    /// Planted control (RV3 job 4): two bodies with one clean verdict, `return 1` and
    /// `return 2`, have two keys; the record of one is refused at the other's key.
    #[test]
    fn one_verdict_two_bodies_two_keys() {
        let (one, two) = (kept(1), kept(2));
        assert_ne!(key_of(&one), key_of(&two));
        assert_eq!(key_of(&one), key_of(&kept(1)), "control: one body, one key (rule 25)");
        match open(a_body(), key_of(&one), &two, Global) {
            Err(BodyRecordError::WrongBytes { found, .. }) => assert_eq!(found, key_of(&two)),
            Err(e) => panic!("refused for another reason: {e}"),
            Ok(_) => panic!("one body's record was read as another's"),
        }
    }
}
