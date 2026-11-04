//! `small_map`: a hash map or set, or a `BTreeMap`/`BTreeSet`, that provably holds a few
//! entries ([`MAX`], `small-map-max-entries` in `clippy.toml`), each under a key known at
//! compile time. Hashing a key and probing a table costs more than comparing it with a few
//! others, and the table is an allocation: a sorted array searched (binary or linear), or a
//! `match` on the key, does the lookup without either.
//!
//! - **A local**: `let m = ..` built empty (`new()`, `default()`, `with_capacity(n)` with a
//!   literal `n` of at most [`MAX`]), or from a literal array (`HashMap::from([..])`,
//!   `[..].into_iter().collect()`) of at most [`MAX`] entries, each key a literal, a `const`
//!   or a unit variant. Every use of the local is a read (`get`, `contains_key`, `contains`,
//!   `len`, `is_empty`, `iter`, `keys`, `values`, ...) or an `insert` of such a key that runs
//!   once (not in a loop or closure). Anything else (passing it on, returning it, `&mut`, an
//!   `extend`, an `entry`, an insert in a loop or of a computed key) leaves it alone, so a map
//!   whose keys come from input or that a public API sees is never found.
//! - **A private field** (any visibility but `pub`) of a struct: the crate's every body is read
//!   once ([`small_fields`]); every struct literal builds it as a local above is built, every
//!   insert's key is constant, at most [`MAX`] distinct keys across the crate, and every other
//!   use is a read, a comparison, a clone, or a borrow handed to `core`'s formatting or hashing
//!   (what derives write). A pattern that binds it, a move, an assignment or a borrow handed
//!   elsewhere leaves it alone.
//!
//! Concurrent maps (`papaya`, `dashmap`) are not checked. No fix: the lookup's code changes
//! with the representation.

use base_db::Db;
use blaze_hash::HashMap;
use hir::{HasVisibility as _, HirDisplay as _};
use hir_def::{
    ConstId, DefWithBodyId, EnumVariantId, FieldId,
    expr_store::{Body, scope::ExprScopes},
    hir::{
        Array, BinaryOp, BindingAnnotation, Expr, ExprId, Literal, Pat, Statement, UnaryOp,
        type_ref::Mutability,
    },
    nameres::crate_def_map,
    resolver::{ValueNs, resolver_for_scope},
    signatures::FunctionSignature,
};
use std::alloc::Global;
use syntax::view::{self as ast, AstNode as _, HasAttrs as _, HasName as _};

use super::{Finding, Lint, alloc::written_callee, facts, hash_order_output::local_of};
use crate::check::{self, Cx};

const LINT: Lint = Lint::SmallMap;

/// The entries a map may hold to be found, by default: below about eight, a linear scan of
/// keys in one or two cache lines beats hashing one.
const MAX: u64 = 8;

/// How many expressions up a use is followed to the block of its `let`.
const UP: usize = 64;

/// The maps and sets a few entries can replace, by crate.
const MAPS: &[(&str, &[&str])] = &[
    ("std", &["HashMap", "HashSet", "BTreeMap", "BTreeSet"]),
    ("alloc", &["BTreeMap", "BTreeSet"]),
    ("hashbrown", &["HashMap", "HashSet"]),
    ("indexmap", &["IndexMap", "IndexSet"]),
    ("blaze_hash", &["HashMap", "HashSet", "Map", "Set"]),
];

/// The methods that read a map or set and leave it as it was.
const READS: &[&str] = &[
    "get",
    "get_key_value",
    "contains_key",
    "contains",
    "len",
    "is_empty",
    "iter",
    "keys",
    "values",
    "first_key_value",
    "last_key_value",
    "clone",
    "eq",
    "ne",
];

/// The methods that build an empty one.
const EMPTY: &[&str] = &["new", "default", "new_in", "with_hasher", "default_in"];

/// A key known at compile time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Key {
    Literal(Literal),
    Negated(Literal),
    Const(ConstId),
    Variant(EnumVariantId),
}

