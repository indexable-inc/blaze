//! The per-crate entry of name resolution: one crate resolved against its dependencies'
//! `CrateScope` records, and its own record written (bottom.md section 6, "Per-crate entry").
//!
//! A dependent reads of a dependency exactly what [`crate::resolve`]'s `Built` serves: its key,
//! root module, edition and proc-macro key; each module's finished scope and chain; each enum's
//! variants; each macro's source. The record holds those and nothing else (no files, no read
//! lists, no notes), plus `reaches`: every other crate whose items its scopes name, near or far,
//! so a dependent asks for all it may walk into in one `Need`.
//!
//! The record's bytes are crate::scope_record's: a fixed header of section offsets, sorted
//! fixed-width rows, one strings section and the blob of each module's blaze-surface
//! `ModuleScope` record. A dependent checks each record once ([`Verified::check`]) and reads it
//! in place ([`DepRecord`]): nothing is decoded, copied or allocated per dependency, so a
//! `core` handed to six hundred crates costs its pages once. [`read_record`], the decode into
//! hash maps this entry read before, is kept as the control form the in-place reads are tested
//! equal against ([`Form::Heap`], rule 78).

use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::Duration;

use blaze_format::FormatError;
use blaze_hash::HashMap;
use blaze_id::{ContentId, CrateItemKey, ItemKey};
use blaze_ids::producer::{Expander, ProducerId};
use blaze_own::{AVec, Alloc, BVec, Bump};
use blaze_surface::{BuildError, Edition, GlobEdge, Namespace, RecordBytes, ScopeBuilder, Unresolved, Visibility};

use crate::asked::{Asked, Question, ReadListError, Stale};
use crate::graph::{CrateGraph, CrateId, GraphCrate, GraphDep, Sysroot};
use crate::prepares::Prepares;
use crate::resolve::{Built, CrateInput, CrateNames, NamesError, SourceTexts, resolve_one};
use crate::scope_record::{DepRecord, Module, Parts, Reaches, ScopeOf, VariantsOf, Verified};
use crate::task::{First, MacroBody, MacroSrc, Par, Variant};

/// A dependency as the resolution reads it: finished in this process, read from its record in
/// place (the default) or decoded (the control form), or solved on demand, group by group (E13,
/// cells/lazy_deps.rs): a read of a module it has not solved yet is noted there and voids the
/// reader's solve, which is asked again once it is.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Dep<'a> {
    Whole(&'a CrateNames<'a>),
    /// A record decoded into hash maps ([`read_record`]): the control form ([`Form::Heap`]).
    Record(&'a DepScope<'a>),
    /// A record read in place ([`DepRecord`]): the form every entry reads ([`Form::InPlace`]).
    InPlace(&'a DepRecord<'a>),
    Lazy(&'a crate::cells::LazyDep<'a>),
}

impl<'a> Dep<'a> {
    pub(crate) fn key(self) -> CrateItemKey {
        match self {
            Dep::Whole(c) => c.key,
            Dep::Record(d) => d.key,
            Dep::InPlace(r) => r.key(),
            Dep::Lazy(l) => l.key,
        }
    }

    pub(crate) fn root(self) -> ItemKey {
        match self {
            Dep::Whole(c) => c.root,
            Dep::Record(d) => d.root,
            Dep::InPlace(r) => r.root(),
            Dep::Lazy(l) => l.root,
        }
    }

    pub(crate) fn name(self) -> &'a str {
        match self {
            Dep::Whole(c) => c.name,
            Dep::Record(d) => d.name,
            Dep::InPlace(r) => r.name(),
            Dep::Lazy(l) => l.name,
        }
    }

    pub(crate) fn edition(self) -> Edition {
        match self {
            Dep::Whole(c) => c.edition,
            Dep::Record(d) => d.edition,
            Dep::InPlace(r) => r.edition(),
            Dep::Lazy(l) => l.edition,
        }
    }

    pub(crate) fn proc_key(self) -> Option<CrateItemKey> {
        match self {
            Dep::Whole(c) => c.proc_key,
            Dep::Record(d) => d.proc_key,
            Dep::InPlace(r) => r.proc_key(),
            Dep::Lazy(l) => l.proc_key,
        }
    }

    pub(crate) fn scope(self, module: ItemKey) -> Option<ScopeOf<'a, &'a str>> {
        match self {
            Dep::Whole(c) => c.done.get(&module).map(|r| ScopeOf::Built(&r.scope)),
            Dep::Record(d) => d.scopes.get(&module).map(ScopeOf::Built),
            Dep::InPlace(r) => r.scope(module).map(ScopeOf::Exports),
            Dep::Lazy(l) => l.scope(module).map(ScopeOf::Built),
        }
    }

    pub(crate) fn chain(self, module: ItemKey) -> Option<&'a [ItemKey]> {
        match self {
            Dep::Whole(c) => c.chains.get(&module).map(|c| c.as_slice()),
            Dep::Record(d) => d.chains.get(&module).map(|c| c.as_slice()),
            Dep::InPlace(r) => r.chain(module),
            Dep::Lazy(l) => l.chain(module),
        }
    }

    pub(crate) fn enum_of(self, key: ItemKey) -> Option<VariantsOf<'a>> {
        match self {
            Dep::Whole(c) => c.tables.enums.get(&key).map(|v| VariantsOf::Slice(v.as_slice())),
            Dep::Record(d) => d.enums.get(&key).map(|v| VariantsOf::Slice(v.as_slice())),
            Dep::InPlace(r) => r.variants(key).map(VariantsOf::Rows),
            Dep::Lazy(l) => l.enum_of(key).map(VariantsOf::Slice),
        }
    }

    pub(crate) fn macro_of(self, key: ItemKey) -> Option<MacroSrc<'a>> {
        match self {
            Dep::Whole(c) => c.tables.macros.get(&key).copied(),
            Dep::Record(d) => d.macros.get(&key).copied(),
            Dep::InPlace(r) => r.macro_of(key),
            Dep::Lazy(l) => l.macro_of(key),
        }
    }

    pub(crate) fn reaches(self) -> Reaches<'a> {
        match self {
            Dep::Whole(_) | Dep::Lazy(_) => Reaches::Keys(&[]),
            Dep::Record(d) => Reaches::Keys(&d.reaches),
            Dep::InPlace(r) => r.reaches(),
        }
    }
}

/// How a handed exports record is read: one seam, both forms tested equal (rule 78,
/// tests/scope_record.rs).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Form {
    /// Checked once ([`Verified::check`]) and read in place: no allocation and no copy per
    /// table, module or name. Every entry's form.
    InPlace,
    /// Decoded into hash maps of working forms ([`read_record`]): the form before 2026-11-21,
    /// kept as the control.
    Heap,
}

/// The record `bytes` (16-aligned, living as long as `alloc`'s values) as a dependency read in
/// `form`.
pub(crate) fn hand<'a>(form: Form, bytes: &'a [u8], alloc: Bump<'a>) -> Result<Dep<'a>, RecordRead> {
    Ok(match form {
        Form::InPlace => Dep::InPlace(alloc.alloc_val(DepRecord::open(Verified::check(bytes)?))),
        Form::Heap => Dep::Record(alloc.alloc_val(read_record(bytes, alloc.alloc())?)),
    })
}

/// Each of `deps` as [`hand`] reads it, in order.
fn handed<'a>(form: Form, deps: &[&'a [u8]], alloc: Bump<'a>) -> Result<AVec<'a, Dep<'a>>, CrateScopeError<'a>> {
    let mut out = AVec::with_capacity_in(deps.len(), alloc.alloc());
    for (at, bytes) in deps.iter().enumerate() {
        out.push(hand(form, bytes, alloc).map_err(|error| CrateScopeError::Record { at, error })?);
    }
    Ok(out)
}

/// A `CrateScope` record's working form, its names borrowed from the record's bytes: the
/// control form ([`Form::Heap`]).
#[derive(Debug)]
pub struct DepScope<'a> {
    pub name: &'a str,
    pub key: CrateItemKey,
    pub root: ItemKey,
    pub edition: Edition,
    pub proc_key: Option<CrateItemKey>,
    /// Every other crate its scopes name an item of, near or far, sorted.
    pub reaches: AVec<'a, CrateItemKey>,
    pub scopes: HashMap<ItemKey, ScopeBuilder<'a, &'a str>, Alloc<'a>>,
    pub chains: HashMap<ItemKey, AVec<'a, ItemKey>, Alloc<'a>>,
    pub enums: HashMap<ItemKey, AVec<'a, Variant<&'a str>>, Alloc<'a>>,
    pub macros: HashMap<ItemKey, MacroSrc<'a>, Alloc<'a>>,
    /// An error crate's record ([`error_record`]): the crate failed (its build script, its own
    /// names), and every name read in it is unknown.
    pub errored: bool,
}

