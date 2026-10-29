//! A crate's `DefMap` and `LocalDefMap`, and its lang items, written as bytes and read back
//! into a running database (frozen dependency artifacts, blaze's `docs/glancer.md`, item 1).
//! [`Enc`] and [`Dec`] write and read other values that name items, as `hir_ty` does impls.
//!
//! The bytes name nothing by a salsa id: a file is the crate it belongs to and its path from
//! that crate's root directory, a module is its crate and its index in that crate's `DefMap`, a
//! crate is its index in the frozen crate's dependency closure, and every interned id is
//! written as the location it interns. Reading interns those locations again, so the ids it
//! makes are the running process's own. A macro call and a syntax context are written once and
//! named by their index after that.
//!
//! Hash maps are written in the order of their keys' fingerprints (the same encoding with
//! every macro call and syntax context replaced by a hash of what it is), so the bytes of a
//! `DefMap` are the same in every process. A frozen `DefMap` must equal the one name
//! resolution computes: [`Mode::Check`] computes both and records any difference, which is a
//! bug, never a reason to use the live one.

use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};

use base_db::{Crate, SourceDatabase};
use base_db::{FileId, VfsPath};
use cfg::{CfgAtom, CfgExpr};
use either::Either;
use hir_expand::{
    EagerCallInfo, ExpandErrorKind, ExpandTo, HirFileId, MacroCallId, MacroCallKind, MacroCallLoc,
    MacroCallStyles, MacroDefId, MacroDefKind,
    attrs::AttrId,
    builtin::{BuiltinAttrExpander, BuiltinDeriveExpander, BuiltinFnLikeExpander, EagerExpander},
    mod_path::{ModPath, PathKind},
    name::Name,
    proc_macro::{CustomProcMacroExpander, ProcMacroKind},
};
use intern::Symbol;
use la_arena::{Idx, RawIdx};
use rustc_hash::{FxHashMap, FxHashSet};
use smallvec::SmallVec;
use span::{
    Edition, ErasedFileAstId, FileAstId, Span, SpanAnchor, SyntaxContext, TextRange, TextSize,
    Transparency,
};
use syntax::ast;
use triomphe::Arc as TArc;
use tt::{Delimiter, DelimiterKind, IdentIsRaw, Leaf, LitKind, Spacing, Subtree, TokenTree};

use crate::{
    AdtId, AssocItemId, AstId, BlockId, BuiltinDeriveImplId, BuiltinDeriveImplLoc, ConstLoc,
    DefWithBodyId, EnumLoc, EnumVariantId, EnumVariantLoc, ExternBlockId, ExternBlockLoc,
    ExternCrateId, ExternCrateLoc, FunctionLoc, ImplId, ImplLoc, Intern, ItemContainerId, Lookup,
    Macro2Loc, MacroExpander, MacroId, MacroRulesLoc, MacroRulesLocFlags, ModuleDefId, ModuleId,
    ModuleIdLt, ProcMacroLoc, StaticLoc, StructLoc, TraitLoc, TypeAliasLoc, UnionLoc, UseId,
    UseLoc,
    builtin_derive::BuiltinDeriveImplTrait,
    builtin_type::BuiltinType,
    expr_store::Body,
    item_scope::{
        DeriveMacroInvocation, GlobId, ImportId, ImportOrDef, ImportOrExternCrate, ImportOrGlob,
        ItemScope,
    },
    item_tree::{TreeId, VisibilityExplicitness},
    lang_item::{LangItemEnum, LangItemTarget, LangItems},
    nameres::{
        DefDiagnostic, DefMap, DefMapCrateData, LocalDefMap, ModuleData, ModuleOrigin, ModulesSlot,
        ScopeSlot, diagnostics::DefDiagnosticKind,
    },
    per_ns::Item,
    unstable_features::UnstableFeatures,
    visibility::Visibility,
};

type R<T = ()> = Result<T, String>;
/// What name resolution does with the frozen bytes of a crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Mode {
    /// The bytes are the crate's `DefMap`: read them instead of resolving.
    Serve,
    /// Resolve, and record a difference between what resolving found and the bytes.
    Check,
    /// Read the bytes, and record a difference between writing what was read and the bytes.
    Verify,
}

/// The frozen `DefMap` of a crate.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Frozen {
    pub bytes: Arc<[u8]>,
    pub mode: Mode,
}

/// The crates whose `DefMap` is frozen, set by whoever has the bytes.
#[salsa::input(singleton, debug)]
pub struct FrozenDefMaps {
    #[returns(ref)]
    pub maps: Arc<FxHashMap<Crate, Frozen>>,
}

/// Has name resolution use `maps`.
pub fn set(db: &mut dyn SourceDatabase, maps: FxHashMap<Crate, Frozen>) {
    use salsa::{Durability, Setter};
    let maps = Arc::new(maps);
    match FrozenDefMaps::try_get(db) {
        Some(it) => {
            it.set_maps(db).with_durability(Durability::HIGH).to(maps);
        }
        None => {
            _ = FrozenDefMaps::builder(maps).durability(Durability::HIGH).new(db);
        }
    }
}

/// Each difference found, by the crate and the kind of what was frozen: a crate's `DefMap`
/// is resolved again when an `include!` loads a file, and only the last one is the crate's.
static DIFFERENCES: Mutex<Vec<(Crate, &'static str, String)>> = Mutex::new(Vec::new());

/// The differences [`Mode::Check`] and [`Mode::Verify`] found since this was last called.
pub fn differences() -> Vec<String> {
    let found = std::mem::take(&mut *DIFFERENCES.lock().unwrap());
    found.into_iter().map(|(_, _, it)| it).collect()
}

/// Forgets a difference found before in the frozen `kind` of `krate`, which agrees now.
fn agrees(krate: Crate, kind: &str) {
    DIFFERENCES.lock().unwrap().retain(|(at, of, _)| (*at, *of) != (krate, kind));
}

fn differs(db: &dyn SourceDatabase, krate: Crate, what: &str, written: &[u8], frozen: &[u8]) {
    record_difference(db, krate, "def map", what, written, frozen);
}

/// Records that the frozen `kind` of `krate` differs from `what`, which wrote `written`.
pub fn record_difference(
    db: &dyn SourceDatabase,
    krate: Crate,
    kind: &'static str,
    what: &str,
    written: &[u8],
    frozen: &[u8],
) {
    let at = written.iter().zip(frozen).position(|(a, b)| a != b).unwrap_or(written.len());
    let name = krate.extra_data(db).display_name.as_ref().map(|it| it.to_string());
    let said = format!(
        "the frozen {kind} of `{}` differs from {what}: {} bytes against {}, first at byte {at}",
        name.as_deref().unwrap_or("?"),
        written.len(),
        frozen.len(),
    );
    agrees(krate, kind);
    DIFFERENCES.lock().unwrap().push((krate, kind, said));
}

/// What [`super::crate_local_def_map`] serves for `krate` instead of resolving it, when its
/// `DefMap` is frozen.
pub(super) fn served(db: &dyn SourceDatabase, krate: Crate) -> Option<(DefMap, LocalDefMap)> {
    let frozen = FrozenDefMaps::try_get(db)?.maps(db).get(&krate)?.clone();
    if frozen.mode == Mode::Check {
        return None;
    }
    let (def_map, local) = decode(db, krate, &frozen.bytes)
        .unwrap_or_else(|why| panic!("a frozen def map does not read: {why}"));
    if frozen.mode == Mode::Verify {
        match encode_maps(db, krate, &def_map, &local) {
            Ok(written) if *written == *frozen.bytes => {}
            Ok(written) => differs(db, krate, "what was read of it", &written, &frozen.bytes),
            Err(why) => differs(db, krate, &format!("what was read of it ({why})"), &[], &[]),
        }
    }
    Some((def_map, local))
}

/// Records a difference between the `DefMap` resolving found and the frozen one, in
/// [`Mode::Check`].
pub(super) fn check(db: &dyn SourceDatabase, krate: Crate, def_map: &DefMap, local: &LocalDefMap) {
    let Some(frozen) = FrozenDefMaps::try_get(db).and_then(|it| it.maps(db).get(&krate).cloned())
    else {
        return;
    };
    if frozen.mode != Mode::Check {
        return;
    }
    match encode_maps(db, krate, def_map, local) {
        Ok(written) if *written == *frozen.bytes => agrees(krate, "def map"),
        Ok(written) => differs(db, krate, "the live one", &written, &frozen.bytes),
        Err(why) => {
            differs(db, krate, &format!("the live one, which it cannot write ({why})"), &[], &[])
        }
    }
}

/// Why the frozen bytes of `krate` cannot be read in this database: a file they name is not
/// loaded.
pub fn unreadable(db: &dyn SourceDatabase, krate: Crate, bytes: &[u8]) -> Option<String> {
    let mut dec = Dec::new(db, krate, bytes);
    dec.files().err()
}

/// The crates whose lang items are frozen, set by whoever has the bytes.
#[salsa::input(singleton, debug)]
pub struct FrozenLangItems {
    #[returns(ref)]
    pub maps: Arc<FxHashMap<Crate, Frozen>>,
}

/// Has finding the lang items of a crate use `maps`.
pub fn set_lang_items(db: &mut dyn SourceDatabase, maps: FxHashMap<Crate, Frozen>) {
    use salsa::{Durability, Setter};
    let maps = Arc::new(maps);
    match FrozenLangItems::try_get(db) {
        Some(it) => {
            it.set_maps(db).with_durability(Durability::HIGH).to(maps);
        }
        None => {
            _ = FrozenLangItems::builder(maps).durability(Durability::HIGH).new(db);
        }
    }
}

fn frozen_lang_items(db: &dyn SourceDatabase, krate: Crate) -> Option<Frozen> {
    FrozenLangItems::try_get(db)?.maps(db).get(&krate).cloned()
}

/// What [`crate_lang_items`](crate::lang_item::crate_lang_items) reads for `krate` instead of
/// finding them, when they are frozen.
pub(crate) fn served_lang_items(
    db: &dyn SourceDatabase,
    krate: Crate,
) -> Option<Option<Box<LangItems>>> {
    let frozen = frozen_lang_items(db, krate).filter(|it| it.mode != Mode::Check)?;
    let read = || {
        let mut dec = Dec::new_checked(db, krate, &frozen.bytes)?;
        let items = dec.option(|dec| {
            let mut items = LangItems::default();
            let n = dec.len()?;
            if n != LangItemEnum::ALL.len() {
                return Err(format!("{n} lang items"));
            }
            for &which in LangItemEnum::ALL {
                if let Some(target) = dec.option(Dec::lang_item_target)?
                    && !items.set(which, target)
                {
                    return Err(format!("a lang item {which:?} of another kind"));
                }
            }
            Ok(Box::new(items))
        })?;
        dec.finish()?;
        Ok::<_, String>(items)
    };
    Some(read().unwrap_or_else(|why| panic!("frozen lang items do not read: {why}")))
}

/// The bytes of the lang items of `krate` as they are now: found, or read in [`Mode::Serve`]
/// and [`Mode::Verify`].
pub fn encode_lang_items(db: &dyn SourceDatabase, krate: Crate) -> R<Vec<u8>> {
    let mut enc = Enc::new(db, krate);
    enc.option(crate::lang_item::crate_lang_items(db, krate), |enc, items| {
        enc.usize(LangItemEnum::ALL.len())?;
        for &which in LangItemEnum::ALL {
            enc.option(which.from_lang_items(items), Enc::lang_item_target)?;
        }
        Ok(())
    })?;
    enc.finish()
}

/// Records a difference between the frozen lang items of `krate` and the live ones in
/// [`Mode::Check`], or what was read of them in [`Mode::Verify`].
pub fn check_lang_items(db: &dyn SourceDatabase, krate: Crate) {
    let Some(frozen) = frozen_lang_items(db, krate) else { return };
    let what = match frozen.mode {
        Mode::Serve => return,
        Mode::Check => "the live ones",
        Mode::Verify => "what was read of them",
    };
    match encode_lang_items(db, krate) {
        Ok(written) if *written == *frozen.bytes => {}
        Ok(written) => record_difference(db, krate, "lang items", what, &written, &frozen.bytes),
        Err(why) => {
            let what = format!("{what}, which cannot be written ({why})");
            record_difference(db, krate, "lang items", &what, &[], &[])
        }
    }
}

/// The bytes of the `DefMap` of `krate` as name resolution computes it, or why it cannot be
/// frozen.
pub fn encode(db: &dyn SourceDatabase, krate: Crate) -> R<Vec<u8>> {
    let pair = super::crate_local_def_map(db, krate);
    encode_maps(db, krate, pair.def_map(db), pair.local(db))
}

/// `krate` and the crates it depends on, transitively, breadth first in the order the crate
/// graph lists dependencies: the crates frozen bytes name by index.
fn closure(db: &dyn SourceDatabase, krate: Crate) -> Vec<Crate> {
    let mut order = vec![krate];
    let mut seen: FxHashSet<Crate> = order.iter().copied().collect();
    let mut at = 0;
    while let Some(&next) = order.get(at) {
        at += 1;
        for dep in &next.data(db).dependencies {
            if seen.insert(dep.crate_id) {
                order.push(dep.crate_id);
            }
        }
    }
    order
}

/// The directory of the root file of `krate`, which frozen bytes name its files from, and its
/// source root.
fn root_dir(db: &dyn SourceDatabase, krate: Crate) -> R<(String, bool, base_db::SourceRootId)> {
    let root = krate.data(db).root_file_id;
    let source_root = db.file_source_root(root).source_root_id(db);
    let path = db.source_root(source_root).source_root(db).path_for_file(&root).cloned();
    let path = path.ok_or("a crate root with no path")?;
    let real = path.as_path().is_some();
    let path = on_disk(&path)?;
    let dir =
        path.rsplit_once('/').map(|it| it.0.to_owned()).ok_or("a crate root in no directory")?;
    Ok((dir, real, source_root))
}

/// The text of `path`, which [`vfs_path`] reads back.
fn on_disk(path: &VfsPath) -> R<String> {
    let text = path.to_string();
    if !text.starts_with('/') {
        return Err(format!("a file with a path that is not absolute: {text}"));
    }
    Ok(text)
}

/// The path `text` is, on disk when `real`.
fn vfs_path(real: bool, text: String) -> VfsPath {
    if real { VfsPath::new_real_path(text) } else { VfsPath::new_virtual_path(text) }
}

/// What stands for the `OUT_DIR` of a crate of the closure in the bytes: in the name of a file
/// below it, in place of the crate's root directory, and, followed by the crate's index and a
/// NUL, in a literal, as `include!(concat!(env!("OUT_DIR"), ..))` leaves one in its argument,
/// of the crate being written or of a dependency whose macro calls it names. A build script
/// writes below the target directory of the checkout that ran it, so the bytes of a crate are
/// the same in every checkout. No path or literal holds a NUL.
const OUT: &str = "\0OUT_DIR";

fn out_dir(db: &dyn SourceDatabase, krate: Crate) -> Option<String> {
    let out = krate.env(db).get("OUT_DIR")?;
    let out = out.trim_end_matches('/');
    (!out.is_empty()).then(|| out.to_owned())
}

/// The `OUT_DIR` of each crate of `closure` that has one, by its index there.
fn out_dirs(db: &dyn SourceDatabase, closure: &[Crate]) -> Vec<(u32, String)> {
    let dirs = closure.iter().enumerate();
    dirs.filter_map(|(index, &krate)| Some((index as u32, out_dir(db, krate)?))).collect()
}

fn holds(bytes: &[u8], part: &[u8]) -> bool {
    !part.is_empty() && bytes.windows(part.len()).any(|it| it == part)
}

/// `to` from the directory `from`, with `..` where it is not under it.
fn relative(from: &str, to: &str) -> String {
    let from: Vec<_> = from.split('/').filter(|it| !it.is_empty()).collect();
    let to: Vec<_> = to.split('/').filter(|it| !it.is_empty()).collect();
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    let mut parts: Vec<&str> = vec![".."; from.len() - common];
    parts.extend(&to[common..]);
    parts.join("/")
}

/// `relative` from the directory `from`: what [`relative`] gave back.
fn joined(from: &str, relative: &str) -> String {
    let mut parts: Vec<_> = from.split('/').filter(|it| !it.is_empty()).collect();
    for part in relative.split('/') {
        match part {
            ".." => {
                parts.pop();
            }
            "" | "." => {}
            it => parts.push(it),
        }
    }
    format!("/{}", parts.join("/"))
}

fn position<T: PartialEq>(all: &[T], it: T) -> u32 {
    all.iter().position(|x| *x == it).unwrap() as u32
}

/// The items of `module` that have a body: a block in the module is in one of theirs.
fn body_owners(db: &dyn SourceDatabase, module: ModuleId) -> Vec<DefWithBodyId> {
    let def_map = module.def_map(db);
    let Some((_, data)) = def_map.modules(db).find(|(id, _)| *id == module) else {
        return Vec::new();
    };
    let scope = data.scope(db);
    let mut owners: Vec<DefWithBodyId> = scope.unnamed_consts().map(Into::into).collect();
    let assoc = |owners: &mut Vec<DefWithBodyId>, items: &[(Name, AssocItemId)]| {
        for (_, item) in items {
            match *item {
                AssocItemId::FunctionId(it) => owners.push(it.into()),
                AssocItemId::ConstId(it) => owners.push(it.into()),
                AssocItemId::TypeAliasId(_) => {}
            }
        }
    };
    for def in scope.declarations() {
        match def {
            ModuleDefId::FunctionId(it) => owners.push(it.into()),
            ModuleDefId::ConstId(it) => owners.push(it.into()),
            ModuleDefId::StaticId(it) => owners.push(it.into()),
            ModuleDefId::AdtId(AdtId::EnumId(it)) => {
                let variants = &crate::signatures::EnumVariants::of(db, it).0.variants;
                owners.extend(variants.values().map(|&(it, _)| DefWithBodyId::VariantId(it)));
            }
            ModuleDefId::TraitId(it) => {
                assoc(&mut owners, &super::assoc::TraitItems::query(db, it).items)
            }
            _ => {}
        }
    }
    for it in scope.impls() {
        assoc(&mut owners, &super::assoc::ImplItems::of(db, it).0.items);
    }
    owners
}

const NEW: u8 = 0;
const SEEN: u8 = 1;

/// Writes values that name items of `krate` and its dependencies as frozen bytes; see the
/// module documentation.
pub struct Enc<'a> {
    db: &'a dyn SourceDatabase,
    out: Vec<u8>,
    /// Writing a fingerprint: macro calls and syntax contexts are written as hashes of what
    /// they are, and nothing is named by an index.
    fingerprinting: bool,
    crates: FxHashMap<Crate, u32>,
    closure: Vec<Crate>,
    dirs: FxHashMap<u32, String>,
    /// The first crate of the closure whose root is in each source root.
    owners: FxHashMap<base_db::SourceRootId, u32>,
    /// Each file: the crate it is named from and its path from that crate's root.
    file_names: FxHashMap<FileId, (u32, String)>,
    files: FxHashMap<FileId, u32>,
    /// The files named, in the order of their index.
    file_order: Vec<FileId>,
    calls: FxHashMap<MacroCallId, u32>,
    contexts: FxHashMap<SyntaxContext, u32>,
    call_hashes: FxHashMap<MacroCallId, u128>,
    context_hashes: FxHashMap<SyntaxContext, u128>,
    modules: FxHashMap<ModuleId, (u32, u32)>,
    indexed: FxHashSet<Crate>,
    /// The body each block that was looked for is in, found by lowering every body of the
    /// modules in `searched`.
    block_owners: FxHashMap<BlockId, DefWithBodyId>,
    searched: FxHashSet<ModuleId>,
    out_dirs: Vec<(u32, String)>,
}

