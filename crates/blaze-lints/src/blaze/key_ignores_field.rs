//! `key_ignores_field`: a key that leaves a field of the type it is built from out without
//! saying so (AGENTS.md rule 21, keys-cover-every-input: a key covers every input its value
//! depends on).
//!
//! Checked are the bodies of a hand-written `impl Hash`, `impl PartialEq` or `impl Ord` (a
//! `derive` covers every field and has no body to check; an `#[automatically_derived]` impl is
//! skipped too), and of a method that builds a key from `self`: it calls `ContentId::of*`, a
//! method of a type named `Hasher` or of a `Hash`/`Hasher` trait, a constructor of a `*Key`
//! type, or makes a `*Key` with a struct literal; and it is key-shaped: it returns a
//! `ContentId` or a `*Key`, takes a `Hasher`, or is named `key`, `id`, `content_id`,
//! `hash_content`, `hash_into`, `fingerprint`, `*_key` or `key_*`.
//!
//! In such a body a finding is:
//! - a struct pattern of the type (or one of its variants) with `..`, naming what it leaves out;
//! - a body that reads some fields of `self` (`self.a`, or `other.a` of the same type) but not
//!   all, when nothing of the type is handed on whole (a call taking `self` may read the rest);
//! - `field: _` (or `_` in a tuple struct's pattern) with no `// not in key: <why>` on its line
//!   or the line above.
//!
//! The fix: destructure every field, `let Self { a, b, cache: _ } = self;`, with the comment
//! saying why `cache` is not part of the key.

use base_db::Db;
use blaze_hash::HashSet;
use hir::AsAssocItem as _;
use hir_def::{
    AttrDefId, DefWithBodyId, FunctionId, ItemContainerId,
    attrs::AttrFlags,
    hir::{Expr, ExprId, Pat, PatId, UnaryOp},
};
use std::alloc::Global;
use syntax::{TextRange, view as ast};

use ast::{AstNode as _, HasName as _};

use super::{
    Finding, Lint,
    alloc::written_callee,
    exempt,
    facts::{self, Parents},
};
use crate::check::Cx;

/// The rule every message ends with.
const RULE: &str = "a key covers every input its value depends on (AGENTS.md rule 21): name every field, and \
                    mark one left out on purpose `field: _` with `// not in key: <why>`";

/// The tag of the comment that says why a field is left out.
const TAG: &str = "not in key";