/// Why a `CrateScope` record was not read ([`Verified::check`]). Each names the table and the
/// byte offset that broke it (rule 19).
#[derive(Debug)]
pub enum RecordRead {
    /// Its bytes do not start on a 16-byte boundary: map or copy them to one.
    Unaligned,
    /// Not `BZCS` and this version (an old home's record: its crate is solved again).
    NotThisRecord,
    /// A byte that is no edition, flag, call shape, proc-macro kind or body tag, or a pad or an
    /// unused field that is not zero.
    BadByte { at: usize, what: &'static str },
    /// A section, a row or a range named at `at` runs past the end of what holds it.
    Past { table: &'static str, at: usize },
    /// The rows of `table` (by its count at `at`) do not end where `next` begins: the count or
    /// the next section's offset is not the writer's.
    Layout { table: &'static str, next: &'static str, at: usize },
    /// A row's run in `table` (named at `at`) does not start where the run before it ends.
    Misplaced { table: &'static str, at: usize },
    /// A row of `table` at `at` is not keyed above the row before it, which its search relies on.
    OutOfOrder { table: &'static str, at: usize },
    /// A text range of `table` named at `at` is not UTF-8.
    Utf8 { table: &'static str, at: usize },
    /// A module's `ModuleScope` record.
    Scope { module: ItemKey, error: FormatError },
    /// A module row's scope is the scope of another module.
    NotItsScope { module: ItemKey, found: ItemKey },
    /// An exports record's scope holds a textual scope or a mark, which only an inner record
    /// may (a dependent reads a module's names, never its `macro_rules!` order).
    NotExports { module: ItemKey },
}

/// Why [`resolve_crate`] gave no record.
#[derive(Debug)]
pub enum CrateScopeError<'a> {
    /// The records of these crates are needed (sorted): a dependency's scopes name their items.
    /// Supply them in `deps` too and call again from the top.
    Need(AVec<'a, CrateItemKey>),
    /// `deps[at]` is not a readable record.
    Record { at: usize, error: RecordRead },
    /// The crate's input names a dependency index past `deps`.
    NoSuchDep { krate: &'a str, name: &'a str, index: usize },
    Names(NamesError<'a>),
    /// [`resolve_by_records`]: a `Need` of `krate` named `need`, which it does not wait on
    /// (no dependency of it, nor a sysroot crate before it) and which is not done: the record
    /// it reaches is outside its dependencies' closure.
    NeedNotWaitedOn { krate: &'a str, need: &'a str },
    /// [`resolve_by_records`]: a free worker waited `waited` and no crate became ready;
    /// `krate` is the first not done in the graph's order.
    Stalled { krate: &'a str, waited: Duration },
    /// Writing its record.
    Write { krate: &'a str, error: BuildError },
    /// [`resolve_by_records`]: the exports record `krate` just wrote did not read back.
    OwnRecord { krate: &'a str, error: RecordRead },
    /// [`resolve_kept`]: the read list kept with `krate`'s solve did not read.
    ReadList { krate: &'a str, error: ReadListError },
}

/// One crate's names: what the resolution made (findings, unresolved imports and macro paths
/// in its scopes' errors) and its two `CrateScope` records ([`Records`]).
#[derive(Debug)]
pub struct CrateScope<'a> {
    pub names: &'a CrateNames<'a>,
    /// The exports record: what a dependent reads, and the only bytes its key covers.
    pub record: RecordBytes<'a>,
    /// The inner record: the rest, the crate's own.
    pub inner: RecordBytes<'a>,
    /// Each question the solve asked of another crate's records ([`crate::asked`]).
    pub(crate) asked: AVec<'a, Question<'a>>,
}

/// The crates `deps`' records reach that none of `deps` is, sorted and without repeats: what
/// [`resolve_crate`] would answer [`CrateScopeError::Need`] for, read from the records' heads
/// in place before any record is checked, so a caller asks for them all before its first call
/// (reach lists are closed, near or far, so one round asks for every one).
pub fn unmet_reaches<'a>(deps: &[&[u8]], alloc: Alloc<'a>) -> Result<AVec<'a, CrateItemKey>, CrateScopeError<'a>> {
    let mut heads = AVec::with_capacity_in(deps.len(), alloc);
    let mut held = AVec::with_capacity_in(deps.len(), alloc);
    for (at, bytes) in deps.iter().enumerate() {
        let (krate, keys) = crate::scope_record::peek_reaches(bytes).map_err(|error| CrateScopeError::Record { at, error })?;
        heads.push(keys);
        held.push(krate.prefix());
    }
    held.sort_unstable();
    let mut need = AVec::new_in(alloc);
    for keys in heads.iter() {
        for key in keys.iter() {
            let key = CrateItemKey(ContentId(*key));
            if held.binary_search(&key.prefix()).is_err() {
                need.push(key);
            }
        }
    }
    need.sort_unstable();
    need.dedup();
    Ok(need)
}

/// A `CrateScope` exports record (its bytes 16-aligned and living as long as `alloc`'s values)
/// checked and DECODED into hash maps of working forms in `alloc`: the control form
/// ([`Form::Heap`]) the in-place reads are tested equal against. Every entry reads in place.
pub fn read_record<'a>(bytes: &'a [u8], alloc: Alloc<'a>) -> Result<DepScope<'a>, RecordRead> {
    let record = DepRecord::open(Verified::check(bytes)?);
    let mut reaches = AVec::with_capacity_in(record.reaches().len(), alloc);
    reaches.extend(record.reaches().iter());
    let mut scopes = HashMap::new_in(alloc);
    let mut chains = HashMap::new_in(alloc);
    for module in record.modules() {
        if let Some(scope) = record.scope(module) {
            scopes.insert(module, ScopeBuilder::from(&scope.view(), alloc));
        }
        if let Some(chain) = record.chain(module) {
            chains.insert(module, AVec::from_slice_in(chain, alloc));
        }
    }
    let mut enums = HashMap::new_in(alloc);
    for owner in record.enums() {
        if let Some(variants) = record.variants(owner) {
            enums.insert(owner, AVec::from_iter_in(variants.iter(), alloc));
        }
    }
    let mut macros = HashMap::new_in(alloc);
    for def in record.macros() {
        if let Some(m) = record.macro_of(def) {
            macros.insert(def, m);
        }
    }
    Ok(DepScope {
        name: record.name(),
        key: record.key(),
        root: record.root(),
        edition: record.edition(),
        proc_key: record.proc_key(),
        reaches,
        scopes,
        chains,
        enums,
        macros,
        errored: record.errored(),
    })
}

/// The exports record of a crate that failed this command, so no record of its names exists
/// (its build script failed, or its own names could not be resolved): its key, its root (an
/// empty scope), no reaches, enums or macros, and the errored flag. A dependent is solved
/// against it as against any record: a path into the crate finds nothing there, and its
/// reporter, seeing the flag ([`is_error_record`]), counts it without a finding, so only the
/// failure itself is reported, once, at its cause (rule 19). Plan row A8: an error stops only
/// what reads it. The bytes are a function of `key`, `name` and `edition` alone (rule 25).
pub fn error_record<'b>(key: CrateItemKey, name: &str, edition: Edition, alloc: Alloc<'b>) -> Result<RecordBytes<'b>, BuildError> {
    let root = crate::task::root_key(key);
    let scope = ScopeBuilder::<'_, &str>::new_in(root, alloc).write(alloc)?;
    let modules = [Module { key: root, scope: &scope, chain: None }];
    crate::scope_record::write(
        &Parts { key, root, edition, proc_key: None, errored: true, name, reaches: &[], modules: &modules, enums: &[], macros: &[] },
        alloc,
    )
}

/// Whether `bytes`, a `CrateScope` record of this version, is an [`error_record`]: one byte of
/// its head read in place, nothing checked. A record of another magic or version is none.
pub fn is_error_record(bytes: &[u8]) -> bool {
    crate::scope_record::is_errored(bytes)
}

/// The crate an [`error_record`] is of, its key and its own name, read in place: what a
/// dependent's reporter names a path behind it by ([`crate::waits_on`]). `None` for a record
/// that is no error record, or one too short to hold its head.
pub fn error_record_crate(bytes: &[u8]) -> Option<(CrateItemKey, &str)> {
    if !is_error_record(bytes) {
        return None;
    }
    crate::scope_record::peek_name(bytes)
}

/// A reporter's reading of a failed import, asked only once a lookup failed, never on the
/// success path. A dependent reads only a crate's exports record, which leaves out each name
/// it cannot see, so a path to another crate's private item fails there as `NotFound`. The
/// Reference, "Visibility and privacy": "If an item is private, it may be accessed by the
/// current module and its descendants"; from anywhere else it is a privacy error. So when
/// `module`, in the crate whose INNER record is `inner`, binds `name` (in any namespace: a
/// `use` imports all three), the failure is `Private`; otherwise `why` stands.
pub fn why_reported<'a>(why: Unresolved, inner: &[u8], module: ItemKey, name: &str, alloc: Alloc<'a>) -> Result<Unresolved, RecordRead> {
    if why != Unresolved::NotFound {
        return Ok(why);
    }
    // Only on a failure's report: the inner record checked whole, one module row found, its
    // bindings scanned in place (no decode).
    let aligned = crate::resolve::AlignedBuf::copy_from(inner, alloc);
    let bound = crate::scope_record::inner_binds(aligned.as_slice(), module, name)?;
    Ok(if bound { Unresolved::Private } else { why })
}

/// What another crate can read of a crate's names: the part every dependent's key covers.
/// The rule (Reference, "Visibility and privacy"; "Macros By Example", "Hygiene"):
/// - a `pub` name, always;
/// - a `pub(crate)` or `pub(in path)` name (a private one is `pub(in self)`) only where a 2.0
///   `macro` of the crate is defined in a module that sees it: such a macro's body resolves its
///   paths at its definition site, with that module's privacy, when a dependent expands it. A
///   `macro_rules!` body (even `#[macro_export]`, through `$crate`) resolves paths with the
///   CALLER's privacy (mixed-site hygiene), so it reaches only `pub` names;
/// - a name no other crate sees that may shadow a name a visible glob of its module brings: a
///   module's own binding shadows its globs whatever its visibility, so without it a dependent
///   would read the glob's name where it must meet a private one;
/// - only the modules a dependent can walk into: the root, every 2.0 macro's definition site
///   and its ancestors, and each module a kept binding or glob of a walked module names.
///
/// The rest (private names, errors, textual `macro_rules!` scopes, unreachable modules) is the
/// crate's own: its inner record, which no dependent reads or keys on.
struct Outside<'b> {
    /// Each module a 2.0 macro is defined in, and its ancestors, sorted.
    def_site: AVec<'b, ItemKey>,
}

impl Outside<'_> {
    fn sees(&self, vis: Visibility) -> bool {
        match vis {
            Visibility::Public => true,
            Visibility::Crate(_) => !self.def_site.is_empty(),
            Visibility::Restricted(m) => self.def_site.binary_search(&m).is_ok(),
        }
    }
}

/// Whether a binding `name` in `ns` of a module whose visible globs are `globs` may shadow a
/// name one of them brings: the glob's source binds the name, or has globs of its own (not
/// walked: the binding is kept).
fn may_shadow<'a>(names: &CrateNames<'_>, built: &Built<'_, 'a>, globs: &[GlobEdge], outside: &Outside<'_>, ns: Namespace, name: &'a str) -> bool {
    let own = names.key.prefix();
    globs.iter().filter(|g| outside.sees(g.vis)).any(|g| {
        let key = g.source.key;
        if key.0.0[..8] == own {
            if let Some(r) = names.done.get(&key) {
                return !r.scope.globs.is_empty() || r.scope.get(ns, name).is_some();
            }
            return names.tables.enums.get(&key).is_none_or(|v| v.iter().any(|v| v.name == name));
        }
        let may = dep_may_shadow(built, key, ns, name);
        built.ask(|| crate::asked::Question::shadow(key, ns, name, may));
        may
    })
}

/// [`may_shadow`] of one glob source in another crate: it binds the name, or has globs of its
/// own; a source of no crate held may.
pub(crate) fn dep_may_shadow(built: &Built<'_, '_>, key: ItemKey, ns: Namespace, name: &str) -> bool {
    let Some(dep) = built.of(key) else { return true };
    if let Some(s) = dep.scope(key) {
        return !s.globs().is_empty() || s.get(ns, name).is_some();
    }
    dep.enum_of(key).is_none_or(|v| v.iter().any(|v| v.name == name))
}

/// A crate's two `CrateScope` records: `exports`, what its dependents read and key on (the rule
/// of [`Outside`]), and `inner`, the rest, read only by the crate itself. Both have one format;
/// a module whose scope is split has a scope in each, to be read as their union.
#[derive(Debug)]
pub struct Records<'b> {
    pub exports: RecordBytes<'b>,
    pub inner: RecordBytes<'b>,
}

/// The two records of `names`, the crates of `built` answering for the items of other crates
/// its scopes name. `leak_private` is the planted bug of the privacy test: every binding is
/// written `pub`.
pub(crate) fn write_records<'a, 'b>(names: &CrateNames<'a>, built: &Built<'_, 'a>, leak_private: bool, alloc: Alloc<'b>) -> Result<Records<'b>, BuildError> {
    let own = names.key.prefix();
    let mut def_site = AVec::new_in(alloc);
    for m in names.tables.macros.values() {
        if matches!(m.body, MacroBody::Macro2 { .. }) {
            def_site.push(m.module);
            if let Some(chain) = names.chains.get(&m.module) {
                def_site.extend_from_slice(chain);
            }
        }
    }
    def_site.sort_unstable();
    def_site.dedup();
    let outside = Outside { def_site };
    // Each module's scope split: what `outside` keeps, and the rest.
    let mut split: HashMap<ItemKey, (ScopeBuilder<'b, &str>, ScopeBuilder<'_, &str>), Alloc<'b>> = HashMap::new_in(alloc);
    for (m, done) in names.done.iter() {
        let mut inner = done.scope.clone();
        if leak_private {
            for e in inner.bindings.iter_mut() {
                e.binding.vis = Visibility::Public;
            }
        }
        let mut exports = ScopeBuilder::new_in(*m, alloc);
        let mut keep = AVec::with_capacity_in(inner.bindings.len(), alloc);
        for e in inner.bindings.iter() {
            keep.push(outside.sees(e.binding.vis) || may_shadow(names, built, &inner.globs, &outside, e.ns, e.name));
        }
        let mut at = 0;
        inner.bindings.retain(|e| {
            at += 1;
            if keep[at - 1] {
                exports.bindings.push(*e);
            }
            !keep[at - 1]
        });
        inner.globs.retain(|g| {
            if outside.sees(g.vis) {
                exports.globs.push(*g);
            }
            !outside.sees(g.vis)
        });
        inner.traits.retain(|t| {
            if outside.sees(t.vis) {
                exports.traits.push(*t);
            }
            !outside.sees(t.vis)
        });
        exports.macro_exports.extend(inner.macro_exports.drain(..));
        split.insert(*m, (exports, inner));
    }
    // The modules a dependent can walk into, and every item key their kept names name.
    let mut walked: HashMap<ItemKey, (), Alloc<'b>> = HashMap::new_in(alloc);
    let mut named: HashMap<ItemKey, (), Alloc<'b>> = HashMap::new_in(alloc);
    let mut work = AVec::new_in(alloc);
    work.push(names.root);
    work.extend_from_slice(&outside.def_site);
    while let Some(m) = work.pop() {
        let Some((exports, _)) = split.get(&m) else { continue };
        if walked.insert(m, ()).is_some() {
            continue;
        }
        let keys = exports
            .bindings
            .iter()
            .map(|e| e.binding.def.key)
            .chain(exports.globs.iter().map(|g| g.source.key))
            .chain(exports.traits.iter().map(|t| t.def.key))
            .chain(exports.macro_exports.iter().map(|t| t.def));
        for key in keys {
            named.insert(key, ());
            if key.0.0[..8] == own && split.contains_key(&key) && !walked.contains_key(&key) {
                work.push(key);
            }
        }
    }
    // A `#[macro_export]` macro is a binding of the root for other crates (its own crate does
    // not see it by path), and a dependent reads its definition by key.
    for (def, m) in names.tables.macros.iter() {
        if m.export {
            named.insert(*def, ());
        }
    }
    let mut modules = AVec::from_iter_in(names.done.keys().copied(), alloc);
    modules.sort_unstable();
    // Every other crate a part's scopes name an item of, and what that crate reaches.
    let mut reach = [AVec::new_in(alloc), AVec::new_in(alloc)];
    let add = |part: Part, key: ItemKey, reach: &mut [AVec<'b, CrateItemKey>; 2]| {
        if key.0.0[..8] == own {
            return;
        }
        if let Some(dep) = built.of(key) {
            reach[part as usize].push(dep.key());
            reach[part as usize].extend(dep.reaches().iter());
        }
    };
    let mut exports_scopes = AVec::new_in(alloc);
    let mut inner_scopes = AVec::new_in(alloc);
    for m in modules.iter() {
        let (exports, inner) = &split[m];
        // An unwalked module's whole scope is the crate's own.
        let to = if walked.contains_key(m) { Part::Exports } else { Part::Inner };
        for e in exports.bindings.iter() {
            add(to, e.binding.def.key, &mut reach);
        }
        for g in exports.globs.iter() {
            add(to, g.source.key, &mut reach);
        }
        for e in inner.bindings.iter() {
            add(Part::Inner, e.binding.def.key, &mut reach);
        }
        for g in inner.globs.iter() {
            add(Part::Inner, g.source.key, &mut reach);
        }
        if to == Part::Exports {
            exports_scopes.push(Some(exports.write(alloc)?));
            inner_scopes.push(Some(inner.write(alloc)?));
        } else {
            exports_scopes.push(None);
            inner_scopes.push(Some(names.done[m].scope.write(alloc)?));
        }
    }
    let [exports_reach, inner_reach] = reach;
    let exports = write_part(names, &modules, &exports_scopes, &exports_reach, &|k| named.contains_key(k), alloc)?;
    let inner = write_part(names, &modules, &inner_scopes, &inner_reach, &|k| !named.contains_key(k), alloc)?;
    Ok(Records { exports, inner })
}

