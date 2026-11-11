//! The front end, as the host reads it for extensions: the bodies of a crate with their MIR,
//! what each call calls, each item's identity, and where a MIR span is in the source.
//!
//! Everything an extension's job reads of a body is taken here, before the job
//! (`blaze_ext::host::Prefetched`): a job asks the database nothing, so it never waits
//! (`blaze_ext::host`, "No waits"). This is the one module of the host that names the front
//! end's queries.

use std::{alloc::Global, fmt};

use base_db::{Db, FileId};
use blaze_ext::{
    Callee, ImplOf, Item,
    host::{self, HostKey, Prefetched},
    mir::{MirBody, MirSpan, TerminatorKind},
};
use blaze_hash::HashMap;
use blaze_ids::ContentId;
use blaze_items::ItemId;
use hir::{AsAssocItem as _, HirDisplay as _};
use hir_def::{DefWithBodyId, FunctionId, signatures::FunctionSignature};
use hir_ty::{InferBodyId, db::HirDatabase as _};
use syntax::TextRange;

/// One body of the crate.
pub struct BodyOf<'db> {
    pub def: DefWithBodyId,
    pub item: Item,
    pub mir: &'db MirBody<'db>,
    pub prefetched: Prefetched,
    /// The bodies of this crate it calls, by their index in [`Bodies::bodies`], in the order
    /// of its blocks, each once.
    pub callees: Vec<u32>,
    /// Which item it is: stable across edits of what it is.
    pub which: ContentId,
    /// What it is, as far as an extension's function can see it: its code hash (each callee
    /// by its interface, so a callee's body edit leaves it as it was) and everything
    /// [`Prefetched`] says of it, which the code hash leaves out (names).
    pub identity: ContentId,
    /// [`BodyOf::identity`]'s preimage while it is built.
    seen: Vec<u8>,
}

const SEEN_DOMAIN: &str = "blaze v2 extension body 1";

/// One optional text of a preimage: a mark, then its length and bytes.
fn text(out: &mut Vec<u8>, text: Option<&str>) {
    match text {
        Some(text) => {
            out.push(1);
            out.extend_from_slice(&(text.len() as u64).to_le_bytes());
            out.extend_from_slice(text.as_bytes());
        }
        None => out.push(0),
    }
}

/// The bodies of a crate, in the order of their items' keys: the same on every run.
pub struct Bodies<'db> {
    pub bodies: Vec<BodyOf<'db>>,
    index: HashMap<DefWithBodyId, u32, Global>,
    /// The functions whose MIR could not be built, by name: their own errors say why, and no
    /// extension is run on them.
    pub unbuilt: Vec<String>,
}

impl Bodies<'_> {
    /// The index of the body of `def`, if it is one of this crate's.
    pub fn index_of(&self, def: DefWithBodyId) -> Option<u32> {
        self.index.get(&def).copied()
    }
}

/// An item of the crate whose identity the front end could not give.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unidentified {
    pub item: String,
    pub why: String,
}

impl fmt::Display for Unidentified {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "`{}` has no code identity, so no extension's fact of it can be keyed: {}",
            self.item, self.why
        )
    }
}

impl std::error::Error for Unidentified {}

