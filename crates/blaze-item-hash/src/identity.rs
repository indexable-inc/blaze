//! A body's identity (docs/v2/plan/design/item-hash.md, section 3): the a-priori hash its
//! inference, MIR and borrow check are filed under, the same for the same body in every
//! database, different for each change that can change what they compute.
//!
//! It hashes, each as its own part:
//!
//! - its crate's inputs (3.1, [`crate_inputs`]) and its crate's lang items by signature;
//! - its owner (3.2): the item it is the body of as "self", by signature and attributes, and
//!   the impl or trait that item is in by key and signature;
//! - its HIR (3.3) as [`hir_def::item_hash::portable_body`] writes it, anonymous, with no
//!   span, and what a location macro expanded to as a slot (`slots`), and whether lowering it
//!   found an error;
//! - what each id the HIR names is (3.4, `entries`);
//! - what each path resolves to where it is written, and the traits in scope (3.5, `names`);
//! - the chain of modules it is in, by key (3.6): privacy and `super` paths.
//!
//! Not hashed: where in its file the body is, its text beyond its HIR, other bodies. What
//! inference reads beyond these is a fact (section 5) or refused (section 6).

mod entries;
mod inputs;
mod item;
pub(crate) mod names;
mod slots;

use base_db::{Crate, Db};
use blaze_ids::{ContentId, Hasher};
use blaze_items::{ItemId, key};
use blaze_mem::scratch;
use hir_def::{
    AttrDefId, DefWithBodyId, GenericDefId, HasModule as _, ItemContainerId, Lookup as _,
    expr_store::{Body, body::ast_node_hash},
    hir::generics::GenericParams,
    item_hash::portable_body,
};
use hir_expand::{
    HirFileId, InFile, MacroCallId, MacroCallKind, MacroDefKind,
    declarative::stored::{def_hash, index},
    name::Name,
    proc_macro::{ProcMacroKind, ProcMacros},
};
use span::ErasedFileAstId;
use smallvec::SmallVec;

pub use inputs::{FrontEnd, crate_inputs, set_build};

/// 5: a record names a path's target by its plain item key and a closure of the body with its
/// kind ([`crate::keys`]); a record of 4 named a target by its place among the body's
/// resolutions, which this build cannot read, so it must not find one.
pub const IDENTITY_DOMAIN: &str = "blaze v2 item identity 5";

/// A body that has no identity, and why: its inference, MIR and borrow check are computed
/// and not filed.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Unidentified {
    pub def: DefWithBodyId,
    pub what: &'static str,
}

/// The identity of `def`'s body.
pub fn identity(db: &Db, def: DefWithBodyId) -> Result<ContentId, Unidentified> {
    identity_of(db, def).map_err(|what| Unidentified { def, what })
}

#[blaze_query::tracked(returns(copy))]
fn identity_of(db: &Db, def: DefWithBodyId) -> Result<ContentId, &'static str> {
    let mut h = Hasher::new(IDENTITY_DOMAIN);
    for (_, part) in identity_parts(db, def)? {
        h.id(&part);
    }
    Ok(h.finish())
}

/// How many parts [`identity_parts`] gives.
pub const PARTS: usize = 7;