/// Which of the two records a name goes to.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Part {
    Exports = 0,
    Inner = 1,
}

/// One record: each of `modules` that has a scope's bytes in `scopes`, the crates it `reaches`,
/// and the enums and macros `keeps`, in crate::scope_record's form.
fn write_part<'b>(
    names: &CrateNames<'_>,
    modules: &[ItemKey],
    scopes: &[Option<RecordBytes<'b>>],
    reaches: &[CrateItemKey],
    keeps: &dyn Fn(&ItemKey) -> bool,
    alloc: Alloc<'b>,
) -> Result<RecordBytes<'b>, BuildError> {
    let mut rows = AVec::with_capacity_in(modules.len(), alloc);
    for (m, bytes) in modules.iter().zip(scopes.iter()) {
        let Some(bytes) = bytes else { continue };
        rows.push(Module { key: *m, scope: bytes, chain: names.chains.get(m).map(|c| c.as_slice()) });
    }
    let enums = AVec::from_iter_in(names.tables.enums.iter().filter(|(k, _)| keeps(k)).map(|(k, v)| (*k, v.as_slice())), alloc);
    let macros = AVec::from_iter_in(names.tables.macros.iter().filter(|(k, _)| keeps(k)).map(|(k, m)| (*k, *m)), alloc);
    let parts = Parts {
        key: names.key,
        root: names.root,
        edition: names.edition,
        proc_key: names.proc_key,
        errored: false,
        name: names.name,
        reaches,
        modules: &rows,
        enums: &enums,
        macros: &macros,
    };
    crate::scope_record::write(&parts, alloc)
}

