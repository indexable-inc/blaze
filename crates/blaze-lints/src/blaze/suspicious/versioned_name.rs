//! `versioned_name`: an item whose name is another item's with a version on it (`parse_v2`,
//! `load_new`, `read_old`, `encode_legacy`, `build_tmp`, `lower2`; `ParserV2`, `StoreOld`)
//! while the item of the plain name is still there, beside it. Two versions of one thing, and
//! the name does not say which is current, which is to be deleted or how they differ: the next
//! edit (an LLM's above all) picks one by the name it guesses, often the stale one.
//!
//! Found by name resolution, not by text: the plain name must be declared in the same module
//! (an item) or among the same type's associated items in this crate (a method or associated
//! item of an impl), and the versioned one is the item checked. A name that only looks like a
//! version (`sha2`, `u32`, `blake3`, `level1`) is not reported while no item of the stem is
//! declared beside it.
//!
//! Not checked: test code, an item a macro wrote, and an item marked `#[deprecated]` (or whose
//! twin is): the deprecation already says which one goes.
//!
//! No fix: which of the two is current is the author's call. The right form is one item under
//! the plain name: finish the move with a `.clippy.toml` ban of the old path and `blaze check
//! --replace OLD=NEW`, then delete the old item; or, when both are meant to stay, name each for
//! what it does differently (`parse_strict`, `parse_lenient`).

use std::alloc::Global;

use syntax::{
    SyntaxKind,
    view::{self as ast, AstNode as _, HasAttrs as _, HasName as _},
};

use super::super::{Lint, std_mistakes::report};
use crate::check::Cx;

const LINT: Lint = Lint::VersionedName;

/// The version suffixes of a `snake_case` name, after an underscore.
const SNAKE: &[&str] = &["new", "old", "legacy", "tmp", "temp"];

/// The version suffixes of an `UpperCamelCase` name.
const CAMEL: &[&str] = &["New", "Old", "Legacy", "Tmp", "Temp"];

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if !matches!(
        node.kind(),
        SyntaxKind::FN
            | SyntaxKind::STRUCT
            | SyntaxKind::ENUM
            | SyntaxKind::UNION
            | SyntaxKind::TRAIT
            | SyntaxKind::CONST
            | SyntaxKind::STATIC
            | SyntaxKind::TYPE_ALIAS
            | SyntaxKind::MODULE
    ) || !cx.on_blaze(LINT)
    {
        return;
    }
    let Some(name) = ast::AnyHasName::cast(node).and_then(|it| it.name()) else { return };
    let text = name.text();
    let Some((stem, version)) = versioned(text) else { return };
    if cx.is_in_test(node) || deprecated(node) {
        return;
    }
    let db = cx.db();
    let Some(def) = cx.def(node) else { return };
    let Some(twin) = twin(cx, def, stem) else { return };
    if twin.deprecated {
        return;
    }
    let Some(range) = cx.range(name.syntax()) else { return };
    let message = format!(
        "`{text}` is `{stem}` with `{version}` on it, and `{stem}` is still declared beside it: two \
         versions of one thing, and neither name says which is current"
    );
    report(
        cx,
        LINT,
        node,
        range,
        message,
        &format!(
            "keep one item under the plain name `{stem}`: move the callers (a `.clippy.toml` ban and \
             `blaze check --replace`) and delete the other; when both stay, name each for what it \
             does differently ({})",
            twin.kind
        ),
        None,
    );
}

