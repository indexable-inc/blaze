//! D189 step 4: `BodyScope(body)`. A body's block scopes are the BODY'S OWN: solved by the one
//! cell solver over the body's own block tree, the outermost blocks' parent the body's module
//! in the crate's FINISHED names, after the crate's names are finished
//! ([`crate::resolve::solve_held_body`], `crate::cells::solve_body_in`). Nothing of the crate is
//! solved again; nothing the crate solve wrote moves.
//!
//! [`body_scope`] is the one function: a surface body's `BodyScope` task calls it once and its
//! record ([`BodyScope`]) is written once; any other body's lowering calls the same function in
//! its own process when it meets a block that writes an item. Under `BlockScopes::Modules` (the
//! default) nothing here runs: the crate solve makes every block a module of the crate
//! (resolve.rs `make_blocks`), as before.
//!
//! The record's KEY is from text, before the work (rules 21, 65): [`body_scope_key`], the
//! version, `BlockScopes::Bodies.key_byte()`, the crate, the body's item key and module, and
//! the body's text. Its READ LIST is what the solve read outside the body's text
//! ([`crate::resolve::BodyReads`]): each finished module of the crate a block's lookup read,
//! with its scope's bytes; each enum of the crate asked of, with its variants; each macro of
//! the crate a call expanded, with its source's digest; every question asked of the
//! dependencies; and the body's cut of its module's `macro_rules!` sequence. An edit to a name
//! the body reads moves the READ LIST, not the key: the record is valid by its key, or else by
//! its read list checked against today's facts (AGENTS.md, the design), so an edit elsewhere
//! re-checks the list and keeps the record when what it read is unchanged.
//!
//! A block is named by its ORDINAL ([`BlockRow::ordinal`]): the index of its `{` among the
//! body's WRITTEN blocks, in the order their `{` is written, from 0 (a written block holds an
//! item that makes a module, a macro call in statement position, or a local `fn`/`const`/
//! `static` whose own body writes one, or is around such a block: crate::expanded_blocks
//! `written`). It is the `N` of the block's module name (`__blaze_block_N`, crate::blocks), so
//! the module's key is `(body, ordinal)`: it is stable under every edit outside the body's text,
//! and under an edit inside it that adds or removes no written block before it.

#![blaze::sans_io]

use core::fmt;

use blaze_id::{CrateItemKey, ItemKey};
use blaze_ids::{ContentId, Hasher};
use blaze_own::{AVec, Alloc, Bump};
use blaze_surface::{BuildError, ExternTarget, Namespace, Rename, ScopeBuilder};

use crate::prepares::Prepares;
use crate::resolve::{At, BodyDeps, BodyReads, BodySolveError, BodySolved, CrateInput, CrateNames, HeldBody, ReadTree, SourceTexts, solve_held_body_scratch};
use crate::surface::BlockScopes;
use crate::{Def, DefKind};

mod impl_table;
pub mod merged;
mod view;
mod world;

pub use view::{BodyScopeReadError, BodyScopeView, ImplAt, RoundTripDiff};
pub use impl_table::{BlockScopesVerdict, BlocksTally, DifferentialError, ImplDiff, ImplHeaders, ImplRow, ModulesSide, OneLine, bodies_impl_table, impl_diff, inside_only, modules_impl_table, written_in_a_body};
pub use world::BodyWorld;

/// The record's version: in its key and its bytes. 4: a block's declared names hold the names
/// its `extern crate`s bind.
pub const BODY_SCOPE_VERSION: u32 = 4;

const KEY_DOMAIN: &str = "blaze-scope body scope 1";
const READS_DOMAIN: &str = "blaze-scope body scope reads 1";
const MAGIC: [u8; 4] = *b"BSCP";

/// One block's module of the body: its scope, the names it declares, where it is.
#[derive(Debug)]
pub struct BlockRow<'a> {
    pub module: ItemKey,
    /// Its ordinal among the body's written blocks (the module doc); `None` for a module made
    /// inside a block, not by the body's own text (an expansion's module or block, or a block
    /// of a local item's body), named by its key alone, which is made from the text that
    /// wrote it.
    pub ordinal: Option<u32>,
    /// The block or module around it: the body's module for an outermost block.
    pub parent: Option<ItemKey>,
    /// What it declares, in text order: its items, then its `mod`s, as its lowering lists them,
    /// then the names its `extern crate`s bind.
    pub declared: AVec<'a, (Namespace, &'a str, Def)>,
    /// Its whole scope, as the cell core wrote it.
    pub scope: ScopeBuilder<'a, &'a str>,
}