/// What builds the key a body is checked for.
enum Kind {
    /// An impl of one of the key traits, by the trait's name.
    Trait(&'static str),
    /// A method that builds a key from `self`.
    Builder,
}

pub(super) fn check(cx: &mut Cx<'_, '_>) {
    if !cx.on_blaze(Lint::KeyIgnoresField) {
        return;
    }
    let found = find(cx);
    facts::report(cx, found);
}

fn find(cx: &Cx<'_, '_>) -> Vec<Finding> {
    // alloc: the findings of one body, reported at the edge
    let mut found = Vec::new();
    let Some(analysis) = cx.analysis() else { return found };
    let Ok(DefWithBodyId::FunctionId(owner)) = DefWithBodyId::try_from(analysis.def()) else {
        return found;
    };
    let db = cx.db();
    let Some((kind, adt)) = kind(cx, db, analysis, owner) else { return found };
    if cx.is_in_test(cx.root()) {
        return found;
    }
    let body = analysis.body();
    let infer = analysis.infer();
    let type_name = adt.name(db);
    let type_name = type_name.as_str();
    let fn_name = hir::Function::from(owner).name(db);
    let what = match kind {
        Kind::Trait(trait_) => format!("`impl {trait_} for {type_name}`"),
        Kind::Builder => format!("the key `{type_name}::{}` builds", fn_name.as_str()),
    };
    let of_self = |ty| analysis.ty(ty).strip_references().as_adt() == Some(adt);
    let fn_range = fn_range(cx);
    let text = cx.text();

    // alloc: the fields one body reads, once per key body checked
    let mut read: HashSet<hir::Name, Global> = HashSet::new_in(Global);
    let mut whole = false;
    let mut rested = false;

    for (_, expr) in body.exprs() {
        if let Expr::Field { expr: base, name } = expr
            && of_self(infer.expr_ty(base))
        {
            read.insert(name.clone()); // clone: an interned name
        }
    }
    let parents = Parents::of(body);
    for (id, expr) in body.exprs() {
        if matches!(expr, Expr::Path(_))
            && of_self(infer.expr_ty(id))
            && handed_on(body, &parents, id)
        {
            whole = true;
        }
    }

    for (pat, shape) in body.pats() {
        let Some(variant) = infer.variant_resolution_for_pat(pat) else { continue };
        let variant = hir::Variant::from(variant);
        if variant.adt(db) != adt {
            continue;
        }
        let fields = variant.fields(db);
        // The fields the pattern names, each with the pattern it binds.
        // alloc: one pattern's fields, once per key pattern
        let mut named: Vec<(hir::Name, PatId)> = Vec::new();
        let rest = match shape {
            Pat::Record { args, ellipsis, .. } => {
                named.extend(args.iter().map(|it| (it.name.clone(), it.pat))); // clone: an interned name
                ellipsis
            }
            Pat::TupleStruct { args, ellipsis, .. } => {
                let split = ellipsis.map_or(args.len(), |it| it as usize);
                let tail = args.len() - split;
                for (at, &arg) in args.iter().enumerate() {
                    let index = if at < split {
                        at
                    } else {
                        fields.len().saturating_sub(tail) + (at - split)
                    };
                    if let Some(field) = fields.get(index) {
                        named.push((field.name(db), arg));
                    }
                }
                ellipsis.is_some()
            }
            _ => continue,
        };
        for (name, arg) in &named {
            read.insert(name.clone()); // clone: an interned name
            if matches!(body.pat(*arg), Pat::Wild) {
                let range = pat_range(analysis, *arg).unwrap_or(fn_range);
                if !exempt::commented(text, range, TAG) {
                    let message = format!(
                        "`{}: _` in {what} leaves `{}` out of the key with no `// {TAG}: <why>`: {RULE}",
                        name.as_str(),
                        name.as_str(),
                    );
                    facts::push(&mut found, Lint::KeyIgnoresField, range, message, None);
                }
            }
        }
        if rest {
            rested = true;
            let missing = missing(db, &fields, |it| named.iter().any(|(name, _)| name == it));
            if !missing.is_empty() {
                let range = pat_range(analysis, pat).unwrap_or(fn_range);
                let message = format!(
                    "`..` in {what} leaves {missing} of `{}` out of the key: {RULE}",
                    variant.name(db).as_str()
                );
                facts::push(&mut found, Lint::KeyIgnoresField, range, message, None);
            }
        }
    }

    if let hir::Adt::Struct(it) = adt
        && !whole
        && !rested
        && !read.is_empty()
    {
        let fields = it.fields(db);
        let missing = missing(db, &fields, |it| read.contains(it));
        if !missing.is_empty() {
            let message = format!(
                "{what} reads some fields of `{type_name}` but leaves {missing} out of the key: {RULE}"
            );
            facts::push(&mut found, Lint::KeyIgnoresField, fn_range, message, None);
        }
    }
    found
}

/// Which key the body of `owner` builds, and of what type: `None` when it builds none.
fn kind(
    cx: &Cx<'_, '_>,
    db: &Db,
    analysis: &hir::BodyAnalysis<'_>,
    owner: FunctionId,
) -> Option<(Kind, hir::Adt)> {
    let ItemContainerId::ImplId(impl_id) = owner.loc(db).container else { return None };
    if AttrFlags::query(db, AttrDefId::ImplId(impl_id)).contains(AttrFlags::AUTOMATICALLY_DERIVED) {
        return None;
    }
    let imp = hir::Impl::from(impl_id);
    let adt = imp.self_ty(db).as_adt()?;
    if let Some(trait_) = imp.trait_(db) {
        let is = |it: Option<hir::Trait>| it == Some(trait_);
        if is(cx.std_trait(&["core", "hash", "Hash"])) {
            return Some((Kind::Trait("Hash"), adt));
        }
        if is(cx.lang_trait(hir::LangItem::PartialEq)) {
            return Some((Kind::Trait("PartialEq"), adt));
        }
        if is(cx.std_trait(&["core", "cmp", "Ord"])) {
            return Some((Kind::Trait("Ord"), adt));
        }
    }
    let body = analysis.body();
    let hir::Adt::Struct(it) = adt else { return None };
    if body.self_param.is_none() || it.fields(db).is_empty() {
        return None;
    }
    let function = hir::Function::from(owner);
    let name = function.name(db);
    let name = name.as_str();
    let shaped = key_type(db, &function.ret_type(db))
        || matches!(
            name,
            "key" | "id" | "content_id" | "hash_content" | "hash_into" | "fingerprint"
        )
        || name.ends_with("_key")
        || name.starts_with("key_")
        || function.assoc_fn_params(db).iter().any(|it| named(db, it.ty(), |it| it == "Hasher"));
    if !shaped {
        return None;
    }
    let infer = analysis.infer();
    let builds = body.exprs().any(|(id, expr)| match expr {
        Expr::Call { .. } | Expr::MethodCall { .. } => {
            written_callee(infer, body, id).is_some_and(|(f, _)| feeds_key(db, f))
        }
        Expr::RecordLit { .. } => key_type(db, &analysis.ty(infer.expr_ty(id))),
        _ => false,
    });
    builds.then_some((Kind::Builder, adt))
}

/// Whether `ty`, its references peeled, is an ADT whose name `is` says yes to.
fn named(db: &Db, ty: &hir::Type<'_>, is: impl Fn(&str) -> bool) -> bool {
    ty.strip_references().as_adt().is_some_and(|it| is(it.name(db).as_str()))
}

/// Whether `ty` is a key: a `ContentId`, or a type named `Key` or `*Key`.
fn key_type(db: &Db, ty: &hir::Type<'_>) -> bool {
    named(db, ty, |it| it == "ContentId" || it.ends_with("Key"))
}

/// Whether a call of `f` feeds a key: a function of `ContentId`, a `Hasher` or a `*Key`, or a
/// method of a `Hash` or `Hasher` trait (its own, or the trait an impl of it is of).
fn feeds_key(db: &Db, f: FunctionId) -> bool {
    if facts::impl_adt(db, f).is_some_and(|it| {
        let name = it.name(db);
        matches!(name.as_str(), "ContentId" | "Hasher") || name.as_str().ends_with("Key")
    }) {
        return true;
    }
    let trait_ = match hir::Function::from(f).as_assoc_item(db).map(|it| it.container(db)) {
        Some(hir::AssocItemContainer::Trait(it)) => Some(it),
        Some(hir::AssocItemContainer::Impl(it)) => it.trait_(db),
        None => None,
    };
    trait_.is_some_and(|it| matches!(it.name(db).as_str(), "Hash" | "Hasher"))
}

/// Whether the path `id`, of the key's type, is handed on whole: to a call, an operator, a
/// return or a value, which may read any field. A field read of it, a `match` on it and a `let`
/// that destructures it are not.
fn handed_on(body: &hir_def::expr_store::Body, parents: &Parents, id: ExprId) -> bool {
    let mut at = id;
    loop {
        let Some(parent) = parents.of_expr(at) else { return true };
        match body.expr(parent) {
            Expr::UnaryOp { op: UnaryOp::Deref, .. } | Expr::Ref { .. } => at = parent,
            Expr::Field { .. } | Expr::Block { .. } | Expr::Let { .. } => return false,
            Expr::Match { expr, .. } => return expr != at,
            _ => return true,
        }
    }
}

/// The fields of `fields` that `has` says no to, as a message names them: "`a`, `b`".
fn missing(db: &Db, fields: &[hir::Field], has: impl Fn(&hir::Name) -> bool) -> String {
    fields
        .iter()
        .map(|it| it.name(db))
        .filter(|it| !has(it))
        .map(|it| format!("`{}`", it.as_str()))
        .collect::<Vec<_>>() // alloc: a finding's names, reported at the edge
        .join(", ")
}

/// Where the pattern `pat` is written, when it is in the walked tree.
fn pat_range(analysis: &hir::BodyAnalysis<'_>, pat: PatId) -> Option<TextRange> {
    let source = analysis.source_map().pat_syntax(pat).ok()?;
    (source.file_id == analysis.tree().1).then(|| source.value.syntax_node_ptr().text_range())
}

/// Where a finding about the whole body goes: the function's name, or the walked item.
fn fn_range(cx: &Cx<'_, '_>) -> TextRange {
    let root = cx.root();
    ast::Fn::cast(root)
        .and_then(|it| it.name())
        .map_or(root.text_range(), |it| it.syntax().text_range())
}

#[cfg(test)]
mod tests {
    use crate::tests::lint;

