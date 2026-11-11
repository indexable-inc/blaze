//! `SpendFacts` over fixtures (docs/v2/plan/design/facts.md): each change as the body before
//! (the `Debug` pipeline, which spends nothing) and after (`Release`), what the body records,
//! the calls that must stay as they are, and the evaluator giving the same outcome with and
//! without. The refinement over random programs is `crate::facts::model`'s.

use base_db::{Crate, Db};
use hir_def::{AdtId, FunctionId, ModuleDefId, nameres::crate_def_map};
use hir_expand::name::Name;
use hir_ty::{
    ParamEnvAndCrate,
    db::HirDatabase,
    facts::{Fact, FactItem, facts_of},
    next_solver::GenericArgs,
};
use hir_ty_mir::mir::{
    MirBody, MirOptLevel, MirOptions, OperandKind, TerminatorKind, interpret_mir,
    relied_on_own_no_unwind, set_interpreter_mir_opt_level,
};
use intern::Symbol;

use super::{
    fixture::{crate_named, db_with},
    fns::fn_named,
};
use crate::program_lifetime::{self, Why};

const CALLS: &str = r#"
//- /lib.rs crate:kernel deps:std
pub struct Guard(u32);

impl Drop for Guard {
    fn drop(&mut self) {}
}

fn quiet(x: u32) -> u32 {
    x ^ 1
}

fn loud(x: u32) -> u32 {
    if x == 0 {
        panic!()
    }
    x
}

pub trait Shape {
    fn area(&self) -> u32;
}

pub struct Square;

impl Shape for Square {
    fn area(&self) -> u32 {
        panic!()
    }
}

fn through(shape: &dyn Shape) -> u32 {
    shape.area()
}

extern "C-unwind" fn other_abi(x: u32) -> u32 {
    x ^ 1
}

unsafe extern "C" {
    fn foreign(x: u32) -> u32;
}

pub fn edge_goes(x: u32) -> u32 {
    let _guard = Guard(x);
    quiet(x)
}

pub fn edge_stays(x: u32) -> u32 {
    let _guard = Guard(x);
    loud(x)
}

pub fn dyn_stays(shape: &dyn Shape) -> u32 {
    let _guard = Guard(0);
    through(shape)
}

pub fn abi_stays(x: u32) -> u32 {
    let _guard = Guard(x);
    other_abi(x)
}

pub fn foreign_stays(x: u32) -> u32 {
    let _guard = Guard(x);
    unsafe { foreign(x) }
}

pub fn pointer_stays(f: fn(u32) -> u32, x: u32) -> u32 {
    let _guard = Guard(x);
    f(x)
}

pub fn twice(x: u32) -> u32 {
    quiet(x) ^ quiet(x)
}

pub fn twice_loud(x: u32) -> u32 {
    loud(x) ^ loud(x)
}

pub fn twice_differs(x: u32, y: u32) -> u32 {
    quiet(x) ^ quiet(y)
}

pub fn run_quiet() -> u32 {
    edge_goes(3) ^ twice(5)
}

pub fn run_loud() -> u32 {
    edge_stays(0)
}

pub fn run_twice_loud() -> u32 {
    twice_loud(0)
}

pub fn run_dyn() -> u32 {
    dyn_stays(&Square)
}
"#;

fn release() -> MirOptions {
    MirOptions::at(MirOptLevel::Release)
}

/// The calls by name of the body's blocks off the unwind path: for each, whether it has an
/// unwind edge.
fn call_edges(body: &MirBody<'_>) -> Vec<bool> {
    body.basic_blocks
        .iter()
        .filter(|(_, block)| !block.is_cleanup)
        .filter_map(|(_, block)| match &block.terminator.as_ref()?.kind {
            TerminatorKind::Call { cleanup, intrinsic: None, .. } => Some(cleanup.is_some()),
            _ => None,
        })
        .collect()
}

/// The calls by name of a function (not a pointer in a local) in the body.
fn named_calls(body: &MirBody<'_>) -> usize {
    body.basic_blocks
        .iter()
        .filter(|(_, block)| {
            matches!(
                block.terminator.as_ref().map(|it| &it.kind),
                Some(TerminatorKind::Call {
                    func: hir_ty_mir::mir::Operand { kind: OperandKind::Constant { .. }, .. },
                    intrinsic: None,
                    ..
                })
            )
        })
        .count()
}