/// `name` split into its stem and its version: `parse_v2` is `("parse", "_v2")`, `lower2` is
/// `("lower", "2")`, `ParserV2` is `("Parser", "V2")`. `None` when it has no version suffix.
fn versioned(name: &str) -> Option<(&str, &str)> {
    let name = name.strip_prefix("r#").unwrap_or(name);
    if let Some((stem, last)) = name.rsplit_once('_') {
        let is_version = SNAKE.iter().any(|it| last.eq_ignore_ascii_case(it))
            || (last.len() > 1
                && last.starts_with(['v', 'V'])
                && last[1..].bytes().all(|it| it.is_ascii_digit()))
            || (!last.is_empty() && last.bytes().all(|it| it.is_ascii_digit()));
        if is_version && !stem.is_empty() {
            return Some((stem, &name[stem.len()..]));
        }
    }
    let first_upper = name.starts_with(|it: char| it.is_ascii_uppercase());
    if first_upper {
        for suffix in CAMEL {
            if let Some(stem) = name.strip_suffix(suffix)
                && stem.len() > 1
            {
                return Some((stem, suffix));
            }
        }
        // `ParserV2`
        let digits = name.bytes().rev().take_while(u8::is_ascii_digit).count();
        if digits > 0 && name[..name.len() - digits].ends_with('V') {
            let stem = &name[..name.len() - digits - 1];
            if stem.len() > 1 {
                return Some((stem, &name[stem.len()..]));
            }
        }
    }
    // `lower2`: one trailing digit from 2 up, on a name of letters.
    let bytes = name.as_bytes();
    if bytes.len() > 2
        && matches!(bytes[bytes.len() - 1], b'2'..=b'9')
        && bytes[bytes.len() - 2].is_ascii_alphabetic()
    {
        return Some((&name[..name.len() - 1], &name[name.len() - 1..]));
    }
    None
}

/// Whether `node`'s own attributes mark it `#[deprecated]`.
fn deprecated(node: ast::Node<'_>) -> bool {
    ast::AnyHasAttrs::cast(node)
        .is_some_and(|it| it.attrs().any(|attr| attr.simple_name() == Some("deprecated")))
}

/// The item named `stem` declared beside `def`.
struct Twin {
    /// What it is, for the note.
    kind: &'static str,
    deprecated: bool,
}

/// The item of the name `stem` declared beside `def`: in its module for an item, among its
/// type's associated items in this crate for an associated item.
fn twin(cx: &Cx<'_, '_>, def: hir::ModuleDef, stem: &str) -> Option<Twin> {
    let db = cx.db();
    if let hir::ModuleDef::Function(f) = def
        && let Some(assoc) = hir::AsAssocItem::as_assoc_item(f, db)
    {
        let hir::AssocItemContainer::Impl(imp) = assoc.container(db) else { return None };
        let impls = hir::Impl::all_for_type(db, cx.krate(), imp.self_ty(db));
        let found = impls
            .into_iter()
            .flat_map(|it| it.items(db))
            .find(|it| it.name(db).is_some_and(|name| name.as_str() == stem))?;
        let deprecated = match found {
            hir::AssocItem::Function(it) => hir::HasAttrs::attrs(it, db).is_deprecated(),
            _ => false,
        };
        return Some(Twin { kind: "an associated item of the same type", deprecated });
    }
    let module = def.module(db)?;
    let mut declared: Vec<hir::ModuleDef, Global> = Vec::new_in(Global);
    declared.extend(module.declarations(db));
    let found = declared
        .into_iter()
        .find(|it| *it != def && it.name(db).is_some_and(|name| name.as_str() == stem))?;
    let deprecated = hir::HasAttrs::attrs(found, db).is_deprecated();
    Some(Twin { kind: "an item of the same module", deprecated })
}

#[cfg(test)]
mod tests {
    use super::versioned;

    #[test]
    fn a_version_suffix_is_split_from_its_stem() {
        assert_eq!(versioned("parse_v2"), Some(("parse", "_v2")));
        assert_eq!(versioned("load_new"), Some(("load", "_new")));
        assert_eq!(versioned("read_old"), Some(("read", "_old")));
        assert_eq!(versioned("encode_legacy"), Some(("encode", "_legacy")));
        assert_eq!(versioned("build_tmp"), Some(("build", "_tmp")));
        assert_eq!(versioned("step_2"), Some(("step", "_2")));
        assert_eq!(versioned("lower2"), Some(("lower", "2")));
        assert_eq!(versioned("ParserV2"), Some(("Parser", "V2")));
        assert_eq!(versioned("StoreOld"), Some(("Store", "Old")));
        assert_eq!(versioned("new"), None);
        assert_eq!(versioned("parse"), None);
        assert_eq!(versioned("u8"), None);
        assert_eq!(versioned("Old"), None);
    }
}
