//! The per-crate entry of name resolution: one crate resolved against its dependencies'
//! `CrateScope` records, and its own record written (bottom.md section 6, "Per-crate entry").
//!
//! A dependent reads of a dependency exactly what [`crate::resolve`]'s `Built` serves: its key,
//! root module, edition and proc-macro key; each module's finished scope and chain; each enum's
//! variants; each macro's source. The record holds those and nothing else (no files, no read
//! lists, no notes), plus `reaches`: every other crate whose items its scopes name, near or far,
//! so a dependent asks for all it may walk into in one `Need`.
//!
//! The record's bytes (all integers little-endian, every table sorted by key):
//! `BZCS`, version u32, crate key, root key, edition u8, has-proc u8, two pad bytes, proc key
//! (zeros without), name (u32 length, bytes); reaches (u32 count, keys); modules (u32 count;
//! each: key, scope offset u32 and length u32 into the blob, chain u32 count (`u32::MAX`: none),
//! chain keys); enums (count; each: key, count, each variant: key, record u8, name); macros
//! (count; each: key, module key, export u8, local-inner u8, body tag u8, name, then the body's
//! fields); pad to 16; the blob: each module's blaze-surface `ModuleScope` record, each padded
//! to 16. The tables are parsed once per read into hash maps (cost: one pass over the record's
//! bytes); the module scopes are read in place by `ScopeView` and turned into their working
//! form there, borrowing every name from the record.

use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::Duration;

use blaze_format::cursor::{self, Reader};
use blaze_format::FormatError;
use blaze_hash::HashMap;
use blaze_id::{ContentId, CrateItemKey, ItemKey};
use blaze_own::{AVec, Alloc, Box};
use blaze_surface::{BuildError, CallShape, Edition, GlobEdge, Namespace, ProcKind, RecordBytes, ScopeBuilder, ScopeView, Unresolved, Visibility};

use crate::asked::{Asked, Question, ReadListError, Stale};
use crate::graph::{CrateGraph, CrateId, GraphCrate, GraphDep, Sysroot};
use crate::prepares::Prepares;
use crate::resolve::{Built, CrateInput, CrateNames, NamesError, SourceTexts, resolve_one};
use crate::task::{First, MacroBody, MacroSrc, Par, Variant};

const MAGIC: [u8; 4] = *b"BZCS";
const VERSION: u32 = 1;
const ALIGN: usize = 16;
const NO_CHAIN: u32 = u32::MAX;

/// A dependency as the resolution reads it: finished in this process, or read from its record.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Dep<'a> {
    Whole(&'a CrateNames<'a>),
    Record(&'a DepScope<'a>),
}

impl<'a> Dep<'a> {
    pub(crate) fn key(self) -> CrateItemKey {
        match self {
            Dep::Whole(c) => c.key,
            Dep::Record(d) => d.key,
        }
    }

    pub(crate) fn root(self) -> ItemKey {
        match self {
            Dep::Whole(c) => c.root,
            Dep::Record(d) => d.root,
        }
    }

    pub(crate) fn name(self) -> &'a str {
        match self {
            Dep::Whole(c) => c.name,
            Dep::Record(d) => d.name,
        }
    }

    pub(crate) fn edition(self) -> Edition {
        match self {
            Dep::Whole(c) => c.edition,
            Dep::Record(d) => d.edition,
        }
    }

    pub(crate) fn proc_key(self) -> Option<CrateItemKey> {
        match self {
            Dep::Whole(c) => c.proc_key,
            Dep::Record(d) => d.proc_key,
        }
    }

    pub(crate) fn scope(self, module: ItemKey) -> Option<&'a ScopeBuilder<'a, &'a str>> {
        match self {
            Dep::Whole(c) => c.done.get(&module).map(|r| &r.scope),
            Dep::Record(d) => d.scopes.get(&module),
        }
    }

    pub(crate) fn chain(self, module: ItemKey) -> Option<&'a [ItemKey]> {
        match self {
            Dep::Whole(c) => c.chains.get(&module).map(|c| c.as_slice()),
            Dep::Record(d) => d.chains.get(&module).map(|c| c.as_slice()),
        }
    }

    pub(crate) fn enum_of(self, key: ItemKey) -> Option<&'a [Variant<&'a str>]> {
        match self {
            Dep::Whole(c) => c.tables.enums.get(&key).map(|v| v.as_slice()),
            Dep::Record(d) => d.enums.get(&key).map(|v| v.as_slice()),
        }
    }

    pub(crate) fn macro_of(self, key: ItemKey) -> Option<MacroSrc<'a>> {
        match self {
            Dep::Whole(c) => c.tables.macros.get(&key).copied(),
            Dep::Record(d) => d.macros.get(&key).copied(),
        }
    }

    pub(crate) fn reaches(self) -> &'a [CrateItemKey] {
        match self {
            Dep::Whole(_) => &[],
            Dep::Record(d) => &d.reaches,
        }
    }
}