/// The parts of `def`'s identity, each hashed on its own and named, in the order
/// [`identity`] hashes them: two identities that differ name the part they differ in.
pub fn identity_parts(
    db: &Db,
    def: DefWithBodyId,
) -> Result<[(&'static str, ContentId); PARTS], &'static str> {
    if inputs::build(db).is_none() {
        return Err("no front-end build is set");
    }
    let home = def.module(db).krate(db);
    let part = |name: &'static str| {
        let mut h = Hasher::new(IDENTITY_DOMAIN);
        h.part(name.as_bytes());
        h
    };
    let mut h = part("owner");
    owner(db, def, &mut h)?;
    let owner = h.finish();
    let names = names::names_of(db, def).as_ref().map_err(|it| *it)?;
    let portable =
        portable_body(Body::of(db, def), &names.blank, &slots::of(db, def), &lifetimes(db, def));
    // A lowering error can leave the HIR as it would be without it (an undeclared label, a
    // literal too large), and the borrow check skips a body with one.
    let lowering_errors = Body::has_lowering_errors(db, def);
    let hir = part("hir").part(portable.bytes()).part(&[u8::from(lowering_errors)]).finish();
    let entries = scratch::with_bytes(|buffer| -> Result<ContentId, &'static str> {
        entries::write(db, home, item::own_item(def), &portable, buffer)?;
        Ok(part("entries").part(buffer).finish())
    })?;
    let resolutions = part("names").part(&names.names).part(&names.traits).finish();
    let modules = module_chain(db, def, part("module chain"));
    Ok([
        ("crate inputs", crate_inputs(db, home)),
        ("lang items", inputs::lang_item_signatures(db, home)?),
        ("owner", owner),
        ("hir", hir),
        ("entries", entries),
        ("names", resolutions),
        ("module chain", modules),
    ])
}

/// Section 3.6: the modules `def` is in, innermost first, by key, onto `h`.
fn module_chain(db: &Db, def: DefWithBodyId, mut h: Hasher) -> ContentId {
    let mut module = Some(def.module(db));
    let mut depth = 0u32;
    while let Some(it) = module {
        h.id(&key::module_key(db, it).0);
        depth += 1;
        module = it.containing_module(db).map(|it| it.to_static());
    }
    h.update(&depth.to_le_bytes()).finish()
}

/// 1: the first form.
pub const EARLY_DOMAIN: &str = "blaze v2 item early identity 1";

/// The identity of `def`'s body made before the body is lowered
/// (docs/v2/plan/design/restore.md, "The entry's key, made before lowering"): what
/// [`crate::RecordedNames`] files a body's steps under. It reads the item's own text and no
/// `Body::of`, resolution or signature: what a step reads of those is a fact of its shape
/// ([`crate::reads::Names::Facts`]), the owner's signature and attributes among them.
pub fn early_identity(db: &Db, def: DefWithBodyId) -> Result<ContentId, Unidentified> {
    early_identity_of(db, def).map_err(|what| Unidentified { def, what })
}

#[blaze_query::tracked(returns(copy))]
fn early_identity_of(db: &Db, def: DefWithBodyId) -> Result<ContentId, &'static str> {
    let mut h = Hasher::new(EARLY_DOMAIN);
    for (_, part) in early_parts(db, def)? {
        h.id(&part);
    }
    Ok(h.finish())
}

/// How many parts [`early_parts`] gives.
pub const EARLY_PARTS: usize = 4;

