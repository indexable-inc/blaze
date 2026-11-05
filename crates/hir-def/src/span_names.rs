//! What a query span on a body is of: the body's crate and its path, `crate::module::item`, in
//! the span's fields `krate` and `function`, so a profile can say which function a slow
//! `infer_query` was for. They are written only while a subscriber records the span; the
//! profiler interns them, so its rows hold a number for each.
//!
//! No two bodies of a crate share a path, or a profile adds up different bodies' times as one
//! (17 `<impl NonZero>::swap_bytes` were one row): an impl is named by its ordinal among its
//! file's impls, `<impl#3>`; a block by its body and its ordinal among that item's blocks,
//! `outer::{block#2}`; a `const _` by its ordinal, `_#1`; a closure, an `async` block and an
//! anonymous const by their expression in the body, `outer::{closure#14}`; a node a macro
//! wrote also by the call, `<impl#0 in macro call 3fa2.5>`, and by each enclosing call out to
//! the source file when the call is in a macro's output too,
//! `<impl#0 in macro call c6a9.0 in macro call 77e1.3>`; a build script by its package,
//! `build_script[serde]::main`. The path is written into the span's one buffer: no part of it
//! takes a buffer of its own.
//!
//! Recording a span never changes the query graph, or a query's reuse would depend on whether
//! a profiler is on. A name is read only from what no query lowers anew for it: interned ids
//! and their ast ids, a module's and a block's identity, the item tree, an impl's or a trait's
//! item list and the crate's inputs, each already made for the body being named; never a
//! signature (so an impl has no type in its name). And it is read in
//! [`blaze_query::db::for_span_text`], so the span's query records none of those reads.

use std::fmt::Write as _;

use base_db::{Crate, CrateOrigin, Db, TargetKind};
use hir_expand::{AstId, HirFileId, name::Name};
use span::{AstIdNode, ErasedFileAstId};

use crate::{
    AdtId, AssocItemId, BlockIdLt, DefWithBodyId, ExpressionStoreOwnerId, GenericDefId, HasModule,
    ItemContainerId, ItemLoc, Lookup, ModuleId, ModuleIdLt, VariantId,
    hir::ExprId,
    item_tree::{ItemTree, SpanItem, block_item_tree_query, file_item_tree},
};

#[cfg(test)]
mod tests;

/// A span named `$name` for the body `$def`, its fields `krate` and `function` written while a
/// subscriber records it.
#[macro_export]
macro_rules! body_span {
    ($db:expr, $def:expr, $name:literal) => {
        $crate::span_names::of_body(
            $db,
            $def,
            ::tracing::info_span!(
                $name,
                krate = ::tracing::field::Empty,
                function = ::tracing::field::Empty
            ),
        )
    };
}

/// `span` with `def`'s crate and path written in it, if a subscriber records it. The span
/// declares the fields `krate` and `function`.
pub fn of_body(db: &Db, def: DefWithBodyId, span: tracing::Span) -> tracing::Span {
    if !span.is_disabled() {
        name(db, def.krate(db), &span, |text| write_body_path(db, def, text));
    }
    span
}

/// [`of_body`] with what `then` writes after the path in `function`: a body compiled more than
/// one way names each way as a key of its own, as a copy of a function specialized on constant
/// arguments (`hir-ty-mir`'s `Specialize`) does with what the copy knows. `then` reads no
/// query, as nothing that names a span may.
pub fn of_body_then(
    db: &Db,
    def: DefWithBodyId,
    span: tracing::Span,
    then: impl FnOnce(&mut String),
) -> tracing::Span {
    if !span.is_disabled() {
        name(db, def.krate(db), &span, |text| {
            write_body_path(db, def, text);
            then(text);
        });
    }
    span
}

/// A body written as an expression of another's store, named by that expression.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InnerBody {
    /// An anonymous const: an array length, a const argument, an enum discriminant.
    Const,
    Closure,
    /// An `async` block (or `gen` block), as its coroutine.
    Coroutine,
    /// An `async` closure's block called by value, apart from [`Self::Coroutine`] of it.
    CoroutineByMove,
}