/// One `impl` written anywhere in the body (a block's, or in a module a block's call wrote).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct BodyImpl<'a> {
    pub key: ItemKey,
    /// The module its header and members are read in.
    pub module: ItemKey,
    /// Its header as written (`impl<T> Tr for X<T>`).
    pub header: &'a str,
}

/// One local item an impl of the body may reach: every type-namespace item a block declares
/// that is no module (a struct, enum, union, trait or alias): a superset of those the body's
/// impls name (rule 3: never fewer).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct LocalHead<'a> {
    pub key: ItemKey,
    pub module: ItemKey,
    pub name: &'a str,
}

/// One body's `BodyScope` record.
#[derive(Debug)]
pub struct BodyScope<'a> {
    /// [`body_scope_key`].
    pub key: ContentId,
    /// The digest of the read list ([`reads_digest`]): with the key, what the record is
    /// checked by.
    pub reads: ContentId,
    /// The digest of the body's impl rows as the crate's impl table takes them
    /// ([`merged::impl_rows_digest`]: each impl's key, module, header and filing through the
    /// body's blocks). What the crate's merged impl table is keyed by, so an edit to the body's
    /// statements that moves no impl moves no table (D189 step 5, S2).
    pub impl_rows: ContentId,
    /// Each block's module, by key.
    pub blocks: AVec<'a, BlockRow<'a>>,
    /// Each impl, by key.
    pub impls: AVec<'a, BodyImpl<'a>>,
    /// Each local item an impl may reach, by key.
    pub locals: AVec<'a, LocalHead<'a>>,
    /// The body's outermost block modules, sorted.
    pub outermost: AVec<'a, ItemKey>,
    /// The whole solve: the tables, expansions and notes its items add (what the body's own
    /// check and the impl table read beside the rows).
    pub solved: BodySolved<'a>,
}

/// Why `body_scope` gave no record: a typed refusal naming the body (rules 3, 19).
#[derive(Clone, Debug)]
pub enum BodyScopeRefusal<'a> {
    /// A `#[macro_export]` macro an EXPANSION in the body wrote (not literal in the body's
    /// text): it would be a name of the crate root, which the finished names do not hold and
    /// no read of the body's text shows (D189: Names solves module-level items only).
    ExportedFromExpansion {
        body: ItemKey,
        /// The block's module the expansion lowered into.
        block: ItemKey,
        name: &'a str,
        def: ItemKey,
        /// The call whose expansion wrote it, and that call's macro name, where the tables
        /// place the definition in an expansion.
        call: Option<ItemKey>,
        call_name: Option<&'a str>,
    },
    /// The body's blocks could not be decided: the solve's own refusal, named
    /// ([`BodySolveError`]: a read the finished names cannot answer, an ambiguity, a walk out of
    /// the body with no cut, too many rounds).
    Undecided { body: ItemKey, error: BodySolveError },
    /// The record's read list or bytes could not be written.
    Record { body: ItemKey, error: BuildError },
    /// An impl of the body could not be filed (its header's tokens), so its rows digest could
    /// not be made.
    Rows { body: ItemKey, error: crate::items_record::ItemsRecordError },
}

impl fmt::Display for BodyScopeRefusal<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BodyScopeRefusal::ExportedFromExpansion { body, block, name, def, call, call_name } => write!(
                f,
                "body {body:?}: block {block:?}: `#[macro_export] macro_rules! {name}` ({def:?}) is written by the expansion of {} ({call:?}): an exported macro from an expansion in a body is refused under --block-scopes=bodies (D189 step 4)",
                call_name.unwrap_or("a call")
            ),
            BodyScopeRefusal::Undecided { body: _, error } => write!(f, "{error} (D189 step 4, rule 3)"),
            BodyScopeRefusal::Record { body, error } => write!(f, "body {body:?}: its BodyScope record: {error:?}"),
            BodyScopeRefusal::Rows { body, error } => write!(f, "body {body:?}: an impl's filing for its impl rows digest: {error:?}"),
        }
    }
}

/// The key of `body`'s `BodyScope` record (rules 21, 65: from text, before the work): the
/// version, the strategy's byte, the crate, the body's item key and module, and its text.
pub fn body_scope_key(krate: CrateItemKey, body: &HeldBody<'_>) -> ContentId {
    let mut h = Hasher::new(KEY_DOMAIN);
    h.update(&BODY_SCOPE_VERSION.to_le_bytes())
        .update(&[BlockScopes::Bodies.key_byte()])
        .update(&krate.0.0)
        .update(&body.src.key.0.0)
        .update(&body.src.module.0.0)
        .update(&u64::try_from(body.src.text.len()).unwrap_or(u64::MAX).to_le_bytes())
        .update(body.src.text.as_bytes());
    h.finish()
}

