//! blaze-names-demand's [`Source`] over real texts.
//!
//! One home for every rule it needs: the module tree and the `mod x;` path rule are
//! blaze-scope's `walk`; a fragment's declarations (keys, cfg, attributes, use trees) are
//! blaze-scope's `lower`; a macro's definition is blaze-scope's `definition_of`; a call's summary
//! is blaze-expand's `macro_writes` read by [`crate::call_writes`]. So every key this source
//! hands out equals the one blaze-scope's whole-crate names give the same item, by construction.
//!
//! A file's fragment and an expansion's are turned into entries by ONE function,
//! [`CrateSource::convert`]. Everything it makes lives in the arena `'w` the caller hands in.
//!
//! Finding a call's macro (to summarise the call) is itself a name ask in the macro namespace.
//! It is asked of a NEW resolver (the open one refuses a nested ask: `Error::Busy`) over this
//! same source one layer deeper. While a module is being summarised its own outline is served
//! provisionally, each of its calls taken as writing nothing; once its summaries are in, each
//! callee found that way is asked again and must be the same ([`SourceError::CalleeMoved`]
//! otherwise). The cycle is bounded by [`crate::MOST_CALLEE_DEPTH`] layers
//! ([`SourceError::CalleeDepth`]); each layer's ask by the driver's step bound.

use core::cell::{Cell, RefCell};

use blaze_expand::{
    Call, CallSite, Callee, Callees, Ctx, ExpandCx, Expanded, HostAsk, HostError, HostWhy, IncludeAs, Included,
    MacroDefinition, Place, PlaceOf, ProcCall, Produced, TokenStream, expand, lex, macro_writes, write_tokens,
};
use blaze_file_items::Outline as FileOutline;
use blaze_id::{ContentId, CrateItemKey, ItemKey};
use blaze_names_demand::Resolver;
use blaze_names_demand::driver::{DriveError, Source, resolve};
use blaze_names_demand::types::{
    Anchor, Answer, Ask, CallRef, CrateRoot, Def, Entry, Extern, ModRef, Ns, Outline, Path, Spaces, Vis, Writes,
};
use blaze_own::{ABTreeMap, AString, AVec, Alloc, BTreeMap, BVec, Bump, Vec};
use blaze_scope::resolve::definition_of;
use blaze_scope::task::{
    AttrOrder, CallSrc, Cfg, Files, Fragment, LowerCx, Lowered, MacroSrc, ProcDecls, Target, Tree, Where, crate_tools,
    file_attrs, item_attrs, lower, root_key, walk,
};
use blaze_surface::{CallShape, DefKind, Edition, ExternTarget, Namespace, PathKind, Rename, UseForm, WrittenVis};
use blaze_unit_graph::{Implicit, Sysroot};

use crate::error::SourceError;
use crate::{MOST_CALLEE_DEPTH, Texts, call_writes};

/// Files one crate may read (rule 18).
pub const MOST_FILES: usize = 1 << 16;
/// Outlines, expansions and macro definitions one source may hold (rule 18).
pub const MOST_HELD: usize = 1 << 20;

/// One crate as the graph gives it.
#[derive(Clone, Copy, Debug)]
pub struct CrateFacts<'w> {
    pub name: &'w str,
    pub key: CrateItemKey,
    pub edition: Edition,
    pub implicit: Implicit,
    /// The root file, relative to the crate's directory (`lib.rs`, `src/lib.rs`).
    pub root: &'w str,
    /// Its dependencies by the name it uses, each an index into the crates given.
    pub deps: &'w [(&'w str, usize)],
    pub features: &'w [&'w str],
}

/// How each call's summary is made: [`Summaries::Read`] from its definition and input;
/// [`Summaries::AllAny`] is the control, every call `Writes::Any`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Summaries {
    Read,
    AllAny,
}

pub struct CrateSourceOpts<'w, T> {
    pub texts: &'w T,
    pub crates: &'w [CrateFacts<'w>],
    /// The toolchain crates, each [`blaze_unit_graph::ToolchainUnit`] an index into `crates`;
    /// an index past them is a crate this graph does not have.
    pub sysroot: Sysroot,
    pub summaries: Summaries,
    pub alloc: Bump<'w>,
}

/// What the source did: beside the machine's own `Counts`, these include every nested layer.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct SourceCounts {
    pub outlines: usize,
    pub expansions: usize,
    pub roots: usize,
    pub summaries: usize,
    pub callee_asks: usize,
}

struct File<'w> {
    /// Relative to the crate's directory, in the arena.
    path: &'w str,
    text: &'w str,
    outline: &'w FileOutline<'w>,
}