/// What the crate does with one private map field.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct FieldUse {
    /// The distinct constant keys put in it, up to one past the most any threshold allows.
    pub(crate) keys: Vec<Key>,
    /// A use that is not a read, a constant insert or a small build.
    pub(crate) blocked: bool,
}

/// The fields of map type the crate's bodies use, and how.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Fields(HashMap<FieldId, FieldUse, Global>);

/// How many distinct keys are kept before a field counts as large whatever the threshold.
const KEPT: usize = 64;

fn add_key(keys: &mut Vec<Key>, key: Key) {
    if keys.len() <= KEPT && !keys.contains(&key) {
        keys.push(key);
    }
}

/// Whether `ty`, its references peeled, is one of [`MAPS`]; its name when it is.
fn map_name(db: &Db, ty: &hir::Type<'_>) -> Option<hir::Name> {
    let adt = ty.strip_references().as_adt()?;
    let name = adt.name(db);
    let krate = adt.module(db).krate(db);
    MAPS.iter()
        .any(|(it, names)| names.contains(&name.as_str()) && facts::crate_is(db, krate, it))
        .then_some(name)
}

/// What a body's walk knows.
struct Walk<'a, 'db> {
    db: &'db Db,
    def: DefWithBodyId,
    body: &'a Body,
    infer: &'a hir_ty::InferenceResult<'db>,
    parents: &'a facts::Parents,
}

impl Walk<'_, '_> {
    fn name(&self, call: ExprId) -> Option<&str> {
        let (f, _) = written_callee(self.infer, self.body, call)?;
        Some(FunctionSignature::of(self.db, f).name.as_str())
    }

    fn value_of(&self, expr: ExprId) -> Option<ValueNs> {
        let Expr::Path(path) = self.body.expr(expr) else { return None };
        let scope = ExprScopes::of(self.db, self.def).scope_for(expr);
        resolver_for_scope(self.db, self.def, scope).resolve_path_in_value_ns_fully(
            self.db,
            path,
            self.body.expr_path_hygiene(expr),
        )
    }

    /// The key `expr` is, when it is known at compile time: a literal, a negated one, a
    /// `const` or a unit variant, maybe borrowed.
    fn const_key(&self, expr: ExprId) -> Option<Key> {
        match self.body.expr(expr) {
            Expr::Literal(it) => Some(Key::Literal(it.clone())), // clone: a literal key, a few per map
            Expr::UnaryOp { expr, op: UnaryOp::Neg } => match self.body.expr(expr) {
                Expr::Literal(it) => Some(Key::Negated(it.clone())), // clone: as above
                _ => None,
            },
            Expr::Ref { expr, mutability: Mutability::Shared, .. } => self.const_key(expr),
            Expr::Path(_) => match self.value_of(expr)? {
                ValueNs::ConstId(it) => Some(Key::Const(it)),
                ValueNs::EnumVariantId(it) => Some(Key::Variant(it)),
                _ => None,
            },
            _ => None,
        }
    }

    /// The keys a map is built with by `init`, when it is built small: empty, or from a literal
    /// array of at most `max` entries with constant keys. `set` says each entry is a key.
    fn built_small(&self, init: ExprId, set: bool, max: u64) -> Option<Vec<Key>> {
        let from_array = |array: ExprId| -> Option<Vec<Key>> {
            let Expr::Array(Array::ElementList { elements }) = self.body.expr(array) else {
                return None;
            };
            if elements.len() as u64 > max {
                return None;
            }
            // alloc: the few keys of one small map
            let mut keys = Vec::new();
            for &element in elements {
                let key = match (set, self.body.expr(element)) {
                    (true, _) => element,
                    (false, Expr::Tuple { exprs: [key, _] }) => *key,
                    (false, _) => return None,
                };
                add_key(&mut keys, self.const_key(key)?);
            }
            Some(keys)
        };
        match self.body.expr(init) {
            Expr::Call { args: [], .. } => EMPTY.contains(&self.name(init)?).then(Vec::new),
            Expr::Call { args: [arg], .. } => match self.name(init)? {
                "with_capacity" => match self.body.expr(*arg) {
                    Expr::Literal(Literal::Int(n, _)) => {
                        (*n >= 0 && (*n as u128) <= u128::from(max)).then(Vec::new)
                    }
                    Expr::Literal(Literal::Uint(n, _)) => (*n <= u128::from(max)).then(Vec::new),
                    _ => None,
                },
                "from" | "from_iter" => from_array(*arg),
                name if EMPTY.contains(&name) => Some(Vec::new()),
                _ => None,
            },
            Expr::MethodCall { receiver, args: [], .. } if self.name(init)? == "collect" => {
                let Expr::MethodCall { receiver: array, args: [], .. } = self.body.expr(receiver)
                else {
                    return None;
                };
                (self.name(receiver)? == "into_iter").then(|| from_array(array)).flatten()
            }
            _ => None,
        }
    }