/// The digest of a body solve's read list as today's facts answer it: each read with what it
/// read now (a finished scope's bytes, an enum's variants, a macro source's digest), the
/// dependencies' questions, and the body's cut. Equal digests: the record still holds.
pub fn reads_digest(c: &CrateNames<'_>, cut: Option<u32>, reads: &BodyReads<'_>, alloc: Alloc<'_>) -> Result<ContentId, BuildError> {
    let mut h = Hasher::new(READS_DOMAIN);
    match cut {
        Some(len) => h.update(&[1]).update(&len.to_le_bytes()),
        None => h.update(&[0]),
    };
    h.update(&count(reads.modules.len()));
    for m in reads.modules.iter() {
        h.update(&m.0.0);
        match c.done.get(m) {
            Some(r) => {
                let bytes = r.scope.write(alloc)?;
                h.update(&[1]).update(&count(bytes.len())).update(&bytes);
            }
            None => {
                h.update(&[0]);
            }
        }
    }
    h.update(&count(reads.enums.len()));
    for e in reads.enums.iter() {
        h.update(&e.0.0);
        match c.tables.enums.get(e) {
            Some(vs) => {
                h.update(&[1]).update(&count(vs.len()));
                for v in vs.iter() {
                    h.update(&v.key.0.0).update(&[u8::from(v.record)]).update(&count(v.name.len())).update(v.name.as_bytes());
                }
            }
            None => {
                h.update(&[0]);
            }
        }
    }
    h.update(&count(reads.macros.len()));
    for (k, d) in reads.macros.iter() {
        h.update(&k.0.0).update(&d.0);
    }
    h.update(&count(reads.deps.len())).update(&reads.deps);
    Ok(h.finish())
}

fn count(n: usize) -> [u8; 8] {
    u64::try_from(n).unwrap_or(u64::MAX).to_le_bytes()
}

/// `BodyScope(body)`: the blocks of `body` of the finished crate `c`, solved as the body's own
/// ([`crate::resolve::solve_held_body`]) over its dependencies `deps`, as a record ([`BodyScope`]). `input` and
/// `source` are the crate's input and its index for `sources`, as its crate solve had them.
#[expect(clippy::too_many_arguments, reason = "one body's every input, named")]
pub fn body_scope<'a, S: SourceTexts<'a>>(
    c: &'a CrateNames<'a>,
    deps: &BodyDeps<'_, 'a>,
    input: &CrateInput<'a>,
    source: usize,
    body: HeldBody<'a>,
    sources: &S,
    prepares: &'a Prepares<'a>,
    alloc: Bump<'a>,
) -> Result<BodyScope<'a>, BodyScopeRefusal<'a>> {
    let tree = ReadTree::of(c, alloc).map_err(|error| BodyScopeRefusal::Undecided { body: body.src.key, error: BodySolveError::TreeRecord { body: body.src.key, error } })?;
    body_scope_in(&tree, deps, input, source, body, sources, prepares, alloc)
}

/// [`body_scope`] given the crate's tree record already read ([`crate::resolve::ReadTree`]):
/// what a pass over a crate's held bodies calls, reading the record once for all of them.
#[expect(clippy::too_many_arguments, reason = "one body's every input, named")]
pub fn body_scope_in<'a, S: SourceTexts<'a>>(
    tree: &ReadTree<'a>,
    deps: &BodyDeps<'_, 'a>,
    input: &CrateInput<'a>,
    source: usize,
    body: HeldBody<'a>,
    sources: &S,
    prepares: &'a Prepares<'a>,
    alloc: Bump<'a>,
) -> Result<BodyScope<'a>, BodyScopeRefusal<'a>> {
    body_scope_scratch(tree, deps, input, source, body, sources, prepares, &crate::task::Scratch::kept, alloc)
}