/// A `CrateScope` record's working form, its names borrowed from the record's bytes.
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
}

/// Why a `CrateScope` record was not read.
#[derive(Debug)]
pub enum RecordRead {
    /// Its bytes do not start on a 16-byte boundary: map or copy them to one.
    Unaligned,
    /// Not `BZCS` and this version.
    NotThisRecord,
    /// A field ran past the end or was not UTF-8.
    Short(cursor::Error),
    /// A byte that is no edition, call shape, proc-macro kind or body tag.
    BadByte { at: usize, what: &'static str },
    /// A module's `ModuleScope` record.
    Scope { module: ItemKey, error: FormatError },
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

/// Reads a `CrateScope` record (its bytes 16-aligned and living as long as `alloc`'s values).
pub fn read_record<'a>(bytes: &'a [u8], alloc: Alloc<'a>) -> Result<DepScope<'a>, RecordRead> {
    if bytes.as_ptr() as usize % ALIGN != 0 {
        return Err(RecordRead::Unaligned);
    }
    let mut r = Reader::starting_at(bytes, 0).map_err(RecordRead::Short)?;
    let short = RecordRead::Short;
    let key = |r: &mut Reader<'a>| r.array::<32>().map(|k| ItemKey(ContentId(*k))).map_err(short);
    if *r.array::<4>().map_err(short)? != MAGIC || r.u32().map_err(short)? != VERSION {
        return Err(RecordRead::NotThisRecord);
    }
    let krate = CrateItemKey(key(&mut r)?.0);
    let root = key(&mut r)?;
    let at = bytes.len() - r.take_rest().len();
    let mut r = Reader::starting_at(bytes, at).map_err(short)?;
    let edition = Edition::from_byte(r.u8().map_err(short)?).ok_or(RecordRead::BadByte { at, what: "edition" })?;
    let has_proc = r.u8().map_err(short)?;
    r.skip(2).map_err(short)?;
    let proc = CrateItemKey(key(&mut r)?.0);
    let name = r.str_u32().map_err(short)?;
    let count = r.u32().map_err(short)? as usize;
    let mut reaches = AVec::with_capacity_in(count.min(bytes.len() / 32), alloc);
    for _ in 0..count {
        reaches.push(CrateItemKey(key(&mut r)?.0));
    }
    let count = r.u32().map_err(short)? as usize;
    let mut places = AVec::with_capacity_in(count.min(bytes.len() / 32), alloc);
    let mut chains = HashMap::new_in(alloc);
    for _ in 0..count {
        let module = key(&mut r)?;
        let (offset, len) = (r.u32().map_err(short)? as usize, r.u32().map_err(short)? as usize);
        places.push((module, offset, len));
        let n = r.u32().map_err(short)?;
        if n != NO_CHAIN {
            let mut chain = AVec::with_capacity_in((n as usize).min(bytes.len() / 32), alloc);
            for _ in 0..n {
                chain.push(key(&mut r)?);
            }
            chains.insert(module, chain);
        }
    }
    let count = r.u32().map_err(short)? as usize;
    let mut enums = HashMap::new_in(alloc);
    for _ in 0..count {
        let owner = key(&mut r)?;
        let n = r.u32().map_err(short)? as usize;
        let mut variants = AVec::with_capacity_in(n.min(bytes.len() / 32), alloc);
        for _ in 0..n {
            let key = key(&mut r)?;
            let record = r.u8().map_err(short)? != 0;
            variants.push(Variant { name: r.str_u32().map_err(short)?, key, record });
        }
        enums.insert(owner, variants);
    }
    let count = r.u32().map_err(short)? as usize;
    let mut macros = HashMap::new_in(alloc);
    for _ in 0..count {
        let def = key(&mut r)?;
        let module = key(&mut r)?;
        let export = r.u8().map_err(short)? != 0;
        let local_inner = r.u8().map_err(short)? != 0;
        let at = bytes.len() - r.take_rest().len();
        let mut rest = Reader::starting_at(bytes, at).map_err(short)?;
        let tag = rest.u8().map_err(short)?;
        let name = rest.str_u32().map_err(short)?;
        let body = match tag {
            0 => MacroBody::Rules(rest.str_u32().map_err(short)?),
            1 => {
                let has = rest.u8().map_err(short)? != 0;
                let params = rest.str_u32().map_err(short)?;
                MacroBody::Macro2 { params: has.then_some(params), body: rest.str_u32().map_err(short)? }
            }
            2 => MacroBody::BuiltinFn(rest.str_u32().map_err(short)?),
            3 => MacroBody::BuiltinOther(CallShape::from_byte(rest.u8().map_err(short)?).ok_or(RecordRead::BadByte { at, what: "call shape" })?),
            4 => {
                let kind = ProcKind::from_byte(rest.u8().map_err(short)?).ok_or(RecordRead::BadByte { at, what: "proc-macro kind" })?;
                MacroBody::Proc { kind, helpers: rest.str_u32().map_err(short)? }
            }
            _ => return Err(RecordRead::BadByte { at, what: "macro body tag" }),
        };
        macros.insert(def, MacroSrc { name, body, module, export, local_inner });
        r = rest;
    }
    let at = bytes.len() - r.take_rest().len();
    let blob = at.next_multiple_of(ALIGN);
    let mut scopes = HashMap::new_in(alloc);
    for (module, offset, len) in places {
        let start = blob + offset;
        let Some(part) = bytes.get(start..start + len) else {
            return Err(RecordRead::Short(cursor::Error { at: start, wanted: cursor::Wanted::Bytes { len, left: bytes.len().saturating_sub(start) } }));
        };
        let view = ScopeView::read(part).map_err(|error| RecordRead::Scope { module, error })?;
        scopes.insert(module, ScopeBuilder::from(&view, alloc));
    }
    Ok(DepScope { name, key: krate, root, edition, proc_key: (has_proc != 0).then_some(proc), reaches, scopes, chains, enums, macros })
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
    let rest = read_record(crate::resolve::aligned(inner, alloc), alloc)?;
    let bound = rest.scopes.get(&module).is_some_and(|scope| scope.bindings.iter().any(|e| e.name == name));
    Ok(if bound { Unresolved::Private } else { why })
}

struct Out<'b> {
    bytes: RecordBytes<'b>,
}

impl Out<'_> {
    fn u8(&mut self, v: u8) {
        self.bytes.push(v);
    }
    fn u32(&mut self, v: usize, what: &'static str) -> Result<(), BuildError> {
        let v = u32::try_from(v).map_err(|_| BuildError::TooLarge { table: what })?;
        self.bytes.extend_from_slice(&v.to_le_bytes());
        Ok(())
    }
    fn key(&mut self, k: ContentId) {
        self.bytes.extend_from_slice(&k.0);
    }
    fn str(&mut self, s: &str) -> Result<(), BuildError> {
        self.u32(s.len(), "CrateScope string")?;
        self.bytes.extend_from_slice(s.as_bytes());
        Ok(())
    }
    fn pad(&mut self) {
        let to = self.bytes.len().next_multiple_of(ALIGN);
        self.bytes.resize(to, 0);
    }
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
        return !s.globs.is_empty() || s.get(ns, name).is_some();
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
    let mut add = |part: Part, key: ItemKey, reach: &mut [AVec<'b, CrateItemKey>; 2]| {
        if key.0.0[..8] == own {
            return;
        }
        if let Some(dep) = built.of(key) {
            reach[part as usize].push(dep.key());
            reach[part as usize].extend_from_slice(dep.reaches());
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
    let [mut exports_reach, mut inner_reach] = reach;
    let exports = write_part(names, &modules, &exports_scopes, &mut exports_reach, &|k| named.contains_key(k), alloc)?;
    let inner = write_part(names, &modules, &inner_scopes, &mut inner_reach, &|k| !named.contains_key(k), alloc)?;
    Ok(Records { exports, inner })
}

/// Which of the two records a name goes to.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Part {
    Exports = 0,
    Inner = 1,
}

/// One record: each of `modules` that has a scope's bytes in `scopes`, the crates it `reaches`,
/// and the enums and macros `keeps`.
fn write_part<'b>(
    names: &CrateNames<'_>,
    modules: &[ItemKey],
    scopes: &[Option<RecordBytes<'b>>],
    reaches: &mut AVec<'b, CrateItemKey>,
    keeps: &dyn Fn(&ItemKey) -> bool,
    alloc: Alloc<'b>,
) -> Result<RecordBytes<'b>, BuildError> {
    reaches.sort_unstable();
    reaches.dedup();