/// The parts of `def`'s early identity, each hashed on its own and named, in the order
/// [`early_identity`] hashes them:
///
/// - "crate inputs": [`crate_inputs`], the crate's settings (its key, edition, target,
///   toolchain, features, the front end's solver version) and nothing of what any crate holds;
/// - "owner": which kind of body it is, the key of the item it is the body of and of the impl
///   or trait that item is in. No signature: two bodies of one text in two impls differ by
///   this part, and what a step reads of either item is a fact;
/// - "module chain": the modules it is in, by key;
/// - "source": for a body written in a file, the position-free hash of the item's own node
///   ([`item_source`]: the whole item, since lowering reads its parameters and attributes
///   with its block); for one a macro writes, the outermost call's node and each
///   macro of the chain of calls that wrote it ([`macro_chain`]).
///
/// `Err` for a body with no key of its own: an item of a block (it is keyed with the body it
/// is in), or one whose chain of calls holds a macro this cannot hash.
pub fn early_parts(
    db: &Db,
    def: DefWithBodyId,
) -> Result<[(&'static str, ContentId); EARLY_PARTS], &'static str> {
    if inputs::build(db).is_none() {
        return Err("no front-end build is set");
    }
    let part = |name: &'static str| {
        let mut h = Hasher::new(EARLY_DOMAIN);
        h.part(name.as_bytes());
        h
    };
    let home = def.module(db).krate(db);
    let InFile { file_id, value: ast_id } = def.erased_ast_id(db);
    if ast_id.in_body() || def.module(db).block(db).is_some() {
        return Err("an item of a block has no key of its own");
    }
    let mut h = part("owner");
    h.update(&[match def {
        DefWithBodyId::FunctionId(_) => 0,
        DefWithBodyId::ConstId(_) => 1,
        DefWithBodyId::StaticId(_) => 2,
        DefWithBodyId::VariantId(_) => 3,
    }]);
    if let DefWithBodyId::VariantId(it) = def {
        h.id(&key::variant_key(db, it).0);
    }
    let (own, container) = crate::reads::owner_keys(db, def);
    h.id(&own);
    match container {
        Some(it) => h.update(&[1]).id(&it),
        None => h.update(&[0]),
    };
    let owner = h.finish();
    let mut h = part("source");
    match file_id.macro_file() {
        None => {
            h.update(&[0]);
            item_source(db, file_id, ast_id, home, &mut h)
                .ok_or("its item has no row in its file's index")?;
        }
        Some(call) => {
            h.update(&[1]);
            macro_chain(db, call, home, &mut h)?;
        }
    }
    let source = h.finish();
    Ok([
        ("crate inputs", crate_inputs(db, home)),
        ("owner", owner),
        ("module chain", module_chain(db, def, part("module chain"))),
        ("source", source),
    ])
}

/// The source hashes of the item `ast` of the file `file`, onto `h`: its
/// [`ItemSource`](index::ItemSource) `header` (the item with every body cut, its attributes
/// with it) and `body`, the whole item between them, hashed from its text and so the same for
/// an outline and a full parse of its file. `None` when the file's index has no row for it.
fn item_source(
    db: &Db,
    file: HirFileId,
    ast: ErasedFileAstId,
    krate: Crate,
    h: &mut Hasher,
) -> Option<()> {
    let source = index::source(db, file, ast, krate)?;
    h.update(&source.header.to_le_bytes());
    match source.body {
        Some(body) => h.update(&[1]).update(&body.to_le_bytes()),
        None => h.update(&[0]),
    };
    Some(())
}

/// What the proc macros of `krate` expand with, onto `h`: each macro's name, kind and the
/// content of its compiled expander (`ProcMacroExpander::code`: the macro crate's code, its
/// dependencies' and the module made of them), as `hir_def::deps`' image key hashes them. A
/// body a proc macro wrote is a function of this and of the call. `Err` while the crate's
/// expander is not loaded or does not say what it is: nothing is expanded to find out.
fn expander_code(db: &Db, krate: Crate, h: &mut Hasher) -> Result<(), &'static str> {
    let macros = ProcMacros::get_for_crate(db, krate)
        .and_then(|it| it.macros())
        .ok_or("a proc macro wrote it, and its crate's expander is not loaded")?;
    h.update(&(macros.len() as u64).to_le_bytes());
    for it in macros {
        let code = it
            .expander
            .code()
            .ok_or("a proc macro wrote it, and its expander does not say what its code is")?;
        h.part(it.name.as_str().as_bytes());
        h.update(&[
            match it.kind {
                ProcMacroKind::CustomDerive => 0,
                ProcMacroKind::Bang => 1,
                ProcMacroKind::Attr => 2,
            },
            u8::from(it.disabled),
        ]);
        h.id(&code);
    }
    Ok(())
}

/// The longest chain of calls [`macro_chain`] follows: past it the body has no early identity.
const MACRO_CHAIN_BOUND: u32 = 256;