/// [`body_scope_in`] with the body's working memory in the driver's scratch `hook`, reset
/// after each use (crate::task::Scratch::run): the cell core's solve of the blocks
/// (resolve.rs `solve_held_body_scratch`) and the bytes of each finished scope the read list
/// digests ([`reads_digest`]). Both are the size of the crate, not of the body; a pass over
/// every held body of a crate calls this so its frame holds the records, never bodies ×
/// modules (crate_scope.rs `block_scopes_bodies_side`). The record is equal under both hooks.
#[expect(clippy::too_many_arguments, reason = "one body's every input, named, and the driver's scratch")]
pub fn body_scope_scratch<'a, S: SourceTexts<'a>>(
    tree: &ReadTree<'a>,
    deps: &BodyDeps<'_, 'a>,
    input: &CrateInput<'a>,
    source: usize,
    body: HeldBody<'a>,
    sources: &S,
    prepares: &'a Prepares<'a>,
    hook: crate::task::ScratchHook<'_>,
    bump: Bump<'a>,
) -> Result<BodyScope<'a>, BodyScopeRefusal<'a>> {
    let alloc = bump.alloc();
    let c = tree.names();
    let at = body.src.key;
    let key = body_scope_key(c.key, &body);
    let mut solved = solve_held_body_scratch(tree, deps, input, source, body, sources, prepares, hook, bump).map_err(|error| BodyScopeRefusal::Undecided { body: at, error })?;

    // The ordinals: the body's written blocks in text order, as crate::blocks numbers them.
    let nested = blaze_file_items::nested::scan(body.src.text, alloc);
    let keys = crate::blocks::block_modules(c.key, body.src.key, &nested, alloc);
    let mut ordinals: AVec<'a, (ItemKey, u32)> = AVec::with_capacity_in(keys.len(), alloc);
    let mut n: u32 = 0;
    for k in keys.iter() {
        if let Some(k) = *k {
            ordinals.push((k, n));
            n += 1;
        }
    }
    ordinals.sort_unstable();

    let scopes = core::mem::replace(&mut solved.scopes, AVec::new_in(alloc));
    let mut blocks: AVec<'a, BlockRow<'a>> = AVec::with_capacity_in(scopes.len(), alloc);
    let mut locals: AVec<'a, LocalHead<'a>> = AVec::new_in(alloc);
    for (module, scope) in scopes {
        // An exported macro an expansion of the body wrote (cells/write.rs lists a module's
        // `#[macro_export]`s from an expansion, depth > 0, in `macro_exports`).
        if let Some(d) = scope.macro_exports.first() {
            let call = match solved.tables.texts.get(&d.def) {
                Some((_, At::Expansion(call))) => Some(*call),
                Some((_, At::File(_))) | None => None,
            };
            let call_name = call.and_then(|k| solved.tables.calls.get(&k).map(|s| s.name));
            return Err(BodyScopeRefusal::ExportedFromExpansion { body: at, block: module, name: d.name, def: d.def, call, call_name });
        }
        let made = solved.made.get(&module);
        let mut declared: AVec<'a, (Namespace, &'a str, Def)> = AVec::new_in(alloc);
        if let Some(m) = made {
            for it in m.decls.items.iter() {
                declared.push((it.ns, it.name, it.def));
                if it.ns == Namespace::Types && it.def.kind != DefKind::Module {
                    locals.push(LocalHead { key: it.def.key, module, name: it.name });
                }
            }
            for ch in m.decls.mods.iter() {
                declared.push((Namespace::Types, ch.name, Def::module(ch.module)));
            }
            // The Reference, "Extern crate declarations" [items.extern-crate.intro]: "The
            // external crate is then bound into the declaring scope as the given identifier in
            // the type namespace" (proptest's `prop_assert!` writes `if !c { extern crate
            // alloc; .. alloc::format!(..) }`): a name the block declares, its definition the
            // crate's root as the block's scope binds it. `as _` binds nothing, and `extern
            // crate self;` without `as` is no binding ([items.extern-crate.self]). The cells
            // bind every one they accept in the block's own scope; one they refuse fails the
            // solve before this (`Unwritten`), so a name the scope lacks is no binding here.
            for ec in m.decls.extern_crates.iter() {
                let name = match (ec.rename, ec.target) {
                    (Rename::To(name), _) | (Rename::Same, ExternTarget::Dep(name)) => name,
                    (Rename::Underscore, _) | (Rename::Same, ExternTarget::SelfCrate) => continue,
                };
                if let Some(bound) = scope.get(Namespace::Types, name) {
                    declared.push((Namespace::Types, name, bound.def));
                }
            }
        }
        let ordinal = ordinals.binary_search_by_key(&module, |o| o.0).ok().map(|i| ordinals[i].1);
        blocks.push(BlockRow { module, ordinal, parent: made.and_then(|m| m.info.parent), declared, scope });
    }
    blocks.sort_unstable_by_key(|b| b.module);
    locals.sort_unstable();
    locals.dedup();

    let mut impls: AVec<'a, BodyImpl<'a>> = AVec::with_capacity_in(solved.tables.impls.len(), alloc);
    impls.extend(solved.tables.impls.iter().map(|i| BodyImpl { key: i.key, module: i.module, header: i.header }));
    impls.sort_unstable();

    let reads = crate::task::Scratch::run(hook, alloc, |work: Alloc<'_>| reads_digest(c, body.cut, &solved.reads, work)).map_err(|error| BodyScopeRefusal::Record { body: at, error })?;
    let outermost = core::mem::replace(&mut solved.outermost, AVec::new_in(alloc));
    let mut scope = BodyScope { key, reads, impl_rows: ContentId([0; 32]), blocks, impls, locals, outermost, solved };
    // Filed through the body's own blocks (BodyWorld reads `scope`), then digested.
    let rows = merged::body_rows(c, &scope, alloc).map_err(|error| BodyScopeRefusal::Rows { body: at, error })?;
    scope.impl_rows = merged::impl_rows_digest(&scope, &rows);
    Ok(scope)
}