impl InnerBody {
    fn label(self) -> &'static str {
        match self {
            InnerBody::Const => "const",
            InnerBody::Closure => "closure",
            InnerBody::Coroutine => "async",
            InnerBody::CoroutineByMove => "async by move",
        }
    }
}

/// `span` named for the `inner` body at `expr` of `owner`'s store: `{closure#14}` after the
/// path of the store's owner, `#14` the expression's index in the store, which no other
/// expression of it has. The span declares `krate` and `function`.
pub fn of_inner(
    db: &Db,
    owner: ExpressionStoreOwnerId,
    expr: ExprId,
    inner: InnerBody,
    span: tracing::Span,
) -> tracing::Span {
    if !span.is_disabled() {
        name(db, owner.krate(db), &span, |text| write_inner_path(db, owner, expr, inner, text));
    }
    span
}

/// `span` named for the file `file` read as part of `krate`: the crate in `krate`, and in
/// `function` the file's path (a macro file by its call, `{macro <id>}`), so the same file
/// lowered for many crates reads as repeats of one key. The span declares `krate` and `function`.
pub fn of_file(db: &Db, file: HirFileId, krate: Crate, span: tracing::Span) -> tracing::Span {
    if !span.is_disabled() {
        name(db, krate, &span, |text| match file {
            HirFileId::FileId(it) => match db.path_for_file(it.file_id(db)) {
                Some(path) => _ = write!(text, "{path}"),
                None => _ = write!(text, "{{file {}}}", it.file_id(db).index()),
            },
            HirFileId::MacroFile(call) => _ = write!(text, "{{macro {call:?}}}"),
        });
    }
    span
}

/// Writes `krate`'s label ([`write_crate_label`]) and the path `path` writes in `span`'s fields
/// `krate` and `function`, reading nothing into the running query. The path's queries may name
/// spans of their own, so each span has its own buffer.
fn name(db: &Db, krate: Crate, span: &tracing::Span, path: impl FnOnce(&mut String)) {
    // alloc: only while a subscriber records the span, which interns it
    let mut text = String::new();
    span_text(db, &mut text, |db, text| write_crate_label(db, krate, text));
    let krate_len = text.len();
    span_text(db, &mut text, |_, text| path(text));
    span.record("krate", &text[..krate_len]);
    span.record("function", &text[krate_len..]);
}

/// Appends what `write` writes to `out`, its reads kept off the running query.
#[expect(
    clippy::disallowed_methods,
    reason = "a span's name is tracing text that never reaches a query's value; its reads would put edges on the named query only while a profiler is on"
)]
fn span_text(db: &Db, out: &mut String, write: impl FnOnce(&Db, &mut String)) {
    blaze_query::db::for_span_text(db, out, write);
}

/// `span`, a span that declares `krate`, with `krate`'s label written in it
/// ([`write_crate_label`]), reading nothing into the running query.
pub fn with_crate(db: &Db, krate: Crate, span: tracing::Span) -> tracing::Span {
    if !span.is_disabled() {
        // alloc: only while a subscriber records the span, which interns it
        let mut text = String::new();
        span_text(db, &mut text, |db, text| write_crate_label(db, krate, text));
        span.record("krate", text.as_str());
    }
    span
}

/// What a span's `krate` says: the crate as one build of it, `name vVERSION (kind[, for])`, so
/// the copies of one package a build makes are told apart, where its name alone made a survey
/// read different bodies as work done again: `serde v1.0.219 (lib)`, its `cfg(test)` copy
/// `serde v1.0.219 (lib-test)`, the proc macros' wasm copy `serde v1.0.219 (lib, wasm)`, a
/// `--target` copy `serde v1.0.219 (lib, x86_64-unknown-linux-gnu)`, and each version apart.
/// No version for a crate with none; [`Crate::named`] for the name.
pub fn write_crate_label(db: &Db, krate: Crate, out: &mut String) {
    let (data, extra) = (krate.data(db), krate.extra_data(db));
    _ = write!(out, "{}", krate.named(db));
    if let Some(version) = &extra.version {
        out.push_str(" v");
        out.push_str(version);
    }
    out.push_str(" (");
    out.push_str(kind_label(data.target_kind, data.manifest_kind));
    match (&extra.built_for.platform, extra.built_for.macro_wasm) {
        (_, true) => out.push_str(", wasm"),
        (Some(triple), false) => {
            out.push_str(", ");
            out.push_str(triple.as_str());
        }
        (None, false) => {}
    }
    out.push(')');
}