    /// Whether `expr` runs once for each run of `block`: no loop or closure between them.
    fn runs_once(&self, expr: ExprId, block: ExprId) -> bool {
        for it in self.parents.up(expr, UP) {
            if it == block {
                return true;
            }
            if matches!(self.body.expr(it), Expr::Loop { .. } | Expr::Closure { .. }) {
                return false;
            }
        }
        false
    }
}

/// The private map fields `krate`'s bodies use, as the module docs say, once per crate.
#[blaze_query::tracked(returns(ref))]
pub(crate) fn small_fields(db: &Db, krate: base_db::Crate) -> Fields {
    let mut fields = Fields(HashMap::new_in(Global));
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
        fields_in(db, def, &mut fields.0);
    }
    fields
}

/// Adds what the body of `def` does with the crate's map fields to `out`.
fn fields_in(db: &Db, def: DefWithBodyId, out: &mut HashMap<FieldId, FieldUse, Global>) {
    let analysis = hir::BodyAnalysis::new(db, hir::DefWithBody::from(def));
    let body = analysis.body();
    let infer = analysis.infer();
    let parents = facts::Parents::of(body);
    let walk = Walk { db, def, body, infer, parents: &parents };
    let is_map = |id: ExprId| map_name(db, &analysis.ty(infer.expr_ty(id)));
    for (id, expr) in body.exprs() {
        match expr {
            Expr::Field { .. } => {
                let Some(either::Either::Left(field)) = infer.field_resolution(id) else {
                    continue;
                };
                if is_map(id).is_none() {
                    continue;
                }
                let entry = out.entry(field).or_default();
                match field_use(&walk, id) {
                    Some(Some(key)) => add_key(&mut entry.keys, key),
                    Some(None) => {}
                    None => entry.blocked = true,
                }
            }
            Expr::RecordLit { fields, .. } => {
                let Some(variant) = infer.variant_resolution_for_expr(id) else { continue };
                let shape = variant.fields(db);
                for it in fields {
                    let Some(local_id) = shape.field(&it.name) else { continue };
                    let Some(name) = is_map(it.expr) else { continue };
                    let entry = out.entry(FieldId { parent: variant, local_id }).or_default();
                    match walk.built_small(it.expr, name.as_str().ends_with("Set"), KEPT as u64) {
                        Some(keys) => {
                            keys.into_iter().for_each(|key| add_key(&mut entry.keys, key))
                        }
                        None => entry.blocked = true,
                    }
                }
            }
            _ => {}
        }
    }
    // A pattern that binds a field moves or borrows it past what is seen here.
    for (id, pat) in body.pats() {
        let Some(variant) = infer.variant_resolution_for_pat(id) else { continue };
        let shape = variant.fields(db);
        match pat {
            Pat::Record { args, .. } => {
                for arg in args {
                    if !matches!(body.pat(arg.pat), Pat::Wild)
                        && let Some(local_id) = shape.field(&arg.name)
                    {
                        out.entry(FieldId { parent: variant, local_id }).or_default().blocked =
                            true;
                    }
                }
            }
            Pat::TupleStruct { .. } => {
                for (local_id, _) in shape.fields().iter() {
                    out.entry(FieldId { parent: variant, local_id }).or_default().blocked = true;
                }
            }
            _ => {}
        }
    }
}

