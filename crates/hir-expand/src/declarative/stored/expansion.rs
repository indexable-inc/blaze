//! The expansion key: what a macro call's expansion is a function of, by content, so the items
//! a macro wrote are keyed by their expansion's inputs and never by the expansion
//! (docs/v2/plan/design/restore.md, "Items read in place", row D; the form agreed in "Agreed
//! with the lanes beside this one"): the outermost call's tokens, each definition's content
//! hash, hygiene, cfg and edition. Written by claude-opus-5-5.
//!
//! The key of a call is made from:
//!
//! - *the definition*: a `macro_rules!` or `macro` by its content hash (the `header` of its
//!   row in its file's index, with its crate's cfg where that text names `cfg`; one a macro
//!   wrote, by the key of the expansion it is written in and its ast id there) and its
//!   crate's edition and flags; a builtin by which builtin it is, for the builtins whose
//!   output is a function of their input, the call's crate's cfg and its edition alone;
//! - *the call*: written in a file, the `header` and `body` of its row in that file's index
//!   (the whole call or, for a derive, the whole item), how the file is read; written in an
//!   expansion, that expansion's key and the call's ast id there, which is a function of it;
//!   the call's kind, what it expands to, which derive of which attribute, and its depth;
//! - *the cfg* of the crate the call is expanded for, always: a derive's input is the item
//!   with its `cfg`s and `cfg_attr`s applied, and a builtin `cfg!` reads it.
//!
//! *Hygiene* needs no part of its own: a call written in a file has its file's root context,
//! which how the file is read names, and every context in an expansion is made from its call's
//! context and marks of the expansions its tokens flowed through, each of which is keyed here,
//! down to a call written in a file. The values kept under these keys hold no context (a value
//! naming one is not kept), so a context is compared only by the key it came from.
//!
//! `None` where the expansion reads something no part names: a proc macro (its expansion may
//! read the environment and files; `ExpansionReads` is the deps image's, not this key's), an
//! eager call (`include!`, `env!`, `concat!`: its input is an expansion and a file or the
//! environment), an attribute macro (which attributes before it are active is a resolution),
//! a builtin that reads a position, a path or a module (`line!`, `file!`, `module_path!`), a
//! call in a body (no row: lane records-bodies) and a call in an expansion whose key is
//! `None`. Such an expansion's items are made from it, as before.
//!
//! The floor: a call written in a file, one index found by content (one store read a file a
//! process) and one hash of under two hundred bytes; a call in an expansion, the same for each
//! expansion around it. Nothing is parsed or expanded.
//!
//! The invariant: two calls with one key have one expansion text, token for token, and their
//! contexts are made the same way from their own calls.

use base_db::{Crate, Db};
use blaze_ids::{ContentId, Hasher};
use span::ErasedFileAstId;
use syntax::ast;

use crate::{
    AstId, BuiltinDeriveExpander, BuiltinFnLikeExpander, HirFileId, MacroCallId, MacroCallKind,
    MacroDefId, MacroDefKind,
};

/// The layout of the key: a change of what a key hashes changes it, so a kept entry under a
/// key of another layout is a miss.
pub const LAYOUT: u32 = 1;

const DOMAIN: &str = "blaze v2 expansion key";

/// How an expansion's text is read, in the place of a file's `read_as` byte
/// ([`super::read_as`]): no file reads as this (an edition is below 16), so an item of an
/// expansion and one of a file never share a key through it.
pub const EXPANDED: u8 = 0xFF;

/// The key of `call`'s expansion, or `None` where the expansion reads something the key does
/// not name (the module's doc). A query of its own so each expansion around a call is hashed
/// once a revision.
#[blaze_query::tracked(returns(copy), mem_tag = Expand)]
pub fn key(db: &Db, call: MacroCallId) -> Option<ContentId> {
    let loc = call.loc(db);
    let mut hasher = Hasher::new(DOMAIN);
    hasher.update(&LAYOUT.to_le_bytes()).update(&loc.macro_depth.to_le_bytes());
    definition(db, loc.def, &mut hasher)?;
    match &loc.kind {
        MacroCallKind::FnLike { ast_id, expand_to, eager } => {
            if eager.is_some() {
                return None;
            }
            hasher.update(&[1, *expand_to as u8]);
            site(db, ast_id.file_id, ast_id.value.erase(), loc.krate, &mut hasher)?;
        }
        MacroCallKind::Derive { ast_id, derive_attr_index, derive_index, .. } => {
            hasher
                .update(&[2])
                .update(&derive_attr_index.item_tree_index().to_le_bytes())
                .update(&derive_index.to_le_bytes());
            site(db, ast_id.file_id, ast_id.value.erase(), loc.krate, &mut hasher)?;
        }
        MacroCallKind::Attr { .. } => return None,
    }
    hasher
        .update(&[loc.krate.data(db).edition as u8])
        .id(&base_db::cfg_options_content(loc.krate.cfg_options(db)));
    Some(hasher.finish())
}