/// `check` given the `Debug` body and the `Release` body of the function `name` of `CALLS`.
#[track_caller]
fn bodies(name: &str, check: impl FnOnce(&Db, FunctionId, &MirBody<'_>, &MirBody<'_>)) {
    let db = db_with(CALLS);
    let func = fn_named(&db, crate_named(&db, "kernel"), name);
    blaze_query::db::attach(&db, || {
        let before = db
            .optimized_mir_body(func.into(), MirOptions::at(MirOptLevel::Debug))
            .unwrap_or_else(|error| panic!("`{name}` at Debug: {error:?}"));
        let after = db
            .optimized_mir_body(func.into(), release())
            .unwrap_or_else(|error| panic!("`{name}` at Release: {error:?}"));
        check(&db, func, before, after);
    });
}

fn relied(db: &Db, krate: Crate, name: &str, fact: Fact) -> [u8; 32] {
    facts_of(db, FactItem::Fn(fn_named(db, krate, name))).relied(fact)
}

#[test]
fn a_call_of_a_function_that_cannot_panic_loses_its_unwind_edge() {
    bodies("edge_goes", |db, func, before, after| {
        assert_eq!(call_edges(before), [true], "as written, the call unwinds to the guard's drop");
        assert_eq!(call_edges(after), [false], "proved unable to panic, it has no edge");
        let cleanup = |body: &MirBody<'_>| body.basic_blocks.iter().filter(|it| it.1.is_cleanup).count();
        assert!(cleanup(before) > 0);
        assert_eq!(cleanup(after), 0, "the cleanup only that edge reached is gone");
        let krate = crate_named(db, "kernel");
        let quiet = relied(db, krate, "quiet", Fact::NoUnwind);
        assert!(after.inlined.relied.contains(&quiet), "the body records what it relied on");
        assert!(before.inlined.relied.is_empty(), "a body nothing changed records nothing");
        // The caller cannot panic either: the fact goes up the call graph by the same walk.
        assert!(facts_of(db, FactItem::Fn(func)).spendable(Fact::NoUnwind));
        assert!(relied_on_own_no_unwind(db, after));
        assert!(!relied_on_own_no_unwind(db, before));
    });
}

/// The calls whose edge must stay, each with why, and whether the caller itself is proved
/// unable to panic all the same (a call of a `C-unwind` function whose body cannot panic is
/// not a panic: the edge stays because of the ABI, not because of what the callee does).
#[test]
fn an_unwind_edge_stays_where_nothing_proves_the_callee() {
    for (name, why, proved) in [
        ("edge_stays", "the callee panics", false),
        ("dyn_stays", "the callee calls through a `dyn`, whose target nobody named", false),
        ("abi_stays", "the callee's ABI is `C-unwind`", true),
        ("foreign_stays", "the callee is foreign code", false),
        ("pointer_stays", "the call is through a function pointer", false),
    ] {
        bodies(name, |db, func, before, after| {
            assert_eq!(call_edges(before), call_edges(after), "`{name}`: {why}");
            assert!(call_edges(after).contains(&true), "`{name}` has the edge: {why}");
            let own = facts_of(db, FactItem::Fn(func));
            assert_eq!(own.spendable(Fact::NoUnwind), proved, "`{name}`: {why}");
            // No callee's fact was relied on: at most the body's own.
            let recorded: &[[u8; 32]] = if proved { &[own.relied(Fact::NoUnwind)] } else { &[] };
            assert_eq!(after.inlined.relied, recorded, "`{name}`: {why}");
        });
    }
}

#[test]
fn a_repeated_call_of_a_pure_function_takes_the_firsts_result() {
    bodies("twice", |db, _, before, after| {
        assert_eq!(named_calls(before), 2);
        assert_eq!(named_calls(after), 1, "the second call is a copy of the first's result");
        let quiet = relied(db, crate_named(db, "kernel"), "quiet", Fact::Pure);
        assert!(after.inlined.relied.contains(&quiet));
    });
    // A pure function that may panic merges too: the first call returned, so the second
    // would have.
    bodies("twice_loud", |_, _, _, after| assert_eq!(named_calls(after), 1));
    // Different arguments are different calls.
    bodies("twice_differs", |_, _, _, after| assert_eq!(named_calls(after), 2));
}