/// How the map at `at` (a field place or a local's path) is used: `Some(None)` for a read,
/// `Some(Some(key))` for an insert of a constant key, `None` for anything else.
fn field_use(walk: &Walk<'_, '_>, at: ExprId) -> Option<Option<Key>> {
    let parent = walk.parents.of_expr(at)?;
    match walk.body.expr(parent) {
        // A set's `insert` takes the key, a map's the key and its value.
        Expr::MethodCall { receiver, args, .. } if receiver == at => match walk.name(parent)? {
            "insert" => walk.const_key(*args.first()?).map(Some),
            name if READS.contains(&name) => Some(None),
            _ => None,
        },
        Expr::BinaryOp { op: Some(BinaryOp::CmpOp(_)), .. } => Some(None),
        // A borrow handed to `core`'s formatting or hashing: what `#[derive(Debug, Hash)]` write.
        Expr::Ref { mutability: Mutability::Shared, .. } => {
            let mut up = parent;
            while let Some(next) = walk.parents.of_expr(up) {
                match walk.body.expr(next) {
                    Expr::Ref { mutability: Mutability::Shared, .. } => up = next,
                    Expr::Call { .. } | Expr::MethodCall { .. } => {
                        let (f, _) = written_callee(walk.infer, walk.body, next)?;
                        let name = FunctionSignature::of(walk.db, f).name.as_str();
                        let krate = hir::Function::from(f).module(walk.db).krate(walk.db);
                        let core = ["core", "std", "alloc"]
                            .iter()
                            .any(|it| facts::crate_is(walk.db, krate, it));
                        let formats = name.starts_with("debug") || name == "fmt" || name == "hash";
                        return (core && formats).then_some(None);
                    }
                    _ => return None,
                }
            }
            None
        }
        _ => None,
    }
}

/// The body's small local maps.
pub(super) fn check(cx: &mut Cx<'_, '_>) {
    if !cx.on_blaze(LINT) || cx.is_in_test(cx.root()) {
        return;
    }
    let found = find(cx);
    facts::report(cx, found);
}

fn max(cx: &Cx<'_, '_>) -> u64 {
    u64::try_from(cx.conf().int("small-map-max-entries", MAX as i64)).unwrap_or(MAX)
}

fn find(cx: &Cx<'_, '_>) -> Vec<Finding> {
    // alloc: the findings of one body, reported at the edge
    let mut found = Vec::new();
    let Some(analysis) = cx.analysis() else { return found };
    let Ok(def) = DefWithBodyId::try_from(analysis.def()) else { return found };
    let db = cx.db();
    let body = analysis.body();
    let infer = analysis.infer();
    let max = max(cx);
    let target = cx.krate().to_display_target(db);
    let mut parents = None;
    for (block, expr) in body.exprs() {
        let Expr::Block { statements, .. } = expr else { continue };
        for statement in statements.iter() {
            let Statement::Let { pat, initializer: Some(init), else_branch: None, .. } = statement
            else {
                continue;
            };
            let Pat::Bind { id: local, subpat: None } = body.pat(pat) else { continue };
            if matches!(body[local].mode, BindingAnnotation::Ref | BindingAnnotation::RefMut) {
                continue;
            }
            let ty = analysis.ty(infer.expr_ty(init));
            let Some(name) = map_name(db, &ty) else { continue };
            let set = name.as_str().ends_with("Set");
            let parents = parents.get_or_insert_with(|| facts::Parents::of(body));
            let walk = Walk { db, def, body, infer, parents };
            let Some(mut keys) = walk.built_small(init, set, max) else { continue };
            let mut small = true;
            for (id, expr) in body.exprs() {
                if !matches!(expr, Expr::Path(_)) || local_of(db, def, body, id) != Some(local) {
                    continue;
                }
                match field_use(&walk, id) {
                    Some(Some(key)) if walk.runs_once(id, block) => add_key(&mut keys, key),
                    Some(None)
                        if !matches!(
                            walk.parents.of_expr(id).map(|it| body.expr(it)),
                            Some(Expr::Ref { .. })
                        ) => {}
                    _ => {
                        small = false;
                        break;
                    }
                }
            }
            if !small || keys.is_empty() || keys.len() as u64 > max {
                continue;
            }
            let Some(range) = analysis.expr_range(init) else { continue };
            let message = format!(
                "this `{}` holds at most {} entries, each under a key known at compile time",
                ty.display(db, target),
                keys.len()
            );
            let note = format!(
                "hashing a key and probing a table costs more than comparing it with {} others, and the table is an \
                 allocation; nothing passes the map on, so its type can change",
                keys.len()
            );
            facts::push(&mut found, LINT, range, message, Some(note));
        }
    }
    found
}