    let mut out = Out { bytes: RecordBytes::new_in(alloc) };
    out.bytes.extend_from_slice(&MAGIC);
    out.u32(VERSION as usize, "version")?;
    out.key(names.key.0);
    out.key(names.root.0);
    out.u8(names.edition.byte());
    out.u8(u8::from(names.proc_key.is_some()));
    out.u8(0);
    out.u8(0);
    out.key(names.proc_key.map_or(ContentId([0; 32]), |k| k.0));
    out.str(names.name)?;
    out.u32(reaches.len(), "CrateScope reaches")?;
    for k in reaches.iter() {
        out.key(k.0);
    }
    let mut blob = RecordBytes::new_in(alloc);
    out.u32(scopes.iter().flatten().count(), "CrateScope modules")?;
    for (m, bytes) in modules.iter().zip(scopes.iter()) {
        let Some(bytes) = bytes else { continue };
        out.key(m.0);
        out.u32(blob.len(), "CrateScope blob")?;
        out.u32(bytes.len(), "CrateScope blob")?;
        blob.extend_from_slice(&bytes);
        blob.resize(blob.len().next_multiple_of(ALIGN), 0);
        match names.chains.get(m) {
            Some(chain) => {
                out.u32(chain.len(), "CrateScope chain")?;
                for k in chain.iter() {
                    out.key(k.0);
                }
            }
            None => out.u32(NO_CHAIN as usize, "CrateScope chain")?,
        }
    }
    let mut enums = AVec::from_iter_in(names.tables.enums.keys().copied().filter(|k| keeps(k)), alloc);
    enums.sort_unstable();
    out.u32(enums.len(), "CrateScope enums")?;
    for owner in enums.iter() {
        let variants = &names.tables.enums[owner];
        out.key(owner.0);
        out.u32(variants.len(), "CrateScope variants")?;
        for v in variants.iter() {
            out.key(v.key.0);
            out.u8(u8::from(v.record));
            out.str(v.name)?;
        }
    }
    let mut macros = AVec::from_iter_in(names.tables.macros.keys().copied().filter(|k| keeps(k)), alloc);
    macros.sort_unstable();
    out.u32(macros.len(), "CrateScope macros")?;
    for def in macros.iter() {
        let m = names.tables.macros[def];
        out.key(def.0);
        out.key(m.module.0);
        out.u8(u8::from(m.export));
        out.u8(u8::from(m.local_inner));
        let tag = match m.body {
            MacroBody::Rules(_) => 0,
            MacroBody::Macro2 { .. } => 1,
            MacroBody::BuiltinFn(_) => 2,
            MacroBody::BuiltinOther(_) => 3,
            MacroBody::Proc { .. } => 4,
        };
        out.u8(tag);
        out.str(m.name)?;
        match m.body {
            MacroBody::Rules(text) | MacroBody::BuiltinFn(text) => out.str(text)?,
            MacroBody::Macro2 { params, body } => {
                out.u8(u8::from(params.is_some()));
                out.str(params.unwrap_or(""))?;
                out.str(body)?;
            }
            MacroBody::BuiltinOther(shape) => out.u8(shape.byte()),
            MacroBody::Proc { kind, helpers } => {
                out.u8(kind.byte());
                out.str(helpers)?;
            }
        }
    }
    out.pad();
    out.bytes.extend_from_slice(&blob);
    Ok(out.bytes)
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
    alloc: Alloc<'a>,
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