/// A crate's kind in its label: what it is built as, and a `--test` or `--bench` crate's
/// target before it (`lib-test`), as cargo names a unit's target and mode.
fn kind_label(kind: TargetKind, manifest: TargetKind) -> &'static str {
    match (kind, manifest) {
        (TargetKind::Test, TargetKind::Lib { .. }) => "lib-test",
        (TargetKind::Test, TargetKind::Bin) => "bin-test",
        (TargetKind::Test, TargetKind::Example) => "example-test",
        (TargetKind::Test, TargetKind::Bench) | (TargetKind::Bench, TargetKind::Bench) => "bench",
        (TargetKind::Bench, TargetKind::Lib { .. }) => "lib-bench",
        (TargetKind::Bench, _) => "bench",
        (TargetKind::Test, _) => "test",
        (TargetKind::Lib { is_proc_macro: true }, _) => "proc-macro",
        (TargetKind::Lib { is_proc_macro: false }, _) => "lib",
        (TargetKind::Bin, _) => "bin",
        (TargetKind::Example, _) => "example",
        (TargetKind::BuildScript, _) => "build-script",
        (TargetKind::Other, _) => "other",
    }
}

/// The first segment of a path in `krate`: its name, or for a build script, whose crate cargo
/// names `build_script_build` in every package, `build_script[<package>]`.
fn write_crate(db: &Db, krate: Crate, out: &mut String) {
    let data = krate.data(db);
    match (data.target_kind, &data.origin) {
        (
            TargetKind::BuildScript,
            CrateOrigin::Local { name: Some(package), .. }
            | CrateOrigin::Library { name: package, .. }
            | CrateOrigin::Rustc { name: package },
        ) => {
            out.push_str("build_script[");
            out.push_str(package.as_str());
            out.push(']');
        }
        _ => _ = write!(out, "{}", krate.named(db)),
    }
}

/// Appends the path of `def`: its crate, its modules (a block under its body), the trait or
/// impl it is in, and its name; an enum's variant under its enum.
pub fn write_body_path(db: &Db, def: DefWithBodyId, out: &mut String) {
    match def {
        DefWithBodyId::FunctionId(it) => {
            let loc = it.lookup(db);
            write_assoc_path(db, loc.container, loc.id, AssocItemId::FunctionId(it), out);
        }
        DefWithBodyId::ConstId(it) => {
            let loc = it.lookup(db);
            write_assoc_path(db, loc.container, loc.id, AssocItemId::ConstId(it), out);
        }
        DefWithBodyId::StaticId(it) => {
            let loc = it.lookup(db);
            container_path(db, loc.container, out);
            write_item_name(db, module_of(db, loc.container), loc.id, out);
        }
        DefWithBodyId::VariantId(it) => {
            let loc = it.lookup(db);
            write_adt_path(db, AdtId::EnumId(loc.parent), out);
            out.push_str("::");
            out.push_str(loc.name.as_str());
        }
    }
}

/// Appends the path of the `inner` body at `expr` of `owner`'s store: the owner's path, then
/// `{closure#N}`; an anonymous const of a signature `{signature const#N}`, of a variant's fields
/// `{field const#N}`, as a signature's store and a body's number their expressions apart.
pub fn write_inner_path(
    db: &Db,
    owner: ExpressionStoreOwnerId,
    expr: ExprId,
    inner: InnerBody,
    out: &mut String,
) {
    let store = match owner {
        ExpressionStoreOwnerId::Body(def) => {
            write_body_path(db, def, out);
            ""
        }
        ExpressionStoreOwnerId::Signature(def) => {
            write_item_path(db, def, out);
            "signature "
        }
        ExpressionStoreOwnerId::VariantFields(variant) => {
            match variant {
                VariantId::EnumVariantId(it) => {
                    write_body_path(db, DefWithBodyId::VariantId(it), out)
                }
                VariantId::StructId(it) => write_adt_path(db, it.into(), out),
                VariantId::UnionId(it) => write_adt_path(db, it.into(), out),
            }
            "field "
        }
    };
    _ = write!(out, "::{{{store}{}#{}}}", inner.label(), expr.into_raw().into_u32());
}

