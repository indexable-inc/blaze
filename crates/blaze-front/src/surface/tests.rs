//! The surface record against small crates: every row kind has a row (and a kind left out is
//! caught), no row is of a fact a body makes or of a listing, the bytes are deterministic, a
//! signature edit moves exactly that fact's digest, a comment edit moves none, a crate
//! opened from its record answers as one built from source, and every late read the crate's
//! expansions make (`env!`, `include!`) is asked in its shape with the value seen.

use std::{alloc::Global, collections::BTreeMap, sync::Arc};

use base_db::{Crate, Db, all_crates};
use blaze_format::Aligned;
use blaze_ids::ContentId;
use blaze_item_hash::values::{self, kinds as own};
use blaze_items::{ItemId, key, kinds as items};
use blaze_keyed::{
    front::Universe,
    kind::{Key, Surface},
    record::{self, Fact, StableId},
};
use blaze_query::QueryKind;
use hir_def::{DefWithBodyId, ModuleDefId, deps::ImageBuf, nameres::crate_def_map};
use hir_expand::fixture;
use hir_ty::next_solver::goal_store::{FactObject, FactRef};

use super::{Opened, TextIds, exports, finish_rows, surface, view};
use crate::facts;

/// One crate with an item of every kind a surface holds a row of.
const EVERY_KIND: &str = r#"
//- /lib.rs crate:lib
#![no_core]
pub mod inner {
    /// A struct with a field.
    pub struct S { pub a: u32 }
}
pub enum E { A, B(u8) }
pub union U { a: u32 }
pub trait T {
    type Out;
    const C: u32;
    fn method(&self) -> u32;
}
impl T for inner::S {
    type Out = u8;
    const C: u32 = 1;
    fn method(&self) -> u32 { 0 }
}
impl inner::S {
    pub fn new() -> inner::S { inner::S { a: 0 } }
}
pub const K: u32 = 3;
pub static G: u32 = 4;
pub type Alias = u32;
pub const fn constant() -> u32 { 5 }
macro_rules! m { () => {} }
"#;

/// Every query kind a surface of [`EVERY_KIND`] must hold a row of.
const KINDS: &[QueryKind] = &[
    own::CRATE_ROOT,
    items::TARGET,
    own::CRATE_NAMES,
    own::CRATE_SETTINGS,
    own::MODULES,
    own::FIELD_ORDER,
    own::SETTING,
    own::TRAITS_IN_SCOPE,
    own::WHOLE_SCOPE,
    own::ATTRS,
    items::RESOLVES,
    items::SIGNATURE,
    items::NAME,
    items::LAYOUT,
    items::DROP_CHECK,
    own::INHERENT_ASSOC_TYPES,
    own::FIELDS,
    items::FIELD,
    items::RPITIT_TYS,
    items::COHERENCE,
    items::SPECIALIZES,
];

/// The kinds no surface holds a row of: what a body makes, and a body's own steps.
const NEVER: &[QueryKind] = &[
    items::CONST_VALUE,
    items::HIDDEN_TYPES,
    own::STEP_INFER,
    own::STEP_MIR,
    own::STEP_LOWER,
    own::MATCH_CHECK,
    items::CODE,
    items::HASHES,
];

fn db_of(text: &str) -> Db {
    fixture::db(text).into_db()
}

fn crate_named(db: &Db, name: &str) -> Crate {
    *all_crates(db)
        .iter()
        .find(|krate| krate.extra_data(db).display_name.as_ref().map(|it| it.canonical_name().as_str()) == Some(name))
        .unwrap_or_else(|| panic!("a crate named {name}"))
}

fn function_named(db: &Db, krate: Crate, name: &str) -> ItemId {
    for (module, _) in crate_def_map(db, krate).modules(db) {
        for def in module.def_map(db).module(db, module).scope(db).declarations() {
            if let ModuleDefId::FunctionId(it) = def
                && hir_def::signatures::FunctionSignature::of(db, it).name.as_str() == name
            {
                return ItemId::Function(it);
            }
        }
    }
    panic!("a function named {name}")
}

