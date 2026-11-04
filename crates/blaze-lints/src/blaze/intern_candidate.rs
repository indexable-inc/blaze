//! `intern_candidate`: a type of the crate that is `Clone`, `PartialEq` and `Hash` (derived or
//! written), recursive or deep, used as the key of a hash map or set, and compared with `==`
//! or cloned in many places of the crate ([`USES`], `intern-candidate-uses` in `clippy.toml`).
//! Every `==` walks both trees, every hash walks one, every clone copies one: interning
//! (hash-consing) stores each distinct value once and hands out a small `Copy` id, so equality
//! and hashing are one integer and a clone is a copy.
//!
//! - **Recursive**: a field holds the type itself behind an owned pointer or collection (`Box`,
//!   `Vec`, `Rc`, `Arc`, a map or set), through `Option`, tuples, arrays and the fields of
//!   other structs, at most [`DEPTH`] types deep.
//! - **Deep**: a field nests owned heap data two levels or more (`Vec<String>`,
//!   `Box<Vec<u8>>`, a struct of the crate holding a `Vec<String>`).
//! - **Used**: the crate's every body is read once ([`intern_uses`]): an expression whose type
//!   is a hash map or set (std's, hashbrown's, indexmap's, ...) or a `BTreeMap`/`BTreeSet`
//!   keyed by the type, and the `==`/`!=` of two of its values, `.eq(..)`/`.ne(..)`, and each
//!   `.clone()` of one. The type's own impls are not counted (a derive comparing fields).
//!
//! Not checked: a generic type (its cost depends on its arguments), a `Copy` one, test code,
//! what a macro wrote, and uses in other crates. No fix: interning changes the type's
//! representation everywhere.

use base_db::Db;
use blaze_hash::HashMap;
use hir::HirDisplay as _;
use hir_def::{
    AdtId, DefWithBodyId,
    expr_store::Body,
    hir::{BinaryOp, CmpOp, Expr},
    nameres::crate_def_map,
    signatures::FunctionSignature,
};
use std::alloc::Global;
use syntax::view::{self as ast, AstNode as _, HasName as _};

use super::{Lint, alloc::written_callee, facts};
use crate::check::{self, Cx};

const LINT: Lint = Lint::InternCandidate;

/// The `==`s and clones of the crate a type needs to be worth interning, by default: below it
/// the comparisons are rare enough that an interner's table costs more than it saves.
const USES: u64 = 8;

/// How many types deep a field is looked through for the type itself or for heap data.
const DEPTH: u32 = 4;

/// The owned pointers and collections of std, by name: what makes a type deep, and what a
/// recursive type holds itself through.
const HEAP: &[&str] = &[
    "Box",
    "Vec",
    "String",
    "Rc",
    "Arc",
    "VecDeque",
    "BTreeMap",
    "BTreeSet",
    "HashMap",
    "HashSet",
    "LinkedList",
    "BinaryHeap",
];

/// The maps and sets whose first type argument is a key, by crate.
const KEYED: &[(&str, &[&str])] = &[
    ("std", &["HashMap", "HashSet", "BTreeMap", "BTreeSet"]),
    ("alloc", &["BTreeMap", "BTreeSet"]),
    ("hashbrown", &["HashMap", "HashSet"]),
    ("indexmap", &["IndexMap", "IndexSet"]),
    ("papaya", &["HashMap", "HashSet"]),
    ("dashmap", &["DashMap", "DashSet"]),
    ("blaze_hash", &["HashMap", "HashSet", "Map", "Set"]),
];

/// What the crate's bodies do with one type.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Count {
    /// Expressions whose type is a map or set keyed by it.
    pub(crate) keyed: u32,
    /// `==`, `!=`, `.eq(..)` and `.ne(..)` of its values.
    pub(crate) compared: u32,
    /// `.clone()`s of its values.
    pub(crate) cloned: u32,
}

/// The [`Count`] of each type of the crate its bodies key a map by, compare or clone.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Uses(HashMap<AdtId, Count, Global>);

/// What `krate`'s bodies do with each type, as the module docs say, once per crate.
#[blaze_query::tracked(returns(ref))]
pub(crate) fn intern_uses(db: &Db, krate: base_db::Crate) -> Uses {
    let mut uses = Uses(HashMap::new_in(Global));
    // alloc: the crate's bodies, once per crate and edit
    let mut defs = Vec::new();
    for (_, module) in crate_def_map(db, krate).modules(db) {
        check::bodies_of(db, module.scope(db), &mut defs);
    }
    let mut next = 0;
    while let Some(&def) = defs.get(next) {
        next += 1;
        for (_, block) in Body::of(db, def).blocks(db) {
            for (_, module) in block.modules(db) {
                check::bodies_of(db, module.scope(db), &mut defs);
            }
        }
        count_in(db, def, &mut uses.0);
    }
    uses
}