/// blaze-scope's [`Files`] over [`Texts`], each file read and outlined the first time the walk
/// asks for it. Paths are absolute under `/`, the crate's directory.
struct FileSet<'w, T> {
    texts: &'w T,
    krate: CrateItemKey,
    edition: Edition,
    alloc: Bump<'w>,
    files: RefCell<AVec<'w, File<'w>>>,
    full: Cell<bool>,
}

impl<'w, T: Texts> Files<'w> for FileSet<'w, T> {
    fn find(&self, path: &str) -> Option<u32> {
        let rel = path.strip_prefix('/')?;
        if let Some(at) = self.files.borrow().iter().position(|f| f.path == rel) {
            return Some(at as u32);
        }
        let texts: &'w T = self.texts;
        let text = texts.read(self.krate, rel)?;
        if self.files.borrow().len() >= MOST_FILES {
            self.full.set(true);
            return None;
        }
        let outline = self.alloc.alloc_val(blaze_file_items::outline(text, self.edition, self.alloc.alloc()));
        let path = self.alloc.alloc_str(rel);
        let mut files = self.files.borrow_mut();
        files.push(File { path, text, outline });
        Some((files.len() - 1) as u32)
    }

    fn text(&self, file: u32) -> &'w str {
        self.files.borrow()[file as usize].text
    }

    fn outline(&self, file: u32) -> &FileOutline<'_> {
        self.files.borrow()[file as usize].outline
    }
}

/// One crate's walk: its files, its module tree, its cfg and tools, its root's attributes.
struct CrateWalk<'w, T> {
    index: usize,
    files: FileSet<'w, T>,
    tree: Tree<'w, 'w>,
    cfg: Cfg<'w, 'w>,
    tools: &'w [&'w str],
    no_std: bool,
    no_core: bool,
}

/// One call as its fragment lowered it, and the macro its summary was made from.
#[derive(Clone, Copy)]
struct CallInfo<'w> {
    src: CallSrc<'w>,
    shape: CallShape,
    def: Option<&'w MacroDefinition<'w>>,
    input: &'w TokenStream<'w>,
}

/// The source. `'w` is the arena every outline, entry and definition lives in.
pub struct CrateSource<'w, T> {
    texts: &'w T,
    crates: &'w [CrateFacts<'w>],
    sysroot: Sysroot,
    summaries: Summaries,
    alloc: Bump<'w>,
    walks: AVec<'w, Option<&'w CrateWalk<'w, T>>>,
    outlines: ABTreeMap<'w, ModRef, &'w Outline<'w>>,
    /// Outlines being summarised, served provisionally to nested layers.
    building: AVec<'w, (ModRef, &'w Outline<'w>)>,
    expansions: ABTreeMap<'w, (ModRef, CallRef), &'w [Entry<'w>]>,
    roots: ABTreeMap<'w, CrateItemKey, &'w CrateRoot<'w>>,
    calls: ABTreeMap<'w, (ModRef, CallRef), CallInfo<'w>>,
    macros: ABTreeMap<'w, ItemKey, (usize, MacroSrc<'w>)>,
    defs: ABTreeMap<'w, ItemKey, &'w MacroDefinition<'w>>,
    counts: SourceCounts,
}

impl<'w, T: Texts> CrateSource<'w, T> {
    pub fn new(opts: CrateSourceOpts<'w, T>) -> CrateSource<'w, T> {
        let bump = opts.alloc;
        let a = bump.alloc();
        let mut walks = Vec::new_in(a);
        walks.extend(opts.crates.iter().map(|_| None));
        CrateSource {
            texts: opts.texts,
            crates: opts.crates,
            sysroot: opts.sysroot,
            summaries: opts.summaries,
            alloc: bump,
            walks,
            outlines: BTreeMap::new_in(a),
            building: Vec::new_in(a),
            expansions: BTreeMap::new_in(a),
            roots: BTreeMap::new_in(a),
            calls: BTreeMap::new_in(a),
            macros: BTreeMap::new_in(a),
            defs: BTreeMap::new_in(a),
            counts: SourceCounts::default(),
        }
    }

    pub fn counts(&self) -> SourceCounts {
        self.counts
    }

    /// The module a crate's root is.
    pub fn root_module(&self, index: usize) -> ModRef {
        let key = self.crates[index].key;
        ModRef { krate: key, key: root_key(key) }
    }

    fn index_of(&self, krate: CrateItemKey) -> Result<usize, SourceError<'w>> {
        self.crates.iter().position(|c| c.key == krate).ok_or(SourceError::NoCrate(krate))
    }