/// What a body written by the macro call `call` is made of, onto `h`, with no expansion made:
/// each macro of the chain of calls from the one that wrote the item out to the one written
/// in a file, by which macro it is (its crate's key, its kind, its edition) and the hash of
/// its definition (`hir_expand::declarative::stored::def_hash`), then that outermost
/// call as it is written in its file (a call's path and tokens by its node's hash; the item a
/// derive or an attribute is on by its `ItemSource`). The calls between are tokens the outer expansion wrote, a function of these. Every
/// macro is named, not only hashed: two definitions of equal tokens in two crates resolve
/// their own names (`$crate`) in different scopes.
///
/// A proc macro is named by its crate's key and its function's ast id, with what its crate's
/// expander is ([`expander_code`]) in place of a definition's hash.
///
/// `Err` for a chain this cannot answer for: a proc macro whose expander is not loaded, an
/// eager macro (its input is expanded before it and `include!` reads a file).
fn macro_chain(
    db: &Db,
    call: MacroCallId,
    home: Crate,
    h: &mut Hasher,
) -> Result<(), &'static str> {
    let mut call = call;
    for _ in 0..MACRO_CHAIN_BOUND {
        let loc = call.loc(db);
        let (tag, id) = match loc.def.kind {
            MacroDefKind::Declarative(id, _) => (0u8, Some(id)),
            MacroDefKind::BuiltIn(id, _) => (1, Some(id)),
            MacroDefKind::BuiltInAttr(id, _) => (2, Some(id)),
            MacroDefKind::BuiltInDerive(id, _) => (3, Some(id)),
            MacroDefKind::UnimplementedBuiltIn(id) => (4, Some(id)),
            MacroDefKind::BuiltInEager(..) => return Err("an eager macro wrote it"),
            MacroDefKind::ProcMacro(function, _, _) => {
                // Which macro of its crate: the ast id of its function, a number made of
                // the function's kind and name.
                h.update(&function.value.erase().into_raw().to_le_bytes());
                expander_code(db, loc.def.krate, h)?;
                (5, None)
            }
        };
        h.update(&[tag, loc.def.edition as u8, u8::from(loc.def.local_inner)]);
        h.id(&key::crate_key(db, loc.def.krate));
        if let Some(id) = id {
            h.update(&def_hash(db, id, loc.def.krate).to_le_bytes());
        }
        let (kind, at) = match &loc.kind {
            MacroCallKind::FnLike { eager: Some(_), .. } => {
                return Err("an eager macro wrote it");
            }
            MacroCallKind::FnLike { ast_id, eager: None, .. } => (0u8, ast_id.erase()),
            MacroCallKind::Derive { ast_id, derive_index, .. } => {
                h.update(&derive_index.to_le_bytes());
                (1, ast_id.erase())
            }
            MacroCallKind::Attr { ast_id, .. } => (2, ast_id.erase()),
        };
        h.update(&[kind]);
        match at.file_id.macro_file() {
            Some(outer) => call = outer,
            None => {
                // The item a derive or an attribute is on has a row in its file's index; a
                // call written as an item may have none, and is a path and tokens, which
                // no parse leaves lazy: its node's hash.
                if kind == 0 {
                    h.update(&[0xff]).update(&ast_node_hash(db, at).to_le_bytes());
                } else {
                    h.update(&[0xfe]);
                    item_source(db, at.file_id, at.value, home, h)
                        .ok_or("the item its macro is on has no row in its file's index")?;
                }
                return Ok(());
            }
        }
    }
    Err("its chain of macro calls is longer than the bound")
}

