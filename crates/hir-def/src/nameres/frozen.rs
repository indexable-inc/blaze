//! A crate's `DefMap` and `LocalDefMap`, and its lang items, written as bytes and read back
//! into a running database (frozen dependency artifacts, blaze's `docs/glancer.md`, item 1).
//! [`Enc`] and [`Dec`] write and read other values that name items, as `hir_ty` does impls.
//!
//! The bytes name nothing by a query database id: a file is the crate it belongs to and its path from
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
use std::sync::Arc;

use base_db::{Crate, Db};
use base_db::{FileId, VfsPath};
use blaze_format::cursor::{Put, Reader};
use blaze_hash::{HashMap, HashSet};
use cfg::{CfgAtom, CfgExpr};
use either::Either;
use hir_expand::{
    BuiltinChild, BuiltinError, EagerCallInfo, ExpandErrorKind, ExpandTo, HirFileId, MacroCallId,
    MacroCallKind, MacroCallLoc, MacroCallStyles, MacroDefId, MacroDefKind, MatchFailure,
    MbeErrorKind,
    attrs::AttrId,
    builtin::{BuiltinAttrExpander, BuiltinDeriveExpander, BuiltinFnLikeExpander, EagerExpander},
    mod_path::{ModPath, PathKind},
    name::Name,
    proc_macro::{CustomProcMacroExpander, ProcMacroKind},
};
use intern::Symbol;
use la_arena::{Idx, RawIdx};
use parking_lot::Mutex;
use smallvec::SmallVec;
use span::{
    Edition, ErasedFileAstId, FileAstId, Span, SpanAnchor, SyntaxContext, TextRange, TextSize,
    Transparency,
};
use syntax::ast;
use thin_vec::ThinVec;
use triomphe::Arc as TArc;
use tt::{Delimiter, DelimiterKind, IdentIsRaw, Leaf, LitKind, Spacing, Subtree, TokenTree};

use crate::{
    AdtId, AstId, BlockId, BuiltinDeriveImplId, BuiltinDeriveImplLoc, ConstLoc, DefWithBodyId,
    EnumLoc, EnumVariantId, EnumVariantLoc, ExternBlockId, ExternBlockLoc, ExternCrateId,
    ExternCrateLoc, FunctionLoc, FxIndexMap, ImplId, ImplLoc, Intern, ItemContainerId, Lookup,
    Macro2Loc, MacroExpander, MacroId, MacroRulesLoc, MacroRulesLocFlags, ModuleDefId, ModuleId,
    ProcMacroLoc, StaticLoc, StructLoc, TraitLoc, TypeAliasLoc, UnionLoc, UseId, UseLoc,
    builtin_derive::BuiltinDeriveImplTrait,
    builtin_type::BuiltinType,
    expr_store::Body,
    item_scope::{
        DeriveMacroInvocation, GlobAmbiguity, GlobId, ImportId, ImportOrDef, ImportOrExternCrate,
        ImportOrGlob, ItemInNs, ItemScope, ShadowedImport, UnderscoreImport,
    },
    item_tree::{TreeId, VisibilityExplicitness},
    lang_item::{LangItemEnum, LangItemTarget, LangItems},
    nameres::{
        DefDiagnostic, DefMap, DefMapCrateData, LocalDefMap, ModuleData, ModuleOrigin, ModulesSlot,
        diagnostics::{DefDiagnosticKind, PreludeError, TraitItemImport},
        uses::{DeriveHelperUse, MacroCallRecord, MacroVia, NameRecords, Use, Used},
    },
    per_ns::{Item, Namespace},
    unstable_features::UnstableFeatures,
    visibility::Visibility,
};

type R<T = ()> = Result<T, String>;

/// The version of what this module writes, for the key frozen bytes are stored under: it
/// changes with every change to the bytes [`Enc`] writes, the list of lang items included, so
/// bytes of another version are never read.
pub const ENCODING: &str = "blaze frozen def map 15";

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
    pub bytes: crate::deps::PartBytes,
    pub mode: Mode,
}

/// The crates whose `DefMap` is frozen, set by whoever has the bytes.
#[blaze_query::input(singleton, debug)]
pub struct FrozenDefMaps {
    #[returns(ref)]
    pub maps: Arc<HashMap<Crate, Frozen>>,
}