/// A struct's private map fields that hold a few constant keys across the crate.
pub(super) fn check_struct<'t>(cx: &mut Cx<'_, 't>, item: ast::Struct<'t>) {
    if !cx.on_blaze(LINT)
        || cx.expansion().is_some()
        || !cx.is_written(item.syntax())
        || cx.is_in_test(item.syntax())
    {
        return;
    }
    let Some(hir::Adt::Struct(def)) = cx.adt_def(ast::Adt::Struct(item)) else { return };
    let db = cx.db();
    let max = max(cx);
    let target = cx.krate().to_display_target(db);
    let mut fields = None;
    for (index, field) in def.fields(db).into_iter().enumerate() {
        if matches!(field.visibility(db), hir::Visibility::Public) {
            continue;
        }
        let ty = field.ty(db);
        if map_name(db, &ty).is_none() {
            continue;
        }
        let fields = fields.get_or_insert_with(|| small_fields(db, cx.krate().base()));
        let Some(used) = fields.0.get(&FieldId::from(field)) else { continue };
        if used.blocked || used.keys.is_empty() || used.keys.len() as u64 > max {
            continue;
        }
        let Some(node) = field_node(item, &field.name(db), index) else { continue };
        let Some(range) = cx.range(node) else { continue };
        let message = format!(
            "the field `{}` of `{}` is a `{}` that holds at most {} entries: the crate only puts constant keys in it",
            field.name(db).as_str(),
            def.name(db).as_str(),
            ty.display(db, target),
            used.keys.len()
        );
        let note = "hashing a key and probing a table costs more than comparing it with a few others, and the table \
                    is an allocation; the field is private, so its type can change";
        let span = cx.span(range);
        cx.blaze(LINT, node, span, message)
            .note(note)
            .help(LINT.help())
            .help(format!("for further information visit {}", LINT.docs_url()));
    }
}

/// The syntax of the field named `name`, the `index`th of `item`.
fn field_node<'t>(item: ast::Struct<'t>, name: &hir::Name, index: usize) -> Option<ast::Node<'t>> {
    match item.field_list()? {
        ast::FieldList::RecordFieldList(list) => list
            .fields()
            .find(|it| it.name().is_some_and(|n| n.text() == name.as_str()))
            .map(|it| it.syntax()),
        ast::FieldList::TupleFieldList(list) => {
            if list.fields().any(|it| it.attrs().next().is_some()) {
                return None;
            }
            list.fields().nth(index).map(|it| it.syntax())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::perf_tests::perf_only;

    const LINT: &str = "small_map";
    const FOUND: &str = "entries, each under a key known at compile time";

    #[test]
    fn a_local_map_of_a_few_constant_keys_is_found_and_one_filled_in_a_loop_is_not() {
        let found = perf_only(
            LINT,
            r#"
use std::collections::HashMap;
const RED: u32 = 1;
pub fn small(k: u32) -> bool {
    let mut m = HashMap::with_capacity(4);
    m.insert(RED, 10u32);
    m.insert(2, 20);
    m.insert(3, 30);
    m.contains_key(&k)
}
pub fn looped(xs: &[u32], k: u32) -> bool {
    let mut m = HashMap::with_capacity(4);
    for x in xs.iter() {
        m.insert(*x, 1u32);
    }
    m.contains_key(&k)
}
pub fn handed(k: u32) -> HashMap<u32, u32> {
    let mut m = HashMap::with_capacity(4);
    m.insert(1, 1);
    m
}
"#,
        );
        assert_eq!(found.matches(FOUND).count(), 1, "{found}");
        assert!(found.contains("holds at most 3 entries"), "{found}");
    }
}
