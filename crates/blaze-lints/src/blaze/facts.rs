//! What `error_debug`, `dropped_error`, `untyped_error`, `hash_order_output` and the lints of
//! waits (`held_across_wait`, `lock_across_query`, `blocking_on_pool`, `unnamed_wait`) ask of a
//! body or a type: which crate an item is of, whether a type is a hash map or set or holds some
//! ADT, what expression each one is inside, where a dependency's item is, and how a body's
//! findings are reported.

use base_db::Db;
use blaze_hash::HashMap;
use hir::AsAssocItem as _;
use hir_def::{FunctionId, ModuleDefId, ModuleId, expr_store::Body, hir::ExprId, nameres::crate_def_map, per_ns::PerNs};
use hir_expand::name::Name;
use hir_ty_mir::mir::MirSpan;
use intern::Symbol;
use syntax::{NodeOrToken, TextRange};

use super::{Finding, Lint};
use crate::check::Cx;

/// Whether `krate`'s name is `name`: how a lint knows `hir_ty`, `anyhow` or `hashbrown` when
/// the crate linted names them only through other crates.
pub(crate) fn crate_is(db: &Db, krate: hir::Crate, name: &str) -> bool {
    krate.base().extra_data(db).display_name.as_ref().is_some_and(|it| it.crate_name().symbol().as_str() == name)
}

/// Whether `adt` is the item `name` of the crate `krate`.
pub(crate) fn adt_is(db: &Db, adt: hir::Adt, krate: &str, name: &str) -> bool {
    adt.name(db).as_str() == name && crate_is(db, adt.module(db).krate(db), krate)
}

/// The hash maps and sets of crates other than std, by crate: their iteration order is their
/// hasher's and their table's, never their keys'.
const HASH_CRATES: &[(&str, &[&str])] = &[
    ("hashbrown", &["HashMap", "HashSet", "HashTable"]),
    ("papaya", &["HashMap", "HashSet"]),
    ("dashmap", &["DashMap", "DashSet"]),
    ("blaze_hash", &["HashMap", "HashSet"]),
];

/// Whether `ty`, its references peeled, is a hash map or set: std's `HashMap` or `HashSet`
/// with any hasher (`FxHashMap` is one), or one of [`HASH_CRATES`]'s.
pub(crate) fn hash_collection(cx: &Cx<'_, '_>, ty: &hir::Type<'_>) -> bool {
    let Some(adt) = ty.strip_references().as_adt() else { return false };
    let std = |path: &[&str]| cx.std_def(path) == Some(hir::ModuleDef::Adt(adt));
    if std(&["std", "collections", "HashMap"]) || std(&["std", "collections", "HashSet"]) {
        return true;
    }
    let db = cx.db();
    let krate = adt.module(db).krate(db);
    let name = adt.name(db);
    HASH_CRATES.iter().any(|(it, names)| names.contains(&name.as_str()) && crate_is(db, krate, it))
}

/// The expression each expression of a body is a child of.
pub(super) struct Parents(HashMap<ExprId, ExprId>);

impl Parents {
    pub(super) fn of(body: &Body) -> Parents {
        // alloc: one map per body a lint of errors or order is on for, once per edit of it
        let mut parents = HashMap::default();
        for (id, _) in body.exprs() {
            body.walk_child_exprs(id, |child| {
                parents.insert(child, id);
            });
        }
        Parents(parents)
    }

    pub(super) fn of_expr(&self, expr: ExprId) -> Option<ExprId> {
        self.0.get(&expr).copied()
    }

    /// `expr`'s parents, innermost first, at most `most` of them.
    pub(super) fn up(&self, expr: ExprId, most: usize) -> impl Iterator<Item = ExprId> + '_ {
        std::iter::successors(self.of_expr(expr), |&it| self.of_expr(it)).take(most)
    }
}

/// Reports `found`, each at the node of the walked tree that covers its range, its level asked
/// there.
pub(super) fn report(cx: &mut Cx<'_, '_>, found: Vec<Finding>) {
    let root = cx.root();
    for finding in found {
        let at = match root.text_range().contains_range(finding.range) {
            true => match root.covering_element(finding.range) {
                NodeOrToken::Node(it) => it,
                NodeOrToken::Token(it) => it.parent().unwrap_or(root),
            },
            false => root,
        };
        let span = cx.span(finding.range);
        let diagnostic = cx.blaze(finding.lint, at, span, finding.message);
        if let Some(note) = finding.note {
            diagnostic.note(note);
        }
        diagnostic
            .help(finding.lint.help())
            .help(format!("for further information visit {}", finding.lint.docs_url()));
    }
}