/// Whether `loc` is an `include!` that reads a file: not the call of its argument, which has
/// the same definition and no argument of its own.
fn includes(loc: &MacroCallLoc) -> bool {
    loc.def.is_include() && matches!(loc.kind, MacroCallKind::FnLike { eager: Some(_), .. })
}

fn hash128(bytes: &[u8]) -> u128 {
    let half = |seed: u8| {
        #[allow(deprecated)]
        let mut hasher = std::hash::SipHasher::new();
        seed.hash(&mut hasher);
        bytes.hash(&mut hasher);
        hasher.finish()
    };
    (u128::from(half(0)) << 64) | u128::from(half(1))
}

fn encode_maps(
    db: &dyn SourceDatabase,
    krate: Crate,
    def_map: &DefMap,
    local: &LocalDefMap,
) -> R<Vec<u8>> {
    let mut enc = Enc::new(db, krate);
    for (index, &module) in def_map.table(db).keys().enumerate() {
        enc.modules.insert(module, (0, index as u32));
    }
    enc.indexed.insert(krate);
    enc.def_map(def_map)?;
    enc.local(local)?;
    enc.finish()
}

impl<'a> Enc<'a> {
    /// Writes values of `krate`, which names its dependencies by their index in its closure.
    pub fn new(db: &'a dyn SourceDatabase, krate: Crate) -> Self {
        let closure = closure(db, krate);
        let crates = closure.iter().enumerate().map(|(i, &it)| (it, i as u32)).collect();
        let out_dirs = out_dirs(db, &closure);
        Enc {
            db,
            out: Vec::new(),
            fingerprinting: false,
            crates,
            closure,
            dirs: FxHashMap::default(),
            owners: FxHashMap::default(),
            file_names: FxHashMap::default(),
            files: FxHashMap::default(),
            file_order: Vec::new(),
            calls: FxHashMap::default(),
            contexts: FxHashMap::default(),
            call_hashes: FxHashMap::default(),
            context_hashes: FxHashMap::default(),
            modules: FxHashMap::default(),
            indexed: FxHashSet::default(),
            block_owners: FxHashMap::default(),
            searched: FxHashSet::default(),
            out_dirs,
        }
    }

    /// The bytes written, after the files they name, which [`Dec::new_checked`] reads first.
    pub fn finish(mut self) -> R<Vec<u8>> {
        // The target directories the closure's build scripts wrote below, which nothing the
        // bytes say may name: they are of one checkout.
        let mut places: Vec<String> = (self.out_dirs.iter())
            .map(|(_, out)| out.find("/build/").map_or(out.clone(), |at| out[..at + 1].to_owned()))
            .collect();
        places.sort_unstable();
        places.dedup();
        let body = std::mem::take(&mut self.out);
        self.usize(self.file_order.len())?;
        for file in std::mem::take(&mut self.file_order) {
            let (owner, path) = self.file_names[&file].clone();
            self.u32(owner);
            self.str(&path)?;
            // A file outside the directory of its crate's root may be one a build script
            // wrote, which the key does not cover: its text is.
            let outside = path.starts_with("../") || path.starts_with(OUT);
            self.bool(outside);
            if outside {
                let text = self.db.file_text(file).text(self.db);
                self.out.extend_from_slice(&hash128(text.as_bytes()).to_le_bytes());
            }
            if !path.starts_with(OUT) {
                let full = joined(&self.dirs[&owner], &path);
                if let Some(place) = places.iter().find(|it| full.starts_with(it.as_str())) {
                    return Err(format!("a file below {place}, a place of one checkout: {full}"));
                }
            }
        }
        self.out.extend_from_slice(&body);
        if let Some(place) = places.iter().find(|it| holds(&self.out, it.as_bytes())) {
            return Err(format!("the bytes name {place}, a place of one checkout"));
        }
        Ok(self.out)
    }

    /// A literal's text with each `OUT_DIR` of the closure as [`OUT`], the crate's index and a
    /// NUL.
    fn unplaced(&self, text: &Symbol) -> R<Symbol> {
        let written = text.as_str();
        if written.contains('\0') {
            return Err("a literal holding a NUL".to_owned());
        }
        let mut unplaced = written.to_owned();
        for (index, out) in &self.out_dirs {
            if unplaced.contains(out.as_str()) {
                unplaced = unplaced.replace(out.as_str(), &format!("{OUT} {index}\0"));
            }
        }
        Ok(if unplaced == written { text.clone() } else { Symbol::intern(&unplaced) })
    }
}