/// One crate's names against its dependencies' `CrateScope` records, and its own record.
///
/// `input.deps` index `deps`; `sysroot` names crates of `deps` too, `CrateId(deps.len())`
/// being this crate itself (it is `core`). `deps` may hold more than the direct dependencies:
/// every record a dependency `reaches` must be in it, or the answer is [`CrateScopeError::Need`].
/// Each of `deps` is 16-aligned and lives as long as `alloc`'s values. `source` is the crate's
/// index for `sources`. A crate in a dependency cycle cannot be given: the driver breaks it
/// (a dev-dependency's test unit is its own crate, keyed apart from the library it uses).
/// `prepares` is the build's one set of prepared macro definitions (by defining item), shared
/// by every crate it resolves at once ([`Prepares`] is safe for concurrent callers).
pub fn resolve_crate<'a, S: SourceTexts<'a>, P: Par>(
    input: &CrateInput<'a>,
    deps: &[&'a [u8]],
    sysroot: Sysroot,
    sources: &S,
    source: usize,
    par: &P,
    prepares: &'a Prepares<'a>,
    alloc: Bump<'a>,
) -> Result<CrateScope<'a>, CrateScopeError<'a>> {
    resolve_crate_with(input, deps, sysroot, sources, source, par, false, prepares, alloc)
}

/// How [`resolve_by_records`] schedules: `workers` workers of its `par` each take the next
/// ready crate (one whose dependencies' `exports` records exist), the lowest `rank` first (ties
/// and `None` by the graph's order). The schedule changes when a crate is resolved, never what
/// its records hold: each crate is given the same records in the same order whatever ran
/// before it (tests/per_crate.rs, `records_equal_across_workers_and_orders`).
pub struct Ready<'r> {
    pub workers: usize,
    /// By crate index: which ready crate a free worker takes first.
    pub rank: Option<&'r [u32]>,
    /// The longest a free worker waits for a crate to become ready (rule 18): past it the
    /// call stops with [`CrateScopeError::Stalled`].
    pub wait: Duration,
    pub hooks: &'r (dyn ReadyHooks + Sync),
}

/// What the caller is told as crates run (a timing, a progress line): never what they hold.
pub trait ReadyHooks {
    /// `krate` was taken by `worker`.
    fn began(&self, krate: usize, worker: usize) {
        let _ = (krate, worker);
    }

    /// `krate`'s records were written (or it failed).
    fn ended(&self, krate: usize, worker: usize) {
        let _ = (krate, worker);
    }

    /// `krate`'s own resolution is done and its exports record is about to be checked (read in
    /// place: [`Verified::check`]; decoded only in the control form): what is allocated from
    /// here to [`ReadyHooks::decoded`] is the handed record (one small box in place).
    fn decoding(&self, krate: usize) {
        let _ = krate;
    }

    /// `krate`'s exports record (`bytes` long) was checked and is handed as it is: once a call.
    fn decoded(&self, krate: usize, bytes: usize) {
        let _ = (krate, bytes);
    }

    /// `krate`'s record was given to a crate being resolved (one of its dependencies, a
    /// sysroot crate before it, or one a `Need` named), once for each try.
    fn handed(&self, krate: usize) {
        let _ = krate;
    }
}

/// The domain of an exports record's digest: what a dependent's read list holds of each
/// dependency it was handed ([`Kept`]).
pub const EXPORTS_DOMAIN: &str = "blaze-scope exports record v1";

/// Earlier solves kept between calls (the driver's records; blaze-world's `names --records`):
/// asked before a crate is solved, told once it was. A crate is found, not solved, when its
/// files are read the same now (the store's own check: [`SourceTexts`] is where it sees them
/// read) and its read list holds ([`crate::asked::check`]): each crate it was handed has the
/// same exports digest, or else each question it asked of a moved one is answered the same.
pub trait Kept: Sync {
    /// `krate`'s exports record, inner record and read list (the bytes `made` was given) if its
    /// files are unchanged; the read list is checked by the caller.
    fn find(&self, krate: usize) -> Option<(&[u8], &[u8], &[u8])>;

    /// `krate`'s kept solve was not found after all: its read list did not hold, for `why`.
    fn stale(&self, krate: usize, why: &Stale<'_>) {
        let _ = (krate, why);
    }

    /// `krate` was solved: its two records and its read list record ([`crate::asked::encode`]).
    fn made(&self, krate: usize, record: &[u8], inner: &[u8], read: &[u8]);

    /// Whether `names` wants each solved crate's full-names record ([`crate::names_record`]):
    /// encoding one costs a pass over the crate's scopes, so nothing is written unasked.
    fn keeps_names(&self) -> bool {
        false
    }

    /// `krate` was solved: its full-names record ([`crate::names_record::encode`]), kept with
    /// the bytes `made` was given and found with them.
    fn names(&self, _krate: usize, _names: &[u8]) {}

    /// Whether `items` wants each solved crate's items record ([`crate::items_record`]): the
    /// second half of a crate's full names, what the type-checking front end reads in place of
    /// the crate's tables. Encoding one costs a pass over the crate's items, so nothing is
    /// written unasked.
    fn keeps_items(&self) -> bool {
        false
    }

    /// `krate` was solved: its items record ([`crate::items_record::encode`]), kept with the
    /// bytes `made` was given and found with them.
    fn items(&self, _krate: usize, _items: &[u8]) {}

    /// Whether each solved crate's expansions are kept ([`crate::expansions`]), and by which
    /// expander's code (decision D105: the id the program's entry hands down, never a typed-in
    /// name): when `None`, no call's key is made and nothing is collected.
    fn keeps_expansions(&self) -> Option<ProducerId<Expander>> {
        None
    }

    /// `krate`'s expansions from its last solve ([`Kept::expansions_made`]'s bytes), WHETHER OR
    /// NOT its files changed since: a crate solved again after an edit finds its unchanged
    /// calls here, each by its text's key. The bytes start at a multiple of
    /// [`blaze_expand::kept::ALIGN`]. Asked only when [`Kept::keeps_expansions`].
    fn expansions(&self, _krate: usize) -> Option<&[u8]> {
        None
    }

    /// `krate` was solved: its expansions set ([`crate::expansions::KeptExpansions::record`]),
    /// kept by the crate, replacing the last. Told only when [`Kept::keeps_expansions`].
    fn expansions_made(&self, _krate: usize, _set: &[u8]) {}

    /// The planted bug a test's store asks for ([`crate::expansions::Planted`]): none, always,
    /// outside the test that must catch it.
    fn expansions_planted(&self) -> crate::expansions::Planted {
        crate::expansions::Planted::None
    }
}

/// One crate's answer from [`resolve_kept`].
#[derive(Debug)]
pub enum Solved<'a> {
    /// Solved in this call.
    Solved(CrateScope<'a>),
    /// Found from an earlier solve ([`Kept::find`]): its two records, nothing solved.
    Found { record: &'a [u8], inner: &'a [u8] },
}

/// No hooks.
pub struct NoHooks;

impl ReadyHooks for NoHooks {}

impl Ready<'static> {
    /// One worker, the graph's order, no hooks.
    pub fn serial(wait: Duration) -> Ready<'static> {
        Ready { workers: 1, rank: None, wait, hooks: &NoHooks }
    }
}