/// The tests' text ids: each body's derived from its item key (the command's come from text),
/// none for a discriminant, as the command gives none; `skip` is given none either (the
/// control of [`every_body_has_its_maker_row`]).
struct Derived<'a> {
    db: &'a Db,
    skip: Option<DefWithBodyId>,
}

impl<'a> Derived<'a> {
    fn of(db: &'a Db) -> Derived<'a> {
        Derived { db, skip: None }
    }
}

impl TextIds for Derived<'_> {
    fn text_id(&self, def: DefWithBodyId) -> Option<StableId> {
        match def {
            DefWithBodyId::VariantId(_) => None,
            _ if Some(def) == self.skip => None,
            _ => Some(StableId(ContentId::of("blaze-front test text id", &values::body_key(self.db, def).0))),
        }
    }
}

fn rows_with(db: &Db, krate: Crate, omit: Option<QueryKind>, text_ids: &Derived<'_>) -> Vec<Fact, Global> {
    let mut rows = Vec::new_in(Global);
    let mut unvalued = None;
    if let Err(failed) = exports::collect(db, krate, omit, text_ids, &mut rows, &mut unvalued) {
        let mut said = String::new();
        if let Some(fact) = unvalued {
            facts::unhashed_line(db, krate, &fact, &mut said);
        }
        panic!("the surface's rows: {failed:?} {said}");
    }
    finish_rows(rows, &key::crate_key(db, krate)).expect("each id one digest").0
}

fn rows_of(db: &Db, krate: Crate, omit: Option<QueryKind>) -> Vec<Fact, Global> {
    rows_with(db, krate, omit, &Derived::of(db))
}

/// The bodies of `rows`' crate with no maker row naming their text id: each body owner the
/// text ids name, whose maker row (of its item key) is missing or holds another text id.
fn makers_missing(db: &Db, krate: Crate, rows: &[Fact]) -> Vec<DefWithBodyId, Global> {
    let named = Derived::of(db);
    let mut out = Vec::new_in(Global);
    for &def in hir_def::nameres::demand::crate_body_owners(db, krate).iter() {
        let Some(text) = named.text_id(def) else { continue };
        let id = facts::maker_id(&values::body_key(db, def));
        if rows.iter().find(|row| row.id == id).map(|row| row.digest) != Some(text.0) {
            out.push(def);
        }
    }
    out
}

/// The invariant: every body of the crate a text id names has its maker row (the row a made
/// fact's id forms, `record::maker_row_of`), whose digest is that text id, and a body with no
/// text id (an enum discriminant) has none. Control: a body the text ids leave out is named
/// as missing, it alone.
#[test]
fn every_body_has_its_maker_row() {
    let db = db_of(EVERY_KIND);
    let lib = crate_named(&db, "lib");
    let rows = rows_of(&db, lib, None);
    assert_eq!(makers_missing(&db, lib, &rows), Vec::<DefWithBodyId, Global>::new_in(Global));
    let owners = hir_def::nameres::demand::crate_body_owners(&db, lib);
    let with_rows = owners.iter().filter(|def| !matches!(def, DefWithBodyId::VariantId(_))).count();
    let makers = rows.iter().filter(|row| record::kind_of(&row.id) == Some(record::FactKind::Maker)).count();
    assert_eq!(makers, with_rows, "one maker row per body with a text id, none for a discriminant");
    // Control: one body left without a text id.
    let left = function_named(&db, lib, "constant");
    let ItemId::Function(left) = left else { panic!("a function") };
    let left = DefWithBodyId::FunctionId(left);
    let rows = rows_with(&db, lib, None, &Derived { db: &db, skip: Some(left) });
    let mut want = Vec::new_in(Global);
    want.push(left);
    assert_eq!(makers_missing(&db, lib, &rows), want);
}

/// The kinds of `kinds` no row of `rows` is of.
fn missing(rows: &[Fact], kinds: &[QueryKind]) -> Vec<QueryKind, Global> {
    let mut out = Vec::new_in(Global);
    for kind in kinds {
        if !rows.iter().any(|row| record::code_of(&row.id) == kind.0) {
            out.push(*kind);
        }
    }
    out
}