impl Enc<'_> {
    pub fn u8(&mut self, it: u8) {
        self.out.push(it);
    }

    pub fn u32(&mut self, mut it: u32) {
        loop {
            let byte = (it & 0x7f) as u8;
            it >>= 7;
            if it == 0 {
                self.out.push(byte);
                return;
            }
            self.out.push(byte | 0x80);
        }
    }

    pub fn usize(&mut self, it: usize) -> R {
        self.u32(u32::try_from(it).map_err(|_| "a length over u32")?);
        Ok(())
    }

    pub fn bool(&mut self, it: bool) {
        self.u8(it as u8);
    }

    pub fn str(&mut self, it: &str) -> R {
        self.usize(it.len())?;
        self.out.extend_from_slice(it.as_bytes());
        Ok(())
    }

    pub fn symbol(&mut self, it: &Symbol) -> R {
        self.str(it.as_str())
    }

    pub fn name(&mut self, it: &Name) -> R {
        self.symbol(it.symbol())
    }

    fn edition(&mut self, it: Edition) {
        self.u8(it as u8);
    }

    pub fn krate(&mut self, it: Crate) -> R {
        let index = *self.crates.get(&it).ok_or("a crate outside the dependency closure")?;
        self.u32(index);
        Ok(())
    }

    /// Writes `f`'s output as a fingerprint, and returns it.
    pub fn fingerprint(&mut self, f: impl FnOnce(&mut Self) -> R) -> R<Vec<u8>> {
        let out = std::mem::take(&mut self.out);
        let was = std::mem::replace(&mut self.fingerprinting, true);
        let result = f(self);
        self.fingerprinting = was;
        let written = std::mem::replace(&mut self.out, out);
        result.map(|()| written)
    }

    /// `entries` in the order of their keys' fingerprints.
    pub fn sorted<K, V>(
        &mut self,
        entries: impl Iterator<Item = (K, V)>,
        key: impl Fn(&mut Self, &K) -> R,
    ) -> R<Vec<(K, V)>> {
        let mut keyed = Vec::new();
        for (k, v) in entries {
            let print = self.fingerprint(|enc| key(enc, &k))?;
            keyed.push((print, k, v));
        }
        keyed.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(keyed.into_iter().map(|(_, k, v)| (k, v)).collect())
    }

    fn file_name(&mut self, file: FileId) -> R<(u32, String)> {
        if let Some(it) = self.file_names.get(&file) {
            return Ok(it.clone());
        }
        let db = self.db;
        let source_root = db.file_source_root(file).source_root_id(db);
        let path = db.source_root(source_root).source_root(db).path_for_file(&file).cloned();
        let path = on_disk(&path.ok_or("a file with no path")?)?;
        if self.owners.is_empty() {
            for (index, &krate) in self.closure.iter().enumerate() {
                let (dir, _, root) = root_dir(db, krate)?;
                self.owners.entry(root).or_insert(index as u32);
                self.dirs.insert(index as u32, dir);
            }
        }
        let owner = *self
            .owners
            .get(&source_root)
            .ok_or_else(|| format!("a file of no crate of the closure: {path}"))?;
        // Named from the crate whose build script wrote it, whichever crate's root is beside it.
        let below_out = self.out_dirs.iter().find_map(|(index, out)| {
            let rest = path.strip_prefix(out.as_str())?;
            rest.starts_with('/').then(|| (*index, format!("{OUT}{rest}")))
        });
        let name = below_out.unwrap_or_else(|| (owner, relative(&self.dirs[&owner], &path)));
        self.file_names.insert(file, name.clone());
        Ok(name)
    }

    fn file(&mut self, file: base_db::EditionedFileId) -> R {
        let (file_id, edition) = file.unpack(self.db);
        self.edition(edition);
        if self.fingerprinting {
            let (owner, path) = self.file_name(file_id)?;
            self.u32(owner);
            return self.str(&path);
        }
        let index = match self.files.get(&file_id) {
            Some(&it) => it,
            None => {
                self.file_name(file_id)?;
                let index = self.file_order.len() as u32;
                self.files.insert(file_id, index);
                self.file_order.push(file_id);
                index
            }
        };
        self.u32(index);
        Ok(())
    }

    fn hir_file(&mut self, it: HirFileId) -> R {
        match it {
            HirFileId::FileId(file) => {
                self.u8(0);
                self.file(file)
            }
            HirFileId::MacroFile(call) => {
                self.u8(1);
                self.call(call)
            }
        }
    }

    fn ast_id<N: syntax::AstNode>(&mut self, it: AstId<N>) -> R {
        self.hir_file(it.file_id)?;
        self.u32(it.value.erase().into_raw());
        Ok(())
    }

    fn erased_ast_id(&mut self, it: ErasedFileAstId) {
        self.u32(it.into_raw());
    }

    fn call(&mut self, call: MacroCallId) -> R {
        if self.fingerprinting {
            let print = self.call_hash(call)?;
            self.out.extend_from_slice(&print.to_le_bytes());
            return Ok(());
        }
        if let Some(&index) = self.calls.get(&call) {
            self.u8(SEEN);
            self.u32(index);
            return Ok(());
        }
        self.u8(NEW);
        self.call_loc(call)?;
        let index = self.calls.len() as u32;
        self.calls.insert(call, index);
        Ok(())
    }

    fn call_hash(&mut self, call: MacroCallId) -> R<u128> {
        if let Some(&it) = self.call_hashes.get(&call) {
            return Ok(it);
        }
        let print = hash128(&self.fingerprint(|enc| enc.call_loc(call))?);
        self.call_hashes.insert(call, print);
        Ok(print)
    }

    fn call_loc(&mut self, call: MacroCallId) -> R {
        let loc = call.loc(self.db).clone();
        let included = includes(&loc).then(|| loc.include_file_id(self.db, call));
        let MacroCallLoc { def, krate, kind, ctxt, macro_depth } = loc;
        self.def_id(def)?;
        self.krate(krate)?;
        self.call_kind(kind)?;
        self.context(ctxt)?;
        self.u32(macro_depth);
        // What `include!` expands to is the file it reads, which the header must name: one a
        // build script writes may not be there yet, or may have other text, where it is read.
        match included {
            None => {}
            Some(Some(file)) => self.file(file)?,
            Some(None) => return Err("an include! of a file that is not loaded".to_owned()),
        }
        Ok(())
    }

    fn call_kind(&mut self, kind: MacroCallKind) -> R {
        match kind {
            MacroCallKind::FnLike { ast_id, expand_to, eager } => {
                self.u8(0);
                self.ast_id(ast_id)?;
                self.u8(match expand_to {
                    ExpandTo::Statements => 0,
                    ExpandTo::Items => 1,
                    ExpandTo::Pattern => 2,
                    ExpandTo::Type => 3,
                    ExpandTo::Expr => 4,
                });
                match eager {
                    None => self.u8(0),
                    Some(eager) => {
                        let EagerCallInfo { arg, arg_id, error, span } = &*eager;
                        if error.is_some() {
                            return Err("an eager macro call whose argument failed".to_owned());
                        }
                        self.u8(1);
                        self.subtree(arg)?;
                        self.call(*arg_id)?;
                        self.span(*span)?;
                    }
                }
            }
            MacroCallKind::Derive { ast_id, derive_attr_index, derive_index, derive_macro_id } => {
                self.u8(1);
                self.ast_id(ast_id)?;
                self.u32(derive_attr_index.item_tree_index());
                self.u32(derive_index);
                self.call(derive_macro_id)?;
            }
            MacroCallKind::Attr { ast_id, attr_args, censored_attr_ids } => {
                self.u8(2);
                self.ast_id(ast_id)?;
                match attr_args {
                    None => self.u8(0),
                    Some(args) => {
                        self.u8(1);
                        self.subtree(&args)?;
                    }
                }
                self.usize(censored_attr_ids.len())?;
                for id in censored_attr_ids.iter() {
                    self.u32(id.item_tree_index());
                }
            }
        }
        Ok(())
    }

    fn def_id(&mut self, it: MacroDefId) -> R {
        let MacroDefId { krate, edition, kind, local_inner, allow_internal_unsafe, edition_panic } =
            it;
        self.krate(krate)?;
        self.edition(edition);
        self.bool(local_inner);
        self.bool(allow_internal_unsafe);
        self.bool(edition_panic);
        match kind {
            MacroDefKind::Declarative(ast_id, styles) => {
                self.u8(0);
                self.ast_id(ast_id)?;
                self.u8(styles.bits());
            }
            MacroDefKind::BuiltIn(ast_id, it) => {
                self.u8(1);
                self.ast_id(ast_id)?;
                self.u32(position(BuiltinFnLikeExpander::ALL, it));
            }
            MacroDefKind::BuiltInAttr(ast_id, it) => {
                self.u8(2);
                self.ast_id(ast_id)?;
                self.u32(position(BuiltinAttrExpander::ALL, it));
            }
            MacroDefKind::BuiltInDerive(ast_id, it) => {
                self.u8(3);
                self.ast_id(ast_id)?;
                self.u32(position(BuiltinDeriveExpander::ALL, it));
            }
            MacroDefKind::BuiltInEager(ast_id, it) => {
                self.u8(4);
                self.ast_id(ast_id)?;
                self.u32(position(EagerExpander::ALL, it));
            }
            MacroDefKind::UnimplementedBuiltIn(ast_id) => {
                self.u8(5);
                self.ast_id(ast_id)?;
            }
            MacroDefKind::ProcMacro(ast_id, expander, kind) => {
                self.u8(6);
                self.ast_id(ast_id)?;
                self.proc_macro(expander, kind)?;
            }
        }
        Ok(())
    }

    /// A procedural macro by its place among those its crate's library lists, which the
    /// crate's source decides. One missing or disabled is so only in this process.
    fn proc_macro(&mut self, expander: CustomProcMacroExpander, kind: ProcMacroKind) -> R {
        let listed = expander.listed().ok_or("a procedural macro that is not loaded")?;
        self.u32(listed);
        self.u8(match kind {
            ProcMacroKind::CustomDerive => 0,
            ProcMacroKind::Bang => 1,
            ProcMacroKind::Attr => 2,
        });
        Ok(())
    }

    fn context(&mut self, ctx: SyntaxContext) -> R {
        if ctx.is_root() {
            self.u8(0);
            self.edition(ctx.edition(self.db));
            return Ok(());
        }
        if self.fingerprinting {
            self.u8(1);
            let print = self.context_hash(ctx)?;
            self.out.extend_from_slice(&print.to_le_bytes());
            return Ok(());
        }
        if let Some(&index) = self.contexts.get(&ctx) {
            self.u8(2);
            self.u32(index);
            return Ok(());
        }
        self.u8(1);
        self.context_data(ctx)?;
        let index = self.contexts.len() as u32;
        self.contexts.insert(ctx, index);
        Ok(())
    }

    fn context_hash(&mut self, ctx: SyntaxContext) -> R<u128> {
        if let Some(&it) = self.context_hashes.get(&ctx) {
            return Ok(it);
        }
        let print = hash128(&self.fingerprint(|enc| enc.context_data(ctx))?);
        self.context_hashes.insert(ctx, print);
        Ok(print)
    }

    fn context_data(&mut self, ctx: SyntaxContext) -> R {
        let db = self.db;
        let outer = ctx.outer_expn(db).ok_or("a syntax context with no expansion")?;
        self.call(MacroCallId::from(outer))?;
        self.u8(match ctx.outer_transparency(db) {
            Transparency::Transparent => 0,
            Transparency::SemiOpaque => 1,
            Transparency::Opaque => 2,
        });
        self.edition(ctx.edition(db));
        self.context(ctx.parent(db))?;
        for other in [ctx.opaque(db), ctx.opaque_and_semiopaque(db)] {
            if other == ctx {
                self.u8(0);
            } else {
                self.u8(1);
                self.context(other)?;
            }
        }
        Ok(())
    }

    fn span(&mut self, it: Span) -> R {
        let Span { range, anchor: SpanAnchor { file_id, ast_id }, ctx } = it;
        self.u32(range.start().into());
        self.u32(range.len().into());
        self.file(base_db::EditionedFileId::from_span_file_id(self.db, file_id))?;
        self.erased_ast_id(ast_id);
        self.context(ctx)
    }

    fn subtree(&mut self, it: &tt::TopSubtree) -> R {
        let tokens: Vec<_> = it.as_token_trees().iter_flat_tokens().collect();
        self.usize(tokens.len())?;
        for token in tokens {
            match token {
                TokenTree::Subtree(Subtree { delimiter: Delimiter { open, close, kind }, len }) => {
                    self.u8(0);
                    self.u8(kind as u8);
                    self.u32(len);
                    self.span(open)?;
                    self.span(close)?;
                }
                TokenTree::Leaf(Leaf::Literal(tt::Literal {
                    text_and_suffix,
                    span,
                    kind,
                    suffix_len,
                })) => {
                    self.u8(1);
                    let text_and_suffix = self.unplaced(&text_and_suffix)?;
                    self.symbol(&text_and_suffix)?;
                    let (kind, hashes) = match kind {
                        LitKind::Byte => (0, 0),
                        LitKind::Char => (1, 0),
                        LitKind::Integer => (2, 0),
                        LitKind::Float => (3, 0),
                        LitKind::Str => (4, 0),
                        LitKind::StrRaw(n) => (5, n),
                        LitKind::ByteStr => (6, 0),
                        LitKind::ByteStrRaw(n) => (7, n),
                        LitKind::CStr => (8, 0),
                        LitKind::CStrRaw(n) => (9, n),
                        LitKind::Err(()) => (10, 0),
                    };
                    self.u8(kind);
                    self.u8(hashes);
                    self.u8(suffix_len);
                    self.span(span)?;
                }
                TokenTree::Leaf(Leaf::Punct(tt::Punct { char, spacing, span })) => {
                    self.u8(2);
                    self.u32(char as u32);
                    self.u8(match spacing {
                        Spacing::Alone => 0,
                        Spacing::Joint => 1,
                        Spacing::JointHidden => 2,
                    });
                    self.span(span)?;
                }
                TokenTree::Leaf(Leaf::Ident(tt::Ident { sym, span, is_raw })) => {
                    self.u8(3);
                    self.symbol(&sym)?;
                    self.bool(matches!(is_raw, IdentIsRaw::Yes));
                    self.span(span)?;
                }
            }
        }
        Ok(())
    }

    pub fn module(&mut self, it: ModuleId) -> R {
        let db = self.db;
        if let Some(block) = it.block(db) {
            // A block is made as its body is lowered: it is named by that body.
            let block = unsafe { block.to_static() };
            let owner = self.block_owner(block)?;
            self.u8(1);
            self.body_owner(owner)?;
            self.ast_id(block.ast_id(db))?;
            let table = super::block_def_map(db, block).table(db);
            let index = table.keys().position(|&m| m == it).ok_or("a module not in its block")?;
            self.u32(index as u32);
            return Ok(());
        }
        self.u8(0);
        let krate = it.krate(db);
        if self.indexed.insert(krate) {
            let owner = *self.crates.get(&krate).ok_or("a module outside the closure")?;
            let def_map = super::crate_def_map(db, krate);
            for (index, &module) in def_map.table(db).keys().enumerate() {
                self.modules.insert(module, (owner, index as u32));
            }
        }
        let &(owner, index) = self.modules.get(&it).ok_or("a module not in its def map")?;
        self.u32(owner);
        self.u32(index);
        Ok(())
    }

    /// The body `block` is in: one of an item of the module the block is in, or, in a block
    /// with items, that block's.
    fn block_owner(&mut self, block: BlockId) -> R<DefWithBodyId> {
        if let Some(&owner) = self.block_owners.get(&block) {
            return Ok(owner);
        }
        let db = self.db;
        let module = unsafe { block.module(db).to_static() };
        if self.searched.insert(module) {
            for owner in body_owners(db, module) {
                for (found, _) in Body::of(db, owner).blocks(db) {
                    self.block_owners.entry(found).or_insert(owner);
                }
            }
        }
        if let Some(&owner) = self.block_owners.get(&block) {
            return Ok(owner);
        }
        let outer = module.block(db).map(|it| unsafe { it.to_static() });
        let owner = outer.ok_or("a block in no body of its module")?;
        let owner = self.block_owner(owner)?;
        if !Body::of(db, owner).blocks(db).any(|(it, _)| it == block) {
            return Err("a block in no body of the block it is in".to_owned());
        }
        self.block_owners.insert(block, owner);
        Ok(owner)
    }

    pub fn lang_item_target(&mut self, it: LangItemTarget) -> R {
        match it {
            LangItemTarget::EnumId(it) => {
                self.u8(0);
                self.adt(it.into())
            }
            LangItemTarget::StructId(it) => {
                self.u8(0);
                self.adt(it.into())
            }
            LangItemTarget::UnionId(it) => {
                self.u8(0);
                self.adt(it.into())
            }
            LangItemTarget::FunctionId(it) => {
                self.u8(1);
                self.def(it.into())
            }
            LangItemTarget::StaticId(it) => {
                self.u8(1);
                self.def(it.into())
            }
            LangItemTarget::TypeAliasId(it) => {
                self.u8(1);
                self.def(it.into())
            }
            LangItemTarget::TraitId(it) => {
                self.u8(1);
                self.def(it.into())
            }
            LangItemTarget::ConstId(it) => {
                self.u8(1);
                self.def(it.into())
            }
            LangItemTarget::EnumVariantId(it) => {
                self.u8(2);
                self.variant(it)
            }
            LangItemTarget::ImplId(it) => {
                self.u8(3);
                self.impl_id(it)
            }
            LangItemTarget::MacroId(it) => {
                self.u8(4);
                self.macro_id(it)
            }
        }
    }

    pub fn body_owner(&mut self, it: DefWithBodyId) -> R {
        match it {
            DefWithBodyId::FunctionId(it) => {
                self.u8(0);
                self.def(it.into())
            }
            DefWithBodyId::StaticId(it) => {
                self.u8(0);
                self.def(it.into())
            }
            DefWithBodyId::ConstId(it) => {
                self.u8(0);
                self.def(it.into())
            }
            DefWithBodyId::VariantId(it) => {
                self.u8(1);
                self.variant(it)
            }
        }
    }

    fn visibility(&mut self, it: Visibility) -> R {
        match it {
            Visibility::Module(module, explicitness) => {
                self.u8(match explicitness {
                    VisibilityExplicitness::Explicit => 0,
                    VisibilityExplicitness::Implicit => 1,
                });
                self.module(module)
            }
            Visibility::PubCrate(krate) => {
                self.u8(2);
                self.krate(krate)
            }
            Visibility::Public => {
                self.u8(3);
                Ok(())
            }
        }
    }

    fn container(&mut self, it: ItemContainerId) -> R {
        let db = self.db;
        match it {
            ItemContainerId::ModuleId(it) => {
                self.u8(0);
                self.module(it)
            }
            ItemContainerId::ImplId(it) => {
                self.u8(1);
                let ImplLoc { container, id } = *it.lookup(db);
                self.module(container)?;
                self.ast_id(id)
            }
            ItemContainerId::TraitId(it) => {
                self.u8(2);
                let TraitLoc { container, id } = *it.lookup(db);
                self.module(container)?;
                self.ast_id(id)
            }
            ItemContainerId::ExternBlockId(it) => {
                self.u8(3);
                self.extern_block(it)
            }
        }
    }

    fn extern_block(&mut self, it: ExternBlockId) -> R {
        let ExternBlockLoc { container, id } = *it.lookup(self.db);
        self.module(container)?;
        self.ast_id(id)
    }

    pub fn adt(&mut self, it: AdtId) -> R {
        let db = self.db;
        match it {
            AdtId::StructId(it) => {
                self.u8(0);
                let StructLoc { container, id } = *it.lookup(db);
                self.module(container)?;
                self.ast_id(id)
            }
            AdtId::UnionId(it) => {
                self.u8(1);
                let UnionLoc { container, id } = *it.lookup(db);
                self.module(container)?;
                self.ast_id(id)
            }
            AdtId::EnumId(it) => {
                self.u8(2);
                let EnumLoc { container, id } = *it.lookup(db);
                self.module(container)?;
                self.ast_id(id)
            }
        }
    }

    fn expander(&mut self, it: MacroExpander) {
        match it {
            MacroExpander::Declarative { styles } => {
                self.u8(0);
                self.u8(styles.bits());
            }
            MacroExpander::BuiltIn(it) => {
                self.u8(1);
                self.u32(position(BuiltinFnLikeExpander::ALL, it));
            }
            MacroExpander::BuiltInAttr(it) => {
                self.u8(2);
                self.u32(position(BuiltinAttrExpander::ALL, it));
            }
            MacroExpander::BuiltInDerive(it) => {
                self.u8(3);
                self.u32(position(BuiltinDeriveExpander::ALL, it));
            }
            MacroExpander::BuiltInEager(it) => {
                self.u8(4);
                self.u32(position(EagerExpander::ALL, it));
            }
            MacroExpander::UnimplementedBuiltIn => self.u8(5),
        }
    }

    fn macro_id(&mut self, it: MacroId) -> R {
        let db = self.db;
        match it {
            MacroId::Macro2Id(it) => {
                self.u8(0);
                let Macro2Loc { container, id, expander, allow_internal_unsafe, edition } =
                    *it.lookup(db);
                self.module(container)?;
                self.ast_id(id)?;
                self.expander(expander);
                self.bool(allow_internal_unsafe);
                self.edition(edition);
                Ok(())
            }
            MacroId::MacroRulesId(it) => {
                self.u8(1);
                let MacroRulesLoc { container, id, expander, flags, edition } = *it.lookup(db);
                self.module(container)?;
                self.ast_id(id)?;
                self.expander(expander);
                self.u8(flags.bits());
                self.edition(edition);
                Ok(())
            }
            MacroId::ProcMacroId(it) => {
                self.u8(2);
                let ProcMacroLoc { container, id, expander, kind, edition } = *it.lookup(db);
                self.module(container)?;
                self.ast_id(id)?;
                self.proc_macro(expander, kind)?;
                self.edition(edition);
                Ok(())
            }
        }
    }

    pub fn def(&mut self, it: ModuleDefId) -> R {
        let db = self.db;
        match it {
            ModuleDefId::ModuleId(it) => {
                self.u8(0);
                self.module(it)
            }
            ModuleDefId::FunctionId(it) => {
                self.u8(1);
                let FunctionLoc { container, id } = *it.lookup(db);
                self.container(container)?;
                self.ast_id(id)
            }
            ModuleDefId::AdtId(it) => {
                self.u8(2);
                self.adt(it)
            }
            ModuleDefId::EnumVariantId(it) => {
                self.u8(3);
                self.variant(it)
            }
            ModuleDefId::ConstId(it) => {
                self.u8(4);
                let ConstLoc { container, id } = *it.lookup(db);
                self.container(container)?;
                self.ast_id(id)
            }
            ModuleDefId::StaticId(it) => {
                self.u8(5);
                let StaticLoc { container, id } = *it.lookup(db);
                self.container(container)?;
                self.ast_id(id)
            }
            ModuleDefId::TraitId(it) => {
                self.u8(6);
                let TraitLoc { container, id } = *it.lookup(db);
                self.module(container)?;
                self.ast_id(id)
            }
            ModuleDefId::TypeAliasId(it) => {
                self.u8(7);
                let TypeAliasLoc { container, id } = *it.lookup(db);
                self.container(container)?;
                self.ast_id(id)
            }
            ModuleDefId::BuiltinType(it) => {
                self.u8(8);
                let all = BuiltinType::all_builtin_types();
                self.u32(all.iter().position(|(_, x)| *x == it).unwrap() as u32);
                Ok(())
            }
            ModuleDefId::MacroId(it) => {
                self.u8(9);
                self.macro_id(it)
            }
        }
    }

    pub fn variant(&mut self, it: EnumVariantId) -> R {
        let EnumVariantLoc { id, parent, name } = it.lookup(self.db).clone();
        self.ast_id(id)?;
        self.adt(AdtId::EnumId(parent))?;
        self.name(&name)
    }

    fn use_id(&mut self, it: UseId) -> R {
        let UseLoc { container, id } = *it.lookup(self.db);
        self.module(container)?;
        self.ast_id(id)
    }

    fn extern_crate(&mut self, it: ExternCrateId) -> R {
        let ExternCrateLoc { container, id } = *it.lookup(self.db);
        self.module(container)?;
        self.ast_id(id)
    }

    pub fn impl_id(&mut self, it: ImplId) -> R {
        let ImplLoc { container, id } = *it.lookup(self.db);
        self.module(container)?;
        self.ast_id(id)
    }

    fn import(&mut self, use_: UseId, idx: Idx<ast::UseTree>) -> R {
        self.use_id(use_)?;
        self.u32(idx.into_raw().into_u32());
        Ok(())
    }

    fn import_or_extern_crate(&mut self, it: ImportOrExternCrate) -> R {
        match it {
            ImportOrExternCrate::Glob(GlobId { use_, idx }) => {
                self.u8(0);
                self.import(use_, idx)
            }
            ImportOrExternCrate::Import(ImportId { use_, idx }) => {
                self.u8(1);
                self.import(use_, idx)
            }
            ImportOrExternCrate::ExternCrate(it) => {
                self.u8(2);
                self.extern_crate(it)
            }
        }
    }

    fn import_or_glob(&mut self, it: ImportOrGlob) -> R {
        match it {
            ImportOrGlob::Glob(GlobId { use_, idx }) => {
                self.u8(0);
                self.import(use_, idx)
            }
            ImportOrGlob::Import(ImportId { use_, idx }) => {
                self.u8(1);
                self.import(use_, idx)
            }
        }
    }

    fn import_or_def(&mut self, it: ImportOrDef) -> R {
        match it {
            ImportOrDef::Import(ImportId { use_, idx }) => {
                self.u8(0);
                self.import(use_, idx)
            }
            ImportOrDef::Glob(GlobId { use_, idx }) => {
                self.u8(1);
                self.import(use_, idx)
            }
            ImportOrDef::ExternCrate(it) => {
                self.u8(2);
                self.extern_crate(it)
            }
            ImportOrDef::Def(it) => {
                self.u8(3);
                self.def(it)
            }
        }
    }

    pub fn option<T>(&mut self, it: Option<T>, f: impl FnOnce(&mut Self, T) -> R) -> R {
        match it {
            None => {
                self.u8(0);
                Ok(())
            }
            Some(it) => {
                self.u8(1);
                f(self, it)
            }
        }
    }

    pub fn builtin_derive_impl(&mut self, it: BuiltinDeriveImplId) -> R {
        let BuiltinDeriveImplLoc { adt, trait_, derive_attr_id, derive_index } = *it.loc(self.db);
        self.adt(adt)?;
        self.u32(BuiltinDeriveImplTrait::ALL.iter().position(|x| *x == trait_).unwrap() as u32);
        self.u32(derive_attr_id.item_tree_index());
        self.u32(derive_index);
        Ok(())
    }

    fn scope(&mut self, it: &ItemScope) -> R {
        let ItemScope {
            types,
            values,
            macros,
            unresolved,
            declarations,
            impls,
            builtin_derive_impls,
            extern_blocks,
            unnamed_consts,
            unnamed_trait_imports,
            use_imports_types,
            use_imports_values,
            use_imports_macros,
            use_decls,
            extern_crate_decls,
            legacy_macros,
            attr_macros,
            macro_invocations,
            derive_macros,
        } = it;
        self.usize(types.len())?;
        for (name, Item { def, vis, import }) in types {
            self.name(name)?;
            self.def(*def)?;
            self.visibility(*vis)?;
            self.option(*import, Self::import_or_extern_crate)?;
        }
        self.usize(values.len())?;
        for (name, Item { def, vis, import }) in values {
            self.name(name)?;
            self.def(*def)?;
            self.visibility(*vis)?;
            self.option(*import, Self::import_or_glob)?;
        }
        self.usize(macros.len())?;
        for (name, Item { def, vis, import }) in macros {
            self.name(name)?;
            self.macro_id(*def)?;
            self.visibility(*vis)?;
            self.option(*import, Self::import_or_extern_crate)?;
        }
        let mut names: Vec<&str> = unresolved.iter().map(|it| it.symbol().as_str()).collect();
        names.sort_unstable();
        self.usize(names.len())?;
        for name in names {
            self.str(name)?;
        }
        self.usize(declarations.len())?;
        for &it in declarations {
            self.def(it)?;
        }
        self.usize(impls.len())?;
        for &(it, trait_impl) in impls {
            self.impl_id(it)?;
            self.bool(trait_impl);
        }
        self.usize(builtin_derive_impls.len())?;
        for &it in builtin_derive_impls {
            self.builtin_derive_impl(it)?;
        }
        self.usize(extern_blocks.len())?;
        for &it in extern_blocks {
            self.extern_block(it)?;
        }
        self.usize(unnamed_consts.len())?;
        for &it in unnamed_consts {
            self.def(ModuleDefId::ConstId(it))?;
        }
        self.usize(unnamed_trait_imports.len())?;
        for &(it, Item { def: (), vis, import }) in unnamed_trait_imports {
            self.def(ModuleDefId::TraitId(it))?;
            self.visibility(vis)?;
            self.option(import, |enc, ImportId { use_, idx }| enc.import(use_, idx))?;
        }
        let types =
            self.sorted(use_imports_types.iter(), |enc, k| enc.import_or_extern_crate(**k))?;
        self.usize(types.len())?;
        for (&k, &v) in types {
            self.import_or_extern_crate(k)?;
            self.import_or_def(v)?;
        }
        let values = self.sorted(use_imports_values.iter(), |enc, k| enc.import_or_glob(**k))?;
        self.usize(values.len())?;
        for (&k, &v) in values {
            self.import_or_glob(k)?;
            self.import_or_def(v)?;
        }
        let macros =
            self.sorted(use_imports_macros.iter(), |enc, k| enc.import_or_extern_crate(**k))?;
        self.usize(macros.len())?;
        for (&k, &v) in macros {
            self.import_or_extern_crate(k)?;
            self.import_or_def(v)?;
        }
        self.usize(use_decls.len())?;
        for &it in use_decls {
            self.use_id(it)?;
        }
        self.usize(extern_crate_decls.len())?;
        for &it in extern_crate_decls {
            self.extern_crate(it)?;
        }
        let mut legacy: Vec<_> = legacy_macros.iter().collect();
        legacy.sort_unstable_by(|a, b| a.0.symbol().as_str().cmp(b.0.symbol().as_str()));
        self.usize(legacy.len())?;
        for (name, ids) in legacy {
            self.name(name)?;
            self.usize(ids.len())?;
            for &it in ids {
                self.macro_id(it)?;
            }
        }
        let attr = self.sorted(attr_macros.iter(), |enc, k| enc.ast_id(**k))?;
        self.usize(attr.len())?;
        for (&k, &v) in attr {
            self.ast_id(k)?;
            self.call(v)?;
        }
        let calls = self.sorted(macro_invocations.iter(), |enc, k| enc.ast_id(**k))?;
        self.usize(calls.len())?;
        for (&k, &v) in calls {
            self.ast_id(k)?;
            self.call(v)?;
        }
        let derives = self.sorted(derive_macros.iter(), |enc, k| enc.ast_id(**k))?;
        self.usize(derives.len())?;
        for (&k, invocations) in derives {
            self.ast_id(k)?;
            self.usize(invocations.len())?;
            for DeriveMacroInvocation { attr_id, attr_call_id, derive_call_ids } in invocations {
                self.u32(attr_id.item_tree_index());
                self.call(*attr_call_id)?;
                self.usize(derive_call_ids.len())?;
                for &it in derive_call_ids {
                    match it {
                        None => self.u8(0),
                        Some(Either::Left(call)) => {
                            self.u8(1);
                            self.call(call)?;
                        }
                        Some(Either::Right(it)) => {
                            self.u8(2);
                            self.builtin_derive_impl(it)?;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn tree_id(&mut self, it: TreeId) -> R {
        if it.is_block() {
            return Err("an item tree of a block".to_owned());
        }
        self.hir_file(it.file_id())
    }

    fn origin(&mut self, it: ModuleOrigin) -> R {
        match it {
            ModuleOrigin::CrateRoot { definition } => {
                self.u8(0);
                self.file(definition)
            }
            ModuleOrigin::File { is_mod_rs, declaration, declaration_tree_id, definition } => {
                self.u8(1);
                self.bool(is_mod_rs);
                self.erased_ast_id(declaration.erase());
                self.tree_id(declaration_tree_id)?;
                self.file(definition)
            }
            ModuleOrigin::Inline { definition_tree_id, definition } => {
                self.u8(2);
                self.tree_id(definition_tree_id)?;
                self.erased_ast_id(definition.erase());
                Ok(())
            }
            ModuleOrigin::BlockExpr { .. } => Err("a block module".to_owned()),
        }
    }

    fn mod_path(&mut self, it: &ModPath) -> R {
        match it.kind {
            PathKind::Plain => self.u8(0),
            PathKind::Super(n) => {
                self.u8(1);
                self.u8(n);
            }
            PathKind::Crate => self.u8(2),
            PathKind::Abs => self.u8(3),
            PathKind::DollarCrate(krate) => {
                self.u8(4);
                self.krate(krate)?;
            }
        }
        self.usize(it.segments().len())?;
        for name in it.segments() {
            self.name(name)?;
        }
        Ok(())
    }

    fn cfg(&mut self, it: &CfgExpr) -> R {
        match it {
            CfgExpr::Invalid => self.u8(0),
            CfgExpr::Atom(CfgAtom::Flag(flag)) => {
                self.u8(1);
                self.symbol(flag)?;
            }
            CfgExpr::Atom(CfgAtom::KeyValue { key, value }) => {
                self.u8(2);
                self.symbol(key)?;
                self.symbol(value)?;
            }
            CfgExpr::All(all) | CfgExpr::Any(all) => {
                self.u8(if matches!(it, CfgExpr::All(_)) { 3 } else { 4 });
                self.usize(all.len())?;
                for it in all {
                    self.cfg(it)?;
                }
            }
            CfgExpr::Not(it) => {
                self.u8(5);
                self.cfg(it)?;
            }
        }
        Ok(())
    }

    fn diagnostic(&mut self, krate: Crate, it: &DefDiagnostic) -> R {
        let DefDiagnostic { in_module, kind } = it;
        self.module(*in_module)?;
        match kind {
            DefDiagnosticKind::UnresolvedModule { ast, candidates } => {
                self.u8(0);
                self.ast_id(*ast)?;
                self.usize(candidates.len())?;
                for it in candidates {
                    self.str(it)?;
                }
            }
            DefDiagnosticKind::UnresolvedExternCrate { ast } => {
                self.u8(1);
                self.ast_id(*ast)?;
            }
            DefDiagnosticKind::UnresolvedImport { id, index } => {
                self.u8(2);
                self.ast_id(*id)?;
                self.u32(index.into_raw().into_u32());
            }
            DefDiagnosticKind::UnconfiguredCode { ast_id, cfg, opts } => {
                if opts != krate.cfg_options(self.db) {
                    return Err("code a cfg other than its crate's left out".to_owned());
                }
                self.u8(3);
                self.hir_file(ast_id.file_id)?;
                self.erased_ast_id(ast_id.value);
                self.cfg(cfg)?;
            }
            DefDiagnosticKind::UnresolvedMacroCall { ast, path } => {
                self.u8(4);
                self.call_kind(ast.clone())?;
                self.mod_path(path)?;
            }
            DefDiagnosticKind::UnimplementedBuiltinMacro { ast } => {
                self.u8(5);
                self.ast_id(*ast)?;
            }
            DefDiagnosticKind::InvalidDeriveTarget { ast, id } => {
                self.u8(6);
                self.ast_id(*ast)?;
                self.u32(id.item_tree_index());
            }
            DefDiagnosticKind::MalformedDerive { ast, id } => {
                self.u8(7);
                self.ast_id(*ast)?;
                self.u32(id.item_tree_index());
            }
            DefDiagnosticKind::MacroDefError { ast, message } => {
                self.u8(8);
                self.ast_id(*ast)?;
                self.str(message)?;
            }
            DefDiagnosticKind::MacroError { ast, path, err } => {
                self.u8(9);
                self.ast_id(*ast)?;
                self.mod_path(path)?;
                match err {
                    ExpandErrorKind::ProcMacroAttrExpansionDisabled => self.u8(0),
                    ExpandErrorKind::MissingProcMacroExpander(krate) => {
                        self.u8(1);
                        self.krate(*krate)?;
                    }
                    ExpandErrorKind::MacroDisabled => self.u8(2),
                    ExpandErrorKind::MacroDefinition => self.u8(3),
                    ExpandErrorKind::RecursionOverflow => self.u8(4),
                    ExpandErrorKind::Other(message) => {
                        self.u8(5);
                        self.str(message)?;
                    }
                    ExpandErrorKind::BuiltinDerive(message) => {
                        self.u8(6);
                        self.str(message)?;
                    }
                    ExpandErrorKind::DefaultNoVariant => self.u8(7),
                    _ => return Err(format!("a macro error: {err:?}")),
                }
            }
        }
        Ok(())
    }

    fn def_map(&mut self, it: &DefMap) -> R {
        let DefMap {
            krate,
            block,
            root,
            modules: _,
            prelude,
            macro_use_prelude,
            derive_helpers_in_scope,
            diagnostics,
            data,
            session_macros,
        } = it;
        let db = self.db;
        if *session_macros {
            return Err("compilation-local macro state".to_owned());
        }
        if block.is_some() {
            return Err("a def map of a block".to_owned());
        }
        let DefMapCrateData {
            exported_derives,
            fn_proc_macro_mapping,
            fn_proc_macro_mapping_back,
            registered_tools,
            unstable_features,
            rustc_coherence_is_core,
            no_core,
            no_std,
            proc_macro,
            edition,
            recursion_limit,
        } = &**data;
        if fn_proc_macro_mapping.len() != fn_proc_macro_mapping_back.len() {
            return Err("procedural macros of more than one function".to_owned());
        }
        self.edition(*edition);
        self.bool(*proc_macro);
        self.bool(*rustc_coherence_is_core);
        self.bool(*no_core);
        self.bool(*no_std);
        self.option(*recursion_limit, |enc, it| {
            enc.u32(it);
            Ok(())
        })?;
        self.usize(registered_tools.len())?;
        for it in registered_tools {
            self.symbol(it)?;
        }
        let mut features: Vec<Symbol> = unstable_features.iter().collect();
        features.sort_unstable_by(|a, b| a.as_str().cmp(b.as_str()));
        self.usize(features.len())?;
        for it in &features {
            self.symbol(it)?;
        }

        // The modules first, so that everything after can name any of them.
        let table = it.table(db);
        if table.get_index(0).map(|it| *it.0) != Some(*root) {
            return Err("a def map whose root is not its first module".to_owned());
        }
        self.usize(table.len())?;
        let index: FxHashMap<ModuleId, u32> =
            table.keys().enumerate().map(|(i, &it)| (it, i as u32)).collect();
        for (id, data) in table.iter() {
            if id.krate(db) != *krate {
                return Err("a module of another crate in a def map".to_owned());
            }
            match data.parent {
                None => self.u8(0),
                Some(parent) => {
                    self.u8(1);
                    self.u32(*index.get(&parent).ok_or("a parent of another def map")?);
                }
            }
            let name = id.name(db);
            self.option(name.as_ref(), |enc, it| enc.name(it))?;
        }
        for (_, data) in table.iter() {
            let ModuleData { origin, visibility, parent: _, scope: _ } = data;
            self.origin(*origin)?;
            self.visibility(*visibility)?;
            self.scope(data.scope(db))?;
            let children = data.children(db);
            self.usize(children.len())?;
            for (name, &child) in children {
                self.name(name)?;
                self.u32(*index.get(&child).ok_or("a child of another def map")?);
            }
        }

        self.option(*prelude, |enc, (module, use_)| {
            enc.module(module)?;
            enc.option(use_, Self::use_id)
        })?;
        let mut macro_use: Vec<_> = macro_use_prelude.iter().collect();
        macro_use.sort_unstable_by(|a, b| a.0.symbol().as_str().cmp(b.0.symbol().as_str()));
        self.usize(macro_use.len())?;
        for (name, &(id, extern_crate)) in macro_use {
            self.name(name)?;
            self.macro_id(id)?;
            self.option(extern_crate, Self::extern_crate)?;
        }
        let helpers = self.sorted(derive_helpers_in_scope.iter(), |enc, k| enc.ast_id(**k))?;
        self.usize(helpers.len())?;
        for (&k, helpers) in helpers {
            self.ast_id(k)?;
            self.usize(helpers.len())?;
            for (name, id, call) in helpers {
                self.name(name)?;
                self.macro_id(*id)?;
                match *call {
                    Either::Left(call) => {
                        self.u8(0);
                        self.call(call)?;
                    }
                    Either::Right(it) => {
                        self.u8(1);
                        self.builtin_derive_impl(it)?;
                    }
                }
            }
        }
        let exported = self.sorted(exported_derives.iter(), |enc, k| enc.macro_id(**k))?;
        self.usize(exported.len())?;
        for (&k, names) in exported {
            self.macro_id(k)?;
            self.usize(names.len())?;
            for it in names {
                self.name(it)?;
            }
        }
        // Each function of a crate of procedural macros that is one, and the other way about.
        let functions =
            self.sorted(fn_proc_macro_mapping.iter(), |enc, k| enc.def((**k).into()))?;
        self.usize(functions.len())?;
        for (&function, &id) in functions {
            self.def(function.into())?;
            self.macro_id(id.into())?;
        }
        self.usize(diagnostics.len())?;
        for it in diagnostics {
            self.diagnostic(*krate, it)?;
        }
        Ok(())
    }

    fn local(&mut self, it: &LocalDefMap) -> R {
        let LocalDefMap { extern_prelude } = it;
        self.usize(extern_prelude.len())?;
        for (name, &(module, extern_crate)) in extern_prelude {
            self.name(name)?;
            self.module(module)?;
            self.option(extern_crate, Self::extern_crate)?;
        }
        Ok(())
    }
}

/// Reads what [`Enc`] wrote.
pub struct Dec<'a> {
    db: &'a dyn SourceDatabase,
    bytes: &'a [u8],
    at: usize,
    krate: Crate,
    closure: Vec<Crate>,
    /// The files of each crate of the closure by their path from its root, read when first
    /// needed.
    paths: FxHashMap<u32, (String, bool, base_db::SourceRootId)>,
    files: Vec<FileId>,
    calls: Vec<MacroCallId>,
    contexts: Vec<SyntaxContext>,
    /// The modules of the crate being read, in its `DefMap`'s order.
    own: Vec<ModuleId>,
    /// Reading the crate's own `DefMap`, whose modules are `own`.
    in_def_map: bool,
    modules: FxHashMap<u32, Vec<ModuleId>>,
}

impl<'a> Dec<'a> {
    fn new(db: &'a dyn SourceDatabase, krate: Crate, bytes: &'a [u8]) -> Self {
        Dec {
            db,
            bytes,
            at: 0,
            krate,
            closure: closure(db, krate),
            paths: FxHashMap::default(),
            files: Vec::new(),
            calls: Vec::new(),
            contexts: Vec::new(),
            own: Vec::new(),
            in_def_map: false,
            modules: FxHashMap::default(),
        }
    }

    /// Reads `bytes` [`Enc::finish`] gave of values of `krate`, from the files they name.
    pub fn new_checked(db: &'a dyn SourceDatabase, krate: Crate, bytes: &'a [u8]) -> R<Self> {
        let mut dec = Dec::new(db, krate, bytes);
        dec.files()?;
        Ok(dec)
    }

    /// A literal's text with each `OUT_DIR` [`Enc::unplaced`] took out as this database has it.
    fn placed(&self, text: Symbol) -> R<Symbol> {
        if !text.as_str().contains('\0') {
            return Ok(text);
        }
        let (mut placed, mut rest) = (String::new(), text.as_str());
        while let Some(at) = rest.find(OUT) {
            placed.push_str(&rest[..at]);
            let mark = rest[at + OUT.len()..].strip_prefix(' ').and_then(|it| it.split_once('\0'));
            let (index, after) = mark.ok_or("a literal with half a mark of an OUT_DIR")?;
            let krate = index.parse::<usize>().ok().and_then(|it| self.closure.get(it).copied());
            let krate = krate.ok_or("a literal with the OUT_DIR of no crate of the closure")?;
            placed.push_str(&out_dir(self.db, krate).ok_or("an OUT_DIR the crate lacks")?);
            rest = after;
        }
        placed.push_str(rest);
        Ok(Symbol::intern(&placed))
    }

    /// Whether the bytes were read to their end.
    pub fn finish(&self) -> R {
        match self.bytes.len() - self.at {
            0 => Ok(()),
            left => Err(format!("{left} bytes left over")),
        }
    }
}

fn decode(db: &dyn SourceDatabase, krate: Crate, bytes: &[u8]) -> R<(DefMap, LocalDefMap)> {
    let mut dec = Dec::new(db, krate, bytes);
    dec.files()?;
    dec.in_def_map = true;
    let mut def_map = dec.def_map()?;
    let local = dec.local()?;
    if dec.at != bytes.len() {
        return Err(format!("{} bytes left over", bytes.len() - dec.at));
    }
    def_map.hand_over_scopes(db);
    Ok((def_map, local))
}

impl Dec<'_> {
    pub fn u8(&mut self) -> R<u8> {
        let it = *self.bytes.get(self.at).ok_or("the bytes end early")?;
        self.at += 1;
        Ok(it)
    }

    pub fn u32(&mut self) -> R<u32> {
        let mut it = 0u32;
        let mut shift = 0;
        loop {
            let byte = self.u8()?;
            it |= u32::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok(it);
            }
            shift += 7;
            if shift > 28 {
                return Err("a number over u32".to_owned());
            }
        }
    }

    pub fn len(&mut self) -> R<usize> {
        Ok(self.u32()? as usize)
    }

    pub fn bool(&mut self) -> R<bool> {
        Ok(self.u8()? != 0)
    }

    pub fn str(&mut self) -> R<&str> {
        let len = self.len()?;
        let bytes = self.bytes.get(self.at..self.at + len).ok_or("a string past the end")?;
        self.at += len;
        std::str::from_utf8(bytes).map_err(|_| "a string that is not UTF-8".to_owned())
    }

    pub fn symbol(&mut self) -> R<Symbol> {
        Ok(Symbol::intern(self.str()?))
    }

    pub fn name(&mut self) -> R<Name> {
        Ok(Name::new_symbol_root(self.symbol()?))
    }

    fn edition(&mut self) -> R<Edition> {
        let it = self.u8()?;
        Edition::iter().find(|e| *e as u8 == it).ok_or_else(|| format!("edition {it}"))
    }

    pub fn krate(&mut self) -> R<Crate> {
        let index = self.len()?;
        self.closure.get(index).copied().ok_or_else(|| format!("crate {index}"))
    }

    pub fn index<T: Copy>(&mut self, all: &[T]) -> R<T> {
        let index = self.len()?;
        all.get(index).copied().ok_or_else(|| format!("variant {index}"))
    }

    /// The files the bytes name, as named in the header: the first that is not loaded, or
    /// that is outside its crate's directory and whose text is not what was written, is an
    /// error. Each that is not loaded is noted as an `include!` that found nothing is, for
    /// whoever owns the VFS to load.
    fn files(&mut self) -> R {
        let n = self.len()?;
        let mut not_loaded = None;
        for _ in 0..n {
            let owner = self.u32()?;
            let path = self.str()?.to_owned();
            let written = match self.bool()? {
                true => {
                    let bytes =
                        self.bytes.get(self.at..self.at + 16).ok_or("a hash past the end")?;
                    self.at += 16;
                    Some(u128::from_le_bytes(bytes.try_into().expect("16 bytes")))
                }
                false => None,
            };
            if !self.paths.contains_key(&owner) {
                let krate = *self.closure.get(owner as usize).ok_or("a file's crate")?;
                self.paths.insert(owner, root_dir(self.db, krate)?);
            }
            let (dir, real, source_root) = &self.paths[&owner];
            let krate = self.closure[owner as usize];
            let full = match path.strip_prefix(OUT) {
                Some(rest) => {
                    format!("{}{rest}", out_dir(self.db, krate).ok_or("a crate with no OUT_DIR")?)
                }
                None => joined(dir, &path),
            };
            let source_root = self.db.source_root(*source_root).source_root(self.db);
            let found = source_root.file_for_path(&vfs_path(*real, full.clone())).copied();
            let Some(found) = found else {
                let anchor = krate.data(self.db).root_file_id;
                let path = if path.starts_with(OUT) { &full } else { &path };
                self.db.include_source(base_db::AnchoredPath { anchor, path }, krate);
                not_loaded.get_or_insert_with(|| format!("{full} is not loaded"));
                continue;
            };
            if let Some(written) = written
                && hash128(self.db.file_text(found).text(self.db).as_bytes()) != written
            {
                return Err(format!("{full} is not the file that was written"));
            }
            self.files.push(found);
        }
        not_loaded.map_or(Ok(()), Err)
    }

    fn file(&mut self) -> R<base_db::EditionedFileId> {
        let edition = self.edition()?;
        let index = self.len()?;
        let file = *self.files.get(index).ok_or("a file not in the header")?;
        Ok(base_db::EditionedFileId::new(self.db, file, edition))
    }

    fn hir_file(&mut self) -> R<HirFileId> {
        match self.u8()? {
            0 => Ok(HirFileId::FileId(self.file()?)),
            1 => Ok(HirFileId::MacroFile(self.call()?)),
            it => Err(format!("hir file tag {it}")),
        }
    }

    fn ast_id<N: syntax::AstNode>(&mut self) -> R<AstId<N>> {
        let file = self.hir_file()?;
        let raw = ErasedFileAstId::from_raw(self.u32()?);
        Ok(AstId::new(file, FileAstId::from_erased_unchecked(raw)))
    }

    fn erased_ast_id(&mut self) -> R<ErasedFileAstId> {
        Ok(ErasedFileAstId::from_raw(self.u32()?))
    }

    fn call(&mut self) -> R<MacroCallId> {
        match self.u8()? {
            SEEN => {
                let index = self.len()?;
                self.calls.get(index).copied().ok_or_else(|| "a macro call not read".to_owned())
            }
            NEW => {
                let loc = self.call_loc()?;
                let include = includes(&loc);
                let call = MacroCallId::new(self.db, loc);
                if include {
                    let written = self.file()?;
                    if call.loc(self.db).include_file_id(self.db, call) != Some(written) {
                        return Err("an include! of another file than was written".to_owned());
                    }
                }
                self.calls.push(call);
                Ok(call)
            }
            it => Err(format!("macro call tag {it}")),
        }
    }

    fn call_loc(&mut self) -> R<MacroCallLoc> {
        let def = self.def_id()?;
        let krate = self.krate()?;
        let kind = self.call_kind()?;
        let ctxt = self.context()?;
        let macro_depth = self.u32()?;
        Ok(MacroCallLoc { def, krate, kind, ctxt, macro_depth })
    }

    fn call_kind(&mut self) -> R<MacroCallKind> {
        Ok(match self.u8()? {
            0 => {
                let ast_id = self.ast_id()?;
                let expand_to = match self.u8()? {
                    0 => ExpandTo::Statements,
                    1 => ExpandTo::Items,
                    2 => ExpandTo::Pattern,
                    3 => ExpandTo::Type,
                    4 => ExpandTo::Expr,
                    it => return Err(format!("expand-to {it}")),
                };
                let eager = match self.u8()? {
                    0 => None,
                    _ => {
                        let arg = self.subtree()?;
                        let arg_id = self.call()?;
                        let span = self.span()?;
                        Some(Box::new(EagerCallInfo { arg, arg_id, error: None, span }))
                    }
                };
                MacroCallKind::FnLike { ast_id, expand_to, eager }
            }
            1 => MacroCallKind::Derive {
                ast_id: self.ast_id()?,
                derive_attr_index: AttrId::from_item_tree_index(self.u32()?),
                derive_index: self.u32()?,
                derive_macro_id: self.call()?,
            },
            2 => {
                let ast_id = self.ast_id()?;
                let attr_args = match self.u8()? {
                    0 => None,
                    _ => Some(Box::new(self.subtree()?)),
                };
                let n = self.len()?;
                let mut ids = Vec::with_capacity(n);
                for _ in 0..n {
                    ids.push(AttrId::from_item_tree_index(self.u32()?));
                }
                let censored_attr_ids = hir_expand::AttrMacroAttrIds::from_many(&ids);
                MacroCallKind::Attr { ast_id, attr_args, censored_attr_ids }
            }
            it => return Err(format!("macro call kind {it}")),
        })
    }

    fn def_id(&mut self) -> R<MacroDefId> {
        let krate = self.krate()?;
        let edition = self.edition()?;
        let local_inner = self.bool()?;
        let allow_internal_unsafe = self.bool()?;
        let edition_panic = self.bool()?;
        let kind = match self.u8()? {
            0 => {
                let ast_id = self.ast_id()?;
                MacroDefKind::Declarative(ast_id, MacroCallStyles::from_bits_retain(self.u8()?))
            }
            1 => MacroDefKind::BuiltIn(self.ast_id()?, self.index(BuiltinFnLikeExpander::ALL)?),
            2 => MacroDefKind::BuiltInAttr(self.ast_id()?, self.index(BuiltinAttrExpander::ALL)?),
            3 => {
                MacroDefKind::BuiltInDerive(self.ast_id()?, self.index(BuiltinDeriveExpander::ALL)?)
            }
            4 => MacroDefKind::BuiltInEager(self.ast_id()?, self.index(EagerExpander::ALL)?),
            5 => MacroDefKind::UnimplementedBuiltIn(self.ast_id()?),
            6 => {
                let ast_id = self.ast_id()?;
                let (expander, kind) = self.proc_macro()?;
                MacroDefKind::ProcMacro(ast_id, expander, kind)
            }
            it => return Err(format!("macro def kind {it}")),
        };
        Ok(MacroDefId { krate, edition, kind, local_inner, allow_internal_unsafe, edition_panic })
    }

    fn context(&mut self) -> R<SyntaxContext> {
        match self.u8()? {
            0 => Ok(SyntaxContext::root(self.edition()?)),
            1 => {
                let outer = self.call()?;
                let transparency = match self.u8()? {
                    0 => Transparency::Transparent,
                    1 => Transparency::SemiOpaque,
                    2 => Transparency::Opaque,
                    it => return Err(format!("transparency {it}")),
                };
                let edition = self.edition()?;
                let parent = self.context()?;
                let mut other = [None, None];
                for it in &mut other {
                    if self.u8()? != 0 {
                        *it = Some(self.context()?);
                    }
                }
                let ctx = SyntaxContext::new(
                    self.db,
                    outer.into(),
                    transparency,
                    edition,
                    parent,
                    other[0],
                    other[1],
                );
                self.contexts.push(ctx);
                Ok(ctx)
            }
            2 => {
                let index = self.len()?;
                self.contexts.get(index).copied().ok_or_else(|| "a context not read".to_owned())
            }
            it => Err(format!("context tag {it}")),
        }
    }

    fn span(&mut self) -> R<Span> {
        let start = TextSize::from(self.u32()?);
        let len = TextSize::from(self.u32()?);
        let file_id = self.file()?.span_file_id(self.db);
        let ast_id = self.erased_ast_id()?;
        let ctx = self.context()?;
        Ok(Span { range: TextRange::at(start, len), anchor: SpanAnchor { file_id, ast_id }, ctx })
    }

    fn subtree(&mut self) -> R<tt::TopSubtree> {
        let n = self.len()?;
        let mut tokens = Vec::with_capacity(n);
        for _ in 0..n {
            let token = match self.u8()? {
                0 => {
                    let kind = match self.u8()? {
                        0 => DelimiterKind::Parenthesis,
                        1 => DelimiterKind::Brace,
                        2 => DelimiterKind::Bracket,
                        3 => DelimiterKind::Invisible,
                        it => return Err(format!("delimiter {it}")),
                    };
                    let len = self.u32()?;
                    let open = self.span()?;
                    let close = self.span()?;
                    TokenTree::Subtree(Subtree { delimiter: Delimiter { open, close, kind }, len })
                }
                1 => {
                    let text_and_suffix = self.symbol()?;
                    let text_and_suffix = self.placed(text_and_suffix)?;
                    let kind = self.u8()?;
                    let hashes = self.u8()?;
                    let kind = match kind {
                        0 => LitKind::Byte,
                        1 => LitKind::Char,
                        2 => LitKind::Integer,
                        3 => LitKind::Float,
                        4 => LitKind::Str,
                        5 => LitKind::StrRaw(hashes),
                        6 => LitKind::ByteStr,
                        7 => LitKind::ByteStrRaw(hashes),
                        8 => LitKind::CStr,
                        9 => LitKind::CStrRaw(hashes),
                        10 => LitKind::Err(()),
                        it => return Err(format!("literal kind {it}")),
                    };
                    let suffix_len = self.u8()?;
                    let span = self.span()?;
                    TokenTree::Leaf(Leaf::Literal(tt::Literal {
                        text_and_suffix,
                        span,
                        kind,
                        suffix_len,
                    }))
                }
                2 => {
                    let char = char::from_u32(self.u32()?).ok_or("a punct that is no char")?;
                    let spacing = match self.u8()? {
                        0 => Spacing::Alone,
                        1 => Spacing::Joint,
                        2 => Spacing::JointHidden,
                        it => return Err(format!("spacing {it}")),
                    };
                    let span = self.span()?;
                    TokenTree::Leaf(Leaf::Punct(tt::Punct { char, spacing, span }))
                }
                3 => {
                    let sym = self.symbol()?;
                    let is_raw = if self.bool()? { IdentIsRaw::Yes } else { IdentIsRaw::No };
                    let span = self.span()?;
                    TokenTree::Leaf(Leaf::Ident(tt::Ident { sym, span, is_raw }))
                }
                it => return Err(format!("token tag {it}")),
            };
            tokens.push(token);
        }
        if !matches!(tokens.first(), Some(TokenTree::Subtree(_))) {
            return Err("a token tree with no top subtree".to_owned());
        }
        Ok(tt::TopSubtree::from_serialized(tokens))
    }

    pub fn module(&mut self) -> R<ModuleId> {
        let db = self.db;
        if self.u8()? == 1 {
            let owner = self.body_owner()?;
            let block: AstId<ast::BlockExpr> = self.ast_id()?;
            let index = self.len()?;
            let (_, def_map) = Body::of(db, owner)
                .blocks(db)
                .find(|(it, _)| it.ast_id(db) == block)
                .ok_or("a block not in its body")?;
            let module = def_map.table(db).keys().nth(index).copied();
            return module.ok_or_else(|| format!("module {index} of a block"));
        }
        let owner = self.u32()?;
        let index = self.len()?;
        if owner == 0 && self.in_def_map {
            return self.own.get(index).copied().ok_or_else(|| format!("own module {index}"));
        }
        if !self.modules.contains_key(&owner) {
            let krate = *self.closure.get(owner as usize).ok_or("a module's crate")?;
            let table = super::crate_def_map(self.db, krate).table(self.db);
            self.modules.insert(owner, table.keys().copied().collect());
        }
        self.modules[&owner].get(index).copied().ok_or_else(|| format!("module {index}"))
    }

    pub fn lang_item_target(&mut self) -> R<LangItemTarget> {
        Ok(match self.u8()? {
            0 => match self.adt()? {
                AdtId::EnumId(it) => it.into(),
                AdtId::StructId(it) => it.into(),
                AdtId::UnionId(it) => it.into(),
            },
            1 => match self.def()? {
                ModuleDefId::FunctionId(it) => it.into(),
                ModuleDefId::StaticId(it) => it.into(),
                ModuleDefId::TypeAliasId(it) => it.into(),
                ModuleDefId::TraitId(it) => it.into(),
                ModuleDefId::ConstId(it) => it.into(),
                _ => return Err("a lang item that is no item".to_owned()),
            },
            2 => self.variant()?.into(),
            3 => self.impl_id()?.into(),
            4 => self.macro_id()?.into(),
            it => return Err(format!("lang item {it}")),
        })
    }

    pub fn body_owner(&mut self) -> R<DefWithBodyId> {
        Ok(match self.u8()? {
            0 => match self.def()? {
                ModuleDefId::FunctionId(it) => it.into(),
                ModuleDefId::StaticId(it) => it.into(),
                ModuleDefId::ConstId(it) => it.into(),
                _ => return Err("a body of an item with none".to_owned()),
            },
            1 => DefWithBodyId::VariantId(self.variant()?),
            it => return Err(format!("body {it}")),
        })
    }

    fn visibility(&mut self) -> R<Visibility> {
        Ok(match self.u8()? {
            0 => Visibility::Module(self.module()?, VisibilityExplicitness::Explicit),
            1 => Visibility::Module(self.module()?, VisibilityExplicitness::Implicit),
            2 => Visibility::PubCrate(self.krate()?),
            3 => Visibility::Public,
            it => return Err(format!("visibility {it}")),
        })
    }

    fn container(&mut self) -> R<ItemContainerId> {
        let db = self.db;
        Ok(match self.u8()? {
            0 => ItemContainerId::ModuleId(self.module()?),
            1 => ItemContainerId::ImplId(self.impl_id()?),
            2 => ItemContainerId::TraitId(
                TraitLoc { container: self.module()?, id: self.ast_id()? }.intern(db),
            ),
            3 => ItemContainerId::ExternBlockId(self.extern_block()?),
            it => return Err(format!("container {it}")),
        })
    }

    fn extern_block(&mut self) -> R<ExternBlockId> {
        Ok(ExternBlockLoc { container: self.module()?, id: self.ast_id()? }.intern(self.db))
    }

    pub fn impl_id(&mut self) -> R<ImplId> {
        Ok(ImplLoc { container: self.module()?, id: self.ast_id()? }.intern(self.db))
    }

    pub fn adt(&mut self) -> R<AdtId> {
        let db = self.db;
        Ok(match self.u8()? {
            0 => AdtId::StructId(
                StructLoc { container: self.module()?, id: self.ast_id()? }.intern(db),
            ),
            1 => AdtId::UnionId(
                UnionLoc { container: self.module()?, id: self.ast_id()? }.intern(db),
            ),
            2 => {
                AdtId::EnumId(EnumLoc { container: self.module()?, id: self.ast_id()? }.intern(db))
            }
            it => return Err(format!("adt {it}")),
        })
    }

    fn expander(&mut self) -> R<MacroExpander> {
        Ok(match self.u8()? {
            0 => {
                MacroExpander::Declarative { styles: MacroCallStyles::from_bits_retain(self.u8()?) }
            }
            1 => MacroExpander::BuiltIn(self.index(BuiltinFnLikeExpander::ALL)?),
            2 => MacroExpander::BuiltInAttr(self.index(BuiltinAttrExpander::ALL)?),
            3 => MacroExpander::BuiltInDerive(self.index(BuiltinDeriveExpander::ALL)?),
            4 => MacroExpander::BuiltInEager(self.index(EagerExpander::ALL)?),
            5 => MacroExpander::UnimplementedBuiltIn,
            it => return Err(format!("expander {it}")),
        })
    }

    fn macro_id(&mut self) -> R<MacroId> {
        let db = self.db;
        Ok(match self.u8()? {
            0 => {
                let container = self.module()?;
                let id = self.ast_id()?;
                let expander = self.expander()?;
                let allow_internal_unsafe = self.bool()?;
                let edition = self.edition()?;
                MacroId::Macro2Id(
                    Macro2Loc { container, id, expander, allow_internal_unsafe, edition }
                        .intern(db),
                )
            }
            1 => {
                let container = self.module()?;
                let id = self.ast_id()?;
                let expander = self.expander()?;
                let flags = MacroRulesLocFlags::from_bits_retain(self.u8()?);
                let edition = self.edition()?;
                MacroId::MacroRulesId(
                    MacroRulesLoc { container, id, expander, flags, edition }.intern(db),
                )
            }
            2 => {
                let container = self.module()?;
                let id = self.ast_id()?;
                let (expander, kind) = self.proc_macro()?;
                let edition = self.edition()?;
                MacroId::ProcMacroId(
                    ProcMacroLoc { container, id, expander, kind, edition }.intern(db),
                )
            }
            it => return Err(format!("macro {it}")),
        })
    }

    fn proc_macro(&mut self) -> R<(CustomProcMacroExpander, ProcMacroKind)> {
        let listed = self.u32()?;
        if listed >= !2 {
            return Err(format!("procedural macro {listed}"));
        }
        let kind = match self.u8()? {
            0 => ProcMacroKind::CustomDerive,
            1 => ProcMacroKind::Bang,
            2 => ProcMacroKind::Attr,
            it => return Err(format!("procedural macro kind {it}")),
        };
        Ok((CustomProcMacroExpander::new(listed), kind))
    }

    pub fn def(&mut self) -> R<ModuleDefId> {
        let db = self.db;
        Ok(match self.u8()? {
            0 => ModuleDefId::ModuleId(self.module()?),
            1 => ModuleDefId::FunctionId(
                FunctionLoc { container: self.container()?, id: self.ast_id()? }.intern(db),
            ),
            2 => ModuleDefId::AdtId(self.adt()?),
            3 => ModuleDefId::EnumVariantId(self.variant()?),
            4 => ModuleDefId::ConstId(
                ConstLoc { container: self.container()?, id: self.ast_id()? }.intern(db),
            ),
            5 => ModuleDefId::StaticId(
                StaticLoc { container: self.container()?, id: self.ast_id()? }.intern(db),
            ),
            6 => ModuleDefId::TraitId(
                TraitLoc { container: self.module()?, id: self.ast_id()? }.intern(db),
            ),
            7 => ModuleDefId::TypeAliasId(
                TypeAliasLoc { container: self.container()?, id: self.ast_id()? }.intern(db),
            ),
            8 => {
                let index = self.len()?;
                let all = BuiltinType::all_builtin_types();
                ModuleDefId::BuiltinType(all.get(index).ok_or("a builtin type")?.1)
            }
            9 => ModuleDefId::MacroId(self.macro_id()?),
            it => return Err(format!("def {it}")),
        })
    }

    pub fn variant(&mut self) -> R<EnumVariantId> {
        let id = self.ast_id()?;
        let AdtId::EnumId(parent) = self.adt()? else {
            return Err("a variant of no enum".to_owned());
        };
        let name = self.name()?;
        Ok(EnumVariantLoc { id, parent, name }.intern(self.db))
    }

    fn use_id(&mut self) -> R<UseId> {
        Ok(UseLoc { container: self.module()?, id: self.ast_id()? }.intern(self.db))
    }

    fn extern_crate(&mut self) -> R<ExternCrateId> {
        Ok(ExternCrateLoc { container: self.module()?, id: self.ast_id()? }.intern(self.db))
    }

    fn import(&mut self) -> R<(UseId, Idx<ast::UseTree>)> {
        let use_ = self.use_id()?;
        Ok((use_, Idx::from_raw(RawIdx::from_u32(self.u32()?))))
    }

    fn import_or_extern_crate(&mut self) -> R<ImportOrExternCrate> {
        Ok(match self.u8()? {
            0 => {
                let (use_, idx) = self.import()?;
                ImportOrExternCrate::Glob(GlobId { use_, idx })
            }
            1 => {
                let (use_, idx) = self.import()?;
                ImportOrExternCrate::Import(ImportId { use_, idx })
            }
            2 => ImportOrExternCrate::ExternCrate(self.extern_crate()?),
            it => return Err(format!("import {it}")),
        })
    }

    fn import_or_glob(&mut self) -> R<ImportOrGlob> {
        let tag = self.u8()?;
        let (use_, idx) = self.import()?;
        Ok(match tag {
            0 => ImportOrGlob::Glob(GlobId { use_, idx }),
            1 => ImportOrGlob::Import(ImportId { use_, idx }),
            it => return Err(format!("import {it}")),
        })
    }

    fn import_or_def(&mut self) -> R<ImportOrDef> {
        Ok(match self.u8()? {
            0 => {
                let (use_, idx) = self.import()?;
                ImportOrDef::Import(ImportId { use_, idx })
            }
            1 => {
                let (use_, idx) = self.import()?;
                ImportOrDef::Glob(GlobId { use_, idx })
            }
            2 => ImportOrDef::ExternCrate(self.extern_crate()?),
            3 => ImportOrDef::Def(self.def()?),
            it => return Err(format!("import {it}")),
        })
    }

    pub fn option<T>(&mut self, f: impl FnOnce(&mut Self) -> R<T>) -> R<Option<T>> {
        match self.u8()? {
            0 => Ok(None),
            _ => f(self).map(Some),
        }
    }

    pub fn builtin_derive_impl(&mut self) -> R<BuiltinDeriveImplId> {
        let adt = self.adt()?;
        let trait_ = self.index(BuiltinDeriveImplTrait::ALL)?;
        let derive_attr_id = AttrId::from_item_tree_index(self.u32()?);
        let derive_index = self.u32()?;
        Ok(BuiltinDeriveImplId::new(
            self.db,
            BuiltinDeriveImplLoc { adt, trait_, derive_attr_id, derive_index },
        ))
    }

    pub fn many<T>(&mut self, mut f: impl FnMut(&mut Self) -> R<T>) -> R<Vec<T>> {
        let n = self.len()?;
        let mut all = Vec::with_capacity(n);
        for _ in 0..n {
            all.push(f(self)?);
        }
        Ok(all)
    }

    fn scope(&mut self) -> R<ItemScope> {
        let types = self.many(|dec| {
            let name = dec.name()?;
            let def = dec.def()?;
            let vis = dec.visibility()?;
            let import = dec.option(Self::import_or_extern_crate)?;
            Ok((name, Item { def, vis, import }))
        })?;
        let values = self.many(|dec| {
            let name = dec.name()?;
            let def = dec.def()?;
            let vis = dec.visibility()?;
            let import = dec.option(Self::import_or_glob)?;
            Ok((name, Item { def, vis, import }))
        })?;
        let macros = self.many(|dec| {
            let name = dec.name()?;
            let def = dec.macro_id()?;
            let vis = dec.visibility()?;
            let import = dec.option(Self::import_or_extern_crate)?;
            Ok((name, Item { def, vis, import }))
        })?;
        let unresolved = self.many(Self::name)?;
        let declarations = self.many(Self::def)?;
        let impls = self.many(|dec| Ok((dec.impl_id()?, dec.bool()?)))?;
        let builtin_derive_impls = self.many(Self::builtin_derive_impl)?;
        let extern_blocks = self.many(Self::extern_block)?;
        let unnamed_consts = self.many(|dec| match dec.def()? {
            ModuleDefId::ConstId(it) => Ok(it),
            _ => Err("an unnamed const that is not one".to_owned()),
        })?;
        let unnamed_trait_imports = self.many(|dec| {
            let ModuleDefId::TraitId(it) = dec.def()? else {
                return Err("a trait import of no trait".to_owned());
            };
            let vis = dec.visibility()?;
            let import = dec.option(|dec| {
                let (use_, idx) = dec.import()?;
                Ok(ImportId { use_, idx })
            })?;
            Ok((it, Item { def: (), vis, import }))
        })?;
        let use_imports_types =
            self.many(|dec| Ok((dec.import_or_extern_crate()?, dec.import_or_def()?)))?;
        let use_imports_values =
            self.many(|dec| Ok((dec.import_or_glob()?, dec.import_or_def()?)))?;
        let use_imports_macros =
            self.many(|dec| Ok((dec.import_or_extern_crate()?, dec.import_or_def()?)))?;
        let use_decls = self.many(Self::use_id)?;
        let extern_crate_decls = self.many(Self::extern_crate)?;
        let legacy_macros = self.many(|dec| {
            let name = dec.name()?;
            let ids = dec.many(Self::macro_id)?;
            Ok((name, SmallVec::from_vec(ids)))
        })?;
        let attr_macros = self.many(|dec| Ok((dec.ast_id()?, dec.call()?)))?;
        let macro_invocations = self.many(|dec| Ok((dec.ast_id()?, dec.call()?)))?;
        let derive_macros = self.many(|dec| {
            let ast_id = dec.ast_id()?;
            let invocations = dec.many(|dec| {
                let attr_id = AttrId::from_item_tree_index(dec.u32()?);
                let attr_call_id = dec.call()?;
                let derive_call_ids = dec.many(|dec| {
                    Ok(match dec.u8()? {
                        0 => None,
                        1 => Some(Either::Left(dec.call()?)),
                        2 => Some(Either::Right(dec.builtin_derive_impl()?)),
                        it => return Err(format!("derive call {it}")),
                    })
                })?;
                Ok(DeriveMacroInvocation {
                    attr_id,
                    attr_call_id,
                    derive_call_ids: SmallVec::from_vec(derive_call_ids),
                })
            })?;
            Ok((ast_id, SmallVec::from_vec(invocations)))
        })?;
        Ok(ItemScope {
            types: types.into_iter().collect(),
            values: values.into_iter().collect(),
            macros: macros.into_iter().collect(),
            unresolved: unresolved.into_iter().collect(),
            declarations: declarations.into_iter().collect(),
            impls: impls.into_iter().collect(),
            builtin_derive_impls: builtin_derive_impls.into_iter().collect(),
            extern_blocks: extern_blocks.into_iter().collect(),
            unnamed_consts: unnamed_consts.into_iter().collect(),
            unnamed_trait_imports: unnamed_trait_imports.into_iter().collect(),
            use_imports_types: use_imports_types.into_iter().collect(),
            use_imports_values: use_imports_values.into_iter().collect(),
            use_imports_macros: use_imports_macros.into_iter().collect(),
            use_decls: use_decls.into_iter().collect(),
            extern_crate_decls: extern_crate_decls.into_iter().collect(),
            legacy_macros: legacy_macros.into_iter().collect(),
            attr_macros: attr_macros.into_iter().collect(),
            macro_invocations: macro_invocations.into_iter().collect(),
            derive_macros: derive_macros.into_iter().collect(),
        })
    }

    fn tree_id(&mut self) -> R<TreeId> {
        Ok(TreeId::new(self.hir_file()?, None))
    }

    fn origin(&mut self) -> R<ModuleOrigin> {
        Ok(match self.u8()? {
            0 => ModuleOrigin::CrateRoot { definition: self.file()? },
            1 => {
                let is_mod_rs = self.bool()?;
                let declaration = FileAstId::from_erased_unchecked(self.erased_ast_id()?);
                let declaration_tree_id = self.tree_id()?;
                let definition = self.file()?;
                ModuleOrigin::File { is_mod_rs, declaration, declaration_tree_id, definition }
            }
            2 => {
                let definition_tree_id = self.tree_id()?;
                let definition = FileAstId::from_erased_unchecked(self.erased_ast_id()?);
                ModuleOrigin::Inline { definition_tree_id, definition }
            }
            it => return Err(format!("origin {it}")),
        })
    }

    fn mod_path(&mut self) -> R<ModPath> {
        let kind = match self.u8()? {
            0 => PathKind::Plain,
            1 => PathKind::Super(self.u8()?),
            2 => PathKind::Crate,
            3 => PathKind::Abs,
            4 => PathKind::DollarCrate(self.krate()?),
            it => return Err(format!("path kind {it}")),
        };
        let segments = self.many(Self::name)?;
        Ok(ModPath::from_segments(kind, segments))
    }

    fn cfg(&mut self) -> R<CfgExpr> {
        Ok(match self.u8()? {
            0 => CfgExpr::Invalid,
            1 => CfgExpr::Atom(CfgAtom::Flag(self.symbol()?)),
            2 => {
                let key = self.symbol()?;
                let value = self.symbol()?;
                CfgExpr::Atom(CfgAtom::KeyValue { key, value })
            }
            3 => CfgExpr::All(self.many(Self::cfg)?.into_boxed_slice()),
            4 => CfgExpr::Any(self.many(Self::cfg)?.into_boxed_slice()),
            5 => CfgExpr::Not(Box::new(self.cfg()?)),
            it => return Err(format!("cfg {it}")),
        })
    }

    fn diagnostic(&mut self) -> R<DefDiagnostic> {
        let in_module = self.module()?;
        let kind = match self.u8()? {
            0 => {
                let ast = self.ast_id()?;
                let candidates = self.many(|dec| Ok(dec.str()?.to_owned()))?;
                DefDiagnosticKind::UnresolvedModule { ast, candidates: candidates.into() }
            }
            1 => DefDiagnosticKind::UnresolvedExternCrate { ast: self.ast_id()? },
            2 => {
                let id = self.ast_id()?;
                let index = Idx::from_raw(RawIdx::from_u32(self.u32()?));
                DefDiagnosticKind::UnresolvedImport { id, index }
            }
            3 => {
                let file = self.hir_file()?;
                let ast_id = hir_expand::InFile::new(file, self.erased_ast_id()?);
                let cfg = self.cfg()?;
                let opts = self.krate.cfg_options(self.db).clone();
                DefDiagnosticKind::UnconfiguredCode { ast_id, cfg, opts }
            }
            4 => {
                let ast = self.call_kind()?;
                DefDiagnosticKind::UnresolvedMacroCall { ast, path: self.mod_path()? }
            }
            5 => DefDiagnosticKind::UnimplementedBuiltinMacro { ast: self.ast_id()? },
            6 => {
                let ast = self.ast_id()?;
                DefDiagnosticKind::InvalidDeriveTarget {
                    ast,
                    id: AttrId::from_item_tree_index(self.u32()?),
                }
            }
            7 => {
                let ast = self.ast_id()?;
                DefDiagnosticKind::MalformedDerive {
                    ast,
                    id: AttrId::from_item_tree_index(self.u32()?),
                }
            }
            8 => {
                let ast = self.ast_id()?;
                DefDiagnosticKind::MacroDefError { ast, message: self.str()?.to_owned() }
            }
            9 => {
                let ast = self.ast_id()?;
                let path = self.mod_path()?;
                let err = match self.u8()? {
                    0 => ExpandErrorKind::ProcMacroAttrExpansionDisabled,
                    1 => ExpandErrorKind::MissingProcMacroExpander(self.krate()?),
                    2 => ExpandErrorKind::MacroDisabled,
                    3 => ExpandErrorKind::MacroDefinition,
                    4 => ExpandErrorKind::RecursionOverflow,
                    5 => ExpandErrorKind::Other(self.str()?.into()),
                    6 => ExpandErrorKind::BuiltinDerive(self.str()?.into()),
                    7 => ExpandErrorKind::DefaultNoVariant,
                    it => return Err(format!("macro error {it}")),
                };
                DefDiagnosticKind::MacroError { ast, path, err }
            }
            it => return Err(format!("diagnostic {it}")),
        };
        Ok(DefDiagnostic { in_module, kind })
    }

    fn def_map(&mut self) -> R<DefMap> {
        let db = self.db;
        let krate = self.krate;
        let edition = self.edition()?;
        let mut data = DefMapCrateData::new(edition);
        data.proc_macro = self.bool()?;
        data.rustc_coherence_is_core = self.bool()?;
        data.no_core = self.bool()?;
        data.no_std = self.bool()?;
        data.recursion_limit = self.option(Self::u32)?;
        data.registered_tools = self.many(Self::symbol)?;
        let mut features = UnstableFeatures::default();
        for it in self.many(Self::symbol)? {
            features.enable(it);
        }
        data.unstable_features = features;

        // The modules: the root as `DefMap::empty` makes it, and the others as the collector
        // does, each in its parent.
        let n = self.len()?;
        let mut skeleton = Vec::with_capacity(n);
        for _ in 0..n {
            let parent = self.option(Self::u32)?;
            let name = self.option(Self::name)?;
            skeleton.push((parent, name));
        }
        let placeholder = ModuleData::new(
            ModuleOrigin::CrateRoot { definition: krate.root_file_id(db) },
            Visibility::Public,
            None,
        );
        let mut def_map = DefMap::empty(db, krate, TArc::new(data), placeholder, None);
        self.own.push(def_map.root);
        for (parent, name) in skeleton.iter().skip(1) {
            let parent = parent.ok_or("a module with no parent")? as usize;
            let parent = *self.own.get(parent).ok_or("a module before its parent")?;
            let name = name.clone().ok_or("a module with no name")?;
            let module =
                unsafe { ModuleIdLt::new(db, krate, None, Some(parent), name).to_static() };
            self.own.push(module);
        }
        let mut table = super::ModulesMap::new();
        for (index, (parent, _)) in skeleton.iter().enumerate() {
            let origin = self.origin()?;
            let visibility = self.visibility()?;
            let scope = self.scope()?;
            let children = self.many(|dec| {
                let name = dec.name()?;
                let child = dec.len()?;
                Ok((name, *dec.own.get(child).ok_or("a child not read")?))
            })?;
            let parent = parent.map(|it| self.own[it as usize]);
            let mut module = ModuleData::new(origin, visibility, parent);
            module.scope = ScopeSlot::Building {
                scope: Box::new(scope),
                children: children.into_iter().collect(),
            };
            table.insert(self.own[index], module);
        }
        def_map.modules = ModulesSlot::Building(table);

        def_map.prelude = self.option(|dec| {
            let module = dec.module()?;
            Ok((module, dec.option(Self::use_id)?))
        })?;
        def_map.macro_use_prelude = self
            .many(|dec| {
                let name = dec.name()?;
                let id = dec.macro_id()?;
                Ok((name, (id, dec.option(Self::extern_crate)?)))
            })?
            .into_iter()
            .collect();
        def_map.derive_helpers_in_scope = self
            .many(|dec| {
                let ast_id = dec.ast_id()?;
                let helpers = dec.many(|dec| {
                    let name = dec.name()?;
                    let id = dec.macro_id()?;
                    let call = match dec.u8()? {
                        0 => Either::Left(dec.call()?),
                        1 => Either::Right(dec.builtin_derive_impl()?),
                        it => return Err(format!("derive helper {it}")),
                    };
                    Ok((name, id, call))
                })?;
                Ok((ast_id, helpers))
            })?
            .into_iter()
            .collect();
        let exported = self.many(|dec| {
            let id = dec.macro_id()?;
            Ok((id, dec.many(Self::name)?.into_boxed_slice()))
        })?;
        let functions = self.many(|dec| {
            let ModuleDefId::FunctionId(function) = dec.def()? else {
                return Err("a procedural macro of no function".to_owned());
            };
            let MacroId::ProcMacroId(id) = dec.macro_id()? else {
                return Err("a function of a macro no procedural one".to_owned());
            };
            Ok((function, id))
        })?;
        let data = TArc::get_mut(&mut def_map.data).ok_or("the def map's crate data is shared")?;
        data.exported_derives = exported.into_iter().collect();
        data.fn_proc_macro_mapping_back = functions.iter().map(|&(f, id)| (id, f)).collect();
        data.fn_proc_macro_mapping = functions.into_iter().collect();
        def_map.diagnostics = self.many(Self::diagnostic)?;
        Ok(def_map)
    }

    fn local(&mut self) -> R<LocalDefMap> {
        let extern_prelude = self.many(|dec| {
            let name = dec.name()?;
            let module = dec.module()?;
            Ok((name, (module, dec.option(Self::extern_crate)?)))
        })?;
        Ok(LocalDefMap { extern_prelude: extern_prelude.into_iter().collect() })
    }
}

#[cfg(test)]
mod tests {
    use test_fixture::WithFixture;

    use super::*;
    use crate::test_db::TestDB;

    const FIXTURE: &str = r#"
//- /main.rs crate:main deps:lib
use lib::{Shown, shown};
mod inner;
#[derive(Clone, Debug)]
struct Local(u8);
fn main() { shown!(); }
#[cfg(any())]
fn left_out() {}

//- /inner.rs
pub(super) use super::Local as Again;
pub(crate) trait T {}
impl T for super::Local {}
macro_rules! m { () => { pub fn made() {} } }
m!();
const _: () = ();
use crate::Local as _;

//- /lib.rs crate:lib
#[macro_export]
macro_rules! shown { () => { () } }
pub struct Shown;
pub enum E { A, B(u8) }
pub use E::*;
pub mod deep { pub mod deeper { pub fn f() {} } }
#[rustc_builtin_macro]
pub macro derive {}
#[rustc_builtin_macro]
macro Clone {}
#[rustc_builtin_macro]
macro Debug {}
pub mod prelude { pub mod rust_2018 { pub use crate::{Clone, Debug, derive}; } }
"#;

    /// The differences are recorded for the whole process: the tests that read them take turns.
    static TURN: Mutex<()> = Mutex::new(());

    fn crate_named(db: &TestDB, name: &str) -> Crate {
        *base_db::all_crates(db)
            .iter()
            .find(|it| {
                it.extra_data(db).display_name.as_ref().is_some_and(|it| it.to_string() == name)
            })
            .unwrap()
    }

    /// The bytes of each crate of a fresh database of [`FIXTURE`], dependencies first.
    fn frozen() -> Vec<(&'static str, Vec<u8>)> {
        let db = TestDB::with_files(FIXTURE);
        ["lib", "main"]
            .into_iter()
            .map(|name| (name, encode(&db, crate_named(&db, name)).unwrap()))
            .collect()
    }

    fn with(mode: Mode, bytes: &[(&'static str, Vec<u8>)]) -> TestDB {
        let mut db = TestDB::with_files(FIXTURE);
        let maps = bytes
            .iter()
            .map(|(name, bytes)| {
                (crate_named(&db, name), Frozen { bytes: bytes.as_slice().into(), mode })
            })
            .collect();
        set(&mut db, maps);
        db
    }

    #[test]
    fn the_same_bytes_in_every_database() {
        assert_eq!(frozen(), frozen());
    }

    #[test]
    fn read_back_writes_the_same_bytes() {
        let _turn = TURN.lock().unwrap_or_else(|it| it.into_inner());
        let bytes = frozen();
        let db = with(Mode::Verify, &bytes);
        for (name, written) in &bytes {
            let krate = crate_named(&db, name);
            _ = super::super::crate_def_map(&db, krate);
            assert_eq!(encode(&db, krate).as_ref(), Ok(written), "{name}");
        }
        assert_eq!(differences(), Vec::<String>::new());
    }

    #[test]
    fn read_back_resolves_the_same() {
        let bytes = frozen();
        let live = TestDB::with_files(FIXTURE);
        let served = with(Mode::Serve, &bytes);
        for name in ["lib", "main"] {
            let dump =
                |db: &TestDB| super::super::crate_def_map(db, crate_named(db, name)).dump(db);
            assert_eq!(dump(&served), dump(&live), "{name}");
        }
    }

    #[test]
    fn a_file_outside_the_crate_is_read_only_as_written() {
        const OUTSIDE: &str = r#"
//- /main.rs crate:main deps:lib
use lib::made::made;
//- /lib/src/lib.rs crate:lib
#[path = "../made.rs"]
pub mod made;
//- /lib/made.rs
pub fn made() {}
"#;
        let db = TestDB::with_files(OUTSIDE);
        let lib = crate_named(&db, "lib");
        let bytes = encode(&db, lib).unwrap();
        assert_eq!(unreadable(&db, lib, &bytes), None);
        let changed = TestDB::with_files(&OUTSIDE.replace("{}\n", "{}\npub fn more() {}\n"));
        let why = unreadable(&changed, crate_named(&changed, "lib"), &bytes);
        assert!(
            why.as_ref().is_some_and(|it| it.ends_with("is not the file that was written")),
            "{why:?}"
        );
    }

    #[test]
    fn an_included_file_is_read_only_as_written() {
        const INCLUDED: &str = r#"
//- /main.rs crate:main deps:lib
lib::unroll!();
//- /lib/src/lib.rs crate:lib
#[rustc_builtin_macro]
macro_rules! include { () => {} }
include!("../out/made.rs");
//- /lib/out/made.rs
#[macro_export]
macro_rules! unroll { () => { pub fn made() {} } }
"#;
        let db = TestDB::with_files(INCLUDED);
        let lib = crate_named(&db, "lib");
        let bytes = encode(&db, lib).unwrap();
        assert_eq!(unreadable(&db, lib, &bytes), None);
        let mut served = TestDB::with_files(INCLUDED);
        let frozen = Frozen { bytes: bytes.as_slice().into(), mode: Mode::Serve };
        let served_lib = crate_named(&served, "lib");
        set(&mut served, [(served_lib, frozen)].into_iter().collect());
        for name in ["lib", "main"] {
            let dump =
                |db: &TestDB| super::super::crate_def_map(db, crate_named(db, name)).dump(db);
            assert_eq!(dump(&served), dump(&db), "{name}");
        }
        let changed = TestDB::with_files(&INCLUDED.replace("unroll {", "unroll_more {"));
        let why = unreadable(&changed, crate_named(&changed, "lib"), &bytes);
        assert!(
            why.as_ref().is_some_and(|it| it.ends_with("is not the file that was written")),
            "{why:?}"
        );
        let missing = TestDB::with_files(INCLUDED.split("//- /lib/out").next().unwrap());
        let why = unreadable(&missing, crate_named(&missing, "lib"), &bytes);
        assert!(why.as_ref().is_some_and(|it| it.ends_with("is not loaded")), "{why:?}");
    }

    #[test]
    fn a_file_a_build_script_wrote_is_named_from_out_dir() {
        // As two checkouts build one registry crate: its build script writes the same file
        // below the target directory of each, and the crate includes it from there.
        // A crate that depends on it names its macro calls, so the dependency's OUT_DIR too.
        const WROTE: &str = r#"
//- /main.rs crate:main deps:lib
lib::unroll!();
pub use lib::inside::f;
//- /registry/lib/src/lib.rs crate:lib env:OUT_DIR=/first/target/out
#[rustc_builtin_macro]
macro_rules! include { () => {} }
include!("../../../first/target/out/made.rs");
//- /first/target/out/made.rs
#[macro_export]
macro_rules! unroll { () => { pub fn made() {} } }
pub mod inside { pub fn f() {} }
"#;
        let first = TestDB::with_files(WROTE);
        let other = TestDB::with_files(&WROTE.replace("first", "other"));
        let bytes: Vec<_> = ["lib", "main"]
            .into_iter()
            .map(|name| (name, encode(&first, crate_named(&first, name)).unwrap()))
            .collect();
        for (name, written) in &bytes {
            let krate = crate_named(&other, name);
            assert_eq!(encode(&other, krate).as_ref(), Ok(written), "{name}");
            assert_eq!(unreadable(&other, krate, written), None, "{name}");
        }
        let mut served = TestDB::with_files(&WROTE.replace("first", "other"));
        let maps = (bytes.iter())
            .map(|(name, bytes)| {
                let frozen = Frozen { bytes: bytes.as_slice().into(), mode: Mode::Serve };
                (crate_named(&served, name), frozen)
            })
            .collect();
        set(&mut served, maps);
        for name in ["lib", "main"] {
            let dump =
                |db: &TestDB| super::super::crate_def_map(db, crate_named(db, name)).dump(db);
            assert_eq!(dump(&served), dump(&other), "{name}");
        }
    }

    #[test]
    fn checking_finds_a_difference() {
        let _turn = TURN.lock().unwrap_or_else(|it| it.into_inner());
        let mut bytes = frozen();
        let db = with(Mode::Check, &bytes);
        _ = super::super::crate_def_map(&db, crate_named(&db, "main"));
        assert_eq!(differences(), Vec::<String>::new());
        bytes[1].1.push(0);
        let db = with(Mode::Check, &bytes);
        _ = super::super::crate_def_map(&db, crate_named(&db, "main"));
        assert_eq!(differences().len(), 1);
    }
}