/// Every crate of `crates` through [`resolve_crate`] on a ready queue ([`Ready`]), each given
/// only records: its direct dependencies' and the sysroot crates before it in the graph's order
/// (it waits for those), then what each `Need` names. One [`Prepares`] for the whole call: a
/// macro defined in one crate and called in fifty is prepared once. A measurement and the
/// proof's other side, not the driver: the driver keys, stores and schedules (drive.md, "SPEC,
/// the NAMES step"). The answer is by crate index; `needs` counts the re-entries a `Need` made.
/// `alloc` is taken from every worker at once: it must be a heap any thread may use (as
/// `par`'s threads already require).
pub fn resolve_by_records<'a, S: SourceTexts<'a>, P: Par>(
    crates: &[CrateInput<'a>],
    sysroot: Sysroot,
    sources: &S,
    par: &P,
    ready: &Ready<'_>,
    alloc: Bump<'a>,
) -> Result<(AVec<'a, CrateScope<'a>>, usize), (usize, CrateScopeError<'a>)> {
    resolve_by_records_with(crates, sysroot, sources, par, ready, false, alloc)
}

/// [`resolve_by_records`] with earlier solves: each crate is first looked up in `kept` and
/// solved only when not found; each solve is told to `kept` with its read list. A found crate's
/// records are exactly those it was solved to (the key is its read list, checked against this
/// call's records), so what dependents are handed is the same either way.
pub fn resolve_kept<'a, S: SourceTexts<'a>, P: Par>(
    crates: &[CrateInput<'a>],
    sysroot: Sysroot,
    sources: &S,
    par: &P,
    ready: &Ready<'_>,
    kept: &dyn Kept,
    alloc: Bump<'a>,
) -> Result<(AVec<'a, Solved<'a>>, usize), (usize, CrateScopeError<'a>)> {
    resolve_queue(crates, sysroot, sources, par, ready, false, Some(kept), Form::InPlace, alloc)
}

/// The ready queue's state, under one lock: held to take a crate, to read records, and to
/// publish one; never across a resolution.
struct Queue<'a> {
    /// By crate index: its exports record once written, checked (or, for the control form,
    /// decoded) once for the whole call and handed to every reader as it is.
    records: AVec<'a, Option<Dep<'a>>>,
    /// By crate index: its exports record's digest ([`EXPORTS_DOMAIN`]) once written.
    digests: AVec<'a, Option<ContentId>>,
    out: AVec<'a, Option<Solved<'a>>>,
    /// By crate index: how many of the crates it waits on are not done.
    blocked: AVec<'a, u32>,
    ready: AVec<'a, usize>,
    in_flight: usize,
    left: usize,
    needs: usize,
    /// The failure earliest in the graph's order: (its position, crate, error).
    failed: Option<(usize, usize, CrateScopeError<'a>)>,
}

impl<'a> Queue<'a> {
    fn fail(&mut self, pos: usize, at: usize, error: CrateScopeError<'a>) {
        if self.failed.as_ref().is_none_or(|(p, _, _)| pos < *p) {
            self.failed = Some((pos, at, error));
        }
    }
}

fn locked<'q, 'a>(queue: &'q Mutex<Queue<'a>>) -> MutexGuard<'q, Queue<'a>> {
    queue.lock().unwrap_or_else(|poisoned| panic!("the names ready queue's lock was poisoned by a panic while held: {poisoned}"))
}

pub(crate) fn resolve_by_records_with<'a, S: SourceTexts<'a>, P: Par>(
    crates: &[CrateInput<'a>],
    sysroot: Sysroot,
    sources: &S,
    par: &P,
    ready: &Ready<'_>,
    leak_private: bool,
    alloc: Bump<'a>,
) -> Result<(AVec<'a, CrateScope<'a>>, usize), (usize, CrateScopeError<'a>)> {
    resolve_by_records_in(crates, sysroot, sources, par, ready, leak_private, Form::InPlace, alloc)
}

/// [`resolve_by_records_with`] with each handed record read in `form`: the seam the two forms
/// are tested equal at (tests/scope_record.rs).
#[allow(clippy::too_many_arguments, reason = "the entry's arguments, the planted bug's switch and the form")]
pub(crate) fn resolve_by_records_in<'a, S: SourceTexts<'a>, P: Par>(
    crates: &[CrateInput<'a>],
    sysroot: Sysroot,
    sources: &S,
    par: &P,
    ready: &Ready<'_>,
    leak_private: bool,
    form: Form,
    alloc: Bump<'a>,
) -> Result<(AVec<'a, CrateScope<'a>>, usize), (usize, CrateScopeError<'a>)> {
    let (solved, needs) = resolve_queue(crates, sysroot, sources, par, ready, leak_private, None, form, alloc)?;
    let mut all = AVec::with_capacity_in(solved.len(), alloc.alloc());
    for (at, s) in solved.into_iter().enumerate() {
        match s {
            Solved::Solved(scope) => all.push(scope),
            // No `Kept` was given, so nothing is found.
            Solved::Found { .. } => return Err((at, CrateScopeError::Names(NamesError::NotResolved { krate: crates[at].name }))),
        }
    }
    Ok((all, needs))
}

#[allow(clippy::too_many_lines, reason = "one worker's loop: take, find or resolve with its Needs, publish")]
#[allow(clippy::too_many_arguments, reason = "the entry's arguments, the planted bug's switch, the kept solves and the form")]
fn resolve_queue<'a, S: SourceTexts<'a>, P: Par>(
    crates: &[CrateInput<'a>],
    sysroot: Sysroot,
    sources: &S,
    par: &P,
    ready: &Ready<'_>,
    leak_private: bool,
    kept: Option<&dyn Kept>,
    form: Form,
    bump: Bump<'a>,
) -> Result<(AVec<'a, Solved<'a>>, usize), (usize, CrateScopeError<'a>)> {
    let alloc = bump.alloc();
    let graph = crate::resolve::graph_of(crates, sysroot, alloc).map_err(|e| (0, CrateScopeError::Names(e)))?;
    let n = crates.len();
    let mut pos: AVec<'a, usize> = AVec::from_iter_in(crates.iter().map(|_| usize::MAX), alloc);
    for (p, id) in graph.order().iter().enumerate() {
        pos[id.index()] = p;
    }
    let by_key: HashMap<CrateItemKey, usize, Alloc<'a>> = {
        let mut m = HashMap::new_in(alloc);
        m.extend(crates.iter().enumerate().map(|(i, c)| (c.key, i)));
        m
    };
    // What each crate waits on: its direct dependencies and the sysroot crates before it in
    // the graph's order (the serial walk's "finished before it"); every edge runs forward in
    // that order, so the queue cannot stall on a cycle.
    let waits_on = |at: usize, out: &mut AVec<'a, usize>| {
        out.clear();
        out.extend(crates[at].deps.iter().map(|&(_, d)| d));
        out.extend(sysroot.all().into_iter().flatten().map(CrateId::index).filter(|&s| s != at && pos[s] < pos[at]));
        out.sort_unstable();
        out.dedup();
    };
    let mut dependents: AVec<'a, AVec<'a, usize>> = AVec::from_iter_in(crates.iter().map(|_| AVec::new_in(alloc)), alloc);
    let mut blocked: AVec<'a, u32> = AVec::from_iter_in(crates.iter().map(|_| 0), alloc);
    let mut on = AVec::new_in(alloc);
    for at in 0..n {
        waits_on(at, &mut on);
        blocked[at] = on.len() as u32;
        for &d in on.iter() {
            dependents[d].push(at);
        }
    }
    let ready_now: AVec<'a, usize> = AVec::from_iter_in((0..n).filter(|&at| blocked[at] == 0), alloc);
    let queue = Mutex::new(Queue {
        records: AVec::from_iter_in(crates.iter().map(|_| None), alloc),
        digests: AVec::from_iter_in(crates.iter().map(|_| None), alloc),
        out: AVec::from_iter_in(crates.iter().map(|_| None), alloc),
        blocked,
        ready: ready_now,
        in_flight: 0,
        left: n,
        needs: 0,
        failed: None,
    });
    let woken = Condvar::new();
    let prepares = Prepares::new_in(bump);
    let rank_of = |at: usize| (ready.rank.map_or(0, |r| r[at]), pos[at]);
    let worker = |w: usize| {
        loop {
            // Take the best ready crate, or wait for one.
            let at = {
                let q = locked(&queue);
                let idle = |q: &mut Queue<'a>| q.failed.is_none() && q.left > 0 && q.ready.is_empty() && q.in_flight > 0;
                let (mut q, waited) = woken.wait_timeout_while(q, ready.wait, idle).unwrap_or_else(|poisoned| panic!("the names ready queue's lock was poisoned by a panic while held: {poisoned}"));
                if q.failed.is_some() || q.left == 0 {
                    return;
                }
                if waited.timed_out() && q.ready.is_empty() {
                    let at = graph.order().iter().map(|id| id.index()).find(|&at| q.out[at].is_none()).unwrap_or(0);
                    Queue::fail(&mut q, 0, at, CrateScopeError::Stalled { krate: crates[at].name, waited: ready.wait });
                    woken.notify_all();
                    return;
                }
                let Some(i) = (0..q.ready.len()).min_by_key(|&i| rank_of(q.ready[i])) else {
                    // Nothing ready and nothing running: every crate left waits on one that
                    // failed (which ends the call) or the waits are wrong.
                    let at = graph.order().iter().map(|id| id.index()).find(|&at| q.out[at].is_none()).unwrap_or(0);
                    Queue::fail(&mut q, pos[at], at, CrateScopeError::Names(NamesError::NotResolved { krate: crates[at].name }));
                    woken.notify_all();
                    return;
                };
                let at = q.ready.swap_remove(i);
                q.in_flight += 1;
                at
            };
            ready.hooks.began(at, w);
            let input = &crates[at];
            // Local order: the direct dependencies, then the sysroot crates before it, then
            // each crate a `Need` names.
            let mut local: AVec<'a, usize> = AVec::new_in(alloc);
            let place = |global: usize, local: &mut AVec<'a, usize>| match local.iter().position(|g| *g == global) {
                Some(p) => p,
                None => {
                    local.push(global);
                    local.len() - 1
                }
            };
            let mut own_deps = BVec::with_capacity_in(input.deps.len(), bump);
            for &(name, index) in input.deps {
                own_deps.push((name, place(index, &mut local)));
            }
            let own_deps: &'a [(&'a str, usize)] = own_deps.into_bump_slice();
            let mut mapped = [None; 4];
            for (slot, s) in mapped.iter_mut().zip(sysroot.all()) {
                *slot = s.filter(|s| s.index() != at && pos[s.index()] < pos[at]).map(|s| place(s.index(), &mut local));
            }
            // An earlier solve whose read list holds today: its records, copied out of the store.
            let now = |key: CrateItemKey| {
                let &g = by_key.get(&key)?;
                let q = locked(&queue);
                Some(q.digests[g].zip(q.records[g]).map(|(d, r)| (g, d, r)))
            };
            let found = match kept.and_then(|k| k.find(at).map(|f| (k, f))) {
                None => Ok(None),
                Some((k, (record, inner, read))) => {
                    let read = crate::resolve::aligned(read, bump);
                    match crate::asked::check(read, &now, alloc) {
                        Err(error) => Err(CrateScopeError::ReadList { krate: input.name, error }),
                        Ok(Some(why)) => {
                            k.stale(at, &why);
                            Ok(None)
                        }
                        Ok(None) => {
                            let (r, i) = (crate::resolve::aligned(record, bump), crate::resolve::aligned(inner, bump));
                            Ok(Some(Solved::Found { record: &r[..record.len()], inner: &i[..inner.len()] }))
                        }
                    }
                }
            };
            let mut tries = 0;
            let result = match found { Err(e) => Err(e), Ok(Some(found)) => Ok(found), Ok(None) => loop {
                tries += 1;
                par.stage(crate::task::Stage::Try);
                let me = local.len();
                let ids = mapped.map(|p| p.map(|p| CrateId(p as u32)));
                let own = |s: Option<CrateId>| s.filter(|s| s.index() == at).map(|_| CrateId(me as u32));
                let all = sysroot.all();
                let local_sysroot = Sysroot {
                    core: ids[0].or(own(all[0])),
                    alloc: ids[1].or(own(all[1])),
                    std: ids[2].or(own(all[2])),
                    bstd: ids[3].or(own(all[3])),
                };
                let deps = {
                    let q = locked(&queue);
                    let mut deps = AVec::with_capacity_in(local.len(), alloc);
                    let mut missing = None;
                    for g in local.iter() {
                        match q.records[*g] {
                            Some(r) => {
                                ready.hooks.handed(*g);
                                deps.push(r);
                            }
                            None => {
                                missing = Some(*g);
                                break;
                            }
                        }
                    }
                    match missing {
                        // A `Need` named a crate this one does not wait on, not done yet.
                        Some(g) => Err(CrateScopeError::NeedNotWaitedOn { krate: input.name, need: crates[g].name }),
                        None => Ok(deps),
                    }
                };
                let deps = match deps {
                    Ok(deps) => deps,
                    Err(e) => break Err(e),
                };
                let local_input = CrateInput { deps: own_deps, ..*input };
                // The crate's expansions kept between solves ([`crate::expansions`]): asked for
                // whether or not its files changed, collected anew by each try, told once the
                // solve finished.
                let keeper = kept.and_then(|k| k.keeps_expansions().map(|expander| (k, expander)));
                let kept_expansions = match keeper {
                    Some((k, expander)) => crate::expansions::KeptExpansions::open_planted(expander, k.expansions(at), k.expansions_planted(), alloc)
                        .unwrap_or_else(|e| panic!("the kept expansions of `{}` did not open: {e}", input.name)),
                    None => crate::expansions::KeptExpansions::none(alloc),
                };
                match resolve_crate_decoded(&local_input, &deps, local_sysroot, sources, at, par, leak_private, prepares, &kept_expansions, bump) {
                    Ok(done) => {
                        if let Some((k, _)) = keeper {
                            par.stage(crate::task::Stage::ExpansionsRecord);
                            let set = kept_expansions.record(alloc).unwrap_or_else(|e| panic!("the kept expansions of `{}` did not write: {e}", input.name));
                            k.expansions_made(at, &set);
                        }
                        break Ok(done);
                    }
                    Err(CrateScopeError::Need(keys)) if tries <= n => {
                        locked(&queue).needs += 1;
                        let mut unknown = false;
                        for k in keys.iter() {
                            match by_key.get(k) {
                                Some(&g) => {
                                    place(g, &mut local);
                                }
                                None => unknown = true,
                            }
                        }
                        if unknown {
                            break Err(CrateScopeError::Need(keys));
                        }
                    }
                    Err(e) => break Err(e),
                }
            }
            .map(|done| {
                // Its read list: each crate whose exports record it was handed (all written),
                // and each question it asked of them.
                if let Some(k) = kept {
                    let mut handed = AVec::with_capacity_in(local.len(), alloc);
                    {
                        let q = locked(&queue);
                        handed.extend(local.iter().filter_map(|&g| q.digests[g].map(|d| (crates[g].key, d))));
                    }
                    par.stage(crate::task::Stage::ReadList);
                    let read = crate::asked::encode(input.key, &handed, &done.asked, alloc);
                    k.made(at, &done.record, &done.inner, &read);
                    if k.keeps_names() {
                        par.stage(crate::task::Stage::NamesRecord);
                        let names = crate::names_record::encode(done.names, alloc).unwrap_or_else(|e| panic!("the names record of `{}` did not write: {e:?}", input.name));
                        k.names(at, &names);
                    }
                    if k.keeps_items() {
                        par.stage(crate::task::Stage::ItemsRecord);
                        let items = crate::items_record::encode(done.names, alloc).unwrap_or_else(|e| panic!("the items record of `{}` did not write: {e:?}", input.name));
                        k.items(at, &items);
                    }
                }
                Solved::Solved(done)
            }) };
            // The crate's exports record, checked once here (rule 74), outside the lock, and
            // handed to every crate that reads it in this call as it is: never once per reader.
            let result = result.and_then(|done| {
                let own: &[u8] = match &done {
                    Solved::Solved(scope) => &scope.record,
                    Solved::Found { record, .. } => record,
                };
                let digest = ContentId::of(EXPORTS_DOMAIN, own);
                par.stage(crate::task::Stage::Decode);
                ready.hooks.decoding(at);
                let bytes = crate::resolve::aligned(own, bump);
                let record = hand(form, bytes, bump).map_err(|error| CrateScopeError::OwnRecord { krate: input.name, error })?;
                ready.hooks.decoded(at, bytes.len());
                Ok((done, record, digest))
            });
            {
                let mut q = locked(&queue);
                q.in_flight -= 1;
                match result {
                    Ok((done, record, digest)) => {
                        q.records[at] = Some(record);
                        q.digests[at] = Some(digest);
                        q.out[at] = Some(done);
                        q.left -= 1;
                        for &d in dependents[at].iter() {
                            q.blocked[d] -= 1;
                            if q.blocked[d] == 0 {
                                q.ready.push(d);
                            }
                        }
                    }
                    Err(e) => Queue::fail(&mut q, pos[at], at, e),
                }
                woken.notify_all();
            }
            par.stage(crate::task::Stage::Done);
            ready.hooks.ended(at, w);
        }
    };
    let mut ran = AVec::new_in(alloc);
    par.map(ready.workers.max(1), worker, &mut ran);
    let q = queue.into_inner().unwrap_or_else(|poisoned| panic!("the names ready queue's lock was poisoned by a panic while held: {poisoned}"));
    if let Some((_, at, e)) = q.failed {
        return Err((at, e));
    }
    let mut all = AVec::with_capacity_in(n, alloc);
    for (at, c) in q.out.into_iter().enumerate() {
        all.push(c.ok_or((at, CrateScopeError::Names(NamesError::NotResolved { krate: crates[at].name })))?);
    }
    Ok((all, q.needs))
}