/// The invariant: a surface of a crate with an item of every kind holds a row of every kind
/// (`KINDS`), each id once and sorted, and none of a fact a body makes, a body's step or a
/// listing across crates.
#[test]
fn every_kind_has_a_row() {
    let db = db_of(EVERY_KIND);
    let lib = crate_named(&db, "lib");
    let rows = rows_of(&db, lib, None);
    assert_eq!(missing(&rows, KINDS), Vec::<QueryKind, Global>::new_in(Global));
    assert_eq!(missing(&rows, NEVER).len(), NEVER.len(), "a row of a fact no surface holds");
    assert!(rows.windows(2).all(|pair| pair[0].id < pair[1].id), "sorted, each id once");
    for row in &rows {
        assert!(
            !matches!(facts::class_of(record::code_of(&row.id)), Some(facts::Class::Asker(facts::Asked::Seen))),
            "a listing's row: {row:?}"
        );
        assert!(!record::kind_of(&row.id).is_some_and(|kind| kind.made_by_body()), "a made fact's row: {row:?}");
    }
}

/// The control: one kind left out of the walk, and the check names exactly it.
#[test]
fn a_kind_left_out_is_caught() {
    let db = db_of(EVERY_KIND);
    let lib = crate_named(&db, "lib");
    let rows = rows_of(&db, lib, Some(items::LAYOUT));
    let mut want = Vec::new_in(Global);
    want.push(items::LAYOUT);
    assert_eq!(missing(&rows, KINDS), want);
    // The same for a crate kind added with the walk of the crate's modules.
    let rows = rows_of(&db, lib, Some(own::MODULES));
    let mut want = Vec::new_in(Global);
    want.push(own::MODULES);
    assert_eq!(missing(&rows, KINDS), want);
}

fn key_of() -> Key<Surface> {
    Key::<Surface>::from_id(ContentId::of("blaze-front surface test key", b"lib"))
}

/// The record of `krate` with no dependency, and the address it is filed at.
fn record_of(db: &Db, krate: Crate) -> (Aligned, Key<Surface>) {
    let universe = Universe { asker: 0, tables: &[], outside: &[] };
    let bytes = surface(db, krate, key_of(), universe, &Derived::of(db)).expect("the surface record");
    let (at, _) = record::check_read::<Surface>(&bytes, key_of()).expect("a read-addressed record");
    (bytes, at)
}

/// The invariant: a surface record is its inputs' alone (rule 25): written twice in one
/// database and once in another of the same text, the bytes are equal; its rows are the walk's
/// and its seen row.
#[test]
fn the_record_is_deterministic() {
    let db = db_of(EVERY_KIND);
    let lib = crate_named(&db, "lib");
    let (first, at) = record_of(&db, lib);
    let (again, _) = record_of(&db, lib);
    assert_eq!(&first[..], &again[..]);
    let other = db_of(EVERY_KIND);
    let (elsewhere, _) = record_of(&other, crate_named(&other, "lib"));
    assert_eq!(&first[..], &elsewhere[..]);
    let opened = view(&first, &at).expect("the record opens at its address");
    assert!(opened.reads.is_empty(), "a surface with no dependency asked something");
    let mut want = rows_of(&db, lib, None);
    let seen = opened.row(&record::seen_id()).expect("the seen row");
    let place = want.partition_point(|it| it.id < record::seen_id());
    want.insert(place, Fact { id: record::seen_id(), digest: seen });
    assert_eq!(opened.facts, &want[..]);
}

/// The rows of `text`'s `lib` by id, the exports row left out (it digests every other row).
fn digests(text: &str) -> BTreeMap<ContentId, ContentId> {
    let db = db_of(text);
    let lib = crate_named(&db, "lib");
    let exports = facts::exports_id(&key::crate_key(&db, lib));
    rows_of(&db, lib, None).iter().filter(|row| row.id != exports).map(|row| (row.id, row.digest)).collect()
}