/// Adds a finding of `lint` at `range` to `found`, once per lint and range.
pub(super) fn push(found: &mut Vec<Finding>, lint: Lint, range: TextRange, message: String, note: Option<String>) {
    if !found.iter().any(|it| it.lint == lint && it.range == range) {
        found.push(Finding { lint, range, message, note });
    }
}

/// How many ADTs deep [`holds`] looks into fields.
pub(crate) const HOLDS_DEPTH: u32 = 4;

/// The first ADT of `ty` that `is` says yes to: `ty` itself, or what it holds through
/// references, tuples, arrays, slices and the fields of its ADTs (an enum's through its type
/// arguments), at most `depth` ADTs deep.
pub(crate) fn holds<'db>(
    db: &'db Db,
    ty: &hir::Type<'db>,
    depth: u32,
    is: &mut impl FnMut(hir::Adt) -> bool,
) -> Option<hir::Adt> {
    let ty = ty.strip_references();
    if let Some(inner) = ty.as_slice() {
        return holds(db, &inner, depth, &mut *is);
    }
    if let Some((inner, _)) = ty.as_array(db) {
        return holds(db, &inner, depth, &mut *is);
    }
    if ty.is_tuple() {
        return ty.tuple_fields(db).iter().find_map(|it| holds(db, it, depth, &mut *is));
    }
    let adt = ty.as_adt()?;
    if is(adt) {
        return Some(adt);
    }
    if depth == 0 {
        return None;
    }
    match adt {
        hir::Adt::Enum(_) => ty.type_arguments().find_map(|it| holds(db, &it, depth - 1, &mut *is)),
        hir::Adt::Struct(_) | hir::Adt::Union(_) => {
            ty.fields(db).iter().find_map(|(_, it)| holds(db, it, depth - 1, &mut *is))
        }
    }
}

/// The ADT whose impl `f` is a function of, inherent or a trait's, its references peeled.
pub(crate) fn impl_adt(db: &Db, f: FunctionId) -> Option<hir::Adt> {
    hir::Function::from(f).as_assoc_item(db)?.implementing_ty(db)?.strip_references().as_adt()
}

/// Where the MIR span `span` of the body `analysis` is of is written, for an expression.
pub(crate) fn mir_range(analysis: &hir::BodyAnalysis<'_>, span: MirSpan) -> Option<TextRange> {
    match span {
        MirSpan::ExprId(it) | MirSpan::ExprEnd(it) => analysis.expr_range(it),
        MirSpan::PatId(_) | MirSpan::BindingId(_) | MirSpan::SelfParam | MirSpan::Unknown | MirSpan::Inlined(_) => {
            None
        }
    }
}

/// The type at the public `path` of the crate named `name`, `krate` itself or one of its
/// dependencies, through its modules' scopes and re-exports: how a lint finds `blaze_graph`'s
/// wait API wherever it is defined inside the crate.
pub(crate) fn dep_type(db: &Db, krate: base_db::Crate, name: &str, path: &[&str]) -> Option<ModuleDefId> {
    let (module, last) = dep_module(db, krate, name, path)?;
    scope_get(db, module, last).take_types()
}

/// The function at the public `path` of the crate named `name`, as [`dep_type`] finds a type.
pub(crate) fn dep_fn(db: &Db, krate: base_db::Crate, name: &str, path: &[&str]) -> Option<FunctionId> {
    let (module, last) = dep_module(db, krate, name, path)?;
    match scope_get(db, module, last).take_values()? {
        ModuleDefId::FunctionId(it) => Some(it),
        _ => None,
    }
}

/// The module of all but the last segment of `path` in the crate named `name`, and the last.
fn dep_module<'p>(db: &Db, krate: base_db::Crate, name: &str, path: &[&'p str]) -> Option<(ModuleId, &'p str)> {
    let is = |it: base_db::Crate| crate_is(db, hir::Crate::from(it), name);
    let home = match is(krate) {
        true => krate,
        false => krate.data(db).dependencies.iter().map(|dep| dep.crate_id).find(|&it| is(it))?,
    };
    let (last, modules) = path.split_last()?;
    let mut module = crate_def_map(db, home).root_module_id();
    for segment in modules {
        match scope_get(db, module, segment).take_types()? {
            ModuleDefId::ModuleId(it) => module = it,
            _ => return None,
        }
    }
    Some((module, last))
}

fn scope_get(db: &Db, module: ModuleId, name: &str) -> PerNs {
    let name = Name::new_symbol_root(Symbol::intern(name));
    module.def_map(db).module(db, module).scope(db).get(&name)
}
