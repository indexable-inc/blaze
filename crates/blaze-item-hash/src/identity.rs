//! A body's key (docs/v2/plan/design/end-state/engine.md 4.2 and 4.5, the `Fn` record;
//! restore.md, "The entry's key, made before lowering"): the base key its inference, MIR,
//! borrow check, constant value and lowering are filed under, made from the parse alone, the
//! same for the same item in every database.
//!
//! It hashes, each as its own part ([`early_parts`]): its crate's settings
//! ([`crate_inputs`]), which kind of body it is and its owner by key, the chain of modules it
//! is in by key, and its item's own text ([`index::ItemSource`]) or, for an item a macro
//! wrote, the chain of calls and definitions that wrote it. Nothing it resolves is hashed:
//! every name a step looks up, found or not, every signature it reads and every impl table it
//! asks is a fact of the step's shape (`crate::reads`), so a hit lowers nothing.
//!
//! The identity made from the lowered, resolved body (`identity`, `identity_parts`, its HIR,
//! entries and names parts, `hir_def::item_hash`) is deleted (end-state/engine.md section 5):
//! a key made after the work is the work.

mod inputs;

use base_db::{Crate, Db};
use blaze_ids::{ContentId, Hasher};
use blaze_items::key;
use hir_def::{DefWithBodyId, HasModule as _, expr_store::body::ast_node_hash};
use hir_expand::{
    HirFileId, InFile, MacroCallId, MacroCallKind, MacroDefKind,
    declarative::stored::{def_hash, index},
    proc_macro::{ProcMacroKind, ProcMacros},
};
use span::ErasedFileAstId;

pub use inputs::{FrontEnd, crate_inputs, set_build};

/// A body that has no key, and why: its inference, MIR and borrow check are computed and not
/// filed.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Unidentified {
    pub def: DefWithBodyId,
    pub what: &'static str,
}

/// The modules `def` is in, innermost first, by key, onto `h`: privacy and `super` paths.
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
/// [`crate::Bodies`] files a body's steps under. It reads the item's own text and no
/// `Body::of`, resolution or signature: what a step reads of those is a fact of its shape
/// ([`crate::reads::classify`]), the owner's signature and attributes among them.
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