/// The ids whose digest differs between `a` and `b`, or that one of them lacks.
fn moved(a: &BTreeMap<ContentId, ContentId>, b: &BTreeMap<ContentId, ContentId>) -> Vec<ContentId, Global> {
    let mut out = Vec::new_in(Global);
    for (id, digest) in a {
        if b.get(id) != Some(digest) {
            out.push(*id);
        }
    }
    for id in b.keys() {
        if !a.contains_key(id) {
            out.push(*id);
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// `A` and `B` differ in shape, not only in name: an item's hashes are anonymous (Unison-style,
/// docs/v2/plan/design/item-hash.md section 1), so a reference to a type is its signature hash,
/// and two unit structs of different names are one signature; `f(_x: A)` to `f(_x: B)` moves
/// `f`'s signature only when `B` is another shape than `A`.
const BEFORE: &str = r#"
//- /lib.rs crate:lib
#![no_core]
pub struct A;
pub struct B(pub u32);
pub fn f(_x: A) {}
pub fn g() {}
"#;

/// The invariant: a signature edit moves that function's `Signature` row and no other.
#[test]
fn a_signature_edit_moves_one_digest() {
    let after = BEFORE.replace("pub fn f(_x: A)", "pub fn f(_x: B)");
    let (before, edited) = (digests(BEFORE), digests(&after));
    let db = db_of(BEFORE);
    let lib = crate_named(&db, "lib");
    let f = key::item_key(&db, function_named(&db, lib, "f")).0;
    let signature = facts::id_of(&FactRef { kind: items::SIGNATURE, subject: f, object: FactObject::None }).expect("a signature's id");
    let mut want = Vec::new_in(Global);
    want.push(signature);
    assert_eq!(moved(&before, &edited), want);
}

/// [`BEFORE`] as it was written until 2026-11-14: `A` and `B` two unit structs, one shape.
const ONE_SHAPE: &str = r#"
//- /lib.rs crate:lib
#![no_core]
pub struct A;
pub struct B;
pub fn f(_x: A) {}
pub fn g() {}
"#;

/// The invariant: `f(_x: A)` to `f(_x: B)`, where `A` and `B` are two unit structs of one
/// shape, moves `f`'s `Signature` row and no other. The row's value is `f`'s signature hash,
/// which names each type it refers to by the type's key as well as its hash (D51,
/// `blaze_items::hash`'s keyed refs): a hash alone says what a type is, not which, and two
/// nominal types of one shape have one hash, so a reader of `f`'s signature (a caller's stored
/// type check, which accepts `f(A)` only with `f(_x: A)`) would otherwise be served stale.
///
/// The planted control, the old behaviour (refs by hash alone), has no seam here: it is
/// `blaze_items::hash`'s private `Names::Free`, which that crate's own test holds the code
/// and interface hashes to (name-free, rightly: codegen binds names outside a template). The
/// control kept here is [`the_same_text_moves_nothing`]: what moves is the edit's.
#[test]
fn a_parameter_moved_between_two_nominal_types_of_one_shape_moves_its_signature() {
    let after = ONE_SHAPE.replace("pub fn f(_x: A)", "pub fn f(_x: B)");
    assert_ne!(after, ONE_SHAPE, "premise: the edit applies");
    let (before, edited) = (digests(ONE_SHAPE), digests(&after));
    let db = db_of(ONE_SHAPE);
    let lib = crate_named(&db, "lib");
    let f = key::item_key(&db, function_named(&db, lib, "f")).0;
    let signature = facts::id_of(&FactRef { kind: items::SIGNATURE, subject: f, object: FactObject::None }).expect("a signature's id");
    let mut want = Vec::new_in(Global);
    want.push(signature);
    assert_eq!(moved(&before, &edited), want, "`f`'s signature, by name, and nothing else");
}

/// The control of the two edit tests: the same text moves nothing, so what moves above is
/// the edit's.
#[test]
fn the_same_text_moves_nothing() {
    assert!(moved(&digests(BEFORE), &digests(BEFORE)).is_empty());
}

/// The invariant: a comment edit moves no digest.
#[test]
fn a_comment_edit_moves_nothing() {
    let after = BEFORE.replace("pub fn g() {}", "// a comment\npub fn g() { /* inside */ }");
    assert!(moved(&digests(BEFORE), &digests(&after)).is_empty());
}

const TWO: &str = r#"
//- /lib.rs crate:lib
#![no_core]
pub mod inner { pub struct S { pub a: u32 } }
pub trait T { fn method(&self) -> u32; }
impl T for inner::S { fn method(&self) -> u32 { 0 } }
pub const K: u32 = 3;
pub type Alias = u32;
//- /main.rs crate:main deps:lib
#![no_core]
pub fn main() {}
"#;

/// The invariant: a surface's shape is one ask per surface of its universe, the dependency's
/// exports row with the answer its table gives, and it replays: every fact of it answered again
/// on the same universe gives the same digest. Control: a dependency's table of another
/// exports digest answers otherwise, so the shape would not replay on it.
#[test]
fn a_surface_shape_asks_each_dependency_and_replays() {
    use blaze_keyed::front::Table;
    let db = db_of(TWO);
    let lib = crate_named(&db, "lib");
    let main = crate_named(&db, "main");
    let (lib_bytes, _) = record_of(&db, lib);
    let lib_owner = record::Owner::of_subject(&key::crate_key(&db, lib));
    let tables = [Table::of(1, lib_owner, &lib_bytes, &[]).expect("lib's table")];
    let universe = Universe { asker: 0, tables: &tables, outside: &[] };
    let bytes = surface(&db, main, key_of(), universe, &Derived::of(&db)).expect("main's surface");
    let (_, root) = record::check_read::<Surface>(&bytes, key_of()).expect("a read-addressed record");
    let shape = root.reads.follow(&bytes).expect("the shape");
    let exports = facts::exports_id(&key::crate_key(&db, lib));
    assert_eq!(shape.len(), 1);
    assert_eq!(shape[0].id, exports);
    for fact in shape {
        assert_eq!(universe.answer(&fact.id), Ok(fact.digest), "a shape that does not replay");
    }
    // Control: lib's exports row with another digest.
    let mut other = Vec::new_in(Global);
    other.extend(tables[0].facts.iter().map(|row| match row.id == exports {
        true => Fact { id: row.id, digest: ContentId::of("blaze-front test", b"another exports digest") },
        false => *row,
    }));
    let moved = [Table { facts: &other, ..tables[0] }];
    let moved = Universe { asker: 0, tables: &moved, outside: &[] };
    assert_ne!(moved.answer(&exports), Ok(shape[0].digest));
}

/// The invariant: `lib` opened from its surface record answers every question its record's
/// rows ask (each row's fact, by id and digest) as `lib` built from source does; and it is
/// served, not lowered.
#[test]
fn opened_answers_as_built() {
    let built = db_of(TWO);
    let lib = crate_named(&built, "lib");
    let (bytes, at) = record_of(&built, lib);
    let want = rows_of(&built, lib, None);

    let mut served = db_of(TWO);
    // dyn: a record held as the store holds it.
    let holder: ImageBuf = Arc::new(bytes);
    let mut surfaces = Vec::new_in(Global);
    surfaces.push((holder, at));
    let opened = Opened::open(&surfaces).expect("the record opens");
    let crates: Vec<(ContentId, Crate), Global> = {
        let mut out = Vec::new_in(Global);
        out.extend(all_crates(&served).iter().map(|&krate| (key::crate_key(&served, krate), krate)));
        out
    };
    let mut edit = served.edit();
    opened
        .give(&mut edit, |crate_key| crates.iter().find(|(it, _)| it == crate_key).map(|&(_, krate)| krate))
        .expect("the surface given");
    _ = edit.commit();
    let lib = crate_named(&served, "lib");
    assert!(hir_def::deps::is_served(&served, lib), "lib read from its surface");
    assert_eq!(rows_of(&served, lib, None), want);
}

thread_local! {
    /// The planted bug of the late tests: a mask of `Late` bytes (`1 << late`) whose reads
    /// [`super::surface`] does not ask, as a surface that missed a late read would.
    pub(super) static DROP_LATE: std::cell::Cell<u8> = const { std::cell::Cell::new(0) };
}

/// The built-in macros a `#![no_core]` fixture crate calls, declared as rust-analyzer's own
/// fixtures declare them.
const BUILTINS: &str = r#"
#[rustc_builtin_macro] macro_rules! include { () => {} }
#[rustc_builtin_macro] macro_rules! concat { () => {} }
#[rustc_builtin_macro] macro_rules! env { () => {} }
"#;

/// A crate that includes the file its variable `X` names: `X` is `x`, `/a.rs` holds `a` and
/// `/b.rs` holds `b`.
fn env_crate(x: &str, a: &str, b: &str) -> String {
    format!(
        "//- /lib.rs crate:lib env:X={x}\n#![no_core]\n{BUILTINS}\ninclude!(concat!(env!(\"X\"), \".rs\"));\n//- /a.rs\n{a}\n//- /b.rs\n{b}\n"
    )
}

/// A crate that includes `/inc.rs`, which holds `inc`.
fn include_crate(inc: &str) -> String {
    format!("//- /lib.rs crate:lib\n#![no_core]\n{BUILTINS}\ninclude!(\"inc.rs\");\n//- /inc.rs\n{inc}\n")
}

/// The fixture text of a file as the fixture writes it: the body and its newline.
fn file_text(body: &str) -> String {
    format!("{body}\n")
}

/// What the host's outside table answers, made by the test from the world it set up, not from
/// what the surface saw: each `(late, name, digest)` as a fact, sorted by id.
fn outside(reads: &[(blaze_keyed::kind::Late, &[u8], ContentId)]) -> Vec<Fact, Global> {
    let mut out = Vec::new_in(Global);
    out.extend(reads.iter().map(|&(late, name, digest)| Fact { id: record::late_id(late as u8, name), digest }));
    out.sort_unstable();
    out
}

fn var(value: &str) -> ContentId {
    blaze_host::outside::var_answer(Some(value.as_bytes()))
}

fn file(body: &str) -> ContentId {
    blaze_host::outside::file_answer(file_text(body).as_bytes())
}

/// `lib`'s surface over `outside` (no dependency): its address and its shape, or why not.
fn late_record(text: &str, outside: &[Fact]) -> Result<(Key<Surface>, Vec<Fact, Global>), blaze_keyed::driver::TaskFailed> {
    let db = db_of(text);
    let lib = crate_named(&db, "lib");
    let universe = Universe { asker: 0, tables: &[], outside };
    let bytes = surface(&db, lib, key_of(), universe, &Derived::of(&db))?;
    let (at, root) = record::check_read::<Surface>(&bytes, key_of()).expect("a read-addressed record");
    let mut shape = Vec::new_in(Global);
    shape.extend_from_slice(root.reads.follow(&bytes).expect("the shape"));
    Ok((at, shape))
}

/// The oracle: one address, one world. Each run is the address its surface was filed at and
/// the outside answers of the world it ran in; two runs at one address in worlds that answer
/// differently are a stale record found by its address. The index pair of the first such.
fn one_address_one_world(runs: &[(Key<Surface>, &[Fact])]) -> Result<(), (usize, usize)> {
    for (i, a) in runs.iter().enumerate() {
        for (j, b) in runs.iter().enumerate().skip(i + 1) {
            if a.0 == b.0 && a.1 != b.1 {
                return Err((i, j));
            }
        }
    }
    Ok(())
}

/// Runs `body` with the late reads of `mask` planted out, and puts the plant back after.
fn planted<T>(mask: u8, body: impl FnOnce() -> T) -> T {
    DROP_LATE.set(mask);
    let out = body();
    DROP_LATE.set(0);
    out
}

/// The invariant: a crate whose expansions read `env!("X")` asks it in its shape, with the
/// digest of the value the expansion saw; another value of `X` is another address; a world
/// that answers another value than the expansion saw fails the task by name. Control: with
/// the late asks planted out, two values of `X` (whose included files hold the same text, so
/// the rows are equal) give one address, which the oracle catches.
#[test]
fn an_env_read_is_asked_and_moves_the_address() {
    use blaze_keyed::kind::Late;
    let same = "pub struct S;";
    let (text_a, text_b) = (env_crate("a", same, same), env_crate("b", same, same));
    let world_a = outside(&[(Late::Env, b"X", var("a")), (Late::Include, b"a.rs", file(same))]);
    let world_b = outside(&[(Late::Env, b"X", var("b")), (Late::Include, b"b.rs", file(same))]);

    let (at_a, shape_a) = late_record(&text_a, &world_a).expect("lib's surface with X=a");
    let (at_b, _) = late_record(&text_b, &world_b).expect("lib's surface with X=b");
    let env = record::late_id(Late::Env as u8, b"X");
    assert_eq!(shape_a.iter().find(|it| it.id == env).map(|it| it.digest), Some(var("a")), "the env read asked");
    assert_ne!(at_a, at_b, "another value of X, the same address");
    assert_eq!(one_address_one_world(&[(at_a, &world_a), (at_b, &world_b)]), Ok(()));

    // The world moved under the task: the expansion saw `a`, the request answers `b`.
    let moved = outside(&[(Late::Env, b"X", var("b")), (Late::Include, b"a.rs", file(same))]);
    assert_eq!(
        late_record(&text_a, &moved).err(),
        Some(blaze_keyed::driver::TaskFailed::Refused { what: super::LATE_MOVED })
    );
    // A late read the request did not read: refused, never answered as absent.
    assert!(late_record(&text_a, &[]).is_err(), "an unanswered late read filed a record");

    // Control: the late asks planted out (the include `X` names moves with `X`, so both).
    let mask = (1 << Late::Env as u8) | (1 << Late::Include as u8);
    let (planted_a, planted_b) = planted(mask, || {
        (late_record(&text_a, &world_a).expect("planted, X=a").0, late_record(&text_b, &world_b).expect("planted, X=b").0)
    });
    assert_eq!(planted_a, planted_b, "the plant left an ask in the shape");
    assert_eq!(one_address_one_world(&[(planted_a, &world_a), (planted_b, &world_b)]), Err((0, 1)));
}

/// The invariant: a crate that includes a file of its source root asks that include in its
/// shape with the file's content digest, and an edit of the file that moves no row (a comment)
/// still moves the address. Control: the include ask planted out, the two texts give one
/// address in two worlds, which the oracle catches. (`include!` stands for the three: at item
/// level `include_str!` and `include_bytes!` are reached only through an attribute's value.)
#[test]
fn an_include_read_is_asked_and_moves_the_address() {
    use blaze_keyed::kind::Late;
    let (one, two) = ("// one\npub struct S;", "// two\npub struct S;");
    let world_one = outside(&[(Late::Include, b"inc.rs", file(one))]);
    let world_two = outside(&[(Late::Include, b"inc.rs", file(two))]);

    let (at_one, shape_one) = late_record(&include_crate(one), &world_one).expect("lib's surface, one");
    let (at_two, _) = late_record(&include_crate(two), &world_two).expect("lib's surface, two");
    let include = record::late_id(Late::Include as u8, b"inc.rs");
    assert_eq!(shape_one.iter().find(|it| it.id == include).map(|it| it.digest), Some(file(one)), "the include asked");
    assert_ne!(at_one, at_two, "another text of the included file, the same address");
    assert_eq!(one_address_one_world(&[(at_one, &world_one), (at_two, &world_two)]), Ok(()));
    assert_eq!(
        late_record(&include_crate(one), &world_two).err(),
        Some(blaze_keyed::driver::TaskFailed::Refused { what: super::LATE_MOVED })
    );

    let (planted_one, planted_two) = planted(1 << Late::Include as u8, || {
        (
            late_record(&include_crate(one), &world_one).expect("planted, one").0,
            late_record(&include_crate(two), &world_two).expect("planted, two").0,
        )
    });
    assert_eq!(planted_one, planted_two, "the plant left an ask in the shape");
    assert_eq!(one_address_one_world(&[(planted_one, &world_one), (planted_two, &world_two)]), Err((0, 1)));
}

/// hir-def names the late macros by byte without depending on blaze-keyed: the two tables are
/// held equal here, where both are in reach.
#[test]
fn hir_defs_late_bytes_are_blaze_keyeds() {
    use blaze_keyed::kind::Late;
    use hir_def::deps;
    assert_eq!(deps::LATE_ENV, Late::Env as u8);
    assert_eq!(deps::LATE_OPTION_ENV, Late::OptionEnv as u8);
    assert_eq!(deps::LATE_INCLUDE, Late::Include as u8);
    assert_eq!(deps::LATE_INCLUDE_STR, Late::IncludeStr as u8);
    assert_eq!(deps::LATE_INCLUDE_BYTES, Late::IncludeBytes as u8);
}