/// Appends the path of the item whose signature `def` is.
fn write_item_path(db: &Db, def: GenericDefId, out: &mut String) {
    match def {
        GenericDefId::AdtId(it) => write_adt_path(db, it, out),
        GenericDefId::ConstId(it) => write_body_path(db, it.into(), out),
        GenericDefId::FunctionId(it) => write_body_path(db, it.into(), out),
        GenericDefId::StaticId(it) => write_body_path(db, it.into(), out),
        GenericDefId::ImplId(it) => write_module_item(db, it.lookup(db), out),
        GenericDefId::TraitId(it) => write_module_item(db, it.lookup(db), out),
        GenericDefId::TypeAliasId(it) => {
            let loc = it.lookup(db);
            write_assoc_path(db, loc.container, loc.id, AssocItemId::TypeAliasId(it), out);
        }
    }
}

fn write_adt_path(db: &Db, adt: AdtId, out: &mut String) {
    match adt {
        AdtId::StructId(it) => write_module_item(db, it.lookup(db), out),
        AdtId::UnionId(it) => write_module_item(db, it.lookup(db), out),
        AdtId::EnumId(it) => write_module_item(db, it.lookup(db), out),
    }
}

/// Appends the path of the module item at `loc`: its module's, then its name.
fn write_module_item<N: AstIdNode>(db: &Db, loc: &ItemLoc<N>, out: &mut String) {
    module_path(db, loc.container, out);
    out.push_str("::");
    write_item_name(db, loc.container, loc.id, out);
}

/// Appends the path of `item`, whose id is `id`, in `container`: a trait's or an impl's item by
/// the name its item list gives it, any other by its item tree's.
fn write_assoc_path<N: AstIdNode>(
    db: &Db,
    container: ItemContainerId,
    id: AstId<N>,
    item: AssocItemId,
    out: &mut String,
) {
    container_path(db, container, out);
    let listed = match container {
        ItemContainerId::TraitId(it) => Some(listed_name(&it.trait_items(db).items, item)),
        ItemContainerId::ImplId(it) => Some(listed_name(&it.impl_items(db).items, item)),
        ItemContainerId::ModuleId(_) | ItemContainerId::ExternBlockId(_) => None,
    };
    match listed {
        Some(Some(name)) => out.push_str(name.as_str()),
        Some(None) => write_unknown(id.value.erase(), out),
        None => write_item_name(db, module_of(db, container), id, out),
    }
}

/// The name a trait's or an impl's item list gives `item`.
fn listed_name(items: &[(Name, AssocItemId)], item: AssocItemId) -> Option<&Name> {
    items.iter().find(|(_, it)| *it == item).map(|(name, _)| name)
}

/// The module whose item tree holds the items of `container`.
fn module_of(db: &Db, container: ItemContainerId) -> ModuleId {
    match container {
        ItemContainerId::ModuleId(it) => it,
        ItemContainerId::ExternBlockId(it) => it.lookup(db).container,
        ItemContainerId::TraitId(it) => it.lookup(db).container,
        ItemContainerId::ImplId(it) => it.lookup(db).container,
    }
}

/// Appends the name of the item `id` declared in `module`, read off the item tree that holds
/// it: an unnamed one (`const _`) as `_#N`, an impl as `<impl#N>`.
fn write_item_name<N: AstIdNode>(db: &Db, module: ModuleId, id: AstId<N>, out: &mut String) {
    let erased = id.value.erase();
    match tree_of(db, module, id.file_id).span_item(erased) {
        Some(SpanItem::Named(name)) => out.push_str(name.as_str()),
        Some(SpanItem::Unnamed) => {
            out.push('_');
            write_ordinal(db, id.file_id, erased, out);
        }
        Some(SpanItem::Impl(ordinal)) => {
            _ = write!(out, "<impl#{ordinal}");
            write_macro_call(db, id.file_id, out);
            out.push('>');
        }
        None => write_unknown(erased, out),
    }
}