/// The bodies of the functions of `krate`: free functions, the methods of impls and the
/// provided methods of traits. Call with the database attached.
pub fn bodies<'db>(
    db: &'db Db,
    krate: hir::Crate,
    key: &HostKey,
) -> Result<Bodies<'db>, Unidentified> {
    let mut functions: Vec<hir::Function> = Vec::new(); // alloc: the crate's functions, once per check
    for module in krate.modules(db) {
        for declared in module.declarations(db) {
            match declared {
                hir::ModuleDef::Function(it) => functions.push(it),
                hir::ModuleDef::Trait(it) => {
                    functions.extend(it.items(db).into_iter().filter_map(function))
                }
                _ => {}
            }
        }
        for imp in module.impl_defs(db) {
            functions.extend(imp.items(db).into_iter().filter_map(function));
        }
    }
    let target = krate.to_display_target(db);
    let mut bodies = Vec::with_capacity(functions.len()); // alloc: the crate's bodies, once per check
    let mut unbuilt = Vec::new(); // alloc: error path
    for function in functions {
        if !function.has_body(db) {
            continue;
        }
        let Ok(def) = DefWithBodyId::try_from(hir::DefWithBody::Function(function)) else {
            continue;
        };
        let DefWithBodyId::FunctionId(id) = def else { continue };
        let name = FunctionSignature::of(db, id).name.as_str().to_owned(); // alloc: an item's name, kept for the check
        let Ok(mir) = db.mir_body(InferBodyId::DefWithBodyId(def)) else {
            unbuilt.push(name);
            continue;
        };
        let item_id = ItemId::of_body(db, def);
        let which = blaze_items::item_key(db, item_id).0;
        let code = match blaze_items::code_hash(db, item_id) {
            Ok(Some(code)) => code.0,
            Ok(None) => {
                return Err(Unidentified {
                    item: name,
                    why: "it has a body and no code hash".to_owned(), // alloc: error path
                });
            }
            Err(why) => return Err(Unidentified { item: name, why: why.to_string() }),
        };
        // What the extension is told of the body, as bytes: part of the body's identity.
        let mut seen = Vec::new(); // alloc: a key's preimage, once per body per check
        seen.extend_from_slice(&code.0);
        text(&mut seen, Some(&name));
        let impl_of = match function.as_assoc_item(db).map(|it| it.container(db)) {
            Some(hir::AssocItemContainer::Impl(imp)) => {
                let trait_ = imp.trait_(db).map(|it| {
                    // alloc: an item's name, kept for the check
                    (it.name(db).as_str().to_owned(), std_top(db, it.module(db)))
                });
                let self_ty = imp.self_ty(db).display(db, target).to_string();
                text(&mut seen, Some(&self_ty));
                text(&mut seen, trait_.as_ref().map(|it| it.0.as_str()));
                text(&mut seen, trait_.as_ref().and_then(|it| it.1.as_deref()));
                Some(ImplOf::new(self_ty, trait_))
            }
            _ => None,
        };
        // alloc: none yet; filled below, once every body has its index
        let prefetched = Prefetched { name, impl_of, callees: Vec::new() };
        bodies.push(BodyOf {
            def,
            item: host::item(key, def),
            mir,
            prefetched,
            callees: Vec::new(), // alloc: as above
            which,
            // Finished below, with what is told of its calls.
            identity: ContentId::of(SEEN_DOMAIN, &seen),
            seen,
        });
    }
    // An order no hash table and no interned id decides: by the items' keys.
    bodies.sort_unstable_by(|a, b| a.which.cmp(&b.which));
    bodies.dedup_by(|a, b| a.def == b.def);
    let mut index = HashMap::new_in(Global); // alloc: the crate's bodies by item, once per check
    for (at, body) in bodies.iter().enumerate() {
        index.insert(body.def, at as u32);
    }
    let whiches: Vec<ContentId> = bodies.iter().map(|it| it.which).collect(); // alloc: once per check
    for body in &mut bodies {
        let mir = body.mir;
        for (block, data) in mir.basic_blocks.iter() {
            let Some(terminator) = &data.terminator else { continue };
            let TerminatorKind::Call { func, .. } = &terminator.kind else { continue };
            let Some((called, _)) = func.fn_item() else { continue };
            let local = index.get(&DefWithBodyId::FunctionId(called)).copied();
            if let Some(local) = local
                && !body.callees.contains(&local)
            {
                body.callees.push(local);
            }
            let item = local.map(|_| host::item(key, DefWithBodyId::FunctionId(called)));
            let block = block.into_raw().into_u32();
            let (callee, parts) = callee(db, called, item);
            body.seen.extend_from_slice(&block.to_le_bytes());
            body.seen.extend_from_slice(&parts);
            // Which item of the crate it is, by its key: no interned id reaches a key.
            match local {
                Some(local) => body.seen.extend_from_slice(&whiches[local as usize].0),
                None => body.seen.push(0),
            }
            body.prefetched.callees.push((block, callee));
        }
        body.prefetched.callees.sort_unstable_by_key(|(block, _)| *block);
        body.identity = ContentId::of(SEEN_DOMAIN, &body.seen);
        body.seen = Vec::new(); // alloc: none; the preimage is done with
    }
    Ok(Bodies { bodies, index, unbuilt })
}

