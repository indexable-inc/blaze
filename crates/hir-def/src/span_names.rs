//! What a query span on a body is of: the body's crate and its path, `crate::module::item`, in
//! the span's fields `krate` and `function`, so a profile can say which function a slow
//! `infer_query` was for. They are written only while a subscriber records the span; the
//! profiler interns them, so its rows hold a number for each.

use std::fmt::Write as _;

use base_db::{Crate, Db};
use hir_expand::HirFileId;

use crate::{
    DefWithBodyId, ExpressionStoreOwnerId, HasModule, ItemContainerId, Lookup, ModuleIdLt,
    hir::type_ref::TypeRef,
    signatures::{
        ConstSignature, EnumSignature, FunctionSignature, ImplSignature, StaticSignature,
        TraitSignature,
    },
};

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
        name(db, def.krate(db), &span, |text| write_body_path(db, def, &mut |it| text.push_str(it)));
    }
    span
}

/// `span` named for an anonymous const of `owner`'s store: `{const}` under the body it is in.
/// One in a signature or in a variant's fields is named by its crate alone.
pub fn of_anon_const(db: &Db, owner: ExpressionStoreOwnerId, span: tracing::Span) -> tracing::Span {
    if span.is_disabled() {
        return span;
    }
    match owner {
        ExpressionStoreOwnerId::Body(def) => name(db, owner.krate(db), &span, |text| {
            write_body_path(db, def, &mut |it| text.push_str(it));
            text.push_str("::{const}");
        }),
        ExpressionStoreOwnerId::Signature(_) | ExpressionStoreOwnerId::VariantFields(_) => {
            span.record("krate", crate_name(db, owner.krate(db)));
        }
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

/// Writes `krate`'s name and the path `path` writes in `span`'s fields `krate` and `function`.
/// The path's queries may name spans of their own, so each span has its own buffer.
fn name(db: &Db, krate: Crate, span: &tracing::Span, path: impl FnOnce(&mut String)) {
    span.record("krate", crate_name(db, krate));
    // alloc: only while a subscriber records the span, which interns it
    let mut text = String::new();
    path(&mut text);
    span.record("function", text.as_str());
}

/// `krate`'s display name, `{crate}` for one with none: a span's `krate`.
pub fn crate_name(db: &Db, krate: Crate) -> &str {
    match &krate.extra_data(db).display_name {
        Some(name) => name.crate_name().symbol().as_str(),
        None => "{crate}",
    }
}

/// The path of `def` a segment at a time: its crate, its modules (`{block}` for a block), the
/// trait or impl it is in (`<impl Type>` by the last segment of the impl's type, `<impl>` for a
/// type that is not a path), and its name; an enum's variant under its enum.
pub fn write_body_path(db: &Db, def: DefWithBodyId, write: &mut impl FnMut(&str)) {
    match def {
        DefWithBodyId::FunctionId(it) => {
            container_path(db, it.lookup(db).container, write);
            write(FunctionSignature::of(db, it).name.as_str());
        }
        DefWithBodyId::ConstId(it) => {
            container_path(db, it.lookup(db).container, write);
            write(ConstSignature::of(db, it).name.as_ref().map_or("_", |it| it.as_str()));
        }
        DefWithBodyId::StaticId(it) => {
            container_path(db, it.lookup(db).container, write);
            write(StaticSignature::of(db, it).name.as_str());
        }
        DefWithBodyId::VariantId(it) => {
            let loc = it.lookup(db);
            let parent = loc.parent.lookup(db);
            module_path(db, parent.container, write);
            write("::");
            write(EnumSignature::of(db, loc.parent).name.as_str());
            write("::");
            write(loc.name.as_str());
        }
    }
}

/// The path of the modules, and the trait or impl, an item in `container` is under, ending in
/// `::`.
fn container_path(db: &Db, container: ItemContainerId, write: &mut impl FnMut(&str)) {
    match container {
        ItemContainerId::ModuleId(it) => module_path(db, it, write),
        ItemContainerId::ExternBlockId(it) => module_path(db, it.lookup(db).container, write),
        ItemContainerId::TraitId(it) => {
            module_path(db, it.lookup(db).container, write);
            write("::");
            write(TraitSignature::of(db, it).name.as_str());
        }
        ItemContainerId::ImplId(it) => {
            module_path(db, it.lookup(db).container, write);
            write("::");
            let impl_ = ImplSignature::of(db, it);
            let named = match impl_.store.type_ref(impl_.self_ty) {
                TypeRef::Path(path) => path.segments().last().map(|it| it.name),
                _ => None,
            };
            match named {
                Some(name) => {
                    write("<impl ");
                    write(name.as_str());
                    write(">");
                }
                None => write("<impl>"),
            }
        }
    }
    write("::");
}

fn module_path<'db>(db: &'db Db, module: ModuleIdLt<'db>, write: &mut impl FnMut(&str)) {
    match module.containing_module(db) {
        Some(parent) => {
            module_path(db, parent, write);
            write("::");
            match module.name(db) {
                Some(name) => write(name.as_str()),
                None => write("{block}"),
            }
        }
        None => write(crate_name(db, module.krate(db))),
    }
}