#[allow(clippy::too_many_arguments, reason = "the entry's arguments and the planted bug's switch")]
pub(crate) fn resolve_crate_with<'a, S: SourceTexts<'a>, P: Par>(
    input: &CrateInput<'a>,
    deps: &[&'a [u8]],
    sysroot: Sysroot,
    sources: &S,
    source: usize,
    par: &P,
    leak_private: bool,
    prepares: &'a Prepares<'a>,
    alloc: Bump<'a>,
) -> Result<CrateScope<'a>, CrateScopeError<'a>> {
    let read = handed(Form::InPlace, deps, alloc)?;
    resolve_crate_decoded(input, &read, sysroot, sources, source, par, leak_private, prepares, &crate::expansions::KeptExpansions::none(alloc.alloc()), alloc)
}

/// [`resolve_crate`] given its dependencies' exports records already checked, each proof made
/// once where its bytes entered the process (a wave's mapped record: blaze-cli engine_names)
/// and handed to every dependent of the request: opening one is O(1) and allocates one small
/// box, nothing per table, module or name.
#[allow(clippy::too_many_arguments, reason = "the entry's arguments, each named")]
pub fn resolve_crate_verified<'a, S: SourceTexts<'a>, P: Par>(
    input: &CrateInput<'a>,
    deps: &[Verified<'a>],
    sysroot: Sysroot,
    sources: &S,
    source: usize,
    par: &P,
    prepares: &'a Prepares<'a>,
    alloc: Bump<'a>,
) -> Result<CrateScope<'a>, CrateScopeError<'a>> {
    let a = alloc.alloc();
    let mut handed = AVec::with_capacity_in(deps.len(), a);
    for proof in deps {
        handed.push(Dep::InPlace(alloc.alloc_val(DepRecord::open(*proof))));
    }
    resolve_crate_decoded(input, &handed, sysroot, sources, source, par, false, prepares, &crate::expansions::KeptExpansions::none(a), alloc)
}

/// [`resolve_crate_with`] given its dependencies' records already handed: the ready queue
/// checks each record once for the whole call and gives every reader the same one.
#[allow(clippy::too_many_arguments, reason = "the entry's arguments and the planted bug's switch")]
fn resolve_crate_decoded<'a, S: SourceTexts<'a>, P: Par>(
    input: &CrateInput<'a>,
    deps: &[Dep<'a>],
    sysroot: Sysroot,
    sources: &S,
    source: usize,
    par: &P,
    leak_private: bool,
    prepares: &'a Prepares<'a>,
    kept: &crate::expansions::KeptExpansions<'_, 'a>,
    bump: Bump<'a>,
) -> Result<CrateScope<'a>, CrateScopeError<'a>> {
    let alloc = bump.alloc();
    let write = |names: CrateNames<'a>, built: &Built<'_, 'a>| -> Result<_, CrateScopeError<'a>> {
        par.stage(crate::task::Stage::Write);
        let Records { exports, inner } = write_records(&names, built, leak_private, alloc).map_err(|error| CrateScopeError::Write { krate: input.name, error })?;
        Ok((names, exports, inner))
    };
    let ((names, record, inner), asked) = with_names(input, deps, sysroot, sources, source, par, prepares, kept, bump, write)?;
    let names: &'a CrateNames<'a> = bump.alloc_val(names);
    Ok(CrateScope { names, record, inner, asked })
}

/// E4's one-group solve of one crate (cells/edit.rs `OneGroup`): the modules `members` solved
/// from their text against the dependencies' records `deps` (as [`resolve_crate_with`] takes
/// them), every other module of the crate answered from its group's record; the answer is the
/// members' groups alone, with the module tree record the solve ended with (the one handed in,
/// or the one it made walking again: the caller hands that one to its next solve). No crate
/// record is written or decoded: the groups' records are the answer (the crate's exports are
/// its groups' business, read by cell).
#[allow(clippy::too_many_arguments, reason = "the entry's arguments, each named")]
pub fn resolve_members<'a, S: SourceTexts<'a>, P: Par>(
    input: &CrateInput<'a>,
    deps: &[&'a [u8]],
    sysroot: Sysroot,
    sources: &S,
    source: usize,
    par: &P,
    members: crate::cells::Members<'_>,
    bump: Bump<'a>,
) -> Result<crate::cells::Solved<'a>, CrateScopeError<'a>> {
    let alloc = bump.alloc();
    let decoded = handed(Form::InPlace, deps, bump)?;
    let prepares = Prepares::new_in(bump);
    let par = OfMembers { par, members };
    let kept = crate::expansions::KeptExpansions::none(alloc);
    let members_of = |mut names: CrateNames<'a>, _: &Built<'_, 'a>| {
        let groups = core::mem::replace(&mut names.groups, AVec::new_in(alloc));
        // In `alloc`: it outlives `names`.
        let tree: &'a [u8] = names.tree_record;
        // The rest lives in `alloc`, which frees it whole (as a crate solve's names are leaked
        // there): walking every module's tables to drop them is the cost of the crate, not of
        // the members (2.7 ms of a 2,000-module crate's one-group solve, 2026-11-17).
        core::mem::forget(names);
        Ok(crate::cells::Solved { groups, tree })
    };
    let (solved, _) = with_names(input, &decoded, sysroot, sources, source, &par, prepares, &kept, bump, members_of)?;
    Ok(solved)
}

/// `par` asked to solve `members` alone, their groups written ([`Par::names_members`]).
struct OfMembers<'p, P> {
    par: &'p P,
    members: crate::cells::Members<'p>,
}

impl<P: Par> Par for OfMembers<'_, P> {
    fn map<T, F: Fn(usize) -> T>(&self, n: usize, f: F, out: &mut AVec<'_, T>) {
        self.par.map(n, f, out);
    }

    fn outline_files<'a>(&self, krate: &'a str, files: &[(&'a str, &'a str)], edition: blaze_surface::Edition, alloc: Alloc<'a>, out: &mut AVec<'a, blaze_file_items::Outline<'a>>) -> Result<(), NamesError<'a>> {
        self.par.outline_files(krate, files, edition, alloc, out)
    }

    fn stage(&self, stage: crate::task::Stage) {
        self.par.stage(stage);
    }

    fn call_scratch(&self, work: &mut dyn FnMut(blaze_own::Alloc<'_>)) -> crate::task::Scratch {
        self.par.call_scratch(work)
    }

    fn expand_part(&self, part: crate::task::ExpandPart, macro_name: &str, size: usize) {
        self.par.expand_part(part, macro_name, size);
    }

    fn expand_call(&self, def: ItemKey, input: &str) {
        self.par.expand_call(def, input);
    }

    fn names_groups(&self) -> bool {
        true
    }

    fn names_members(&self) -> Option<crate::cells::Members<'_>> {
        Some(self.members)
    }

    fn names_part(&self, part: crate::task::NamesPart) {
        self.par.names_part(part);
    }

    fn block_scopes(&self) -> crate::surface::BlockScopes {
        self.par.block_scopes()
    }
}

/// `par` with D189's seam set to `scopes` ([`Par::block_scopes`]), for a whole-crate solve:
/// no groups, no D116 check (the differential's two solves are the crate's own, once each).
struct Strategy<'p, P> {
    par: &'p P,
    scopes: crate::surface::BlockScopes,
}