/// How running the function `name` of `CALLS` ends at `level`: whether it returned.
fn returns(db: &Db, name: &str, level: MirOptLevel) -> bool {
    let krate = crate_named(db, "kernel");
    let func = fn_named(db, krate, name);
    blaze_query::db::attach(db, || {
        let env = ParamEnvAndCrate { param_env: db.trait_environment(func.into()), krate }.store();
        let args = GenericArgs::new_from_slice(&[]).store();
        let body = match level {
            MirOptLevel::Off => db.monomorphized_runtime_mir_body(func.into(), args, env),
            level => db.monomorphized_optimized_mir_body(
                func.into(),
                MirOptions::at(level),
                args,
                env,
            ),
        }
        .unwrap_or_else(|error| panic!("`{name}` at {level:?}: {error:?}"));
        set_interpreter_mir_opt_level(level);
        let ran = interpret_mir(db, body, None);
        set_interpreter_mir_opt_level(MirOptLevel::Off);
        match ran {
            Ok((Ok(_), _)) => true,
            Ok((Err(error), _)) | Err(error) => {
                let said = format!("{error:?}");
                assert!(said.contains("Panic"), "`{name}` at {level:?} ends in no panic: {said}");
                false
            }
        }
    })
}

#[test]
fn the_evaluator_ends_the_same_with_facts_spent() {
    let db = db_with(CALLS);
    for (name, want) in
        [("run_quiet", true), ("run_loud", false), ("run_twice_loud", false), ("run_dyn", false)]
    {
        assert_eq!(returns(&db, name, MirOptLevel::Off), want, "`{name}` as written");
        assert_eq!(returns(&db, name, MirOptLevel::Release), want, "`{name}` with facts spent");
    }
}

const TYPES: &str = r#"
//- /lib.rs crate:kernel deps:std
#[blaze::program_lifetime]
pub struct Node {
    kind: u16,
    parent: Option<&'static Node>,
    text: &'static str,
    span: (u32, u32),
}

#[blaze::program_lifetime]
pub enum Token {
    Word(&'static Node),
    End,
}

pub struct Plain {
    bits: [u8; 4],
}

#[blaze::program_lifetime]
pub struct HoldsPlain {
    plain: Plain,
    token: Token,
}

#[blaze::program_lifetime]
pub struct Generic<T> {
    value: T,
}

#[blaze::program_lifetime]
pub struct Dropped(u32);

impl Drop for Dropped {
    fn drop(&mut self) {}
}

pub struct Inner {
    name: std::string::String,
}

#[blaze::program_lifetime]
pub struct Owns {
    inner: Inner,
}

#[blaze::program_lifetime]
pub struct Borrows<'a> {
    text: &'a str,
}

#[blaze::program_lifetime]
pub struct Opaque {
    hook: &'static dyn Fn(),
    run: fn(u32) -> u32,
}

pub struct Unmarked {
    kind: u16,
}
"#;

#[track_caller]
fn adt_named(db: &Db, krate: Crate, name: &str) -> AdtId {
    let root = crate_def_map(db, krate).root_module_id();
    let data = root.to_db(db).def_map(db).module(db, root);
    match data.name_in_scope(db, &Name::new_symbol_root(Symbol::intern(name))).take_types() {
        Some(ModuleDefId::AdtId(adt)) => adt,
        _ => panic!("no type `{name}` at the root of the crate"),
    }
}