    fn walk_of(&mut self, index: usize) -> Result<&'w CrateWalk<'w, T>, SourceError<'w>> {
        if let Some(w) = self.walks[index] {
            return Ok(w);
        }
        let facts = self.crates[index];
        let bump = self.alloc;
        let a = bump.alloc();
        let files = FileSet { texts: self.texts, krate: facts.key, edition: facts.edition, alloc: bump, files: RefCell::new(Vec::new_in(a)), full: Cell::new(false) };
        let mut root_path = AString::new_in(a);
        root_path.push_str("/");
        root_path.push_str(facts.root);
        let root = files.find(root_path.as_str()).ok_or(SourceError::NoFile { krate: facts.key, path: facts.root })?;
        let root_dir = facts.root.rsplit_once('/').map_or("", |(dir, _)| dir);
        let mut dir = AString::new_in(a);
        dir.push_str("/");
        dir.push_str(root_dir);
        let cfg = Cfg::of(&Target::AARCH64_APPLE_DARWIN, facts.features, a);
        let tree = walk(&files, &cfg, facts.key, facts.name, root, dir.as_str(), facts.edition, bump);
        if files.full.get() {
            return Err(SourceError::Full { table: "files of a crate", most: MOST_FILES });
        }
        let attrs = file_attrs(&cfg, files.outline(root), files.text(root), a);
        let tools: &'w [&'w str] = bump.alloc_slice_copy(&crate_tools(&attrs, a));
        let (no_std, no_core) = (attrs.no_std, attrs.no_core);
        let w: &'w CrateWalk<'w, T> = bump.alloc_val(CrateWalk { index, files, tree, cfg, tools, no_std, no_core });
        self.walks[index] = Some(w);
        Ok(w)
    }

    fn outline_at(&mut self, module: ModRef, depth: usize) -> Result<&'w Outline<'w>, SourceError<'w>> {
        if let Some(&o) = self.outlines.get(&module) {
            return Ok(o);
        }
        if let Some(&(_, o)) = self.building.iter().find(|(m, _)| *m == module) {
            return Ok(o);
        }
        let index = self.index_of(module.krate)?;
        let w = self.walk_of(index)?;
        let &at = w.tree.by_key.get(&module.key).ok_or(SourceError::UnknownModule(module))?;
        let m = &w.tree.modules[at];
        let bump = self.alloc;
        let a = bump.alloc();
        let mut chain: BVec<'w, ItemKey> = Vec::new_in(bump);
        let mut up = m.parent;
        while let Some(p) = up {
            chain.push(w.tree.modules[p].key);
            up = w.tree.modules[p].parent;
        }
        chain.reverse();
        let file_outline = w.files.outline(m.file);
        let digest = match m.item {
            None => file_outline.surface(),
            Some(item) => file_outline.names_digest(item),
        };
        let text = w.files.text(m.file);
        let fragment = Fragment {
            module: module.key,
            owner: module.key,
            text,
            outline: file_outline,
            items: m.item,
            at: Where::File(m.file),
            depth: 0,
            made: false,
            members_of: None,
            written: text_path(w, m.file),
            kept: None,
            marks: None,
            tokens: None,
        };
        let entry = self.crates[index].implicit.entry && m.parent.is_none();
        let low = lower(lower_cx(w, &self.crates[index], entry), fragment, bump);
        let head = Head { chain: chain.into_bump_slice(), digest, no_implicit_prelude: m.no_implicit_prelude };
        let mut asked: AVec<'w, (CallRef, Ask<'w>, ItemKey)> = Vec::new_in(a);
        let entries = self.convert(module, index, &low, depth, Some(head), &mut asked)?;
        let outline = bump.alloc_val(Outline { module, digest, chain: head.chain, no_implicit_prelude: head.no_implicit_prelude, entries });
        self.hold(self.outlines.len())?;
        self.outlines.insert(module, outline);
        self.counts.outlines += 1;
        // The callees found while this module was provisional must still be found now.
        for &(call, ask, was) in asked.iter() {
            match self.ask_macro(ask, depth)? {
                Answer::Found { def: Def::Item(now), .. } if now == was => {}
                _ => return Err(SourceError::CalleeMoved { module, call }),
            }
        }
        Ok(outline)
    }

    fn expansion_at(&mut self, module: ModRef, call: CallRef, depth: usize) -> Result<&'w [Entry<'w>], SourceError<'w>> {
        if let Some(&e) = self.expansions.get(&(module, call)) {
            return Ok(e);
        }
        let info = *self.calls.get(&(module, call)).ok_or(SourceError::UnknownCall { module, call })?;
        if info.shape != CallShape::FnLike {
            return Err(SourceError::Shape { module, call });
        }
        let def = info.def.ok_or(SourceError::NoMacro { module, call })?;
        let index = self.index_of(module.krate)?;
        let facts = self.crates[index];
        let bump = self.alloc;
        let a = bump.alloc();
        let site = CallSite { edition: facts.edition, panic_edition: facts.edition, ctx: Ctx::root(facts.edition) };
        let made = expand(def, &Call::fn_like(info.input, site), &NoHost, a).map_err(|_| SourceError::Expand { module, call })?;
        let Expanded::Expansion(made) = made else { return Err(SourceError::Eager { module, call }) };
        if made.diagnostics.iter().any(|d| d.is_error()) {
            return Err(SourceError::Failed { module, call });
        }
        let mut rendered = AString::new_in(a);
        write_tokens(&made.tokens, &mut rendered);
        if rendered.as_str().contains("$crate") {
            return Err(SourceError::DollarCrate { module, call });
        }
        let text: &'w str = bump.alloc_str(rendered.as_str());
        let w = self.walk_of(index)?;
        let outline = bump.alloc_val(blaze_file_items::outline(text, facts.edition, a));
        let fragment = Fragment {
            module: module.key,
            owner: ItemKey(call.0),
            text,
            outline,
            items: None,
            at: Where::Expansion(info.src.loc),
            depth: info.src.depth + 1,
            made: false,
            members_of: None,
            written: info.src.written,
            kept: None,
            marks: None,
            tokens: None,
        };
        let low = lower(lower_cx(w, &facts, false), fragment, bump);
        if !low.mods_made.is_empty() {
            return Err(SourceError::MadeModule { module, call });
        }
        let mut asked = Vec::new_in(a);
        let entries = self.convert(module, index, &low, depth, None, &mut asked)?;
        self.hold(self.expansions.len())?;
        self.expansions.insert((module, call), entries);
        self.counts.expansions += 1;
        Ok(entries)
    }

    fn root_at(&mut self, krate: CrateItemKey) -> Result<&'w CrateRoot<'w>, SourceError<'w>> {
        if let Some(&r) = self.roots.get(&krate) {
            return Ok(r);
        }
        let index = self.index_of(krate)?;
        let w = self.walk_of(index)?;
        let facts = self.crates[index];
        let bump = self.alloc;
        let a = bump.alloc();
        let mut externs: BVec<'w, Extern<'w>> = Vec::new_in(bump);
        for &(name, dep) in facts.deps {
            let key = self.crates.get(dep).ok_or(SourceError::Internal("a dependency index past the crates given"))?.key;
            externs.push(Extern { name, krate: key, root: root_key(key) });
        }
        if !w.no_core {
            for (name, unit) in facts.implicit.externs(&self.sysroot) {
                let Some(dep) = self.crates.get(unit.0 as usize) else { continue };
                if (name == "std" && w.no_std) || externs.iter().any(|e| e.name == name) {
                    continue;
                }
                externs.push(Extern { name, krate: dep.key, root: root_key(dep.key) });
            }
        }
        let prelude = (!w.no_core).then(|| {
            let std = if w.no_std { "core" } else { "std" };
            Path { anchor: Anchor::Extern(std), segs: bump.alloc_slice_copy(&["prelude", facts.implicit.prelude.module()]) }
        });
        let mut export_modules: BVec<'w, ItemKey> = Vec::new_in(bump);
        for m in w.tree.modules.iter() {
            let o = w.files.outline(m.file);
            let text = w.files.text(m.file);
            let writes_exports = o.children(m.item).any(|(_, item)| {
                let attrs = item_attrs(&w.cfg, o, item, text, a);
                attrs.cfg_ok
                    && (matches!(item.detail, blaze_surface::ItemDetail::MacroCall { .. })
                        || matches!(item.detail, blaze_surface::ItemDetail::MacroRules { exported } if exported || attrs.macro_export)
                        || !attrs.derives.is_empty()
                        || !attrs.others.is_empty())
            });
            if writes_exports {
                export_modules.push(m.key);
            }
        }
        let mut h = blaze_ids::Hasher::new("blaze-names-source crate root");
        h.id(&krate.0);
        for e in externs.iter() {
            h.part(e.name.as_bytes()).id(&e.root.0);
        }
        if let Some(p) = prelude {
            for s in p.segs {
                h.part(s.as_bytes());
            }
        }
        for m in export_modules.iter() {
            h.id(&m.0);
        }
        let root = bump.alloc_val(CrateRoot {
            krate,
            digest: h.finish(),
            extern_prelude: externs.into_bump_slice(),
            prelude,
            export_modules: export_modules.into_bump_slice(),
        });
        self.roots.insert(krate, root);
        self.counts.roots += 1;
        Ok(root)
    }

    /// THE conversion: one lowered fragment (a module's own text, or a call's expansion) to its
    /// entries. With `head`, the fragment is a module's outline: while its calls are summarised
    /// it is served provisionally, its calls writing nothing, and each callee found through it is
    /// pushed to `asked` to be asked again.
    fn convert(
        &mut self,
        module: ModRef,
        index: usize,
        low: &Lowered<'w, 'w>,
        depth: usize,
        head: Option<Head<'w>>,
        asked: &mut AVec<'w, (CallRef, Ask<'w>, ItemKey)>,
    ) -> Result<&'w [Entry<'w>], SourceError<'w>> {
        let bump = self.alloc;
        let a = bump.alloc();
        let facts = self.crates[index];
        let parent = self.parent_of(index, module)?;
        let d = &low.decls;
        for &(key, src) in low.macros.iter() {
            self.hold(self.macros.len())?;
            self.macros.insert(key, (index, src));
        }
        let mut entries: BVec<'w, Entry<'w>> = Vec::new_in(bump);
        for item in d.items.iter() {
            let spaces = match item.ns {
                Namespace::Types => Spaces::TYPE,
                Namespace::Values => Spaces::VALUE,
                Namespace::Macros => Spaces::MACRO,
            };
            let def = match item.def.kind {
                DefKind::Module => Def::Module(ModRef { krate: module.krate, key: item.def.key }),
                _ => Def::Item(item.def.key),
            };
            let vis = vis_of(&item.vis, module, parent)?;
            entries.push(Entry::Item { name: item.name, spaces, vis, def, exported: false });
        }
        for m in d.mods.iter() {
            let vis = vis_of(&m.vis, module, parent)?;
            let def = Def::Module(ModRef { krate: module.krate, key: m.module });
            entries.push(Entry::Item { name: m.name, spaces: Spaces::TYPE, vis, def, exported: false });
        }
        for r in d.macro_rules.iter() {
            let vis = if r.export { Vis::Pub } else { Vis::In(module.key) };
            entries.push(Entry::Item { name: r.name, spaces: Spaces::MACRO, vis, def: Def::Item(r.def), exported: r.export });
        }
        for u in d.uses.iter() {
            let vis = vis_of(&u.vis, module, parent)?;
            let segs: &[&'w str] = &u.path.segments[..];
            match u.leaf {
                UseForm::Glob => {
                    let (anchor, rest) = anchor_of(u.path.kind, segs, true, facts.edition, module)?;
                    entries.push(Entry::Glob { path: Path { anchor, segs: bump.alloc_slice_copy(rest) }, vis });
                }
                UseForm::Named { rename, .. } => {
                    let Some((&target, before)) = segs.split_last() else { return Err(SourceError::UseRoot { module }) };
                    let name = match rename {
                        Rename::Same => target,
                        Rename::To(n) => n,
                        Rename::Underscore => continue,
                    };
                    let (anchor, rest) = match (u.path.kind, before.is_empty()) {
                        (PathKind::Abs, true) => return Err(SourceError::UseRoot { module }),
                        _ => anchor_of(u.path.kind, before, true, facts.edition, module)?,
                    };
                    entries.push(Entry::Use { name, path: Path { anchor, segs: bump.alloc_slice_copy(rest) }, target, vis });
                }
            }
        }
        for e in d.extern_crates.iter() {
            let vis = vis_of(&e.vis, module, parent)?;
            let (written, krate) = match e.target {
                ExternTarget::SelfCrate => ("self", module.krate),
                ExternTarget::Dep(n) => (n, self.extern_crate(index, module, n)?),
            };
            let name = match e.rename {
                Rename::To(n) => n,
                Rename::Same if written != "self" => written,
                Rename::Same | Rename::Underscore => continue,
            };
            entries.push(Entry::ExternCrate { name, krate, root: root_key(krate), vis });
        }
        let first_call = entries.len();
        for c in d.calls.iter() {
            let call = CallRef(c.call.0);
            let src = low.calls.iter().find(|(k, _)| *k == c.call).map(|(_, s)| *s).ok_or(SourceError::UnknownCall { module, call })?;
            let input: &'w TokenStream<'w> = bump.alloc_val(lex(src.input, facts.edition, a).map_err(|_| SourceError::Lex { module, call })?);
            self.hold(self.calls.len())?;
            self.calls.insert((module, call), CallInfo { src, shape: c.kind, def: None, input });
            entries.push(Entry::Call { call, writes: Writes::Only { names: &[], exports: false } });
        }
        let provisional = match head {
            Some(h) => {
                let entries = bump.alloc_slice_copy(&entries);
                let o = bump.alloc_val(Outline { module, digest: h.digest, chain: h.chain, no_implicit_prelude: h.no_implicit_prelude, entries });
                self.building.push((module, o));
                true
            }
            None => false,
        };
        let summed = self.summarise(module, index, low, depth, provisional, asked, &mut entries[first_call..]);
        if provisional {
            self.building.pop();
        }
        summed?;
        Ok(entries.into_bump_slice())
    }

    /// Each call's summary, written into its placeholder entry.
    fn summarise(
        &mut self,
        module: ModRef,
        index: usize,
        low: &Lowered<'w, 'w>,
        depth: usize,
        provisional: bool,
        asked: &mut AVec<'w, (CallRef, Ask<'w>, ItemKey)>,
        out: &mut [Entry<'w>],
    ) -> Result<(), SourceError<'w>> {
        let bump = self.alloc;
        let a = bump.alloc();
        let facts = self.crates[index];
        for (c, slot) in low.decls.calls.iter().zip(out.iter_mut()) {
            let call = CallRef(c.call.0);
            if c.kind != CallShape::FnLike {
                *slot = Entry::Call { call, writes: Writes::Any };
                continue;
            }
            let segs: &[&'w str] = &c.path.segments[..];
            let Some((&name, before)) = segs.split_last() else { return Err(SourceError::NoMacro { module, call }) };
            // The textual scope of this fragment: the last `macro_rules! name` written before it.
            let textual = (c.path.kind == PathKind::Plain && before.is_empty())
                .then(|| low.decls.macro_rules.iter().filter(|r| r.name == name && r.pos < c.pos).max_by_key(|r| r.pos).map(|r| r.def))
                .flatten();
            let key = match textual {
                Some(key) => key,
                None => {
                    let (anchor, rest) = anchor_of(c.path.kind, before, false, facts.edition, module)?;
                    let ask = Ask { from: module, path: Path { anchor, segs: bump.alloc_slice_copy(rest) }, name, ns: Ns::Macro };
                    let Answer::Found { def: Def::Item(key), .. } = self.ask_macro(ask, depth)? else {
                        return Err(SourceError::NoMacro { module, call });
                    };
                    if provisional {
                        asked.push((call, ask, key));
                    }
                    key
                }
            };
            let def = self.def_of(key)?;
            if let Some(info) = self.calls.get_mut(&(module, call)) {
                info.def = Some(def);
            }
            let input = self.calls.get(&(module, call)).map(|i| i.input).ok_or(SourceError::UnknownCall { module, call })?;
            let writes = match self.summaries {
                Summaries::AllAny => Writes::Any,
                Summaries::Read => {
                    let ns = MacroNs { src: RefCell::new(&mut *self), module, def_krate: def_krate(def), depth, failed: Cell::new(None) };
                    let summary = macro_writes(def, &ns, a);
                    if let Some(why) = ns.failed.get() {
                        return Err(why);
                    }
                    let summary = summary.map_err(|_| SourceError::Definition { item: key })?;
                    call_writes(&summary, Some(input), bump)
                }
            };
            self.counts.summaries += 1;
            *slot = Entry::Call { call, writes };
        }
        Ok(())
    }

    /// A name in the macro namespace, asked of a new resolver one layer deeper.
    fn ask_macro(&mut self, ask: Ask<'w>, depth: usize) -> Result<Answer, SourceError<'w>> {
        if depth + 1 > MOST_CALLEE_DEPTH {
            return Err(SourceError::CalleeDepth { module: ask.from });
        }
        self.counts.callee_asks += 1;
        let a = self.alloc.alloc();
        let mut resolver = Resolver::new(a);
        let mut layer = Layer { src: self, depth: depth + 1 };
        match resolve(&mut resolver, &mut layer, ask, a) {
            Ok(answer) => Ok(answer),
            Err(DriveError::Machine(why)) => Err(SourceError::Machine(why)),
            Err(DriveError::Source { why, .. }) => Err(why),
            Err(DriveError::Bound { .. }) => Err(SourceError::Steps { module: ask.from }),
        }
    }

    fn def_of(&mut self, key: ItemKey) -> Result<&'w MacroDefinition<'w>, SourceError<'w>> {
        if let Some(&d) = self.defs.get(&key) {
            return Ok(d);
        }
        let &(index, src) = self.macros.get(&key).ok_or(SourceError::Definition { item: key })?;
        let facts = self.crates[index];
        let def = definition_of(src, facts.key, None, facts.edition, CallShape::FnLike, self.alloc.alloc()).map_err(|_| SourceError::Definition { item: key })?;
        let def = self.alloc.alloc_val(def);
        self.hold(self.defs.len())?;
        self.defs.insert(key, def);
        Ok(def)
    }

    fn extern_crate(&self, index: usize, module: ModRef, name: &'w str) -> Result<CrateItemKey, SourceError<'w>> {
        let facts = self.crates[index];
        if let Some(&(_, dep)) = facts.deps.iter().find(|(n, _)| *n == name) {
            return self.crates.get(dep).map(|c| c.key).ok_or(SourceError::Internal("a dependency index past the crates given"));
        }
        facts
            .implicit
            .externs(&self.sysroot)
            .find(|(n, _)| *n == name)
            .and_then(|(_, unit)| self.crates.get(unit.0 as usize))
            .map(|c| c.key)
            .ok_or(SourceError::ExternCrate { module, name })
    }

    /// The module a `pub(super)` names: the parent in the walk, or the module itself for an
    /// expansion's fragment (its module's parent, as the walk has it).
    fn parent_of(&mut self, index: usize, module: ModRef) -> Result<Option<ItemKey>, SourceError<'w>> {
        let w = self.walk_of(index)?;
        let &at = w.tree.by_key.get(&module.key).ok_or(SourceError::UnknownModule(module))?;
        Ok(w.tree.modules[at].parent.map(|p| w.tree.modules[p].key))
    }

    fn hold(&self, len: usize) -> Result<(), SourceError<'w>> {
        if len >= MOST_HELD {
            return Err(SourceError::Full { table: "outlines, expansions, calls and definitions", most: MOST_HELD });
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct Head<'w> {
    chain: &'w [ItemKey],
    digest: ContentId,
    no_implicit_prelude: bool,
}