/// Adds what the body of `def` does with each type to `out`.
fn count_in(db: &Db, def: DefWithBodyId, out: &mut HashMap<AdtId, Count, Global>) {
    let analysis = hir::BodyAnalysis::new(db, hir::DefWithBody::from(def));
    let body = analysis.body();
    let infer = analysis.infer();
    // The type's own impls (a derive's `eq` comparing its fields) are not its uses.
    let own = match def {
        DefWithBodyId::FunctionId(f) => facts::impl_adt(db, f).map(AdtId::from),
        _ => None,
    };
    let adt_of = |id| analysis.ty(infer.expr_ty(id)).strip_references().as_adt().map(AdtId::from);
    let mut add = |adt: Option<AdtId>, what: fn(&mut Count)| {
        if let Some(adt) = adt
            && Some(adt) != own
        {
            what(out.entry(adt).or_default());
        }
    };
    for (id, expr) in body.exprs() {
        add(key_of(db, &analysis.ty(infer.expr_ty(id))), |it| it.keyed += 1);
        match expr {
            Expr::BinaryOp { lhs, op: Some(BinaryOp::CmpOp(CmpOp::Eq { .. })), .. } => {
                add(adt_of(lhs), |it| it.compared += 1);
            }
            Expr::MethodCall { receiver, .. } => {
                let Some((callee, _)) = written_callee(infer, body, id) else { continue };
                match FunctionSignature::of(db, callee).name.as_str() {
                    "clone" => add(adt_of(receiver), |it| it.cloned += 1),
                    "eq" | "ne" => add(adt_of(receiver), |it| it.compared += 1),
                    _ => {}
                }
            }
            Expr::Call { args: [first, ..], .. } => {
                let Some((callee, _)) = written_callee(infer, body, id) else { continue };
                match FunctionSignature::of(db, callee).name.as_str() {
                    "clone" => add(adt_of(*first), |it| it.cloned += 1),
                    "eq" | "ne" => add(adt_of(*first), |it| it.compared += 1),
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

/// The type `ty`, its references peeled, is a map or set keyed by, when that is an ADT.
fn key_of(db: &Db, ty: &hir::Type<'_>) -> Option<AdtId> {
    let ty = ty.strip_references();
    let adt = ty.as_adt()?;
    let name = adt.name(db);
    let krate = adt.module(db).krate(db);
    if !KEYED
        .iter()
        .any(|(it, names)| names.contains(&name.as_str()) && facts::crate_is(db, krate, it))
    {
        return None;
    }
    ty.type_arguments().next()?.strip_references().as_adt().map(AdtId::from)
}

/// Whether `adt` is one of std's owned pointers or collections ([`HEAP`]).
fn heap(db: &Db, adt: hir::Adt) -> bool {
    let krate = adt.module(db).krate(db);
    HEAP.contains(&adt.name(db).as_str())
        && ["alloc", "std", "core"].iter().any(|it| facts::crate_is(db, krate, it))
}

/// The most owned heap types `ty` nests, one in another; and whether it holds `me` behind one.
fn depth(
    db: &Db,
    ty: &hir::Type<'_>,
    me: hir::Adt,
    under_heap: bool,
    budget: u32,
    recursive: &mut bool,
) -> u32 {
    let ty = ty.strip_references();
    if let Some(inner) = ty.as_slice() {
        return depth(db, &inner, me, under_heap, budget, recursive);
    }
    if let Some((inner, _)) = ty.as_array(db) {
        return depth(db, &inner, me, under_heap, budget, recursive);
    }
    if ty.is_tuple() {
        let fields = ty.tuple_fields(db);
        return fields
            .iter()
            .map(|it| depth(db, it, me, under_heap, budget, recursive))
            .max()
            .unwrap_or(0);
    }
    let Some(adt) = ty.as_adt() else { return 0 };
    if adt == me {
        *recursive |= under_heap;
        return 0;
    }
    if budget == 0 {
        return 0;
    }
    if heap(db, adt) {
        let inner =
            ty.type_arguments().map(|it| depth(db, &it, me, true, budget - 1, recursive)).max();
        return 1 + inner.unwrap_or(0);
    }
    match adt {
        hir::Adt::Struct(_) | hir::Adt::Union(_) => ty
            .fields(db)
            .iter()
            .map(|(_, it)| depth(db, it, me, under_heap, budget - 1, recursive))
            .max()
            .unwrap_or(0),
        hir::Adt::Enum(_) => ty
            .type_arguments()
            .map(|it| depth(db, &it, me, under_heap, budget - 1, recursive))
            .max()
            .unwrap_or(0),
    }
}

/// The types of `adt`'s own fields, every variant's for an enum.
fn field_types(db: &Db, adt: hir::Adt) -> Vec<hir::Type<'_>> {
    let fields = match adt {
        hir::Adt::Struct(it) => it.fields(db),
        hir::Adt::Union(it) => it.fields(db),
        hir::Adt::Enum(it) => it.variants(db).into_iter().flat_map(|it| it.fields(db)).collect(),
    };
    // alloc: the fields of one candidate type
    fields.into_iter().map(|it| it.ty(db)).collect()
}

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, item: ast::Adt<'t>) {
    if !cx.on_blaze(LINT) || cx.expansion().is_some() || !cx.is_written(item.syntax()) {
        return;
    }
    let Some(adt) = cx.adt_def(item) else { return };
    let db = cx.db();
    if adt.has_non_default_type_params(db) || cx.is_in_test(item.syntax()) {
        return;
    }
    let ty = adt.ty(db);
    if ty.is_copy(db) {
        return;
    }
    let (Some(clone), Some(eq), Some(hash)) = (
        cx.lang_trait(hir::LangItem::Clone),
        cx.lang_trait(hir::LangItem::PartialEq),
        cx.std_trait(&["core", "hash", "Hash"]),
    ) else {
        return;
    };
    // `Eq` has no lang item; a map's key needs it, and the use as a key below says it is there.
    if !ty.impls_trait(db, clone, &[])
        || !ty.impls_trait(db, eq, std::slice::from_ref(&ty))
        || !ty.impls_trait(db, hash, &[])
    {
        return;
    }
    let mut recursive = false;
    let deepest = field_types(db, adt)
        .iter()
        .map(|it| depth(db, it, adt, false, DEPTH, &mut recursive))
        .max();
    let deep = deepest.unwrap_or(0) >= 2;
    if !recursive && !deep {
        return;
    }
    let uses = intern_uses(db, cx.krate().base());
    let Some(count) = uses.0.get(&AdtId::from(adt)).copied() else { return };
    let least = u64::try_from(cx.conf().int("intern-candidate-uses", USES as i64)).unwrap_or(USES);
    let used = u64::from(count.compared) + u64::from(count.cloned);
    if count.keyed == 0 || used < least {
        return;
    }
    let target = cx.krate().to_display_target(db);
    let shape = match recursive {
        true => "recursive",
        false => "deep",
    };
    let message = format!(
        "`{}` is a {shape} `Hash + Eq + Clone` type used as a map key and compared or cloned in {used} places of the \
         crate ({} `==`, {} clones)",
        ty.display(db, target),
        count.compared,
        count.cloned,
    );
    let note = "each `==` walks both values, each hash walks one, each clone copies one; an interned value is \
                stored once and named by a small `Copy` id, so `==` and hashing compare one integer";
    let at = item.name().map_or(item.syntax(), |it| it.syntax());
    let Some(range) = cx.range(at) else { return };
    let span = cx.span(range);
    cx.blaze(LINT, item.syntax(), span, message)
        .note(note)
        .help(LINT.help())
        .help(format!("for further information visit {}", LINT.docs_url()));
}

#[cfg(test)]
mod tests {
    use super::super::perf_tests::perf_only_with;

    const LINT: &str = "intern_candidate";
    const FOUND: &str = "used as a map key and compared or cloned";

    #[test]
    fn a_recursive_key_compared_often_is_found_and_one_compared_rarely_is_not() {
        let found = perf_only_with(
            "sized, copy, clone, option, fn, iterator, iterators, slice, deref, drop, sync, send, builtin_impls, index, eq, hash",
            LINT,
            r#"
use std::collections::HashMap;
pub enum Expr { Leaf(u32), Node(Vec<Expr>) }
impl Clone for Expr { fn clone(&self) -> Expr { loop {} } }
impl PartialEq for Expr { fn eq(&self, other: &Expr) -> bool { true } }
impl Eq for Expr {}
impl core::hash::Hash for Expr { fn hash<H: core::hash::Hasher>(&self, state: &mut H) {} }
pub enum Rare { Leaf(u32), Node(Vec<Rare>) }
impl Clone for Rare { fn clone(&self) -> Rare { loop {} } }
impl PartialEq for Rare { fn eq(&self, other: &Rare) -> bool { true } }
impl Eq for Rare {}
impl core::hash::Hash for Rare { fn hash<H: core::hash::Hasher>(&self, state: &mut H) {} }
pub fn seen(m: &HashMap<Expr, u32>, r: &HashMap<Rare, u32>, a: &Expr, b: &Expr, x: &Rare) -> u32 {
    let mut n = 0;
    if *a == *b { n += 1; }
    if *a == *b { n += 1; }
    if *a != *b { n += 1; }
    if *b == *a { n += 1; }
    if a.eq(b) { n += 1; }
    let c = a.clone();
    let d = b.clone();
    let e = c.clone();
    if *x == *x { n += 1; }
    if m.contains_key(a) && r.contains_key(x) { n += 1; }
    n
}
"#,
        );
        assert_eq!(found.matches(FOUND).count(), 1, "{found}");
        assert!(found.contains("`Expr` is a recursive `Hash + Eq + Clone` type"), "{found}");
    }
}