/// The names of the lifetime parameters `def`'s body sees, its impl's or trait's first: a
/// lifetime of the body is hashed by its place among them, not its name.
fn lifetimes(db: &Db, def: DefWithBodyId) -> SmallVec<[Name; 4]> {
    let container = match def {
        DefWithBodyId::FunctionId(it) => Some(it.lookup(db).container),
        DefWithBodyId::ConstId(it) => Some(it.lookup(db).container),
        DefWithBodyId::StaticId(_) | DefWithBodyId::VariantId(_) => None,
    };
    let parent = match container {
        Some(ItemContainerId::ImplId(it)) => Some(GenericDefId::from(it)),
        Some(ItemContainerId::TraitId(it)) => Some(GenericDefId::from(it)),
        Some(ItemContainerId::ModuleId(_) | ItemContainerId::ExternBlockId(_)) | None => None,
    };
    // alloc: inline for a few, once per identity computed
    let mut names = SmallVec::new();
    for generic in parent.into_iter().chain([def.generic_def(db)]) {
        names.extend(GenericParams::of(db, generic).iter_lt().map(|(_, it)| it.name));
    }
    names
}

/// Section 3.2: the item `def` is the body of, as "self" by its signature and attributes (a
/// variant's discriminant: its enum by key and signature), and the impl or trait that item is
/// in by key and signature (its generics and where clauses are the body's too).
fn owner(db: &Db, def: DefWithBodyId, h: &mut Hasher) -> Result<(), &'static str> {
    let container = match def {
        DefWithBodyId::FunctionId(it) => {
            h.update(&[0]);
            Some(it.lookup(db).container)
        }
        DefWithBodyId::ConstId(it) => {
            h.update(&[1]);
            Some(it.lookup(db).container)
        }
        DefWithBodyId::StaticId(it) => {
            h.update(&[2]);
            Some(it.lookup(db).container)
        }
        DefWithBodyId::VariantId(it) => {
            h.update(&[3]).update(&item::variant_part(db, it)?);
            None
        }
    };
    match item::own_item(def) {
        Some(own) => {
            h.update(&item::self_part(db, own)?);
            h.id(&own_attrs(db, def).ok_or("its attributes have no hash")?);
        }
        None => {
            h.update(&item::item_part(db, ItemId::of_body(db, def))?);
        }
    }
    match container {
        Some(ItemContainerId::ImplId(it)) => {
            h.update(&[1]).update(&item::item_part(db, ItemId::Impl(it))?);
        }
        Some(ItemContainerId::TraitId(it)) => {
            h.update(&[2]).update(&item::item_part(db, ItemId::Trait(it))?);
        }
        Some(ItemContainerId::ModuleId(_) | ItemContainerId::ExternBlockId(_)) | None => {
            h.update(&[0]);
        }
    }
    Ok(())
}

/// The value of the `Attrs` fact of the item `def` is the body of, which [`owner`] hashes, so
/// that a read of it by the body's steps is covered; `None` for a variant's discriminant.
pub(crate) fn own_attrs(db: &Db, def: DefWithBodyId) -> Option<ContentId> {
    let owner = match def {
        DefWithBodyId::FunctionId(it) => AttrDefId::FunctionId(it),
        DefWithBodyId::ConstId(it) => AttrDefId::ConstId(it),
        DefWithBodyId::StaticId(it) => AttrDefId::StaticId(it),
        DefWithBodyId::VariantId(_) => return None,
    };
    crate::values::value(db, def.krate(db), crate::values::attrs(db, owner)?)
}

/// Which reads of a [`covered_queries`] query the identity covers, by its key.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Key {
    /// Any key: name resolution, which the identity hashes as what each path of the body
    /// resolves to (3.5), its traits in scope and its module chain (3.6).
    Any,
    /// Only the body's own: its HIR, scopes and upvars are of its own bytes (3.3).
    OwnBody,
    /// Only the body's crate: its crate inputs (3.1).
    OwnCrate,
}