fn lower_cx<'w, T>(w: &'w CrateWalk<'w, T>, facts: &CrateFacts<'w>, entry: bool) -> LowerCx<'w, 'w, Tree<'w, 'w>> {
    LowerCx {
        krate: facts.key,
        tools: w.tools,
        edition: facts.edition,
        cfg: &w.cfg,
        walked: &w.tree,
        procs: ProcDecls::Text,
        entry,
        // The proc-macro harness (D175) is made by blaze-scope's cells driver only: this source
        // answers no `CallRole::ProcMacroHarness` call yet, so it writes none.
        harness: false,
        order: AttrOrder::Written,
    }
}

/// The file's own path, as the walk found it (a fragment's `written`).
fn text_path<'w, T>(w: &CrateWalk<'w, T>, file: u32) -> &'w str {
    w.files.files.borrow()[file as usize].path
}

/// The crate a macro's `$crate` names: its definition site's.
fn def_krate(def: &MacroDefinition<'_>) -> CrateItemKey {
    def.site().krate
}

fn vis_of<'e, P>(vis: &WrittenVis<P>, module: ModRef, parent: Option<ItemKey>) -> Result<Vis, SourceError<'e>> {
    Ok(match vis {
        WrittenVis::Private | WrittenVis::SelfOnly => Vis::In(module.key),
        WrittenVis::Pub => Vis::Pub,
        WrittenVis::Crate => Vis::Crate(module.krate),
        WrittenVis::Super => Vis::In(parent.ok_or(SourceError::SuperPastRoot { module })?),
        WrittenVis::In(_) | WrittenVis::WithoutIn(_) => return Err(SourceError::VisPath { module }),
    })
}