impl<P: Par> Par for Strategy<'_, P> {
    fn map<T, F: Fn(usize) -> T>(&self, n: usize, f: F, out: &mut AVec<'_, T>) {
        self.par.map(n, f, out);
    }

    fn outline_files<'a>(&self, krate: &'a str, files: &[(&'a str, &'a str)], edition: blaze_surface::Edition, alloc: Alloc<'a>, out: &mut AVec<'a, blaze_file_items::Outline<'a>>) -> Result<(), NamesError<'a>> {
        self.par.outline_files(krate, files, edition, alloc, out)
    }

    fn stage(&self, stage: crate::task::Stage) {
        self.par.stage(stage);
    }

    fn call_scratch(&self, work: &mut dyn FnMut(blaze_own::Alloc<'_>)) -> crate::task::Scratch {
        self.par.call_scratch(work)
    }

    fn expand_part(&self, part: crate::task::ExpandPart, macro_name: &str, size: usize) {
        self.par.expand_part(part, macro_name, size);
    }

    fn expand_call(&self, def: ItemKey, input: &str) {
        self.par.expand_call(def, input);
    }

    fn names_part(&self, part: crate::task::NamesPart) {
        self.par.names_part(part);
    }

    fn tree_record_read(&self, bytes: usize) {
        self.par.tree_record_read(bytes);
    }

    fn block_scopes(&self) -> crate::surface::BlockScopes {
        self.scopes
    }
}