fn function(item: hir::AssocItem) -> Option<hir::Function> {
    match item {
        hir::AssocItem::Function(it) => Some(it),
        _ => None,
    }
}

/// What a call of `called` calls, as an extension matches it, and the same as bytes for the
/// caller's identity.
fn callee(db: &Db, called: FunctionId, item: Option<Item>) -> (Callee, Vec<u8>) {
    let function = hir::Function::from(called);
    let name = FunctionSignature::of(db, called).name.as_str().to_owned(); // alloc: a callee's name, kept for the check
    let container = function.as_assoc_item(db).map(|it| it.container(db));
    let assoc = container.is_some();
    let self_adt = match container {
        Some(hir::AssocItemContainer::Impl(imp)) => {
            imp.self_ty(db).strip_references().as_adt().map(|adt| {
                // alloc: as the name
                (adt.name(db).as_str().to_owned(), std_top(db, adt.module(db)))
            })
        }
        _ => None,
    };
    let top = std_top(db, function.module(db));
    let mut parts = Vec::new(); // alloc: a key's preimage, once per call per check
    text(&mut parts, Some(&name));
    text(&mut parts, top.as_deref());
    text(&mut parts, self_adt.as_ref().map(|it| it.0.as_str()));
    text(&mut parts, self_adt.as_ref().and_then(|it| it.1.as_deref()));
    parts.push(u8::from(assoc));
    (Callee::new(item, name, top, self_adt, assoc), parts)
}

/// The first named module under the crate root of `module`'s path (`option` for
/// `core::option::Option`'s), when its crate is `core`, `alloc` or `std`.
fn std_top(db: &Db, module: hir::Module) -> Option<String> {
    let of_std =
        module.krate(db).base().extra_data(db).display_name.as_ref().is_some_and(|it| {
            ["core", "alloc", "std"].contains(&it.crate_name().symbol().as_str())
        });
    if !of_std {
        return None;
    }
    // alloc: a module's name, kept for the check
    module
        .path_to_root(db)
        .into_iter()
        .rev()
        .find_map(|it| it.name(db))
        .map(|it| it.as_str().to_owned())
}

/// Where the MIR span `span` of the body of `def` is written: its file and its range there.
/// `None` for a span with no expression of its own (a binding, an inlined callee's) and for
/// one a macro wrote; the caller places such a finding at the body ([`whole`]).
pub fn place(db: &Db, def: DefWithBodyId, span: MirSpan) -> Option<(FileId, TextRange)> {
    let (MirSpan::ExprId(expr) | MirSpan::ExprEnd(expr)) = span else { return None };
    expr_place(db, def, expr)
}

/// Where the body of `def` is written.
pub fn whole(db: &Db, def: DefWithBodyId) -> Option<(FileId, TextRange)> {
    let (body, _) = hir_def::expr_store::Body::with_source_map(db, def);
    expr_place(db, def, body.root_expr())
}

fn expr_place(
    db: &Db,
    def: DefWithBodyId,
    expr: hir_def::hir::ExprId,
) -> Option<(FileId, TextRange)> {
    let (_, map) = hir_def::expr_store::Body::with_source_map(db, def);
    let source = map.store.expr_syntax(expr).ok()?;
    let file = source.file_id.file_id()?.file_id(db);
    Some((file, source.value.syntax_node_ptr().text_range()))
}