/// Has name resolution use `maps`.
pub fn set(db: &mut Db, maps: HashMap<Crate, Frozen>) {
    use blaze_query::db::{Durability, Setter};
    // alloc: Check/Verify only, which record differences; never when serving
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
///
/// A lock, not a query result: only [`Mode::Check`] and [`Mode::Verify`] write it (a mode for
/// testing the bytes, never serving them), the reader is whoever set the mode, outside any
/// query, and each method takes it once and drops it before returning, so it is never held
/// across a database call. [`Mode::Serve`] never touches it.
struct Differences(Mutex<Vec<(Crate, &'static str, String)>>);

impl Differences {
    fn found(&self) -> parking_lot::MutexGuard<'_, Vec<(Crate, &'static str, String)>> {
        self.0.lock()
    }

    fn take(&self) -> Vec<(Crate, &'static str, String)> {
        std::mem::take(&mut *self.found())
    }

    /// Forgets a difference found before in the frozen `kind` of `krate`, and records `said`
    /// in its place if there is one.
    fn record(&self, krate: Crate, kind: &'static str, said: Option<String>) {
        let mut found = self.found();
        found.retain(|(at, of, _)| (*at, *of) != (krate, kind));
        found.extend(said.map(|said| (krate, kind, said)));
    }
}

// alloc: Check/Verify only, which record differences; never when serving
static DIFFERENCES: Differences = Differences(Mutex::new(Vec::new()));

/// The differences [`Mode::Check`] and [`Mode::Verify`] found since this was last called.
pub fn differences() -> Vec<String> {
    // alloc: Check/Verify only, which record differences; never when serving
    DIFFERENCES.take().into_iter().map(|(_, _, it)| it).collect()
}

/// Forgets a difference found before in the frozen `kind` of `krate`, which agrees now.
fn agrees(krate: Crate, kind: &'static str) {
    DIFFERENCES.record(krate, kind, None);
}

fn differs(db: &Db, krate: Crate, what: &str, written: &[u8], frozen: &[u8]) {
    record_difference(db, krate, "def map", what, written, frozen);
}

/// Records that the frozen `kind` of `krate` differs from `what`, which wrote `written`.
pub fn record_difference(
    db: &Db,
    krate: Crate,
    kind: &'static str,
    what: &str,
    written: &[u8],
    frozen: &[u8],
) {
    let at = written.iter().zip(frozen).position(|(a, b)| a != b).unwrap_or(written.len());
    // alloc: Check/Verify only, which record differences; never when serving
    let name = krate.extra_data(db).display_name.as_ref().map(|it| it.to_string());
    // alloc: Check/Verify only, which record differences; never when serving
    let said = format!(
        "the frozen {kind} of `{}` differs from {what}: {} bytes against {}, first at byte {at}",
        name.as_deref().unwrap_or("?"),
        written.len(),
        frozen.len(),
    );
    DIFFERENCES.record(krate, kind, Some(said));
}

/// Records a difference between `frozen`, the bytes of `krate`'s `kind`, and what `encode`
/// writes now: the live ones in [`Mode::Check`], what was read of them in [`Mode::Verify`].
pub fn check_frozen(
    db: &Db,
    krate: Crate,
    kind: &'static str,
    frozen: &Frozen,
    encode: impl FnOnce() -> Result<Vec<u8>, String>,
) {
    let what = match frozen.mode {
        Mode::Serve => return,
        Mode::Check => "the live ones",
        Mode::Verify => "what was read of them",
    };
    match encode() {
        Ok(written) if *written == *frozen.bytes => {}
        Ok(written) => record_difference(db, krate, kind, what, &written, &frozen.bytes),
        Err(why) => {
            // alloc: Check/Verify only, which record differences; never when serving
            let what = format!("{what}, which cannot be written ({why})");
            record_difference(db, krate, kind, &what, &[], &[])
        }
    }
}

/// The frozen `DefMap` of `krate`, by value: a [`set`] that leaves it as it was is backdated
/// here, so it runs none of the queries of `krate` again.
#[blaze_query::tracked(returns(as_ref))]
fn frozen_def_map(db: &Db, krate: Crate) -> Option<Frozen> {
    // clone: a count of the image the bytes are in, or of their own buffer
    FrozenDefMaps::try_get(db)?.maps(db).get(&krate).cloned()
}

/// What [`super::crate_local_def_map`] serves for `krate` instead of resolving it, when its
/// `DefMap` is frozen.
pub(super) fn served(db: &Db, krate: Crate) -> Option<(DefMap, LocalDefMap)> {
    let frozen = frozen_def_map(db, krate)?;
    if frozen.mode == Mode::Check {
        return None;
    }
    let (def_map, local) = decode(db, krate, &frozen.bytes)
        .unwrap_or_else(|why| panic!("a frozen def map does not read: {why}"));
    if frozen.mode == Mode::Verify {
        match encode_maps(db, krate, &def_map, &local) {
            Ok(written) if *written == *frozen.bytes => {}
            Ok(written) => differs(db, krate, "what was read of it", &written, &frozen.bytes),
            // alloc: error path
            Err(why) => differs(db, krate, &format!("what was read of it ({why})"), &[], &[]),
        }
    }
    Some((def_map, local))
}

/// Whether [`served`] gives `krate`'s `DefMap` from the image, without decoding it: the name
/// resolution maintainer never maintains such a crate.
pub(super) fn is_served(db: &Db, krate: Crate) -> bool {
    frozen_def_map(db, krate).is_some_and(|it| it.mode != Mode::Check)
}

/// Records a difference between the `DefMap` resolving found and the frozen one, in
/// [`Mode::Check`].
pub(super) fn check(db: &Db, krate: Crate, def_map: &DefMap, local: &LocalDefMap) {
    let Some(frozen) = frozen_def_map(db, krate) else { return };
    if frozen.mode != Mode::Check {
        return;
    }
    match encode_maps(db, krate, def_map, local) {
        Ok(written) if *written == *frozen.bytes => agrees(krate, "def map"),
        Ok(written) => differs(db, krate, "the live one", &written, &frozen.bytes),
        Err(why) => {
            // alloc: Check/Verify only, which record differences; never when serving
            differs(db, krate, &format!("the live one, which it cannot write ({why})"), &[], &[])
        }
    }
}

/// Why the frozen bytes of `krate` cannot be read in this database: a file they name is not
/// loaded.
pub fn unreadable(db: &Db, krate: Crate, bytes: &[u8]) -> Option<String> {
    let mut dec = Dec::new(db, krate, bytes);
    dec.files().err()
}

/// The crates whose lang items are frozen, set by whoever has the bytes.
#[blaze_query::input(singleton, debug)]
pub struct FrozenLangItems {
    #[returns(ref)]
    pub maps: Arc<HashMap<Crate, Frozen>>,
}

/// Has finding the lang items of a crate use `maps`.
pub fn set_lang_items(db: &mut Db, maps: HashMap<Crate, Frozen>) {
    use blaze_query::db::{Durability, Setter};
    // alloc: once per load of the frozen bytes
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

/// The frozen lang items of `krate`, by value, as [`frozen_def_map`] is.
#[blaze_query::tracked(returns(as_ref))]
fn frozen_lang_items(db: &Db, krate: Crate) -> Option<Frozen> {
    // clone: a count of the image the bytes are in, or of their own buffer
    FrozenLangItems::try_get(db)?.maps(db).get(&krate).cloned()
}

/// What [`crate_lang_items`](crate::lang_item::crate_lang_items) reads for `krate` instead of
/// finding them, when they are frozen.
pub(crate) fn served_lang_items(
    db: &Db,
    krate: Crate,
) -> Option<Option<Box<LangItems>>> {
    let frozen = frozen_lang_items(db, krate).filter(|it| it.mode != Mode::Check)?;
    let read = || {
        let mut dec = Dec::new_checked(db, krate, &frozen.bytes)?;
        let items = dec.option(|dec| {
            let mut items = LangItems::default();
            let n = dec.len()?;
            if n != LangItemEnum::ALL.len() {
                // alloc: error path
                return Err(format!("{n} lang items"));
            }
            for &which in LangItemEnum::ALL {
                if let Some(target) = dec.option(Dec::lang_item_target)?
                    && !items.set(which, target)
                {
                    // alloc: error path
                    return Err(format!("a lang item {which:?} of another kind"));
                }
            }
            // alloc: once per load of the frozen bytes
            Ok(Box::new(items))
        })?;
        dec.finish()?;
        Ok::<_, String>(items)
    };
    Some(read().unwrap_or_else(|why| panic!("frozen lang items do not read: {why}")))
}

/// The bytes of the lang items of `krate` as they are now: found, or read in [`Mode::Serve`]
/// and [`Mode::Verify`].
pub fn encode_lang_items(db: &Db, krate: Crate) -> R<Vec<u8>> {
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
pub fn check_lang_items(db: &Db, krate: Crate) {
    let Some(frozen) = frozen_lang_items(db, krate) else { return };
    check_frozen(db, krate, "lang items", &frozen, || encode_lang_items(db, krate));
}

/// The bytes of the `DefMap` of `krate` as name resolution computes it, or why it cannot be
/// frozen.
pub fn encode(db: &Db, krate: Crate) -> R<Vec<u8>> {
    let pair = super::crate_local_def_map(db, krate);
    encode_maps(db, krate, pair.def_map(db), pair.local(db))
}

/// `krate` and the crates it depends on, transitively, breadth first in the order the crate
/// graph lists dependencies: the crates frozen bytes name by index.
fn closure(db: &Db, krate: Crate) -> Vec<Crate> {
    // alloc: encoding, once per crate per image write (not on a build's path)
    let mut order = vec![krate];
    // alloc: encoding, once per crate per image write (not on a build's path)
    let mut seen: HashSet<Crate> = order.iter().copied().collect();
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
fn root_dir(db: &Db, krate: Crate) -> R<(String, bool, base_db::SourceRootId)> {
    let root = krate.data(db).root_file_id;
    let source_root = db.file_source_root(root).source_root_id(db);
    let path = db.source_root(source_root).source_root(db).path_for_file(&root).cloned();
    let path = path.ok_or("a crate root with no path")?;
    let real = path.as_path().is_some();
    let path = on_disk(&path)?;
    let dir =
        // alloc: encoding, once per crate per image write (not on a build's path)
        path.rsplit_once('/').map(|it| it.0.to_owned()).ok_or("a crate root in no directory")?;
    Ok((dir, real, source_root))
}

/// The text of `path`, which [`vfs_path`] reads back.
fn on_disk(path: &VfsPath) -> R<String> {
    // alloc: encoding, once per crate per image write (not on a build's path)
    let text = path.to_string();
    if !text.starts_with('/') {
        // alloc: error path
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

fn out_dir(db: &Db, krate: Crate) -> Option<String> {
    let out = krate.env(db).get("OUT_DIR")?;
    let out = out.trim_end_matches('/');
    // alloc: encoding, once per crate per image write (not on a build's path)
    (!out.is_empty()).then(|| out.to_owned())
}

/// The `OUT_DIR` of each crate of `closure` that has one, by its index there.
fn out_dirs(db: &Db, closure: &[Crate]) -> Vec<(u32, String)> {
    let dirs = closure.iter().enumerate();
    // alloc: encoding, once per crate per image write (not on a build's path)
    dirs.filter_map(|(index, &krate)| Some((index as u32, out_dir(db, krate)?))).collect()
}

fn holds(bytes: &[u8], part: &[u8]) -> bool {
    !part.is_empty() && bytes.windows(part.len()).any(|it| it == part)
}

/// `to` from the directory `from`, with `..` where it is not under it.
fn relative(from: &str, to: &str) -> String {
    // alloc: encoding, once per crate per image write (not on a build's path)
    let from: Vec<_> = from.split('/').filter(|it| !it.is_empty()).collect();
    // alloc: encoding, once per crate per image write (not on a build's path)
    let to: Vec<_> = to.split('/').filter(|it| !it.is_empty()).collect();
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    // alloc: encoding, once per crate per image write (not on a build's path)
    let mut parts: Vec<&str> = vec![".."; from.len() - common];
    parts.extend(&to[common..]);
    parts.join("/")
}

/// `relative` from the directory `from`: what [`relative`] gave back.
fn joined(from: &str, relative: &str) -> String {
    // alloc: encoding, once per crate per image write (not on a build's path)
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
    // alloc: encoding, once per crate per image write (not on a build's path)
    format!("/{}", parts.join("/"))
}

fn position<T: PartialEq>(all: &[T], it: T) -> u32 {
    all.iter().position(|x| *x == it).unwrap() as u32
}

const NEW: u8 = 0;
const SEEN: u8 = 1;

/// Writes values that name items of `krate` and its dependencies as frozen bytes; see the
/// module documentation.
pub struct Enc<'a> {
    db: &'a Db,
    out: Vec<u8>,
    /// Writing a fingerprint: macro calls and syntax contexts are written as hashes of what
    /// they are, and nothing is named by an index.
    fingerprinting: bool,
    crates: HashMap<Crate, u32>,
    closure: Vec<Crate>,
    dirs: HashMap<u32, String>,
    /// The crates of the closure whose root is in each source root, in the closure's order.
    owners: HashMap<base_db::SourceRootId, Vec<u32>>,
    /// Each file: the crate it is named from and its path from that crate's root.
    file_names: HashMap<FileId, (u32, String)>,
    files: HashMap<FileId, u32>,
    /// The files named, in the order of their index.
    file_order: Vec<FileId>,
    calls: HashMap<MacroCallId, u32>,
    contexts: HashMap<SyntaxContext, u32>,
    call_hashes: HashMap<MacroCallId, u128>,
    context_hashes: HashMap<SyntaxContext, u128>,
    modules: HashMap<ModuleId, (u32, u32)>,
    indexed: HashSet<Crate>,
    /// The body each block that was looked for is in.
    block_owners: HashMap<BlockId, DefWithBodyId>,
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
    db: &Db,
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
    pub fn new(db: &'a Db, krate: Crate) -> Self {
        let closure = closure(db, krate);
        // alloc: encoding, once per crate per image write (not on a build's path)
        let crates = closure.iter().enumerate().map(|(i, &it)| (it, i as u32)).collect();
        let out_dirs = out_dirs(db, &closure);
        Enc {
            db,
            // alloc: encoding, once per crate per image write (not on a build's path)
            out: Vec::new(),
            fingerprinting: false,
            crates,
            closure,
            dirs: HashMap::default(),
            owners: HashMap::default(),
            file_names: HashMap::default(),
            files: HashMap::default(),
            // alloc: encoding, once per crate per image write (not on a build's path)
            file_order: Vec::new(),
            calls: HashMap::default(),
            contexts: HashMap::default(),
            call_hashes: HashMap::default(),
            context_hashes: HashMap::default(),
            modules: HashMap::default(),
            indexed: HashSet::default(),
            block_owners: HashMap::default(),
            out_dirs,
        }
    }

    /// The bytes written, after the files they name, which [`Dec::new_checked`] reads first.
    pub fn finish(mut self) -> R<Vec<u8>> {
        // The target directories the closure's build scripts wrote below, which nothing the
        // bytes say may name: they are of one checkout.
        let mut places: Vec<String> = (self.out_dirs.iter())
            // alloc: encoding, once per crate per image write (not on a build's path)
            .map(|(_, out)| out.find("/build/").map_or(out.clone(), |at| out[..at + 1].to_owned()))
            // alloc: encoding, once per crate per image write (not on a build's path)
            .collect();
        places.sort_unstable();
        places.dedup();
        let body = std::mem::take(&mut self.out);
        self.usize(self.file_order.len())?;
        for file in std::mem::take(&mut self.file_order) {
            // clone: encoding, once per crate per image write (not on a build's path)
            let (owner, path) = self.file_names[&file].clone();
            self.u32(owner);
            self.str(&path)?;
            // A file outside the directory of its crate's root may be one a build script
            // wrote, which the key does not cover: its text is.
            let outside = path.starts_with("../") || path.starts_with(OUT);
            self.bool(outside);
            if outside {
                let text = self.db.file_text(file).text(self.db);
                self.out.put_u128(hash128(text.as_bytes()));
            }
            if !path.starts_with(OUT) {
                let full = joined(&self.dirs[&owner], &path);
                if let Some(place) = places.iter().find(|it| full.starts_with(it.as_str())) {
                    // alloc: error path
                    return Err(format!("a file below {place}, a place of one checkout: {full}"));
                }
            }
        }
        self.out.extend_from_slice(&body);
        if let Some(place) = places.iter().find(|it| holds(&self.out, it.as_bytes())) {
            // alloc: error path
            return Err(format!("the bytes name {place}, a place of one checkout"));
        }
        Ok(self.out)
    }

    /// A literal's text with each `OUT_DIR` of the closure as [`OUT`], the crate's index and a
    /// NUL.
    fn unplaced(&self, text: &Symbol) -> R<Symbol> {
        let written = text.as_str();
        if written.contains('\0') {
            // alloc: error path
            return Err("a literal holding a NUL".to_owned());
        }
        // alloc: encoding, once per crate per image write (not on a build's path)
        let mut unplaced = written.to_owned();
        for (index, out) in &self.out_dirs {
            if unplaced.contains(out.as_str()) {
                // alloc: encoding, once per crate per image write (not on a build's path)
                unplaced = unplaced.replace(out.as_str(), &format!("{OUT} {index}\0"));
            }
        }
        Ok(if unplaced == written { *text } else { Symbol::intern(&unplaced) })
    }
}

impl Enc<'_> {
    pub fn u8(&mut self, it: u8) {
        self.out.push(it);
    }

    pub fn u32(&mut self, it: u32) {
        self.out.put_uleb(it.into());
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
        // alloc: encoding, once per crate per image write (not on a build's path)
        let mut keyed = Vec::new();
        for (k, v) in entries {
            let print = self.fingerprint(|enc| key(enc, &k))?;
            keyed.push((print, k, v));
        }
        keyed.sort_by(|a, b| a.0.cmp(&b.0));
        // alloc: encoding, once per crate per image write (not on a build's path)
        Ok(keyed.into_iter().map(|(_, k, v)| (k, v)).collect())
    }

    fn file_name(&mut self, file: FileId) -> R<(u32, String)> {
        if let Some(it) = self.file_names.get(&file) {
            // clone: encoding, once per crate per image write (not on a build's path)
            return Ok(it.clone());
        }
        let db = self.db;
        let source_root = db.file_source_root(file).source_root_id(db);
        let path = db.source_root(source_root).source_root(db).path_for_file(&file).cloned();
        let path = on_disk(&path.ok_or("a file with no path")?)?;
        if self.owners.is_empty() {
            for (index, &krate) in self.closure.iter().enumerate() {
                let (dir, _, root) = root_dir(db, krate)?;
                // alloc: encoding, once per crate per image write (not on a build's path)
                self.owners.entry(root).or_default().push(index as u32);
                self.dirs.insert(index as u32, dir);
            }
        }
        let candidates = self
            .owners
            .get(&source_root)
            // alloc: error path
            .ok_or_else(|| format!("a file of no crate of the closure: {path}"))?;
        // Crates share a source root (a package's lib and its build script, every crate of a
        // test fixture), so the file is named from the one nearest it: whose root's directory
        // is the deepest above it, among those the one whose root it is, then the closure's
        // first. Named from another crate's directory, it would change when that crate moves.
        let dirs = &self.dirs;
        let closure = &self.closure;
        let depth = |index: u32| {
            let dir = &dirs[&index];
            let above = path.strip_prefix(dir.as_str()).is_some_and(|rest| rest.starts_with('/'));
            above.then(|| dir.split('/').filter(|it| !it.is_empty()).count())
        };
        let nearest = candidates.iter().filter_map(|&it| Some((it, depth(it)?))).max_by_key(
            |&(index, depth)| {
                let is_root = closure[index as usize].data(db).root_file_id == file;
                (depth, is_root, std::cmp::Reverse(index))
            },
        );
        let owner = match nearest {
            Some((owner, _)) => owner,
            // Above every crate's directory (`#[path = "../x.rs"]`): from the closure's first.
            None => candidates[0],
        };
        // Named from the crate whose build script wrote it, whichever crate's root is beside it.
        let below_out = self.out_dirs.iter().find_map(|(index, out)| {
            let rest = path.strip_prefix(out.as_str())?;
            // alloc: encoding, once per crate per image write (not on a build's path)
            rest.starts_with('/').then(|| (*index, format!("{OUT}{rest}")))
        });
        let name = below_out.unwrap_or_else(|| (owner, relative(&self.dirs[&owner], &path)));
        // clone: encoding, once per crate per image write (not on a build's path)
        self.file_names.insert(file, name.clone());
        Ok(name)
    }

    pub(crate) fn file(&mut self, file: base_db::EditionedFileId) -> R {
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

    pub(crate) fn hir_file(&mut self, it: HirFileId) -> R {
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

    pub(crate) fn ast_id<N: syntax::AstNode>(&mut self, it: AstId<N>) -> R {
        self.hir_file(it.file_id)?;
        self.u32(it.value.erase().into_raw());
        Ok(())
    }

    fn erased_ast_id(&mut self, it: ErasedFileAstId) {
        self.u32(it.into_raw());
    }

    pub(crate) fn call(&mut self, call: MacroCallId) -> R {
        if self.fingerprinting {
            let print = self.call_hash(call)?;
            self.out.put_u128(print);
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
        // clone: encoding, once per crate per image write (not on a build's path)
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
            // alloc: error path
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
                            // alloc: error path
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

    pub(crate) fn context(&mut self, ctx: SyntaxContext) -> R {
        if ctx.is_root() {
            self.u8(0);
            self.edition(ctx.edition(self.db));
            return Ok(());
        }
        if self.fingerprinting {
            self.u8(1);
            let print = self.context_hash(ctx)?;
            self.out.put_u128(print);
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
        // alloc: encoding, once per crate per image write (not on a build's path)
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
                    let (kind, hashes) = kind.code();
                    self.u8(kind);
                    self.u8(hashes);
                    self.u32(suffix_len);
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
            // What gets written is the owner's path and indexes, never the id.
            let block = block.to_static();
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

    /// The body `block` is in, found without lowering any other body.
    fn block_owner(&mut self, block: BlockId) -> R<DefWithBodyId> {
        if let Some(&owner) = self.block_owners.get(&block) {
            return Ok(owner);
        }
        let owner = super::demand::block_owner(self.db, block)?;
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
        // clone: encoding, once per crate per image write (not on a build's path)
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

    fn item_in_ns(&mut self, it: ItemInNs) -> R {
        match it {
            ItemInNs::Types(it) => {
                self.u8(0);
                self.def(it)
            }
            ItemInNs::Values(it) => {
                self.u8(1);
                self.def(it)
            }
            ItemInNs::Macros(it) => {
                self.u8(2);
                self.macro_id(it)
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
            trait_impl,
            reservation_impl,
            builtin_derive_impls,
            extern_blocks,
            unnamed_consts,
            unnamed_trait_imports,
            underscore_imports,
            glob_ambiguities,
            shadowed_imports,
            use_imports_types,
            use_imports_values,
            use_imports_macros,
            use_decls,
            extern_crate_decls,
            legacy_macros,
            attr_macros,
            macro_invocations,
            derive_macros,
            names: _,
            sorted: _,
            by_def: _,
        } = it;
        self.usize(types.len())?;
        for (name, Item { def, vis, import }) in types.iter() {
            self.name(name)?;
            self.def(def)?;
            self.visibility(vis)?;
            self.option(import, Self::import_or_extern_crate)?;
        }
        self.usize(values.len())?;
        for (name, Item { def, vis, import }) in values.iter() {
            self.name(name)?;
            self.def(def)?;
            self.visibility(vis)?;
            self.option(import, Self::import_or_glob)?;
        }
        self.usize(macros.len())?;
        for (name, Item { def, vis, import }) in macros.iter() {
            self.name(name)?;
            self.macro_id(def)?;
            self.visibility(vis)?;
            self.option(import, Self::import_or_extern_crate)?;
        }
        // alloc: encoding, once per crate per image write (not on a build's path)
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
        for (at, &it) in impls.iter().enumerate() {
            self.impl_id(it)?;
            self.bool(trait_impl.contains(at));
            self.bool(reservation_impl.contains(at));
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
            self.option(import, |enc, import| enc.import_or_extern_crate(import))?;
        }
        self.usize(underscore_imports.len())?;
        for &UnderscoreImport { import: ImportId { use_, idx }, def, vis } in underscore_imports {
            self.import(use_, idx)?;
            self.option(def, Self::def)?;
            self.visibility(vis)?;
        }
        self.usize(glob_ambiguities.len())?;
        for &GlobAmbiguity { ref name, other, glob: GlobId { use_, idx }, vis } in glob_ambiguities
        {
            self.name(name)?;
            self.item_in_ns(other)?;
            self.import(use_, idx)?;
            self.visibility(vis)?;
        }
        self.usize(shadowed_imports.len())?;
        for &ShadowedImport { import, ref name, item, vis } in shadowed_imports {
            self.import_or_extern_crate(import)?;
            self.name(name)?;
            self.item_in_ns(item)?;
            self.visibility(vis)?;
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
        // alloc: encoding, once per crate per image write (not on a build's path)
        let mut legacy: Vec<_> = legacy_macros.iter().collect();
        legacy.sort_unstable_by(|a, b| a.0.symbol().cmp_order(b.0.symbol()));
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
            // alloc: error path
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
            // alloc: error path
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

    fn diagnostic(&mut self, it: &DefDiagnostic) -> R {
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
            DefDiagnosticKind::TraitItemImport { id, index, why } => {
                self.u8(10);
                self.ast_id(*id)?;
                self.u32(index.into_raw().into_u32());
                self.u8(match why {
                    TraitItemImport::Unstable => 0,
                    TraitItemImport::ItemsFromMacros => 1,
                });
            }
            DefDiagnosticKind::Prelude { krate, error } => {
                self.u8(11);
                self.name(krate)?;
                self.u8(match error {
                    PreludeError::CrateNotFound => 0,
                    PreludeError::NoPreludeModule => 1,
                });
            }
            DefDiagnosticKind::UnconfiguredCode { ast_id, cfg } => {
                self.u8(3);
                self.hir_file(ast_id.file_id)?;
                self.erased_ast_id(ast_id.value);
                self.cfg(cfg)?;
            }
            DefDiagnosticKind::UnresolvedMacroCall { ast, path } => {
                self.u8(4);
                // clone: encoding, once per crate per image write (not on a build's path)
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
                    ExpandErrorKind::MissingProcMacroExpander { krate, why } => {
                        self.u8(1);
                        self.krate(*krate)?;
                        self.str(why)?;
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
                    ExpandErrorKind::Builtin(it) => {
                        let BuiltinError { message, code, also_at, children, derive } = &**it;
                        self.u8(8);
                        self.str(message)?;
                        self.option(*code, |enc, code| {
                            enc.u32(code.into());
                            Ok(())
                        })?;
                        self.usize(also_at.len())?;
                        for &span in also_at {
                            self.span(span)?;
                        }
                        self.usize(children.len())?;
                        for (kind, text) in children {
                            self.bool(matches!(kind, BuiltinChild::Help));
                            self.str(text)?;
                        }
                        self.bool(*derive);
                    }
                    ExpandErrorKind::ProcMacroPanic(message) => {
                        self.u8(9);
                        self.str(message)?;
                    }
                    ExpandErrorKind::Mbe(it) => {
                        self.u8(10);
                        self.mbe_error(it)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn mbe_error(&mut self, it: &MbeErrorKind) -> R {
        match it {
            MbeErrorKind::BindingError(message) => {
                self.u8(0);
                self.str(message)?;
            }
            MbeErrorKind::UnresolvedBinding(name) => {
                self.u8(1);
                self.str(name)?;
            }
            MbeErrorKind::LeftoverTokens => self.u8(2),
            MbeErrorKind::LimitExceeded => self.u8(3),
            MbeErrorKind::NoMatchingRule => self.u8(4),
            MbeErrorKind::UnexpectedToken => self.u8(5),
            MbeErrorKind::MatchFailure(it) => {
                let MatchFailure { token, remaining, remaining_span, missing_comma } = &**it;
                self.u8(6);
                self.option(token.as_deref(), |enc, it| enc.str(it))?;
                self.str(remaining)?;
                self.option(*remaining_span, |enc, it| enc.span(it))?;
                self.option(*missing_comma, |enc, it| enc.span(it))?;
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
            derive_helpers_in_scope: _,
            diagnostics: _,
            diagnostics_by_module: _,
            records: _,
            data,
            session_macros,
            names: _,
        } = it;
        let db = self.db;
        let super::Records { names, diagnostics, derive_helpers_in_scope, .. } = it.records(db);
        if *session_macros {
            // alloc: error path
            return Err("compilation-local macro state".to_owned());
        }
        if block.is_some() {
            // alloc: error path
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
            // alloc: error path
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
        // alloc: encoding, once per crate per image write (not on a build's path)
        let mut features: Vec<Symbol> = unstable_features.iter().collect();
        features.sort_unstable_by(Symbol::cmp_order);
        self.usize(features.len())?;
        for it in &features {
            self.symbol(it)?;
        }

        // The modules first, so that everything after can name any of them.
        let table = it.table(db);
        if table.get_index(0).map(|it| *it.0) != Some(*root) {
            // alloc: error path
            return Err("a def map whose root is not its first module".to_owned());
        }
        self.usize(table.len())?;
        let index: HashMap<ModuleId, u32> =
            // alloc: encoding, once per crate per image write (not on a build's path)
            table.keys().enumerate().map(|(i, &it)| (it, i as u32)).collect();
        for (id, data) in table.iter() {
            if id.krate(db) != *krate {
                // alloc: error path
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
        // alloc: encoding, once per crate per image write (not on a build's path)
        let mut macro_use: Vec<_> = macro_use_prelude.iter().collect();
        macro_use.sort_unstable_by(|a, b| a.0.symbol().cmp_order(b.0.symbol()));
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
        self.name_records(names)?;
        self.usize(diagnostics.len())?;
        for it in diagnostics {
            self.diagnostic(it)?;
        }
        Ok(())
    }

    fn name_records(&mut self, it: &NameRecords) -> R {
        let NameRecords {
            uses,
            unresolved,
            glob_targets,
            macro_calls,
            derive_helpers,
            not_introducing,
        } = it;
        self.usize(uses.len())?;
        for &Use { import, ref name, ns, used } in uses {
            self.import_or_extern_crate(import)?;
            self.name(name)?;
            self.namespace(ns);
            self.u8(match used {
                Used::Scope => 0,
                Used::Other => 1,
            });
        }
        self.usize(unresolved.len())?;
        for &it in unresolved {
            self.import_or_extern_crate(it)?;
        }
        self.usize(glob_targets.len())?;
        for &(GlobId { use_, idx }, module) in glob_targets {
            self.import(use_, idx)?;
            self.module(module)?;
        }
        self.usize(macro_calls.len())?;
        for &MacroCallRecord { call, def, via } in macro_calls {
            self.call(call)?;
            self.macro_id(def)?;
            match via {
                MacroVia::Textual => self.u8(0),
                MacroVia::Path(import) => {
                    self.u8(1);
                    self.option(import, Self::import_or_extern_crate)?;
                }
                MacroVia::MacroUse(extern_crate) => {
                    self.u8(2);
                    self.option(extern_crate, Self::extern_crate)?;
                }
                MacroVia::Prelude => self.u8(3),
            }
        }
        self.usize(derive_helpers.len())?;
        for &DeriveHelperUse { item, attr, derive, before_derive } in derive_helpers {
            self.ast_id(item)?;
            self.u32(attr.item_tree_index());
            self.macro_id(derive)?;
            self.bool(before_derive);
        }
        self.usize(not_introducing.len())?;
        for &it in not_introducing {
            self.extern_crate(it)?;
        }
        Ok(())
    }

    fn namespace(&mut self, it: Namespace) {
        self.u8(match it {
            Namespace::Types => 0,
            Namespace::Values => 1,
            Namespace::Macros => 2,
        });
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
    db: &'a Db,
    r: Reader<'a>,
    krate: Crate,
    closure: Vec<Crate>,
    /// The files of each crate of the closure by their path from its root, read when first
    /// needed.
    paths: HashMap<u32, (String, bool, base_db::SourceRootId)>,
    files: Vec<FileId>,
    calls: Vec<MacroCallId>,
    contexts: Vec<SyntaxContext>,
    /// The modules of the crate being read, in its `DefMap`'s order.
    own: Vec<ModuleId>,
    /// Reading the crate's own `DefMap`, whose modules are `own`.
    in_def_map: bool,
    modules: HashMap<u32, Vec<ModuleId>>,
}

impl<'a> Dec<'a> {
    fn new(db: &'a Db, krate: Crate, bytes: &'a [u8]) -> Self {
        Dec {
            db,
            r: Reader::new(bytes),
            krate,
            closure: closure(db, krate),
            paths: HashMap::default(),
            // alloc: the decoded map's own storage, once per crate served
            files: Vec::new(),
            // alloc: the decoded map's own storage, once per crate served
            calls: Vec::new(),
            // alloc: the decoded map's own storage, once per crate served
            contexts: Vec::new(),
            // alloc: the decoded map's own storage, once per crate served
            own: Vec::new(),
            in_def_map: false,
            modules: HashMap::default(),
        }
    }

    /// Reads `bytes` [`Enc::finish`] gave of values of `krate`, from the files they name.
    pub fn new_checked(db: &'a Db, krate: Crate, bytes: &'a [u8]) -> R<Self> {
        let mut dec = Dec::new(db, krate, bytes);
        dec.files()?;
        Ok(dec)
    }

    /// A literal's text with each `OUT_DIR` [`Enc::unplaced`] took out as this database has it.
    fn placed(&self, text: Symbol) -> R<Symbol> {
        if !text.as_str().contains('\0') {
            return Ok(text);
        }
        // alloc: the decoded map's own storage, once per crate served
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
        match self.r.left() {
            0 => Ok(()),
            // alloc: error path
            left => Err(format!("{left} bytes left over")),
        }
    }
}

fn decode(db: &Db, krate: Crate, bytes: &[u8]) -> R<(DefMap, LocalDefMap)> {
    let mut dec = Dec::new(db, krate, bytes);
    dec.files()?;
    dec.in_def_map = true;
    let mut def_map = dec.def_map()?;
    let local = dec.local()?;
    dec.finish()?;
    def_map.hand_over_scopes(db);
    Ok((def_map, local))
}

impl Dec<'_> {
    pub fn u8(&mut self) -> R<u8> {
        Ok(self.r.u8()?)
    }

    pub fn u32(&mut self) -> R<u32> {
        Ok(self.r.uleb32()?)
    }

    pub fn len(&mut self) -> R<usize> {
        Ok(self.u32()? as usize)
    }

    pub fn bool(&mut self) -> R<bool> {
        Ok(self.u8()? != 0)
    }

    pub fn str(&mut self) -> R<&str> {
        let len = self.len()?;
        let bytes = self.r.take(len)?;
        // alloc: error path
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
        // alloc: error path
        Edition::iter().find(|e| *e as u8 == it).ok_or_else(|| format!("edition {it}"))
    }

    pub fn krate(&mut self) -> R<Crate> {
        let index = self.len()?;
        // alloc: error path
        self.closure.get(index).copied().ok_or_else(|| format!("crate {index}"))
    }

    pub fn index<T: Copy>(&mut self, all: &[T]) -> R<T> {
        let index = self.len()?;
        // alloc: error path
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
            // alloc: the decoded map's own storage, once per crate served
            let path = self.str()?.to_owned();
            let written = match self.bool()? {
                true => Some(self.r.u128()?),
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
                    // alloc: error path
                    format!("{}{rest}", out_dir(self.db, krate).ok_or("a crate with no OUT_DIR")?)
                }
                None => joined(dir, &path),
            };
            let source_root = self.db.source_root(*source_root).source_root(self.db);
            // clone: the decoded map's own storage, once per crate served
            let found = source_root.file_for_path(&vfs_path(*real, full.clone())).copied();
            let Some(found) = found else {
                let anchor = krate.data(self.db).root_file_id;
                let path = if path.starts_with(OUT) { &full } else { &path };
                self.db.include_source(base_db::AnchoredPath { anchor, path }, krate);
                // alloc: error path
                not_loaded.get_or_insert_with(|| format!("{full} is not loaded"));
                continue;
            };
            if let Some(written) = written
                && hash128(self.db.file_text(found).text(self.db).as_bytes()) != written
            {
                // alloc: error path
                return Err(format!("{full} is not the file that was written"));
            }
            self.files.push(found);
        }
        not_loaded.map_or(Ok(()), Err)
    }

    pub(crate) fn file(&mut self) -> R<base_db::EditionedFileId> {
        let edition = self.edition()?;
        let index = self.len()?;
        let file = *self.files.get(index).ok_or("a file not in the header")?;
        Ok(base_db::EditionedFileId::new(self.db, file, edition))
    }

    pub(crate) fn hir_file(&mut self) -> R<HirFileId> {
        match self.u8()? {
            0 => Ok(HirFileId::FileId(self.file()?)),
            1 => Ok(HirFileId::MacroFile(self.call()?)),
            // alloc: error path
            it => Err(format!("hir file tag {it}")),
        }
    }

    pub(crate) fn ast_id<N: syntax::AstNode>(&mut self) -> R<AstId<N>> {
        let file = self.hir_file()?;
        let raw = ErasedFileAstId::from_raw(self.u32()?);
        Ok(AstId::new(file, FileAstId::from_erased_unchecked(raw)))
    }

    fn erased_ast_id(&mut self) -> R<ErasedFileAstId> {
        Ok(ErasedFileAstId::from_raw(self.u32()?))
    }

    pub(crate) fn call(&mut self) -> R<MacroCallId> {
        match self.u8()? {
            SEEN => {
                let index = self.len()?;
                // alloc: error path
                self.calls.get(index).copied().ok_or_else(|| "a macro call not read".to_owned())
            }
            NEW => {
                let loc = self.call_loc()?;
                let include = includes(&loc);
                let call = MacroCallId::new(self.db, loc);
                if include {
                    let written = self.file()?;
                    if call.loc(self.db).include_file_id(self.db, call) != Some(written) {
                        // alloc: error path
                        return Err("an include! of another file than was written".to_owned());
                    }
                }
                self.calls.push(call);
                Ok(call)
            }
            // alloc: error path
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
                    // alloc: error path
                    it => return Err(format!("expand-to {it}")),
                };
                let eager = match self.u8()? {
                    0 => None,
                    _ => {
                        let arg = self.subtree()?;
                        let arg_id = self.call()?;
                        let span = self.span()?;
                        Some(EagerCallInfo { arg, arg_id, error: None, span }.into())
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
                    _ => Some(self.subtree()?.into()),
                };
                let n = self.len()?;
                let mut ids = Vec::with_capacity(n);
                for _ in 0..n {
                    ids.push(AttrId::from_item_tree_index(self.u32()?));
                }
                let censored_attr_ids = hir_expand::AttrMacroAttrIds::from_many(&ids);
                MacroCallKind::Attr { ast_id, attr_args, censored_attr_ids }
            }
            // alloc: error path
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
            // alloc: error path
            it => return Err(format!("macro def kind {it}")),
        };
        Ok(MacroDefId { krate, edition, kind, local_inner, allow_internal_unsafe, edition_panic })
    }

    pub(crate) fn context(&mut self) -> R<SyntaxContext> {
        match self.u8()? {
            0 => Ok(SyntaxContext::root(self.edition()?)),
            1 => {
                let outer = self.call()?;
                let transparency = match self.u8()? {
                    0 => Transparency::Transparent,
                    1 => Transparency::SemiOpaque,
                    2 => Transparency::Opaque,
                    // alloc: error path
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
                // alloc: error path
                self.contexts.get(index).copied().ok_or_else(|| "a context not read".to_owned())
            }
            // alloc: error path
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
                        // alloc: error path
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
                    // alloc: error path
                    let kind = LitKind::from_code(kind, hashes).ok_or_else(|| format!("literal kind {kind}"))?;
                    let suffix_len = self.u32()?;
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
                        // alloc: error path
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
                // alloc: error path
                it => return Err(format!("token tag {it}")),
            };
            tokens.push(token);
        }
        if !matches!(tokens.first(), Some(TokenTree::Subtree(_))) {
            // alloc: error path
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
            // alloc: error path
            return module.ok_or_else(|| format!("module {index} of a block"));
        }
        let owner = self.u32()?;
        let index = self.len()?;
        if owner == 0 && self.in_def_map {
            // alloc: error path
            return self.own.get(index).copied().ok_or_else(|| format!("own module {index}"));
        }
        if !self.modules.contains_key(&owner) {
            let krate = *self.closure.get(owner as usize).ok_or("a module's crate")?;
            let table = super::crate_def_map(self.db, krate).table(self.db);
            // alloc: the decoded map's own storage, once per crate served
            self.modules.insert(owner, table.keys().copied().collect());
        }
        // alloc: error path
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
                // alloc: error path
                _ => return Err("a lang item that is no item".to_owned()),
            },
            2 => self.variant()?.into(),
            3 => self.impl_id()?.into(),
            4 => self.macro_id()?.into(),
            // alloc: error path
            it => return Err(format!("lang item {it}")),
        })
    }

    pub fn body_owner(&mut self) -> R<DefWithBodyId> {
        Ok(match self.u8()? {
            0 => match self.def()? {
                ModuleDefId::FunctionId(it) => it.into(),
                ModuleDefId::StaticId(it) => it.into(),
                ModuleDefId::ConstId(it) => it.into(),
                // alloc: error path
                _ => return Err("a body of an item with none".to_owned()),
            },
            1 => DefWithBodyId::VariantId(self.variant()?),
            // alloc: error path
            it => return Err(format!("body {it}")),
        })
    }

    fn visibility(&mut self) -> R<Visibility> {
        Ok(match self.u8()? {
            0 => Visibility::Module(self.module()?, VisibilityExplicitness::Explicit),
            1 => Visibility::Module(self.module()?, VisibilityExplicitness::Implicit),
            2 => Visibility::PubCrate(self.krate()?),
            3 => Visibility::Public,
            // alloc: error path
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
            // alloc: error path
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
            // alloc: error path
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
            // alloc: error path
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
            // alloc: error path
            it => return Err(format!("macro {it}")),
        })
    }

    fn proc_macro(&mut self) -> R<(CustomProcMacroExpander, ProcMacroKind)> {
        let listed = self.u32()?;
        if listed >= !2 {
            // alloc: error path
            return Err(format!("procedural macro {listed}"));
        }
        let kind = match self.u8()? {
            0 => ProcMacroKind::CustomDerive,
            1 => ProcMacroKind::Bang,
            2 => ProcMacroKind::Attr,
            // alloc: error path
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
            // alloc: error path
            it => return Err(format!("def {it}")),
        })
    }

    pub fn variant(&mut self) -> R<EnumVariantId> {
        let id = self.ast_id()?;
        let AdtId::EnumId(parent) = self.adt()? else {
            // alloc: error path
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
            // alloc: error path
            it => return Err(format!("import {it}")),
        })
    }

    fn import_or_glob(&mut self) -> R<ImportOrGlob> {
        let tag = self.u8()?;
        let (use_, idx) = self.import()?;
        Ok(match tag {
            0 => ImportOrGlob::Glob(GlobId { use_, idx }),
            1 => ImportOrGlob::Import(ImportId { use_, idx }),
            // alloc: error path
            it => return Err(format!("import {it}")),
        })
    }

    fn item_in_ns(&mut self) -> R<ItemInNs> {
        Ok(match self.u8()? {
            0 => ItemInNs::Types(self.def()?),
            1 => ItemInNs::Values(self.def()?),
            2 => ItemInNs::Macros(self.macro_id()?),
            // alloc: a decode error, the edge.
            it => return Err(format!("item in namespace {it}")),
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
            // alloc: error path
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

    /// A list's written length, and as much room as the bytes left can fill: every entry is at
    /// least a byte, so a length the bytes cannot hold reserves no more than they could.
    fn len_and_room(&mut self) -> R<(usize, usize)> {
        let n = self.len()?;
        Ok((n, n.min(self.r.left())))
    }

    pub fn many<T>(&mut self, mut f: impl FnMut(&mut Self) -> R<T>) -> R<Vec<T>> {
        let (n, room) = self.len_and_room()?;
        let mut all = Vec::with_capacity(room);
        for _ in 0..n {
            all.push(f(self)?);
        }
        Ok(all)
    }

    /// Reads a list into `into`, which grows once to the written length.
    fn fill<C: Extend<T>, T>(
        &mut self,
        into: &mut C,
        reserve: impl FnOnce(&mut C, usize),
        mut f: impl FnMut(&mut Self) -> R<T>,
    ) -> R {
        let (n, room) = self.len_and_room()?;
        reserve(into, room);
        for _ in 0..n {
            into.extend([f(self)?]);
        }
        Ok(())
    }

    /// Reads a list of up to `N` entries without allocating, the rest onto the heap once.
    fn small<T, const N: usize>(&mut self, f: impl FnMut(&mut Self) -> R<T>) -> R<SmallVec<[T; N]>>
    where
        [T; N]: smallvec::Array<Item = T>,
    {
        let mut all = SmallVec::new();
        self.fill(&mut all, SmallVec::reserve_exact, f)?;
        Ok(all)
    }

    /// A scope read straight into its columns; the name index is built once, at the end.
    fn scope(&mut self) -> R<ItemScope> {
        let mut scope = ItemScope::default();
        let (n, room) = self.len_and_room()?;
        scope.types.reserve(room);
        for _ in 0..n {
            let name = self.name()?;
            let def = self.def()?;
            let vis = self.visibility()?;
            let import = self.option(Self::import_or_extern_crate)?;
            scope.types.push_unindexed(name, Item { def, vis, import });
        }
        let (n, room) = self.len_and_room()?;
        scope.values.reserve(room);
        for _ in 0..n {
            let name = self.name()?;
            let def = self.def()?;
            let vis = self.visibility()?;
            let import = self.option(Self::import_or_glob)?;
            scope.values.push_unindexed(name, Item { def, vis, import });
        }
        let (n, room) = self.len_and_room()?;
        scope.macros.reserve(room);
        for _ in 0..n {
            let name = self.name()?;
            let def = self.macro_id()?;
            let vis = self.visibility()?;
            let import = self.option(Self::import_or_extern_crate)?;
            scope.macros.push_unindexed(name, Item { def, vis, import });
        }
        self.fill(&mut scope.unresolved, ThinVec::reserve_exact, Self::name)?;
        self.fill(&mut scope.declarations, ThinVec::reserve_exact, Self::def)?;
        let (n, room) = self.len_and_room()?;
        scope.impls.reserve_exact(room);
        for at in 0..n {
            scope.impls.push(self.impl_id()?);
            scope.trait_impl.set_growing(at, self.bool()?);
            scope.reservation_impl.set_growing(at, self.bool()?);
        }
        self.fill(
            &mut scope.builtin_derive_impls,
            ThinVec::reserve_exact,
            Self::builtin_derive_impl,
        )?;
        self.fill(&mut scope.extern_blocks, ThinVec::reserve_exact, Self::extern_block)?;
        self.fill(&mut scope.unnamed_consts, ThinVec::reserve_exact, |dec| match dec.def()? {
            ModuleDefId::ConstId(it) => Ok(it),
            // alloc: error path
            _ => Err("an unnamed const that is not one".to_owned()),
        })?;
        self.fill(&mut scope.unnamed_trait_imports, ThinVec::reserve_exact, |dec| {
            let ModuleDefId::TraitId(it) = dec.def()? else {
                // alloc: error path
                return Err("a trait import of no trait".to_owned());
            };
            let vis = dec.visibility()?;
            let import = dec.option(|dec| dec.import_or_extern_crate())?;
            Ok((it, Item { def: (), vis, import }))
        })?;
        self.fill(&mut scope.underscore_imports, ThinVec::reserve_exact, |dec| {
            let (use_, idx) = dec.import()?;
            let def = dec.option(Self::def)?;
            let vis = dec.visibility()?;
            Ok(UnderscoreImport { import: ImportId { use_, idx }, def, vis })
        })?;
        self.fill(&mut scope.glob_ambiguities, ThinVec::reserve_exact, |dec| {
            let name = dec.name()?;
            let other = dec.item_in_ns()?;
            let (use_, idx) = dec.import()?;
            let vis = dec.visibility()?;
            Ok(GlobAmbiguity { name, other, glob: GlobId { use_, idx }, vis })
        })?;
        self.fill(&mut scope.shadowed_imports, ThinVec::reserve_exact, |dec| {
            let import = dec.import_or_extern_crate()?;
            let name = dec.name()?;
            let item = dec.item_in_ns()?;
            let vis = dec.visibility()?;
            Ok(ShadowedImport { import, name, item, vis })
        })?;
        let (n, room) = self.len_and_room()?;
        scope.use_imports_types.reserve(room);
        for _ in 0..n {
            let (k, v) = (self.import_or_extern_crate()?, self.import_or_def()?);
            scope.use_imports_types.push_unindexed(k, v);
        }
        let (n, room) = self.len_and_room()?;
        scope.use_imports_values.reserve(room);
        for _ in 0..n {
            let (k, v) = (self.import_or_glob()?, self.import_or_def()?);
            scope.use_imports_values.push_unindexed(k, v);
        }
        let (n, room) = self.len_and_room()?;
        scope.use_imports_macros.reserve(room);
        for _ in 0..n {
            let (k, v) = (self.import_or_extern_crate()?, self.import_or_def()?);
            scope.use_imports_macros.push_unindexed(k, v);
        }
        self.fill(&mut scope.use_decls, ThinVec::reserve_exact, Self::use_id)?;
        self.fill(&mut scope.extern_crate_decls, ThinVec::reserve_exact, Self::extern_crate)?;
        let (n, room) = self.len_and_room()?;
        scope.legacy_macros.reserve(room);
        for _ in 0..n {
            let name = self.name()?;
            let ids = self.small(Self::macro_id)?;
            scope.legacy_macros.push_unindexed(name, ids);
        }
        let (n, room) = self.len_and_room()?;
        scope.attr_macros.reserve(room);
        for _ in 0..n {
            let (k, v) = (self.ast_id()?, self.call()?);
            scope.attr_macros.push_unindexed(k, v);
        }
        let (n, room) = self.len_and_room()?;
        scope.macro_invocations.reserve(room);
        for _ in 0..n {
            let (k, v) = (self.ast_id()?, self.call()?);
            scope.macro_invocations.push_unindexed(k, v);
        }
        let (n, room) = self.len_and_room()?;
        scope.derive_macros.reserve(room);
        for _ in 0..n {
            let ast_id = self.ast_id()?;
            let invocations = self.small(|dec| {
                let attr_id = AttrId::from_item_tree_index(dec.u32()?);
                let attr_call_id = dec.call()?;
                let derive_call_ids = dec.small(|dec| {
                    Ok(match dec.u8()? {
                        0 => None,
                        1 => Some(Either::Left(dec.call()?)),
                        2 => Some(Either::Right(dec.builtin_derive_impl()?)),
                        // alloc: error path
                        it => return Err(format!("derive call {it}")),
                    })
                })?;
                Ok(DeriveMacroInvocation { attr_id, attr_call_id, derive_call_ids })
            })?;
            scope.derive_macros.push_unindexed(ast_id, invocations);
        }
        scope.seal().map_err(str::to_owned)?;
        Ok(scope)
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
            // alloc: error path
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
            // alloc: error path
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
            // alloc: the decoded map's own storage, once per crate served
            5 => CfgExpr::Not(Box::new(self.cfg()?)),
            // alloc: error path
            it => return Err(format!("cfg {it}")),
        })
    }

    fn diagnostic(&mut self) -> R<DefDiagnostic> {
        let in_module = self.module()?;
        let kind = match self.u8()? {
            0 => {
                let ast = self.ast_id()?;
                // alloc: the decoded map's own storage, once per crate served
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
                DefDiagnosticKind::UnconfiguredCode { ast_id, cfg }
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
                // alloc: the decoded map's own storage, once per crate served
                DefDiagnosticKind::MacroDefError { ast, message: self.str()?.to_owned() }
            }
            9 => {
                let ast = self.ast_id()?;
                let path = self.mod_path()?;
                let err = match self.u8()? {
                    0 => ExpandErrorKind::ProcMacroAttrExpansionDisabled,
                    1 => ExpandErrorKind::MissingProcMacroExpander { krate: self.krate()?, why: self.str()?.into() },
                    2 => ExpandErrorKind::MacroDisabled,
                    3 => ExpandErrorKind::MacroDefinition,
                    4 => ExpandErrorKind::RecursionOverflow,
                    5 => ExpandErrorKind::Other(self.str()?.into()),
                    6 => ExpandErrorKind::BuiltinDerive(self.str()?.into()),
                    7 => ExpandErrorKind::DefaultNoVariant,
                    8 => {
                        let message = self.str()?.into();
                        let code = self.option(|dec| {
                            // alloc: error path
                            u16::try_from(dec.u32()?).map_err(|_| "an error code over u16".to_owned())
                        })?;
                        let also_at_len = self.len()?;
                        let also_at = (0..also_at_len)
                            .map(|_| self.span())
                            .collect::<R<Box<[Span]>>>()?;
                        let children_len = self.len()?;
                        let children = (0..children_len)
                            .map(|_| {
                                let kind = match self.bool()? {
                                    true => BuiltinChild::Help,
                                    false => BuiltinChild::Note,
                                };
                                Ok((kind, Box::<str>::from(self.str()?)))
                            })
                            .collect::<R<Box<[_]>>>()?;
                        let derive = self.bool()?;
                        // alloc: the decoded map's own storage, once per crate served
                        ExpandErrorKind::Builtin(Box::new(BuiltinError {
                            message,
                            code,
                            also_at,
                            children,
                            derive,
                        }))
                    }
                    9 => ExpandErrorKind::ProcMacroPanic(self.str()?.into()),
                    10 => ExpandErrorKind::Mbe(self.mbe_error()?),
                    // alloc: error path
                    it => return Err(format!("macro error {it}")),
                };
                DefDiagnosticKind::MacroError { ast, path, err }
            }
            10 => {
                let id = self.ast_id()?;
                let index = Idx::from_raw(RawIdx::from_u32(self.u32()?));
                let why = match self.u8()? {
                    0 => TraitItemImport::Unstable,
                    1 => TraitItemImport::ItemsFromMacros,
                    // alloc: error path
                    it => return Err(format!("trait item import {it}")),
                };
                DefDiagnosticKind::TraitItemImport { id, index, why }
            }
            11 => {
                let krate = self.name()?;
                let error = match self.u8()? {
                    0 => PreludeError::CrateNotFound,
                    1 => PreludeError::NoPreludeModule,
                    // alloc: error path
                    it => return Err(format!("prelude error {it}")),
                };
                DefDiagnosticKind::Prelude { krate, error }
            }
            // alloc: error path
            it => return Err(format!("diagnostic {it}")),
        };
        Ok(DefDiagnostic { in_module, kind })
    }

    fn mbe_error(&mut self) -> R<MbeErrorKind> {
        Ok(match self.u8()? {
            // alloc: the decoded map's own storage, once per crate served
            0 => MbeErrorKind::BindingError(Box::new(self.str()?.into())),
            // alloc: the decoded map's own storage, once per crate served
            1 => MbeErrorKind::UnresolvedBinding(Box::new(self.str()?.into())),
            2 => MbeErrorKind::LeftoverTokens,
            3 => MbeErrorKind::LimitExceeded,
            4 => MbeErrorKind::NoMatchingRule,
            5 => MbeErrorKind::UnexpectedToken,
            6 => {
                let token = self.option(|dec| Ok(Box::<str>::from(dec.str()?)))?;
                let remaining = self.str()?.into();
                let remaining_span = self.option(Self::span)?;
                let missing_comma = self.option(Self::span)?;
                // alloc: the decoded map's own storage, once per crate served
                MbeErrorKind::MatchFailure(Box::new(MatchFailure {
                    token,
                    remaining,
                    remaining_span,
                    missing_comma,
                }))
            }
            // alloc: error path
            it => return Err(format!("macro_rules! error {it}")),
        })
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
        let (n, room) = self.len_and_room()?;
        let mut skeleton = Vec::with_capacity(room);
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
            // clone: the decoded map's own storage, once per crate served
            let name = name.clone().ok_or("a module with no name")?;
            let module = super::facts::queries::child_module(db, krate, None, parent, name);
            self.own.push(module);
        }
        let mut table = super::ModulesMap::new();
        for (index, (parent, _)) in skeleton.iter().enumerate() {
            let origin = self.origin()?;
            let visibility = self.visibility()?;
            let scope = self.scope()?;
            let (n, room) = self.len_and_room()?;
            let mut children = FxIndexMap::with_capacity_and_hasher(room, Default::default());
            for _ in 0..n {
                let name = self.name()?;
                let child = self.len()?;
                children.insert(name, *self.own.get(child).ok_or("a child not read")?);
            }
            let parent = parent.map(|it| self.own[it as usize]);
            let module = ModuleData::collected(
                self.db,
                self.own[index],
                origin,
                visibility,
                parent,
                scope,
                children,
            );
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
            // alloc: the decoded map's own storage, once per crate served
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
                        // alloc: error path
                        it => return Err(format!("derive helper {it}")),
                    };
                    Ok((name, id, call))
                })?;
                Ok((ast_id, helpers))
            })?
            .into_iter()
            // alloc: the decoded map's own storage, once per crate served
            .collect();
        let exported = self.many(|dec| {
            let id = dec.macro_id()?;
            Ok((id, dec.many(Self::name)?.into_boxed_slice()))
        })?;
        let functions = self.many(|dec| {
            let ModuleDefId::FunctionId(function) = dec.def()? else {
                // alloc: error path
                return Err("a procedural macro of no function".to_owned());
            };
            let MacroId::ProcMacroId(id) = dec.macro_id()? else {
                // alloc: error path
                return Err("a function of a macro no procedural one".to_owned());
            };
            Ok((function, id))
        })?;
        let data = TArc::get_mut(&mut def_map.data).ok_or("the def map's crate data is shared")?;
        // alloc: the decoded map's own storage, once per crate served
        data.exported_derives = exported.into_iter().collect();
        // alloc: the decoded map's own storage, once per crate served
        data.fn_proc_macro_mapping_back = functions.iter().map(|&(f, id)| (id, f)).collect();
        // alloc: the decoded map's own storage, once per crate served
        data.fn_proc_macro_mapping = functions.into_iter().collect();
        def_map.names = self.name_records()?;
        def_map.diagnostics = self.many(Self::diagnostic)?;
        Ok(def_map)
    }

    fn name_records(&mut self) -> R<NameRecords> {
        let uses = self.many(|dec| {
            let import = dec.import_or_extern_crate()?;
            let name = dec.name()?;
            let ns = dec.namespace()?;
            let used = match dec.u8()? {
                0 => Used::Scope,
                1 => Used::Other,
                // alloc: a decode error, the edge.
                it => return Err(format!("use {it}")),
            };
            Ok(Use { import, name, ns, used })
        })?;
        let unresolved = self.many(Self::import_or_extern_crate)?;
        let glob_targets = self.many(|dec| {
            let (use_, idx) = dec.import()?;
            Ok((GlobId { use_, idx }, dec.module()?))
        })?;
        let macro_calls = self.many(|dec| {
            let call = dec.call()?;
            let def = dec.macro_id()?;
            let via = match dec.u8()? {
                0 => MacroVia::Textual,
                1 => MacroVia::Path(dec.option(Self::import_or_extern_crate)?),
                2 => MacroVia::MacroUse(dec.option(Self::extern_crate)?),
                3 => MacroVia::Prelude,
                // alloc: a decode error, the edge.
                it => return Err(format!("macro found through {it}")),
            };
            Ok(MacroCallRecord { call, def, via })
        })?;
        let derive_helpers = self.many(|dec| {
            let item = dec.ast_id()?;
            let attr = AttrId::from_item_tree_index(dec.u32()?);
            let derive = dec.macro_id()?;
            let before_derive = dec.bool()?;
            Ok(DeriveHelperUse { item, attr, derive, before_derive })
        })?;
        let not_introducing = self.many(Self::extern_crate)?;
        Ok(NameRecords {
            uses: uses.into_boxed_slice(),
            unresolved: unresolved.into_boxed_slice(),
            glob_targets: glob_targets.into_boxed_slice(),
            macro_calls: macro_calls.into_boxed_slice(),
            derive_helpers: derive_helpers.into_boxed_slice(),
            not_introducing: not_introducing.into_boxed_slice(),
        })
    }

    fn namespace(&mut self) -> R<Namespace> {
        Ok(match self.u8()? {
            0 => Namespace::Types,
            1 => Namespace::Values,
            2 => Namespace::Macros,
            // alloc: a decode error, the edge.
            it => return Err(format!("namespace {it}")),
        })
    }

    fn local(&mut self) -> R<LocalDefMap> {
        let extern_prelude = self.many(|dec| {
            let name = dec.name()?;
            let module = dec.module()?;
            Ok((name, (module, dec.option(Self::extern_crate)?)))
        })?;
        // alloc: the decoded map's own storage, once per crate served
        Ok(LocalDefMap { extern_prelude: extern_prelude.into_iter().collect() })
    }
}

#[cfg(test)]
mod tests {
    use hir_expand::fixture;

    use super::*;
    use crate::test_db::TestDB;

    const FIXTURE: &str = r#"
//- /main.rs crate:main deps:lib
#[macro_use]
extern crate lib;
use lib::{Shown, shown};
mod inner;
#[derive(Clone, Debug)]
struct Local(u8);
fn main() { shown!(); }
#[cfg(any())]
fn left_out() {}
mod globs {
    pub mod a { pub fn x() {} }
    pub mod b { pub fn x() {} }
    pub use a::*;
    pub use b::*;
}
pub use globs::*;
pub use lib::deep::deeper as d;
pub use d::f;
pub struct Shadowing;
pub use lib::Shown as Shadowing;
mod reex { pub(crate) use crate::inner::T as _; }
use reex::*;

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
        let db = fixture::db(FIXTURE).into_db();
        ["lib", "main"]
            .into_iter()
            .map(|name| (name, encode(&db, crate_named(&db, name)).unwrap()))
            .collect()
    }

    fn with(mode: Mode, bytes: &[(&'static str, Vec<u8>)]) -> TestDB {
        let mut db = fixture::db(FIXTURE).into_db();
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

    /// The names `script`'s encoder gives its own root and its dependency's, two crates of one
    /// source root, with `script` at `at`.
    fn names_with_script_at(at: &str) -> [(u32, String); 2] {
        let text = format!("//- {at} crate:script deps:dep\n//- /dep.rs crate:dep\npub fn f() {{}}\n");
        let db = fixture::db(&text).into_db();
        let (script, dep) = (crate_named(&db, "script"), crate_named(&db, "dep"));
        let mut enc = Enc::new(&db, script);
        [script, dep].map(|it| enc.file_name(it.data(&db).root_file_id).unwrap())
    }

    #[test]
    fn a_file_is_named_from_the_nearest_crate_of_its_source_root() {
        let names = names_with_script_at("/build.rs");
        assert_eq!(names, [(0, "build.rs".to_owned()), (1, "dep.rs".to_owned())]);
        assert_eq!(names_with_script_at("/tools/gen/build.rs"), names, "moving one crate renames none");
    }

    #[test]
    fn read_back_writes_the_same_bytes() {
        let _turn = TURN.lock();
        let bytes = frozen();
        let db = with(Mode::Verify, &bytes);
        for (name, written) in &bytes {
            let krate = crate_named(&db, name);
            _ = super::super::crate_def_map(&db, krate);
            assert_eq!(encode(&db, krate).as_ref(), Ok(written), "{name}");
        }
        assert_eq!(differences(), Vec::<String>::new());
    }

    /// The bytes of each module's scope, which name everything by what it is, so two databases'
    /// scopes compare by them.
    fn scope_bytes(db: &TestDB, name: &str) -> Vec<Vec<u8>> {
        let krate = crate_named(db, name);
        let def_map = super::super::crate_def_map(db, krate);
        let table = def_map.table(db);
        let modules: HashMap<ModuleId, (u32, u32)> =
            table.keys().enumerate().map(|(index, &it)| (it, (0, index as u32))).collect();
        (table.iter())
            .map(|(_, module)| {
                let mut enc = Enc::new(db, krate);
                enc.modules = modules.clone();
                enc.indexed.insert(krate);
                enc.scope(module.scope(db)).unwrap();
                enc.out
            })
            .collect()
    }

    /// A builtin macro's error, a proc macro's panic and `macro_rules!` errors read back field
    /// for field.
    #[test]
    fn a_builtin_macro_error_reads_back_whole() {
        let db = fixture::db(FIXTURE).into_db();
        let krate = crate_named(&db, "main");
        let def_map = super::super::crate_def_map(&db, krate);
        let root = def_map.root;
        let local = Name::new_symbol_root(Symbol::intern("Local"));
        let found = def_map.module(&db, root).scope(&db).get(&local).types.unwrap().def;
        let ModuleDefId::AdtId(AdtId::StructId(local)) = found else { panic!("{found:?}") };
        let ast: AstId<ast::Item> = local.lookup(&db).id.upcast();
        let span = ast.file_id.span_map(&db).span_for_range(ast.to_range(&db));
        let m = Name::new_symbol_root(Symbol::intern("m"));
        let path = ModPath::from_segments(PathKind::Plain, [m]);
        let builtin = ExpandErrorKind::Builtin(Box::new(BuiltinError {
            message: "expected a literal".into(),
            code: Some(802),
            also_at: Box::new([span, span]),
            children: Box::new([
                (BuiltinChild::Note, Box::<str>::from("a note")),
                (BuiltinChild::Help, Box::<str>::from("a help")),
            ]),
            derive: true,
        }));
        let panicked = ExpandErrorKind::ProcMacroPanic("it panicked".into());
        let unmatched = ExpandErrorKind::Mbe(MbeErrorKind::MatchFailure(Box::new(MatchFailure {
            token: Some("`x`".into()),
            remaining: "`,`".into(),
            remaining_span: Some(span),
            missing_comma: None,
        })));
        let at_end = ExpandErrorKind::Mbe(MbeErrorKind::MatchFailure(Box::new(MatchFailure {
            token: None,
            remaining: "end of macro".into(),
            remaining_span: None,
            missing_comma: Some(span),
        })));
        let binding = ExpandErrorKind::Mbe(MbeErrorKind::BindingError(Box::new("a binding".into())));
        let no_rule = ExpandErrorKind::Mbe(MbeErrorKind::NoMatchingRule);
        let diagnostics = [
            DefDiagnostic::macro_error(root, ast, path.clone(), builtin),
            DefDiagnostic::macro_error(root, ast, path.clone(), panicked),
            DefDiagnostic::macro_error(root, ast, path.clone(), unmatched),
            DefDiagnostic::macro_error(root, ast, path.clone(), at_end),
            DefDiagnostic::macro_error(root, ast, path.clone(), binding),
            DefDiagnostic::macro_error(root, ast, path, no_rule),
        ];
        let mut enc = Enc::new(&db, krate);
        for it in &diagnostics {
            enc.diagnostic(it).unwrap();
        }
        let bytes = enc.finish().unwrap();
        let mut dec = Dec::new_checked(&db, krate, &bytes).unwrap();
        for it in &diagnostics {
            assert_eq!(&dec.diagnostic().unwrap(), it);
        }
        dec.finish().unwrap();
    }

    /// The fixture has what the records hold, so the bytes above cover them.
    #[test]
    fn the_name_records_read_back() {
        let bytes = frozen();
        let live = fixture::db(FIXTURE).into_db();
        let served = with(Mode::Serve, &bytes);
        fn main(db: &TestDB) -> &super::super::DefMap {
            super::super::crate_def_map(db, crate_named(db, "main"))
        }
        let records = main(&live).name_records(&live);
        assert!(!records.uses.is_empty() && !records.glob_targets.is_empty());
        assert!(!records.macro_calls.is_empty() && !records.not_introducing.is_empty());
        let scopes = |db: &TestDB| {
            let def_map = main(db);
            let root = def_map.module(db, def_map.root).scope(db);
            let globs: usize =
                def_map.modules(db).map(|(_, it)| it.scope(db).glob_ambiguities().len()).sum();
            (root.shadowed_imports().len(), globs)
        };
        // `Shadowing` in both namespaces, and `x` of `globs`.
        assert_eq!(scopes(&live), (2, 1));
        assert_eq!(scopes(&served), scopes(&live));
        let encoded = |db: &TestDB| encode(db, crate_named(db, "main")).unwrap();
        assert_eq!(encoded(&served), encoded(&live));
    }

    #[test]
    fn each_scope_reads_back_as_it_was_written() {
        let bytes = frozen();
        let live = fixture::db(FIXTURE).into_db();
        let served = with(Mode::Serve, &bytes);
        for name in ["lib", "main"] {
            assert_eq!(scope_bytes(&served, name), scope_bytes(&live, name), "{name}");
        }
    }

    #[test]
    fn read_back_resolves_the_same() {
        let bytes = frozen();
        let live = fixture::db(FIXTURE).into_db();
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
        let db = fixture::db(OUTSIDE).into_db();
        let lib = crate_named(&db, "lib");
        let bytes = encode(&db, lib).unwrap();
        assert_eq!(unreadable(&db, lib, &bytes), None);
        let changed = fixture::db(&OUTSIDE.replace("{}\n", "{}\npub fn more() {}\n")).into_db();
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
        let db = fixture::db(INCLUDED).into_db();
        let lib = crate_named(&db, "lib");
        let bytes = encode(&db, lib).unwrap();
        assert_eq!(unreadable(&db, lib, &bytes), None);
        let mut served = fixture::db(INCLUDED).into_db();
        let frozen = Frozen { bytes: bytes.as_slice().into(), mode: Mode::Serve };
        let served_lib = crate_named(&served, "lib");
        set(&mut served, [(served_lib, frozen)].into_iter().collect());
        for name in ["lib", "main"] {
            let dump =
                |db: &TestDB| super::super::crate_def_map(db, crate_named(db, name)).dump(db);
            assert_eq!(dump(&served), dump(&db), "{name}");
        }
        let changed = fixture::db(&INCLUDED.replace("unroll {", "unroll_more {")).into_db();
        let why = unreadable(&changed, crate_named(&changed, "lib"), &bytes);
        assert!(
            why.as_ref().is_some_and(|it| it.ends_with("is not the file that was written")),
            "{why:?}"
        );
        let missing = fixture::db(INCLUDED.split("//- /lib/out").next().unwrap()).into_db();
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
        let first = fixture::db(WROTE).into_db();
        let other = fixture::db(&WROTE.replace("first", "other")).into_db();
        let bytes: Vec<_> = ["lib", "main"]
            .into_iter()
            .map(|name| (name, encode(&first, crate_named(&first, name)).unwrap()))
            .collect();
        for (name, written) in &bytes {
            let krate = crate_named(&other, name);
            assert_eq!(encode(&other, krate).as_ref(), Ok(written), "{name}");
            assert_eq!(unreadable(&other, krate, written), None, "{name}");
        }
        let mut served = fixture::db(&WROTE.replace("first", "other")).into_db();
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
        let _turn = TURN.lock();
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