/// D189 step 4's whole-crate differential (proof 3) on one crate as the command resolves it,
/// against its dependencies' records `deps` (the arguments are [`resolve_crate`]'s): the crate
/// solved twice, under `BlockScopes::Modules` and under `Bodies`, every body the `Bodies` solve
/// held then solved by [`crate::body_scope::body_scope`], and the two impl tables compared row by
/// row ([`crate::body_scope::impl_diff`]). A body `body_scope` refuses is a verdict row, never
/// skipped silently. The crate's record is not written; nothing is kept.
///
/// Both halves in the one `alloc`: [`block_scopes_modules_side`] then
/// [`block_scopes_bodies_side`]. The command runs the halves each in a frame of its own, in
/// sequence (blaze-cli names/scope.rs), so its peak is one solve, never two.
#[allow(clippy::too_many_arguments, reason = "resolve_crate's arguments, each named")]
pub fn block_scopes_differential<'a, S: SourceTexts<'a>, P: Par>(
    input: &CrateInput<'a>,
    deps: &[&'a [u8]],
    sysroot: Sysroot,
    sources: &S,
    source: usize,
    par: &P,
    prepares: &'a Prepares<'a>,
    alloc: Bump<'a>,
) -> Result<crate::body_scope::BlockScopesVerdict<'a>, crate::body_scope::DifferentialError<'a>> {
    let modules = block_scopes_modules_side(input, deps, sysroot, sources, source, par, prepares, alloc, alloc)?;
    block_scopes_bodies_side(&modules, input, deps, sysroot, sources, source, par, prepares, alloc)
}

/// The `Modules` half of [`block_scopes_differential`]: the crate solved under
/// `BlockScopes::Modules` in `alloc`, and what the comparison reads of it (its impl rows, their
/// modules' chains, its headers) copied into `out` ([`crate::body_scope::ModulesSide`]). The
/// caller may then drop `alloc`'s frame: nothing of the result is in it. An error is in `alloc`.
#[allow(clippy::too_many_arguments, reason = "resolve_crate's arguments, each named")]
pub fn block_scopes_modules_side<'a, 'o, S: SourceTexts<'a>, P: Par>(
    input: &CrateInput<'a>,
    deps: &[&'a [u8]],
    sysroot: Sysroot,
    sources: &S,
    source: usize,
    par: &P,
    prepares: &'a Prepares<'a>,
    bump: Bump<'a>,
    out: Bump<'o>,
) -> Result<crate::body_scope::ModulesSide<'o>, crate::body_scope::DifferentialError<'a>> {
    use crate::body_scope::{DifferentialError, ModulesSide};
    use crate::surface::BlockScopes;

    let alloc = bump.alloc();
    let decoded = handed(Form::InPlace, deps, bump).map_err(DifferentialError::Names)?;
    let kept = crate::expansions::KeptExpansions::none(alloc);
    let modules_par = Strategy { par, scopes: BlockScopes::Modules };
    let (side, _) = with_names(input, &decoded, sysroot, sources, source, &modules_par, prepares, &kept, bump, |names, _| Ok(ModulesSide::of(&names, alloc, out))).map_err(DifferentialError::Names)?;
    side.map_err(|error| DifferentialError::Rows { krate: input.name, error })
}

/// The `Bodies` half of [`block_scopes_differential`]: the crate solved under
/// `BlockScopes::Bodies` in `alloc`, every held body solved as its own, and the impl table
/// compared with the `Modules` side `modules` ([`block_scopes_modules_side`], in a frame that
/// outlives this one).
#[allow(clippy::too_many_arguments, reason = "resolve_crate's arguments, each named")]
pub fn block_scopes_bodies_side<'a, 'o: 'a, S: SourceTexts<'a>, P: Par>(
    modules: &crate::body_scope::ModulesSide<'o>,
    input: &CrateInput<'a>,
    deps: &[&'a [u8]],
    sysroot: Sysroot,
    sources: &S,
    source: usize,
    par: &P,
    prepares: &'a Prepares<'a>,
    bump: Bump<'a>,
) -> Result<crate::body_scope::BlockScopesVerdict<'a>, crate::body_scope::DifferentialError<'a>> {
    use crate::body_scope::{BodyScope, BodyScopeRefusal, DifferentialError};
    use crate::surface::BlockScopes;

    let alloc = bump.alloc();
    let decoded = handed(Form::InPlace, deps, bump).map_err(DifferentialError::Names)?;
    let kept = crate::expansions::KeptExpansions::none(alloc);
    let leak = |names: CrateNames<'a>| -> &'a CrateNames<'a> { bump.alloc_val(names) };

    let bodies_par = Strategy { par, scopes: BlockScopes::Bodies };
    let solve = |names: CrateNames<'a>, built: &Built<'_, 'a>| -> Result<_, CrateScopeError<'a>> {
        let c = leak(names);
        let body_deps = crate::resolve::BodyDeps::of_built(built, alloc);
        let mut scopes: AVec<'a, BodyScope<'a>> = AVec::with_capacity_in(c.bodies.len(), alloc);
        let mut refused: AVec<'a, BodyScopeRefusal<'a>> = AVec::new_in(alloc);
        // The block modules (as `Modules` keys them) of each refused body: its impls are
        // missing from the `Bodies` table because of the refusal (the verdict says how many).
        let mut refused_blocks: AVec<'a, ItemKey> = AVec::new_in(alloc);
        // The crate's tree record read once for every held body (rule 74, D189's floor).
        let tree = crate::resolve::ReadTree::of(c, bump).map_err(|error| CrateScopeError::Names(NamesError::TreeRecord { krate: input.name, error }))?;
        // Each body's work (its cells over its chain and the modules it reads, cells/mod.rs
        // `load_body`; the read list's scope bytes) in the driver's scratch, reset after it: tempo's `core` outgrew its 4 GiB
        // frame with every body's work kept (21 Nov 05:33). No macro call is open here (the
        // crate is solved), and a body's own calls run in this frame (resolve.rs
        // `solve_held_body_scratch`), so the scratch is never asked twice at once.
        let hook = |work: &mut dyn FnMut(Alloc<'_>)| par.call_scratch(work);
        for held in c.bodies.iter() {
            match crate::body_scope::body_scope_scratch(&tree, &body_deps, input, source, *held, sources, prepares, &hook, bump) {
                Ok(scope) => scopes.push(scope),
                Err(refusal) => {
                    let nested = blaze_file_items::nested::scan(held.src.text, alloc);
                    refused_blocks.extend(crate::blocks::block_modules(c.key, held.src.key, &nested, alloc).iter().flatten().copied());
                    refused.push(refusal);
                }
            }
        }
        refused_blocks.sort_unstable();
        refused_blocks.dedup();
        Ok((c, scopes, refused, refused_blocks))
    };
    let ((bodies, scopes, refused, refused_blocks), _) = with_names(input, &decoded, sysroot, sources, source, &bodies_par, prepares, &kept, bump, solve).map_err(DifferentialError::Names)?;

    let bodies_rows = crate::body_scope::bodies_impl_table(bodies, &scopes, alloc).map_err(|error| DifferentialError::Rows { krate: input.name, error })?;
    let diff = crate::body_scope::impl_diff(&modules.rows, &bodies_rows, alloc);
    let under_refused = |module: ItemKey| {
        refused_blocks.binary_search(&module).is_ok() || modules.chain(module).is_some_and(|chain| chain.iter().any(|m| refused_blocks.binary_search(m).is_ok()))
    };
    let only_modules_refused = diff.only_modules.iter().filter(|r| under_refused(r.module)).count();
    let mut headers = crate::body_scope::ImplHeaders::of(bodies, &scopes, alloc);
    headers.add_side(modules);
    Ok(crate::body_scope::BlockScopesVerdict {
        krate: input.name,
        held: bodies.bodies.len(),
        solved: scopes.len(),
        modules_made: scopes.iter().map(|s| s.solved.made_outside).sum(),
        refused,
        modules_rows: modules.rows.len(),
        bodies_rows: bodies_rows.len(),
        diff,
        only_modules_refused,
        headers,
    })
}

/// One crate's names (through [`resolve_one`]) against its dependencies' records, handed to
/// `then` with the dependencies as the solve read them; the answer is `then`'s, and every
/// question the solve asked of the dependencies.
#[allow(clippy::too_many_arguments, reason = "the entry's arguments, each named")]
fn with_names<'a, S: SourceTexts<'a>, P: Par, R>(
    input: &CrateInput<'a>,
    deps: &[Dep<'a>],
    sysroot: Sysroot,
    sources: &S,
    source: usize,
    par: &P,
    prepares: &'a Prepares<'a>,
    kept: &crate::expansions::KeptExpansions<'_, 'a>,
    bump: Bump<'a>,
    then: impl FnOnce(CrateNames<'a>, &Built<'_, 'a>) -> Result<R, CrateScopeError<'a>>,
) -> Result<(R, AVec<'a, Question<'a>>), CrateScopeError<'a>> {
    let alloc = bump.alloc();
    let mut slots: AVec<'a, Option<Dep<'a>>> = AVec::with_capacity_in(deps.len() + 1, alloc);
    let mut by_prefix = HashMap::new_in(alloc);
    let mut graph_crates = AVec::with_capacity_in(deps.len() + 1, alloc);
    for (at, &d) in deps.iter().enumerate() {
        by_prefix.insert(d.key().prefix(), at);
        slots.push(Some(d));
        graph_crates.push(GraphCrate {
            name: d.name(),
            edition: d.edition(),
            implicit: input.implicit,
            features: AVec::new_in(alloc),
            cfg: AVec::new_in(alloc),
            deps: AVec::new_in(alloc),
        });
    }
    let mut need = AVec::new_in(alloc);
    for d in slots.iter().flatten() {
        need.extend(d.reaches().iter().filter(|k| !by_prefix.contains_key(&k.prefix())));
    }
    if !need.is_empty() {
        need.sort_unstable();
        need.dedup();
        return Err(CrateScopeError::Need(need));
    }
    let mut own_deps = AVec::with_capacity_in(input.deps.len(), alloc);
    for &(name, index) in input.deps {
        let krate = u32::try_from(index)
            .ok()
            .filter(|_| index < deps.len())
            .ok_or(CrateScopeError::NoSuchDep { krate: input.name, name, index })?;
        own_deps.push(GraphDep { name, krate: CrateId(krate) });
    }
    graph_crates.push(GraphCrate {
        name: input.name,
        edition: input.edition,
        implicit: input.implicit,
        features: AVec::from_slice_in(input.features, alloc),
        cfg: AVec::from_slice_in(input.cfg, alloc),
        deps: own_deps,
    });
    slots.push(None);
    let graph = CrateGraph::new_in(graph_crates, sysroot, alloc).map_err(|e| CrateScopeError::Names(NamesError::Graph(e)))?;
    let id = CrateId(u32::try_from(deps.len()).map_err(|_| CrateScopeError::NoSuchDep { krate: input.name, name: input.name, index: deps.len() })?);
    let asked = Asked::new_in(bump);
    let built = Built { crates: &slots, by_prefix: &by_prefix, asked: Some(&asked) };
    // Each handed crate's facts decide what it reaches (`Need`) and the record's reach list.
    for d in slots.iter().flatten() {
        built.ask(|| Question::krate(*d, alloc));
    }
    let names = resolve_one::<S, P>(&graph, input, id, source, &built, sources, par, First::Prefetch, prepares, kept, bump).map_err(CrateScopeError::Names)?;
    let answer = then(names, &built)?;
    Ok((answer, asked.take()))
}