impl<'a> BodyScope<'a> {
    /// The record's bytes: one deterministic layout (every list sorted by key, integers little
    /// endian, each string and scope by its length): the magic, the version, the key, the read
    /// list's digest, the impl rows' digest (version 3), the blocks (module, ordinal or `u32::MAX`, parent or zeros, the declared
    /// names, the scope's own record bytes), the impls (key, module, header), the locals (key,
    /// module, name), the outermost blocks.
    pub fn write<'o>(&self, alloc: Alloc<'o>) -> Result<AVec<'o, u8>, BuildError> {
        let mut out: AVec<'o, u8> = AVec::new_in(alloc);
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&BODY_SCOPE_VERSION.to_le_bytes());
        out.extend_from_slice(&self.key.0);
        out.extend_from_slice(&self.reads.0);
        out.extend_from_slice(&self.impl_rows.0);
        out.extend_from_slice(&count(self.blocks.len()));
        for b in self.blocks.iter() {
            out.extend_from_slice(&b.module.0.0);
            out.extend_from_slice(&b.ordinal.unwrap_or(u32::MAX).to_le_bytes());
            out.extend_from_slice(&b.parent.map_or([0u8; 32], |p| p.0.0));
            out.extend_from_slice(&count(b.declared.len()));
            for &(ns, name, def) in b.declared.iter() {
                out.push(ns_byte(ns));
                str_into(&mut out, name);
                out.extend_from_slice(&def.key.0.0);
                out.push(kind_byte(def.kind));
            }
            let scope = b.scope.write(alloc)?;
            out.extend_from_slice(&count(scope.len()));
            // The scope's record starts at a multiple of `blaze_format::ALIGN` from the record's
            // start, so a reader of a record placed aligned reads it in place (view.rs).
            out.resize(out.len().next_multiple_of(blaze_format::ALIGN), 0);
            out.extend_from_slice(&scope);
        }
        out.extend_from_slice(&count(self.impls.len()));
        for i in self.impls.iter() {
            out.extend_from_slice(&i.key.0.0);
            out.extend_from_slice(&i.module.0.0);
            str_into(&mut out, i.header);
        }
        out.extend_from_slice(&count(self.locals.len()));
        for l in self.locals.iter() {
            out.extend_from_slice(&l.key.0.0);
            out.extend_from_slice(&l.module.0.0);
            str_into(&mut out, l.name);
        }
        out.extend_from_slice(&count(self.outermost.len()));
        for o in self.outermost.iter() {
            out.extend_from_slice(&o.0.0);
        }
        Ok(out)
    }

    /// The names block `module` declares, in text order; `None` when it is no block of this body.
    pub fn declared(&self, module: ItemKey) -> Option<&[(Namespace, &'a str, Def)]> {
        let i = self.blocks.binary_search_by_key(&module, |b| b.module).ok()?;
        Some(&self.blocks[i].declared)
    }

    /// The scope of block `module`; `None` when it is no block of this body.
    pub fn scope(&self, module: ItemKey) -> Option<&ScopeBuilder<'a, &'a str>> {
        let i = self.blocks.binary_search_by_key(&module, |b| b.module).ok()?;
        Some(&self.blocks[i].scope)
    }
}

fn str_into(out: &mut AVec<'_, u8>, s: &str) {
    out.extend_from_slice(&count(s.len()));
    out.extend_from_slice(s.as_bytes());
}

fn ns_byte(ns: Namespace) -> u8 {
    match ns {
        Namespace::Types => 0,
        Namespace::Values => 1,
        Namespace::Macros => 2,
    }
}

/// A definition kind's byte; a macro's call shapes are its scope's (the scope bytes hold them).
fn kind_byte(kind: DefKind) -> u8 {
    match kind {
        DefKind::Module => 0,
        DefKind::Enum => 1,
        DefKind::Trait => 2,
        DefKind::Macro(_) => 3,
        DefKind::Other => 4,
    }
}
