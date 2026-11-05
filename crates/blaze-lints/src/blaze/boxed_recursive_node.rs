//! `boxed_recursive_node`: a type of the crate that links to itself through the heap, one
//! allocation per node: `Box<Self>`, `Option<Box<Self>>`, `Vec<Box<Self>>`, `Rc<RefCell<Self>>`
//! or `Option<Rc<RefCell<Self>>>` (a tree, a linked list). Building it allocates once per node,
//! dropping it frees once per node (recursively, so a long list can overflow the stack), and
//! walking it chases a pointer per step to wherever the allocator put the node. An arena, a
//! `Vec<Node>` with `u32` indices as links, holds the nodes in one allocation, next to each
//! other, and frees them at once; Goose builds its trees in a bump region the same way.
//!
//! Only a type other crates can't name (any visibility but `pub`) is checked, so the arena is
//! the crate's own choice; not one with `#[repr(..)]`, test code or macro output. A
//! `Vec<Box<Self>>` is left to clippy's `vec_box` when that is on (it suggests `Vec<Self>`,
//! itself a step toward the arena). No fix: every link and every walk changes.

use hir::{HasVisibility as _, HirDisplay as _};
use syntax::view::{self as ast, AstNode as _, HasAttrs as _, HasName as _};

use super::{Lint, facts};
use crate::{check::Cx, lints::Implemented};

const LINT: Lint = Lint::BoxedRecursiveNode;

/// Whether `adt` is the std item `name` (in core, alloc or std).
fn std_is(db: &base_db::Db, adt: hir::Adt, name: &str) -> bool {
    ["core", "alloc", "std"].iter().any(|it| facts::adt_is(db, adt, it, name))
}

/// The link `ty` is to `node`, as the message names it, when it is one of the module docs'.
fn link(
    db: &base_db::Db,
    ty: &hir::Type<'_>,
    node: hir::Adt,
    vec_box: bool,
) -> Option<&'static str> {
    let adt = ty.as_adt()?;
    let arg = || ty.type_arguments().next();
    let is_node = |it: &hir::Type<'_>| it.as_adt() == Some(node);
    let boxed = |it: &hir::Type<'_>| {
        it.as_adt().is_some_and(|b| std_is(db, b, "Box"))
            && it.type_arguments().next().is_some_and(|t| is_node(&t))
    };
    let shared = |it: &hir::Type<'_>| {
        it.as_adt().is_some_and(|rc| std_is(db, rc, "Rc"))
            && it.type_arguments().next().is_some_and(|cell| {
                cell.as_adt().is_some_and(|c| std_is(db, c, "RefCell"))
                    && cell.type_arguments().next().is_some_and(|t| is_node(&t))
            })
    };
    if boxed(ty) {
        return Some("Box<Self>");
    }
    if shared(ty) {
        return Some("Rc<RefCell<Self>>");
    }
    let inner = arg()?;
    if std_is(db, adt, "Option") {
        if boxed(&inner) {
            return Some("Option<Box<Self>>");
        }
        if shared(&inner) {
            return Some("Option<Rc<RefCell<Self>>>");
        }
    }
    if std_is(db, adt, "Vec") && boxed(&inner) && !vec_box {
        return Some("Vec<Box<Self>>");
    }
    None
}

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, item: ast::Adt<'t>) {
    if !cx.on_blaze(LINT)
        || cx.expansion().is_some()
        || !cx.is_written(item.syntax())
        || cx.is_in_test(item.syntax())
    {
        return;
    }
    if item.attrs().any(|it| it.simple_name() == Some("repr")) {
        return;
    }
    let Some(adt) = cx.adt_def(item) else { return };
    let db = cx.db();
    let public = match adt {
        hir::Adt::Struct(it) => matches!(it.visibility(db), hir::Visibility::Public),
        hir::Adt::Enum(it) => matches!(it.visibility(db), hir::Visibility::Public),
        hir::Adt::Union(_) => return,
    };
    if public {
        return;
    }
    let fields = match adt {
        hir::Adt::Struct(it) => it.fields(db),
        // alloc: one enum's fields
        hir::Adt::Enum(it) => it.variants(db).into_iter().flat_map(|v| v.fields(db)).collect(),
        hir::Adt::Union(_) => return,
    };
    let vec_box = cx.on(Implemented::VecBox);
    let Some((field, kind)) =
        fields.iter().find_map(|f| link(db, &f.ty(db), adt, vec_box).map(|k| (*f, k)))
    else {
        return;
    };
    let name = item.name().map_or(item.syntax(), |it| it.syntax());
    let Some(range) = cx.range(name) else { return };
    let target = cx.krate().to_display_target(db);
    let type_name = adt.name(db);
    let message = format!(
        "`{}` links to itself through the heap (`{}: {}`, a `{kind}`): one allocation per node",
        type_name.as_str(),
        field.name(db).as_str(),
        field.ty(db).display(db, target)
    );
    let note = format!(
        "keep the nodes in an arena, `Vec<{0}>`, and link them by a `u32` index (`struct {0}Id(u32)`): one allocation \
         for every node, the nodes next to each other, freed at once and with no recursive drop (Goose builds trees \
         in a bump region)",
        type_name.as_str()
    );
    let span = cx.span(range);
    cx.blaze(LINT, name, span, message)
        .note(note)
        .help(LINT.help())
        .help(format!("for further information visit {}", LINT.docs_url()));
}