/// An item its tree or item list does not have, by its ast id, `{item 3fa2.0}`: a bug in which
/// tree [`tree_of`] picks, written out rather than guessed at.
fn write_unknown(id: ErasedFileAstId, out: &mut String) {
    _ = write!(out, "{{item {:04x}.{}}}", id.hash_value(), id.index());
}

/// The item tree of `file` that holds the items `module` declares there: its block's, for a
/// module of a block's def map written in the block, else the file's.
fn tree_of(db: &Db, module: ModuleId, file: HirFileId) -> &ItemTree {
    let krate = module.krate(db);
    match module.block(db) {
        Some(block) if block.ast_id(db).file_id == file => {
            block_item_tree_query(db, block.to_static(), krate)
        }
        _ => file_item_tree(db, file, krate),
    }
}

/// Appends the path of the modules, and the trait or impl, an item in `container` is under,
/// ending in `::`.
fn container_path(db: &Db, container: ItemContainerId, out: &mut String) {
    match container {
        ItemContainerId::ModuleId(it) => module_path(db, it, out),
        ItemContainerId::ExternBlockId(it) => module_path(db, it.lookup(db).container, out),
        ItemContainerId::TraitId(it) => write_module_item(db, it.lookup(db), out),
        ItemContainerId::ImplId(it) => write_module_item(db, it.lookup(db), out),
    }
    out.push_str("::");
}

fn module_path<'db>(db: &'db Db, module: ModuleIdLt<'db>, out: &mut String) {
    let Some(parent) = module.containing_module(db) else {
        return write_crate(db, module.krate(db), out);
    };
    match (module.name(db), module.block(db)) {
        (Some(name), _) => {
            module_path(db, parent, out);
            out.push_str("::");
            out.push_str(name.as_str());
        }
        (None, Some(block)) => write_block(db, block, out),
        (None, None) => {
            module_path(db, parent, out);
            out.push_str("::{module}");
        }
    }
}

/// `{block#N}` after the path of the body the block is in: `N` its ordinal among the blocks of
/// its parent item. A block of no body (a signature's) is after its module, and its parent's
/// hash tells it from a block of another item there: `{block#0 of 3fa2}`.
fn write_block<'db>(db: &'db Db, block: BlockIdLt<'db>, out: &mut String) {
    let owner = block.owner(db);
    match owner {
        Some(owner) => write_body_path(db, owner, out),
        None => module_path(db, block.module(db), out),
    }
    let at = block.ast_id(db);
    let id = at.value.erase();
    out.push_str("::{block");
    write_ordinal(db, at.file_id, id, out);
    if owner.is_none() {
        _ = write!(out, " of {:04x}", id.hash_value());
    }
    out.push('}');
}

/// `#N`, the node's ordinal among its parent's nodes of its kind, and for one a macro wrote the
/// call, ` in derive macro 3fa2.0` (the call's hash and ordinal in its own file): the ordinals
/// of a macro's output count from 0 again.
fn write_ordinal(db: &Db, file: HirFileId, id: ErasedFileAstId, out: &mut String) {
    _ = write!(out, "#{}", id.index());
    write_macro_call(db, file, out);
}

/// ` in macro call 3fa2.5` for a node of `file` when a macro call's expansion is the file: the
/// call by its hash and ordinal in its own file, and, when that file is a macro's output too,
/// the call that wrote it, out to the source file: ` in macro call c6a9.0 in macro call
/// 77e1.3`. A call's ordinal counts within its own file only, so the inner calls of two outer
/// calls (cranelift's `entity_impl!` calling itself, 15 entities) are one ordinal of one hash;
/// without the outer call their bodies shared a name (selfbuild2: 1,691 of the test run's
/// `mir_body_query` "repeats" were such bodies). The chain ends: each macro file is the
/// expansion of a call in a file made before it.
fn write_macro_call(db: &Db, file: HirFileId, out: &mut String) {
    let mut file = file;
    while let HirFileId::MacroFile(call) = file {
        let kind = &call.lookup(db).kind;
        let at = kind.erased_ast_id();
        _ = write!(out, " in {} {:04x}.{}", kind.descr(), at.hash_value(), at.index());
        file = kind.file_id();
    }
}