/// Hashes the definition `def` into `hasher`: `None` where its expansion is not a function of
/// what is hashed.
fn definition(db: &Db, def: MacroDefId, hasher: &mut Hasher) -> Option<()> {
    let flags = def.local_inner as u8
        | (def.allow_internal_unsafe as u8) << 1
        | (def.edition_panic as u8) << 2;
    hasher.update(&[def.edition as u8, flags]);
    match def.kind {
        MacroDefKind::Declarative(id, _) => {
            hasher.update(&[1]);
            declared(db, id, def.krate, hasher)
        }
        MacroDefKind::BuiltIn(_, expander) => {
            pure_fn_like(expander)?;
            hasher.update(&[2, expander as u8]);
            Some(())
        }
        MacroDefKind::BuiltInDerive(_, expander) => {
            pure_derive(expander);
            hasher.update(&[3, expander as u8]);
            Some(())
        }
        MacroDefKind::BuiltInAttr(..)
        | MacroDefKind::BuiltInEager(..)
        | MacroDefKind::UnimplementedBuiltIn(_)
        | MacroDefKind::ProcMacro(..) => None,
    }
}

/// Hashes the `macro_rules!` or `macro` `id` of `def_crate` by its content: its row's `header`
/// (with the crate's cfg where it names `cfg`, as a `cfg_attr` on the definition reads it), or,
/// written by a macro, its expansion's key and its ast id there.
fn declared(db: &Db, id: AstId<ast::Macro>, def_crate: Crate, hasher: &mut Hasher) -> Option<()> {
    match id.file_id {
        HirFileId::FileId(file) => {
            let source = super::index::of(db, file, def_crate).get(id.value.erase())?;
            hasher.update(&[0, super::read_as(db, file)]).update(&source.header.to_le_bytes());
            if source.attrs_cfg || source.header_cfg {
                hasher.update(&[1]).id(&base_db::cfg_options_content(def_crate.cfg_options(db)));
            } else {
                hasher.update(&[0]);
            }
        }
        HirFileId::MacroFile(call) => {
            hasher.update(&[1]).id(&key(db, call)?).update(&id.value.erase().into_raw().to_le_bytes());
        }
    }
    Some(())
}

/// Hashes where a call is written: the whole node `ast` of `file` as its file's index has it
/// (`header` and `body`: a call's tokens, a derive's item), or its expansion's key and its ast
/// id there. `None` for a node with no row (a call in a body) and in an expansion with no key.
fn site(db: &Db, file: HirFileId, ast: ErasedFileAstId, krate: Crate, hasher: &mut Hasher) -> Option<()> {
    match file {
        HirFileId::FileId(file) => {
            let source = super::index::of(db, file, krate).get(ast)?;
            // `header` covers `attrs`; a derive's item has no own body (an enum's discriminants
            // are its variants' bodies, which no builtin derive writes out).
            hasher.update(&[0, super::read_as(db, file)]).update(&source.header.to_le_bytes());
            match source.body {
                Some(body) => hasher.update(&[1]).update(&body.to_le_bytes()),
                None => hasher.update(&[0]),
            };
        }
        HirFileId::MacroFile(call) => {
            hasher.update(&[1]).id(&key(db, call)?).update(&ast.into_raw().to_le_bytes());
        }
    }
    Some(())
}

/// `Some` for a builtin whose output is a function of its input tokens, the call's edition and
/// its crate's cfg. Every builtin is named, so a new one is decided here before it compiles.
fn pure_fn_like(expander: BuiltinFnLikeExpander) -> Option<()> {
    use BuiltinFnLikeExpander as E;
    match expander {
        E::Assert
        | E::Stringify
        | E::Asm
        | E::GlobalAsm
        | E::NakedAsm
        | E::CfgSelect
        | E::Cfg
        | E::CorePanic
        | E::StdPanic
        | E::Unreachable
        | E::LogSyntax
        | E::TraceMacros
        | E::FormatArgs
        | E::ConstFormatArgs
        | E::FormatArgsNl
        | E::Quote
        | E::PatternType => Some(()),
        // A position, a path or a module of the call, which no part of the key names.
        E::Column | E::File | E::Line | E::ModulePath => None,
        // Not read for what it reads.
        E::Gca => None,
    }
}

/// Every builtin derive writes from its input item alone (`$crate` paths, no read of the
/// database): named so a new one is decided here before it compiles.
fn pure_derive(expander: BuiltinDeriveExpander) {
    use BuiltinDeriveExpander as E;
    match expander {
        E::Copy
        | E::Clone
        | E::Default
        | E::Debug
        | E::Hash
        | E::Ord
        | E::PartialOrd
        | E::Eq
        | E::PartialEq
        | E::CoercePointee
        | E::Reborrow
        | E::CoerceShared
        | E::ConstParamTy
        | E::From => {}
    }
}