    /// `krate`'s own resolution is done and its exports record is about to be decoded: what is
    /// allocated from here to [`ReadyHooks::decoded`] is the decoded record.
    fn decoding(&self, krate: usize) {
        let _ = krate;
    }

    /// `krate`'s exports record (`bytes` long) was decoded into its working form: once a call.
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

    /// Whether each solved crate's expansions are kept ([`crate::expansions`]): when not, no
    /// call's key is made and nothing is collected.
    fn keeps_expansions(&self) -> bool {
        false
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
    alloc: Alloc<'a>,
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
    alloc: Alloc<'a>,
) -> Result<(AVec<'a, Solved<'a>>, usize), (usize, CrateScopeError<'a>)> {
    resolve_queue(crates, sysroot, sources, par, ready, false, Some(kept), alloc)
}

/// The ready queue's state, under one lock: held to take a crate, to read records, and to
/// publish one; never across a resolution.
struct Queue<'a> {
    /// By crate index: its exports record once written, decoded once for the whole call.
    records: AVec<'a, Option<&'a DepScope<'a>>>,
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
    alloc: Alloc<'a>,
) -> Result<(AVec<'a, CrateScope<'a>>, usize), (usize, CrateScopeError<'a>)> {
    let (solved, needs) = resolve_queue(crates, sysroot, sources, par, ready, leak_private, None, alloc)?;
    let mut all = AVec::with_capacity_in(solved.len(), alloc);
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
#[allow(clippy::too_many_arguments, reason = "the entry's arguments, the planted bug's switch and the kept solves")]
fn resolve_queue<'a, S: SourceTexts<'a>, P: Par>(
    crates: &[CrateInput<'a>],
    sysroot: Sysroot,
    sources: &S,
    par: &P,
    ready: &Ready<'_>,
    leak_private: bool,
    kept: Option<&dyn Kept>,
    alloc: Alloc<'a>,
) -> Result<(AVec<'a, Solved<'a>>, usize), (usize, CrateScopeError<'a>)> {
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
    let prepares = Prepares::new_in(alloc);
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
            let mut own_deps = AVec::with_capacity_in(input.deps.len(), alloc);
            for &(name, index) in input.deps {
                own_deps.push((name, place(index, &mut local)));
            }
            let own_deps: &'a [(&'a str, usize)] = AVec::leak(own_deps);
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
                    let read = crate::resolve::aligned(read, alloc);
                    match crate::asked::check(read, &now, alloc) {
                        Err(error) => Err(CrateScopeError::ReadList { krate: input.name, error }),
                        Ok(Some(why)) => {
                            k.stale(at, &why);
                            Ok(None)
                        }
                        Ok(None) => {
                            let (r, i) = (crate::resolve::aligned(record, alloc), crate::resolve::aligned(inner, alloc));
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
                let keeper = kept.filter(|k| k.keeps_expansions());
                let kept_expansions = match keeper {
                    Some(k) => crate::expansions::KeptExpansions::open_planted(k.expansions(at), k.expansions_planted(), alloc)
                        .unwrap_or_else(|e| panic!("the kept expansions of `{}` did not open: {e}", input.name)),
                    None => crate::expansions::KeptExpansions::none(alloc),
                };
                match resolve_crate_decoded(&local_input, &deps, local_sysroot, sources, at, par, leak_private, prepares, &kept_expansions, alloc) {
                    Ok(done) => {
                        if let Some(k) = keeper {
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
            // The crate's exports record, decoded once here, outside the lock, and shared by
            // every crate that reads it in this call: never once per reader.
            let result = result.and_then(|done| {
                let own: &[u8] = match &done {
                    Solved::Solved(scope) => &scope.record,
                    Solved::Found { record, .. } => record,
                };
                let digest = ContentId::of(EXPORTS_DOMAIN, own);
                par.stage(crate::task::Stage::Decode);
                ready.hooks.decoding(at);
                let bytes = crate::resolve::aligned(own, alloc);
                let decoded = read_record(bytes, alloc).map_err(|error| CrateScopeError::OwnRecord { krate: input.name, error })?;
                ready.hooks.decoded(at, bytes.len());
                let decoded: &'a DepScope<'a> = Box::leak(Box::new_in(decoded, alloc));
                Ok((done, decoded, digest))
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
    alloc: Alloc<'a>,
) -> Result<CrateScope<'a>, CrateScopeError<'a>> {
    let mut decoded = AVec::with_capacity_in(deps.len(), alloc);
    for (at, bytes) in deps.iter().enumerate() {
        decoded.push(&*Box::leak(Box::new_in(read_record(bytes, alloc).map_err(|error| CrateScopeError::Record { at, error })?, alloc)));
    }
    resolve_crate_decoded(input, &decoded, sysroot, sources, source, par, leak_private, prepares, &crate::expansions::KeptExpansions::none(alloc), alloc)
}

/// [`resolve_crate_with`] given its dependencies' records already decoded: the ready queue
/// decodes each record once for the whole call and gives every reader the same one.
#[allow(clippy::too_many_arguments, reason = "the entry's arguments and the planted bug's switch")]
fn resolve_crate_decoded<'a, S: SourceTexts<'a>, P: Par>(
    input: &CrateInput<'a>,
    deps: &[&'a DepScope<'a>],
    sysroot: Sysroot,
    sources: &S,
    source: usize,
    par: &P,
    leak_private: bool,
    prepares: &'a Prepares<'a>,
    kept: &crate::expansions::KeptExpansions<'_, 'a>,
    alloc: Alloc<'a>,
) -> Result<CrateScope<'a>, CrateScopeError<'a>> {
    let mut slots: AVec<'a, Option<Dep<'a>>> = AVec::with_capacity_in(deps.len() + 1, alloc);
    let mut by_prefix = HashMap::new_in(alloc);
    let mut graph_crates = AVec::with_capacity_in(deps.len() + 1, alloc);
    for (at, &d) in deps.iter().enumerate() {
        by_prefix.insert(d.key.prefix(), at);
        slots.push(Some(Dep::Record(d)));
        graph_crates.push(GraphCrate {
            name: d.name,
            edition: d.edition,
            implicit: input.implicit,
            features: AVec::new_in(alloc),
            cfg: AVec::new_in(alloc),
            deps: AVec::new_in(alloc),
        });
    }
    let mut need = AVec::new_in(alloc);
    for d in slots.iter().flatten() {
        need.extend(d.reaches().iter().filter(|k| !by_prefix.contains_key(&k.prefix())).copied());
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
    let asked = Asked::new_in(alloc);
    let built = Built { crates: &slots, by_prefix: &by_prefix, asked: Some(&asked) };
    // Each handed crate's facts decide what it reaches (`Need`) and the record's reach list.
    for d in slots.iter().flatten() {
        built.ask(|| Question::krate(*d, alloc));
    }
    let names = resolve_one::<S, P>(&graph, input, id, source, &built, sources, par, First::Prefetch, prepares, kept, alloc).map_err(CrateScopeError::Names)?;
    par.stage(crate::task::Stage::Write);
    let Records { exports, inner } = write_records(&names, &built, leak_private, alloc).map_err(|error| CrateScopeError::Write { krate: input.name, error })?;
    let names: &'a CrateNames<'a> = Box::leak(Box::new_in(names, alloc));
    Ok(CrateScope { names, record: exports, inner, asked: asked.take() })
}