#[test]
fn program_lifetime_is_accepted_of_plain_and_static_shapes() {
    let db = db_with(TYPES);
    let krate = crate_named(&db, "kernel");
    blaze_query::db::attach(&db, || {
        for name in ["Node", "Token", "HoldsPlain", "Opaque"] {
            let adt = adt_named(&db, krate, name);
            let refused = program_lifetime::shape(&db, adt);
            assert!(refused.is_none(), "{}", refused.as_ref().map_or_else(String::new, |it| it.said()));
            let facts = facts_of(&db, FactItem::Adt(adt));
            assert!(facts.holds(Fact::ProgramLifetime), "`{name}` has the fact");
            // Declared and checked, not inferred: no pass changes code on it yet.
            assert!(!facts.spendable(Fact::ProgramLifetime));
        }
        // The fact is declared: a type nobody marked has none, whatever its shape.
        let unmarked = adt_named(&db, krate, "Unmarked");
        assert!(program_lifetime::shape(&db, unmarked).is_none());
        assert!(!facts_of(&db, FactItem::Adt(unmarked)).holds(Fact::ProgramLifetime));
    });
}

#[test]
fn program_lifetime_is_refused_naming_the_reason() {
    let db = db_with(TYPES);
    let krate = crate_named(&db, "kernel");
    blaze_query::db::attach(&db, || {
        for (name, why, path, says) in [
            ("Generic", Why::Generic, "", "has a generic parameter"),
            ("Dropped", Why::Drops, "", "has a `Drop` impl"),
            ("Owns", Why::FieldDrops, "inner.name", "holds a value with a `Drop` impl"),
            ("Borrows", Why::Generic, "", "has a generic parameter"),
        ] {
            let adt = adt_named(&db, krate, name);
            let refused = program_lifetime::shape(&db, adt)
                .as_ref()
                .unwrap_or_else(|| panic!("`{name}` should be refused"));
            assert_eq!(refused.why, why, "{}", refused.said());
            assert!(refused.path.starts_with(path), "{}", refused.said());
            let said = refused.said();
            assert!(said.contains(&format!("`{name}` is `#[blaze::program_lifetime]`, but")), "{said}");
            assert!(said.contains(says), "{said}");
            assert!(!facts_of(&db, FactItem::Adt(adt)).holds(Fact::ProgramLifetime), "{said}");
        }
    });
}

/// Functions whose signatures name their own late-bound lifetime inside an associated type's
/// projection: behind a field (`Unit<R>` holds an `R::Offset`), and written out.
const LATE_BOUND: &str = r#"
//- /lib.rs crate:kernel deps:std
pub trait Reader {
    type Offset;
}

pub struct Slice<'a>(&'a [u8]);

impl<'a> Reader for Slice<'a> {
    type Offset = usize;
}

pub struct Unit<R: Reader> {
    pub offset: R::Offset,
}

pub struct Summary<'a> {
    pub unit: &'a Unit<Slice<'a>>,
}

pub fn summary<'a>(unit: &'a Unit<Slice<'a>>) -> Summary<'a> {
    Summary { unit }
}

pub fn written<'a>(
    reader: Slice<'a>,
    offset: <Slice<'a> as Reader>::Offset,
) -> <Slice<'a> as Reader>::Offset {
    let _ = reader;
    offset
}
"#;

/// The facts of every function are asked (`SpendFacts` asks each body's and each callee's), and
/// purity's scans the parameters' types, normalizing what it reaches: the signature's
/// late-bound lifetimes are instantiated first, so `<Slice<'a> as Reader>::Offset` is a goal
/// the solver takes. Skipped instead, the projection kept the bound lifetime and the solver
/// panicked wrapping it in a binder ("has escaping bound vars, so it cannot be wrapped in a
/// dummy binder"): wasmtime's `log_get_die_ref<'a, 'r>(entry: &'a ConvertUnitEntry<'a,
/// EndianSlice<'r, LittleEndian>>)`, whose gimli types hold `<R as gimli::Reader>::Offset`,
/// which stopped blaze building blaze at stage 2.
#[test]
fn a_late_bound_lifetime_in_a_parameters_projection_is_scanned() {
    let db = db_with(LATE_BOUND);
    let krate = crate_named(&db, "kernel");
    blaze_query::db::attach(&db, || {
        for name in ["summary", "written"] {
            let func = fn_named(&db, krate, name);
            if let crate::FnPurity::Impure(refusal) = crate::fn_purity_generic(&db, func) {
                panic!("`{name}` is pure: {}", refusal.said());
            }
            assert!(facts_of(&db, FactItem::Fn(func)).holds(Fact::Pure), "`{name}` is pure");
        }
    });
}