/// The front-end queries a body's identity stands for, each with the keys it covers: a read
/// of one of them by the body's inference, MIR building or borrow check with such a key is
/// covered (section 6) and records nothing. Lane item-hash-facts' `reads` holds each to the
/// same key.
pub fn covered_queries() -> &'static [(&'static str, Key)] {
    use Key::{Any, OwnBody, OwnCrate};
    &[
        // 3.3: the HIR is hashed as written; its source map is not (a read of it is refused).
        ("Body::of", OwnBody),
        // 3.3: whether lowering found an error, which the HIR part hashes beside the HIR.
        ("Body::has_lowering_errors", OwnBody),
        // 3.3: the lowering step's answer (`hir_ty::body_store::lowering`): the sizes of the
        // HIR, its closures and whether lowering failed, all of the HIR hashed here.
        ("lowering", OwnBody),
        // 3.3: the scopes are a function of the HIR.
        ("ExprScopes::body_expr_scopes", OwnBody),
        // 3.3: the closures and what they capture are of the HIR's expressions and bindings.
        // `closure_exprs` is keyed by an expression store: covered for the body's own.
        ("closure_exprs", OwnBody),
        ("body_upvars_mentioned", OwnBody),
        // 3.3 and 3.4: whether each macro call expanded as rustc parses it is of the calls,
        // which the entries hash; a body refused for one builds no MIR to store.
        ("failed_expansion", OwnBody),
        // 3.3 and 3.4: which literals `line!()` made is of the HIR and of the macro call each
        // came from, which the entries hash with its macro.
        ("line_literals", OwnBody),
        // 3.1: the crate's lang items by key and signature, and by key in the goal store's
        // crate key.
        ("lang_items", OwnCrate),
        ("crate_lang_items", OwnCrate),
        // 3.1: the `#![feature]` set.
        ("UnstableFeatures::query", OwnCrate),
        // 3.1: `recursion_limit`.
        ("crate_recursion_limit", OwnCrate),
        // 3.1: the crates loaded, in order, by key in the goal store's crate key.
        ("loaded_crates", OwnCrate),
        // 3.1: the goal store's crate key itself, and the crate key it holds.
        ("goal_crate_key", OwnCrate),
        ("crate_key", OwnCrate),
        // 3.1: the target, in the crate's workspace data.
        ("target_data_layout_query", OwnCrate),
        // 3.1: the `Crate` input's fields: the edition, origin, target kind and cfg (`data`,
        // `cfg_options`) and the name and version (`extra_data`) through the crate key, the
        // workspace data whole. Its dependencies are read through name resolution (3.5) and
        // `loaded_crates`, its root file and attributes through the HIR and the def map.
        ("data", OwnCrate),
        ("cfg_options", OwnCrate),
        ("extra_data", OwnCrate),
        ("workspace_data", OwnCrate),
        // 3.5 and 3.6: name resolution, the traits in scope, the module chain.
        ("crate_local_def_map", Any),
        ("block_def_map", Any),
        ("def_map", Any),
        ("DefMapPair", Any),
        ("crate_shadows_primitive_type", Any),
        ("scope", Any),
        ("children", Any),
        ("row", Any),
        ("module_trait_set", Any),
        ("ModuleScope", Any),
        ("ModuleScope::name", Any),
        ("ModuleScope::legacy_macro", Any),
        ("ModuleScope::traits", Any),
        ("Modules", Any),
        ("Modules::module", Any),
        ("Modules::name", Any),
        ("CrateResolution::module", Any),
        ("ScopeInput::name", Any),
        ("ScopeInput::legacy_macro", Any),
        ("ScopeInput::traits", Any),
        ("ModuleIdLt", Any),
        ("BlockIdLt", Any),
        // The same reads through the other arms of a module's scope: the maintainer's input
        // (`ScopeInput`'s fields) and a crate read from pages (its scope memos). The table of
        // a collected def map's modules is not covered: it is every module, a scan
        // (`reads::Rule::AllModules`).
        ("module", Any),
        ("name_memo", Any),
        ("legacy_macro_memo", Any),
        ("scope_memo", Any),
        ("traits_memo", Any),
    ]
}