    const MAIN: &str = r#"
//- minicore: sized, copy, clone, eq, ord, hash, option, derive
//- /src/lib.rs crate:main
use core::hash::{Hash, Hasher};
"#;

    fn lint_main(main: &str) -> String {
        lint(
            &format!("{MAIN}{}", main.trim_start_matches('\n')),
            "-A clippy::all --warn=blaze::key_ignores_field",
        )
    }

    /// A `Hash` that reads two of three fields, an `Eq` with `..`, and a `field: _` with no
    /// comment are found; every field named, one left out with its reason, and a derive are
    /// not.
    #[test]
    fn a_key_impl_that_leaves_a_field_out_is_found() {
        let found = lint_main(
            r#"
pub struct Partial {
    a: u32,
    b: u32,
    cache: u32,
}
impl Hash for Partial {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.a.hash(state);
        self.b.hash(state);
    }
}
pub struct Rest {
    a: u32,
    b: u32,
}
impl PartialEq for Rest {
    fn eq(&self, other: &Rest) -> bool {
        let Rest { a, .. } = self;
        *a == other.a
    }
}
pub struct Silent {
    a: u32,
    b: u32,
}
impl Ord for Silent {
    fn cmp(&self, other: &Silent) -> core::cmp::Ordering {
        let Silent { a, b: _ } = self;
        a.cmp(&other.a)
    }
}
pub struct Named {
    a: u32,
    b: u32,
}
impl Hash for Named {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let Named {
            a,
            b: _, // not in key: a cache of `a`
        } = self;
        a.hash(state);
    }
}
#[derive(PartialEq, Hash)]
pub struct Derived {
    a: u32,
    b: u32,
}
"#,
        );
        assert_eq!(found.matches("AGENTS.md rule 21").count(), 3, "{found}");
        assert!(
            found.contains("`impl Hash for Partial` reads some fields of `Partial` but leaves `cache` out of the key"),
            "{found}"
        );
        assert!(
            found.contains("`..` in `impl PartialEq for Rest` leaves `b` of `Rest` out of the key"),
            "{found}"
        );
        assert!(
            found.contains("`b: _` in `impl Ord for Silent` leaves `b` out of the key with no"),
            "{found}"
        );
        assert!(
            !found.contains("for Named`"),
            "a field left out with its reason is fine:\n{found}"
        );
        assert!(!found.contains("Derived"), "a derive covers every field:\n{found}");
    }

    /// A method that builds a `*Key` from some fields of `self` is found; a getter that hands a
    /// stored key back, and a method that hands `self` on whole, are not.
    #[test]
    fn a_key_builder_that_reads_some_fields_is_found() {
        let found = lint_main(
            r#"
pub struct RunKey {
    a: u32,
}
pub struct Job {
    a: u32,
    b: u32,
    stored: RunKey,
}
impl Job {
    pub fn key(&self) -> RunKey {
        RunKey { a: self.a }
    }
    pub fn stored_key(&self) -> &RunKey {
        &self.stored
    }
    pub fn run_key(&self) -> RunKey {
        RunKey { a: self.a + whole(self) }
    }
}
fn whole(_: &Job) -> u32 {
    0
}
"#,
        );
        assert_eq!(found.matches("AGENTS.md rule 21").count(), 1, "{found}");
        assert!(
            found.contains("the key `Job::key` builds reads some fields of `Job` but leaves `b`, `stored` out of the key"),
            "{found}"
        );
    }
}