/// A path's anchor and the segments after it. `in_use`: a `use` path's first segment is
/// [`Anchor::Uniform`] (Rust 2018 on; `crate::` in 2015), any other's [`Anchor::Lexical`].
fn anchor_of<'s, 'w, 'e>(
    kind: PathKind,
    segs: &'s [&'w str],
    in_use: bool,
    edition: Edition,
    module: ModRef,
) -> Result<(Anchor<'w>, &'s [&'w str]), SourceError<'e>> {
    Ok(match kind {
        PathKind::Plain if in_use && edition == Edition::E2015 => (Anchor::Crate, segs),
        PathKind::Plain if in_use => (Anchor::Uniform, segs),
        PathKind::Plain => (Anchor::Lexical, segs),
        PathKind::Crate => (Anchor::Crate, segs),
        PathKind::SelfMod => (Anchor::SelfMod, segs),
        PathKind::Super(n) => (Anchor::Super(n as usize), segs),
        PathKind::Abs => match segs.split_first() {
            Some((&first, rest)) => (Anchor::Extern(first), rest),
            None => return Err(SourceError::UseRoot { module }),
        },
        PathKind::DollarCrate(k) => (Anchor::Root(ModRef { krate: k, key: root_key(k) }), segs),
    })
}

/// The source one nested resolver asks through.
struct Layer<'s, 'w, T> {
    src: &'s mut CrateSource<'w, T>,
    depth: usize,
}

impl<'w, T: Texts> Source<'w> for Layer<'_, 'w, T> {
    type Error = SourceError<'w>;

    fn outline(&mut self, module: ModRef) -> Result<&'w Outline<'w>, SourceError<'w>> {
        self.src.outline_at(module, self.depth)
    }

    fn expansion(&mut self, module: ModRef, call: CallRef) -> Result<&'w [Entry<'w>], SourceError<'w>> {
        self.src.expansion_at(module, call, self.depth)
    }

    fn root(&mut self, krate: CrateItemKey) -> Result<&'w CrateRoot<'w>, SourceError<'w>> {
        self.src.root_at(krate)
    }
}

impl<'w, T: Texts> Source<'w> for CrateSource<'w, T> {
    type Error = SourceError<'w>;

    fn outline(&mut self, module: ModRef) -> Result<&'w Outline<'w>, SourceError<'w>> {
        self.outline_at(module, 0)
    }

    fn expansion(&mut self, module: ModRef, call: CallRef) -> Result<&'w [Entry<'w>], SourceError<'w>> {
        self.expansion_at(module, call, 0)
    }

    fn root(&mut self, krate: CrateItemKey) -> Result<&'w CrateRoot<'w>, SourceError<'w>> {
        self.root_at(krate)
    }
}

/// blaze-expand's [`Callees`] by the macro namespace: a nested call of a definition, found as
/// a macro-namespace ask from the calling module (`m!`) or the definition's crate root
/// (`$crate::m!`). The first refusal is kept in `failed` and given back after `macro_writes`.
struct MacroNs<'s, 'w, T> {
    src: RefCell<&'s mut CrateSource<'w, T>>,
    module: ModRef,
    def_krate: CrateItemKey,
    depth: usize,
    failed: Cell<Option<SourceError<'w>>>,
}

impl<'w, T: Texts> Callees<'w> for MacroNs<'_, 'w, T> {
    fn callee(&self, callee: Callee<'w>) -> Option<&'w MacroDefinition<'w>> {
        if self.failed.get().is_some() {
            return None;
        }
        let (anchor, name) = match callee {
            Callee::Bare(name) => (Anchor::Lexical, name),
            Callee::Crate(name) => (Anchor::Root(ModRef { krate: self.def_krate, key: root_key(self.def_krate) }), name),
        };
        let ask = Ask { from: self.module, path: Path { anchor, segs: &[] }, name, ns: Ns::Macro };
        let mut src = self.src.borrow_mut();
        let found = match src.ask_macro(ask, self.depth) {
            Ok(Answer::Found { def: Def::Item(key), .. }) => src.def_of(key).map(Some),
            Ok(_) => Ok(None),
            Err(why) => Err(why),
        };
        match found {
            Ok(def) => def,
            Err(why) => {
                self.failed.set(Some(why));
                None
            }
        }
    }
}

/// No host: an expansion that reads the environment, a file, a cfg, a place or a proc macro is
/// refused, never answered with a default (rule 3).
struct NoHost;

impl ExpandCx for NoHost {
    fn env(&self, _: &str) -> Result<Option<&str>, HostError> {
        Err(HostError { ask: HostAsk::Env, why: HostWhy::Refused })
    }

    fn cfg(&self, _: &TokenStream<'_>) -> Result<bool, HostError> {
        Err(HostError { ask: HostAsk::Cfg, why: HostWhy::Refused })
    }

    fn include(&self, _: &str, _: IncludeAs) -> Result<Included<'_>, HostError> {
        Err(HostError { ask: HostAsk::Include, why: HostWhy::Refused })
    }

    fn place(&self, _: PlaceOf) -> Result<Place<'_>, HostError> {
        Err(HostError { ask: HostAsk::Place, why: HostWhy::Refused })
    }

    fn run_proc_macro<'a>(&self, _: &MacroDefinition<'_>, _: ProcCall<'_>, _: Alloc<'a>) -> Result<Produced<'a>, HostError> {
        Err(HostError { ask: HostAsk::ProcMacro, why: HostWhy::Refused })
    }
}
