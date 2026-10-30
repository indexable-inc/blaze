//! Compiled functions kept between runs of the daemon by their stable names.
//!
//! An [`Entry`] is a function of a crate that no edit reaches, instantiated with types of such
//! crates, as it was stitched into an object: its code, and what the code refers to by names
//! that mean the same in every run. A build that finds an entry for an instance, and one for
//! everything the entry calls, puts them in the object without asking the database anything
//! about the instance.
//!
//! What an entry imports from rustc is a symbol of the program it was compiled for. Another
//! program may link against a build of the crates without it, so a lookup checks that the
//! symbols are there.

use std::sync::{Arc, Mutex, OnceLock};

use cranelift_codegen::{
    binemit::{CodeOffset, Reloc},
    ir::Signature,
};
use cranelift_module::{ModuleReloc, ModuleRelocTarget};
use rustc_hash::{FxHashMap, FxHashSet};
use serde_derive::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{
    ConstBlob, DataIdentity,
    debuginfo::{FunctionDebug, LineRow, SlotPlaces, TypeSet},
    fragment::Code,
    store::{KEY, Store},
    unwind::FunctionUnwind,
};

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub(super) struct Entry {
    /// What the symbol of the function is made from.
    pub(super) label: String,
    pub(super) funcs: Vec<CachedFunc>,
    /// A blob indexes `funcs` and the data before it, as the blobs of `Requests` do.
    pub(super) datas: Vec<CachedData>,
    pub(super) code: CachedCode,
    /// What a debugger is told about the function, when the build that kept it told one.
    pub(super) debug: Option<FunctionDebug>,
    /// For code of the checkout: what a patch base that has it from the store tells an edit
    /// of it by, without compiling it.
    pub(super) tracked: Option<Tracked>,
}

/// What a patch base knows of an entry of the checkout. See [`super::patch::stable_mark`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(super) struct Tracked {
    /// The stable mark of the code.
    pub(super) mark: u128,
    /// The crate it is compiled as, by the id it is named by, when that is of the checkout.
    pub(super) home: Option<[u8; 32]>,
    /// The files its code comes from, as [`super::stable::StableNames::file_path`] names
    /// them; `None` when any file can be one.
    pub(super) files: Option<Vec<String>>,
    /// The items of those files its code comes from, as they were written then; `None` when
    /// one of them can't be told. An edit of nothing but bodies that leaves each of them as
    /// it was leaves the code as it was.
    pub(super) items: Option<Vec<WrittenItem>>,
}

/// An item of the checkout as code compiled from it saw it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub(super) struct WrittenItem {
    /// As [`super::stable::StableNames::file_path`] names it.
    pub(super) file: String,
    /// Its id in the file, which an edit of bodies alone leaves it.
    pub(super) id: u32,
    /// Of its text.
    pub(super) hash: u64,
    /// The line and column it begins on, for one whose place the code tells, or the one it is
    /// written in when its mark has its lines; `None` for the others.
    pub(super) at: Option<(u32, u32)>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub(super) struct CachedFunc {
    pub(super) target: CachedTarget,
    pub(super) sig: Signature,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub(super) enum CachedTarget {
    /// `checked` when the symbol was one of the program's rustc build or of an object blaze
    /// built before (the standard library's unit), which another program's may lack.
    Import { symbol: String, checked: bool },
    /// The entry of this name, or whatever else the build has for the instance.
    Named(String),
    /// A function of the checkout that is compiled whenever it is called, by its stable name.
    Root(String),
    /// The `TypeId::of` that returns this.
    TypeId(u128),
    /// The function an `asm!` block is assembled as, by its assembly.
    Asm(String),
    /// rustc's copy `symbol` of the function of the checkout named `name`, which the build
    /// wants compiled too, as a call of it in a build that links rustc's code has it.
    Copy { name: String, symbol: String },
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub(super) enum CachedData {
    Blob {
        blob: ConstBlob,
        identity: DataIdentity,
    },
    Vtable {
        key: String,
        blob: ConstBlob,
    },
    /// A static of rustc's by its symbol.
    Static {
        symbol: String,
        thread_local: bool,
    },
    /// A static by its stable name, where its symbol is not what tells whose it is: one of
    /// the checkout is rustc's or ours as the build has it, and rustc exports no symbol for
    /// one it kept to its crate.
    OwnStatic(String),
    /// The file, line and column of a `core::panic::Location`.
    Location(Option<(String, u32, u32)>),
    FilePath(String),
}

#[derive(Debug, Serialize, Deserialize)]
struct CachedReloc {
    offset: CodeOffset,
    kind: Reloc,
    name: ModuleRelocTarget,
    addend: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct CachedCode {
    signature: Signature,
    bytes: Vec<u8>,
    align: u64,
    relocs: Vec<CachedReloc>,
    elf_types: Vec<(u32, u32)>,
    unwind: Option<FunctionUnwind>,
    lines: Vec<LineRow>,
    slots: SlotPlaces,
}

impl PartialEq for CachedCode {
    fn eq(&self, other: &CachedCode) -> bool {
        // `ModuleRelocTarget` is not `PartialEq`: compare what is written out.
        postcard::to_allocvec(self).ok() == postcard::to_allocvec(other).ok()
    }
}

impl From<&Code> for CachedCode {
    fn from(code: &Code) -> CachedCode {
        CachedCode {
            signature: code.signature.clone(),
            bytes: code.bytes.clone(),
            align: code.align,
            relocs: code
                .relocs
                .iter()
                .map(|it| CachedReloc {
                    offset: it.offset,
                    kind: it.kind,
                    name: it.name.clone(),
                    addend: it.addend,
                })
                .collect(),
            elf_types: code.elf_types.clone(),
            unwind: code.unwind.clone(),
            lines: code.lines.clone(),
            slots: code.slots.clone(),
        }
    }
}

impl CachedCode {
    /// Whether every relocation is inside the code and against one of the first
    /// `count(namespace)` things of its namespace, which code read from the store is not
    /// known to be.
    fn sound(&self, count: impl Fn(u32) -> usize) -> bool {
        (self.align == 0 || self.align.is_power_of_two())
            && self.relocs.iter().all(|it| {
                let known = match it.name {
                    ModuleRelocTarget::User { namespace, index } => {
                        (index as usize) < count(namespace)
                    }
                    _ => true,
                };
                known && (it.offset as usize) < self.bytes.len()
            })
    }

    pub(super) fn signature(&self) -> &Signature {
        &self.signature
    }

    pub(super) fn code(&self) -> Code {
        Code {
            signature: self.signature.clone(),
            bytes: self.bytes.clone(),
            align: self.align,
            relocs: self
                .relocs
                .iter()
                .map(|it| ModuleReloc {
                    offset: it.offset,
                    kind: it.kind,
                    name: it.name.clone(),
                    addend: it.addend,
                })
                .collect(),
            elf_types: self.elf_types.clone(),
            unwind: self.unwind.clone(),
            lines: self.lines.clone(),
            slots: self.slots.clone(),
        }
    }
}

/// What LLVM made of a function, kept by the IR it was made from. Cranelift keeps its own
/// code the same way, through its incremental cache.
///
/// LLVM asks for things the IR did not name: library functions it calls of its own accord,
/// and tables of addresses. Those are kept here, and relocations against them say which one
/// by its place among them, in a namespace of their own.
#[cfg(feature = "llvm")]
#[derive(Serialize, Deserialize)]
pub(super) struct StoredCode {
    code: CachedCode,
    imports: Vec<String>,
    blobs: Vec<super::ConstBlob>,
}

#[cfg(feature = "llvm")]
impl StoredCode {
    const IMPORTS: u32 = 2;
    const BLOBS: u32 = 3;

    /// The key for `func` compiled at `optimization`. Cranelift's key of the IR leaves out
    /// which requests the IR's names stand for, which LLVM's code has in it.
    pub(super) fn key<'a>(
        optimization: super::backend::Optimization,
        func: &cranelift_codegen::ir::Function,
        named: impl Iterator<Item = cranelift_codegen::ir::StackSlot>,
        constants: impl Iterator<Item = (u32, &'a [u8])>,
        asm: impl Iterator<Item = (u32, &'a str)>,
        pointees: &[(u32, super::fragment::Pointee)],
    ) -> Option<[u8; KEY]> {
        let isa = super::target().ok()?;
        let ir = cranelift_codegen::incremental_cache::compute_cache_key(&**isa, func);
        let mut hasher = Sha256::new();
        hasher.update(b"blaze llvm 9");
        // Nor what the types of the parameters say of the memory they point to.
        for (at, pointee) in pointees {
            let super::fragment::Pointee { unique, frozen, size, align } = *pointee;
            hasher.update(at.to_le_bytes());
            hasher.update([u8::from(unique), u8::from(frozen)]);
            hasher.update(size.to_le_bytes());
            hasher.update(align.to_le_bytes());
        }
        hasher.update(u32::MAX.to_le_bytes());
        hasher.update(super::Backend::Llvm(optimization).to_string());
        // Nor LLVM's inliner threshold.
        hasher.update(super::llvm::inline_threshold().to_le_bytes());
        hasher.update(ir.to_string());
        // Nor does it tell which statement an instruction is of, which the code's lines do.
        for block in func.layout.blocks() {
            for inst in func.layout.block_insts(block) {
                hasher.update(func.srcloc(inst).bits().to_le_bytes());
            }
        }
        // Nor which slots hold a local with a name, which the code says the places of.
        for slot in named {
            hasher.update(slot.as_u32().to_le_bytes());
        }
        hasher.update(u32::MAX.to_le_bytes());
        for (_, name) in func.params.user_named_funcs().iter() {
            hasher.update(name.namespace.to_le_bytes());
            hasher.update(name.index.to_le_bytes());
        }
        hasher.update(u32::MAX.to_le_bytes());
        // Nor what the data a name stands for holds, which the code has in place of a load
        // where that is known.
        for (index, bytes) in constants {
            hasher.update(index.to_le_bytes());
            hasher.update((bytes.len() as u64).to_le_bytes());
            hasher.update(bytes);
        }
        hasher.update(u32::MAX.to_le_bytes());
        // Nor which `asm!` block a call of a wrapper runs, which LLVM's code has in place of
        // the call: two functions that each pass a `u64` through a block of their own are one
        // IR (the corpus's `att`, `addq $7`, was the code of asmedits.py's `scale`, a `lea`).
        for (index, text) in asm {
            hasher.update(index.to_le_bytes());
            hasher.update((text.len() as u64).to_le_bytes());
            hasher.update(text);
        }
        Some(hasher.finalize().into())
    }

    /// `code` as it can be kept, if everything asked for since `mark` is of a kind that is.
    pub(super) fn of(
        code: &Code,
        recorder: &super::fragment::Wants,
        mark: super::fragment::Mark,
    ) -> Option<StoredCode> {
        use super::fragment::{DataRequest, FuncTarget};
        let (funcs, datas) = recorder.since(mark);
        let imports = funcs.iter().map(|it| match &it.target {
            FuncTarget::Symbol(symbol) => Some(symbol.clone()),
            _ => None,
        });
        let blobs = datas.iter().map(|it| match it {
            DataRequest::Blob { blob, identity: DataIdentity::Shared } => Some(blob.clone()),
            _ => None,
        });
        let mut stored = StoredCode {
            code: CachedCode::from(code),
            imports: imports.collect::<Option<_>>()?,
            blobs: blobs.collect::<Option<_>>()?,
        };
        for reloc in &mut stored.code.relocs {
            reloc.name = match reloc.name {
                ModuleRelocTarget::User { namespace: 0, index } if index as usize >= mark.funcs => {
                    ModuleRelocTarget::user(Self::IMPORTS, index - mark.funcs as u32)
                }
                ModuleRelocTarget::User { namespace: 1, index } if index as usize >= mark.datas => {
                    ModuleRelocTarget::user(Self::BLOBS, index - mark.datas as u32)
                }
                ref other => other.clone(),
            };
        }
        Some(stored)
    }

    /// The code, with what LLVM asked for asked of `recorder` again.
    pub(super) fn restore(self, recorder: &mut super::fragment::Wants) -> Option<Code> {
        if !self.sound(recorder.mark()) {
            return None;
        }
        let sig = recorder.make_signature();
        let imports: Vec<_> =
            self.imports.iter().map(|it| recorder.import(it, &sig).ok()).collect::<Option<_>>()?;
        let blobs: Vec<_> = self
            .blobs
            .into_iter()
            .map(|it| recorder.define_const(it).ok())
            .collect::<Option<_>>()?;
        let mut code = self.code.code();
        for reloc in &mut code.relocs {
            reloc.name = match reloc.name {
                ModuleRelocTarget::User { namespace: Self::IMPORTS, index } => {
                    ModuleRelocTarget::user(0, imports.get(index as usize)?.as_u32())
                }
                ModuleRelocTarget::User { namespace: Self::BLOBS, index } => {
                    ModuleRelocTarget::user(1, blobs.get(index as usize)?.as_u32())
                }
                ref other => other.clone(),
            };
        }
        Some(code)
    }

    /// Whether the code and the blobs refer only to what is kept with them and to what a
    /// function that had asked for `asked` before it was compiled could name.
    fn sound(&self, asked: super::fragment::Mark) -> bool {
        let code = self.code.sound(|namespace| match namespace {
            0 => asked.funcs,
            1 => asked.datas,
            Self::IMPORTS => self.imports.len(),
            Self::BLOBS => self.blobs.len(),
            _ => 0,
        });
        code && self.blobs.iter().all(|it| it.sound(asked.funcs, asked.datas))
    }
}

/// What the store keeps under [`StoredCode::key`]: LLVM's code of the function, or why LLVM
/// left it to Cranelift. The refusal is kept as the code is, so that the next build of the
/// same IR, in this process or another, goes to Cranelift and its entry in the store straight
/// away, instead of trying LLVM again and missing in the store every time.
#[cfg(feature = "llvm")]
#[derive(Serialize, Deserialize)]
pub(super) enum LlvmEntry {
    Code(StoredCode),
    LeftToCranelift(String),
}

#[cfg(feature = "llvm")]
impl LlvmEntry {
    pub(super) fn encode(&self) -> Option<Vec<u8>> {
        postcard::to_allocvec(self).ok()
    }

    pub(super) fn decode(bytes: &[u8]) -> Option<LlvmEntry> {
        postcard::from_bytes(bytes).ok()
    }
}

/// What a debugger is told of a function, which the entry of it kept has.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Told {
    Nothing,
    /// Its lines alone: no variables, nor the scopes that hold them.
    Lines,
    /// Its variables and their types too.
    Variables,
}

impl Told {
    pub(super) fn of(debug: bool, variables: bool) -> Told {
        match (debug, variables) {
            (false, _) => Told::Nothing,
            (true, false) => Told::Lines,
            (true, true) => Told::Variables,
        }
    }
}

impl Entry {
    /// The key of the entry for `name`. The store tells the entries of this build of the
    /// compiler from another's.
    ///
    /// An entry kept with what a debugger is told is not the one kept without, nor one kept
    /// with its lines alone the one kept with its variables too.
    ///
    /// Whether the code checks arithmetic for overflow is in the name.
    ///
    /// `program` is the one program the entry is of, where what the calls of an entry reach
    /// is not the same in every program that has a function of its name.
    pub(super) fn key(name: &str, debug: Told, program: Option<&[u8; 32]>) -> Option<[u8; KEY]> {
        let mut hasher = Sha256::new();
        // Each with its length, so that no two lists of them are the same bytes.
        let mut field = |bytes: &[u8]| {
            hasher.update((bytes.len() as u64).to_le_bytes());
            hasher.update(bytes);
        };
        // 12: an import of the standard library's unit is checked (#328).
        field(b"blaze entry 12");
        field(program.map_or(&[][..], |it| it));
        // Code of one producer is not what another would have given.
        field(super::Backend::from_env().ok()?.to_string().as_bytes());
        // Nor is code of an optimized body; the keys of the default level stay what they were.
        let level = super::mir_opt_level().number();
        if level != 0 {
            field(&[b'M', level]);
        }
        // Nor is code for one target what another runs.
        field(super::Target::selected().ok()?.triple().as_bytes());
        // Nor code that gives the frame pointer's register to values; the keys without stay.
        if super::backend::frame_pointers() {
            field(b"frame pointers");
        }
        // Nor code compiled where rustc's `-C link-dead-code` decides some of it.
        if super::backend::links_dead_code() {
            field(b"link dead code");
        }
        field(&[(debug != Told::Nothing) as u8]);
        // The keys of the other two stay what they were.
        if debug == Told::Lines {
            field(b"lines alone");
        }
        field(name.as_bytes());
        {
            static NAMES: OnceLock<Option<Mutex<std::fs::File>>> = OnceLock::new();
            let file = NAMES.get_or_init(|| {
                let dir = std::env::var_os("BLAZE_DEBUG_NAMES")?;
                let path = std::path::Path::new(&dir).join(std::process::id().to_string());
                std::fs::File::options().append(true).create(true).open(path).ok().map(Mutex::new)
            });
            if let Some(file) = file {
                use std::io::Write;
                _ = writeln!(file.lock().unwrap_or_else(|e| e.into_inner()), "{name}");
            }
        }
        Some(hasher.finalize().into())
    }

    pub(super) fn encode(&self) -> Option<Vec<u8>> {
        postcard::to_allocvec(self).ok()
    }

    /// The entry the bytes are of, when they are of one that can be stitched: everything
    /// it refers to by its place is in it.
    pub(super) fn decode(bytes: &[u8]) -> Option<Entry> {
        let entry: Entry = postcard::from_bytes(bytes).ok()?;
        entry.sound().then_some(entry)
    }

    fn sound(&self) -> bool {
        let funcs = self.funcs.len();
        // A data object points only to the ones before it.
        let datas = self.datas.iter().enumerate().all(|(before, data)| match data {
            CachedData::Blob { blob, .. } | CachedData::Vtable { blob, .. } => {
                blob.sound(funcs, before)
            }
            CachedData::Static { .. }
            | CachedData::OwnStatic(_)
            | CachedData::Location(_)
            | CachedData::FilePath(_) => true,
        });
        datas && self.code.sound(|namespace| if namespace == 0 { funcs } else { self.datas.len() })
    }

    /// Whether the program has everything of rustc's that the entry was linked against.
    pub(super) fn links(&self, defines: &dyn Fn(&str) -> bool) -> bool {
        let funcs = self.funcs.iter().all(|it| match &it.target {
            CachedTarget::Import { symbol, checked } => !checked || defines(symbol),
            CachedTarget::Copy { symbol, .. } => defines(symbol),
            CachedTarget::Named(_)
            | CachedTarget::Root(_)
            | CachedTarget::TypeId(_)
            | CachedTarget::Asm(_) => true,
        });
        let datas = self.datas.iter().all(|it| match it {
            CachedData::Static { symbol, .. } => defines(symbol),
            _ => true,
        });
        funcs && datas
    }

    fn named(&self) -> impl Iterator<Item = &str> {
        self.funcs.iter().filter_map(|it| match &it.target {
            CachedTarget::Named(name) => Some(name.as_str()),
            CachedTarget::Import { .. }
            | CachedTarget::Copy { .. }
            | CachedTarget::Root(_)
            | CachedTarget::TypeId(_)
            | CachedTarget::Asm(_) => None,
        })
    }
}

/// Whether `bytes` have the name of the directory `place` in them.
pub(super) fn holds(bytes: &[u8], place: &str) -> bool {
    !place.is_empty() && bytes.windows(place.len()).any(|it| it == place.as_bytes())
}

impl Entry {
    /// The names of the files the entry tells of: in its panic locations, in what `file!()`
    /// is, and in what a debugger is told.
    fn files(&mut self) -> impl Iterator<Item = &mut String> {
        let named = self.datas.iter_mut().filter_map(|it| match it {
            CachedData::Location(Some((file, ..))) | CachedData::FilePath(file) => Some(file),
            _ => None,
        });
        named.chain(self.debug.iter_mut().flat_map(|it| &mut it.files))
    }

    fn of_cargo(&mut self) -> bool {
        self.files().any(|it| super::moved(it, super::CARGO_HOME, "").is_some())
    }

    /// Names the files under `cargo_home` and the standard library's as every machine does,
    /// on the way to the store. Not to be kept when a file is already named as one that
    /// cargo fetched and is not: it would be read back as one.
    pub(super) fn leave(
        &mut self,
        cargo_home: Option<&str>,
        library: Option<&super::LibrarySources>,
    ) -> bool {
        for file in self.files() {
            if let Some(there) = library.and_then(|it| it.stored(file)) {
                *file = there;
            }
        }
        if cargo_home == Some(super::CARGO_HOME) {
            return true;
        }
        if self.of_cargo() {
            return false;
        }
        let Some(home) = cargo_home else { return true };
        for file in self.files() {
            if let Some(there) = super::moved(file, home, super::CARGO_HOME) {
                *file = there;
            }
        }
        true
    }

    /// Names the files of cargo's directory and the standard library's as this machine
    /// does, on the way from the store. `None` when the entry has files of cargo's and this
    /// machine has no such directory.
    pub(super) fn arrived(
        mut self,
        cargo_home: Option<&str>,
        library: Option<&super::LibrarySources>,
    ) -> Option<Entry> {
        for file in self.files() {
            if let Some(here) = library.and_then(|it| it.local(file)) {
                *file = here;
            }
        }
        if cargo_home == Some(super::CARGO_HOME) || !self.of_cargo() {
            return Some(self);
        }
        let home = cargo_home?;
        for file in self.files() {
            if let Some(here) = super::moved(file, super::CARGO_HOME, home) {
                *file = here;
            }
        }
        Some(self)
    }
}

/// The entries of the store that one program was asked about.
pub(super) struct Entries {
    store: &'static Store,
    /// What a debugger is told by the entries asked for.
    debug: Told,
    /// The one program the entries are of, when they are not of any: see [`Entry::key`].
    program: Option<[u8; 32]>,
    /// Where cargo keeps the sources it fetched on this machine.
    cargo_home: Option<String>,
    library: Option<super::LibrarySources>,
    /// Each read once however many threads ask for it at the same time: the callees of the
    /// functions being built are mostly the same few, and the threads come to them together.
    entries: Sharded<Arc<str>, Arc<OnceLock<Option<Arc<Entry>>>>>,
    /// Whether the program can be built from an entry and everything it calls.
    usable: Sharded<Arc<str>, bool>,
    types: Sharded<[u8; KEY], Arc<OnceLock<Option<TypeSet>>>>,
}

/// A map behind a lock for each of its shards: a restart has every thread look up the stored
/// roots at once, an entry and each of its sets of types, and one lock had them take turns.
struct Sharded<K, V>(Box<[Mutex<FxHashMap<K, V>>]>);

impl<K: std::hash::Hash + Eq, V> Default for Sharded<K, V> {
    fn default() -> Self {
        Sharded((0..64).map(|_| Mutex::default()).collect())
    }
}

impl<K: std::hash::Hash + Eq, V> Sharded<K, V> {
    fn shard<Q: std::hash::Hash + ?Sized>(
        &self,
        key: &Q,
    ) -> std::sync::MutexGuard<'_, FxHashMap<K, V>> {
        use std::hash::BuildHasher;
        let hash = rustc_hash::FxBuildHasher.hash_one(key);
        // The high bits: the low ones also pick the bucket inside the shard's map.
        let at = (hash >> 58) as usize % self.0.len();
        self.0[at].lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The sets of types that were given to the store, each when the first entry with a
/// variable of the type was.
#[derive(Default)]
pub(super) struct KeptTypes {
    /// By where the set is, with the set so that nothing else comes to be there.
    keys: FxHashMap<usize, (TypeSet, [u8; KEY])>,
}

impl KeptTypes {
    /// The key the set is in the store under, which is of nothing but what the set tells.
    pub(super) fn keep(&mut self, store: &Store, types: &TypeSet) -> [u8; KEY] {
        let at = types.at();
        if let Some((_, key)) = self.keys.get(&at) {
            return *key;
        }
        let bytes = postcard::to_allocvec(&types.to_vec()).unwrap_or_default();
        let mut hasher = Sha256::new();
        hasher.update(b"blaze types 1");
        hasher.update(&bytes);
        let key = hasher.finalize().into();
        store.insert(&key, &bytes);
        self.keys.insert(at, (types.clone(), key));
        key
    }
}

/// What the store is given of a compiled instance.
pub(super) enum Kept {
    Entry {
        key: [u8; KEY],
        entry: Box<Entry>,
    },
    /// The sets of an entry the store has: they may have been dropped from under it.
    Types(Vec<TypeSet>),
}

/// How many entries a [`Writer`] added, and how many it did not for a file of theirs
/// under what stands for cargo's directory.
#[derive(Default)]
pub(super) struct Written {
    pub(super) added: usize,
    pub(super) of_cargo: usize,
}

/// Gives the store what is compiled on threads of its own, so that the thread that
/// compiles does not wait for an entry to be written before it stitches the next.
pub(super) struct Writer {
    store: &'static Store,
    cargo_home: Option<String>,
    library: Option<super::LibrarySources>,
    types: KeptTypes,
    written: Written,
    threads: Option<(std::sync::mpsc::Sender<Kept>, Vec<std::thread::JoinHandle<Written>>)>,
    /// Whether there is no thread to be had, and the entries are written where they are kept.
    alone: bool,
}

impl Writer {
    pub(super) fn new(
        store: &'static Store,
        cargo_home: Option<String>,
        library: Option<super::LibrarySources>,
    ) -> Self {
        let mut writer = Writer::alone(store, cargo_home, library);
        writer.alone = false;
        writer
    }

    /// A writer that writes where it is asked to.
    pub(super) fn alone(
        store: &'static Store,
        cargo_home: Option<String>,
        library: Option<super::LibrarySources>,
    ) -> Self {
        let (types, written) = Default::default();
        Writer { store, cargo_home, library, types, written, threads: None, alone: true }
    }

    pub(super) fn keep(&mut self, kept: Kept) {
        if self.threads.is_none() && !self.alone {
            let (jobs, queued) = std::sync::mpsc::channel::<Kept>();
            let queued = Arc::new(Mutex::new(queued));
            // A whole build hands over most of its entries at once, when its last wave is
            // compiled: one thread would write them after the link.
            let count =
                std::thread::available_parallelism().map_or(1, |it| it.get() / 8).clamp(1, 8);
            let threads: Vec<_> = (0..count)
                .map_while(|_| {
                    let mut writer =
                        Writer::alone(self.store, self.cargo_home.clone(), self.library.clone());
                    let queued = queued.clone();
                    let work = move || {
                        loop {
                            let next = queued.lock().unwrap_or_else(|e| e.into_inner()).recv();
                            let Ok(kept) = next else { break };
                            writer.write(kept);
                        }
                        std::mem::take(&mut writer.written)
                    };
                    std::thread::Builder::new().name("blaze store".to_owned()).spawn(work).ok()
                })
                .collect();
            match threads.is_empty() {
                true => self.alone = true,
                false => self.threads = Some((jobs, threads)),
            }
        }
        match &self.threads {
            Some((jobs, _)) => _ = jobs.send(kept),
            None => self.write(kept),
        }
    }

    fn write(&mut self, kept: Kept) {
        let (key, mut entry) = match kept {
            Kept::Entry { key, entry } => (key, entry),
            Kept::Types(types) => {
                types.iter().for_each(|it| _ = self.types.keep(self.store, it));
                return;
            }
        };
        match self.encoded(&mut entry) {
            Some(bytes) => {
                self.store.insert(&key, &bytes);
                self.written.added += 1;
            }
            None => self.written.of_cargo += 1,
        }
    }

    /// The entry as the store keeps it, with its sets of types in the store. `None` is an
    /// entry with a file of its own under what stands for cargo's directory.
    pub(super) fn encoded(&mut self, entry: &mut Entry) -> Option<Vec<u8>> {
        if let Some(debug) = &mut entry.debug {
            let keys = debug.types.iter().map(|it| self.types.keep(self.store, it));
            debug.stored_types = keys.collect();
        }
        entry.leave(self.cargo_home.as_deref(), self.library.as_ref()).then(|| entry.encode())?
    }

    /// Waits for everything kept so far to be in the store, and tells what was written
    /// since the last time.
    pub(super) fn finish(&mut self) -> Written {
        let mut written = std::mem::take(&mut self.written);
        if let Some((jobs, threads)) = self.threads.take() {
            drop(jobs);
            for theirs in threads.into_iter().filter_map(|it| it.join().ok()) {
                written.added += theirs.added;
                written.of_cargo += theirs.of_cargo;
            }
        }
        written
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        self.finish();
    }
}

/// An entry on the way down from the one asked about.
struct Visit {
    name: Arc<str>,
    entry: Arc<Entry>,
    /// How many of the functions the entry refers to have been looked at.
    seen: usize,
    /// The highest visit that this one or one below it calls back into: until that one is
    /// settled, neither is this.
    leans_on: usize,
    /// Where in the list of unsettled entries those below this one start.
    below: usize,
}

impl Entries {
    pub(super) fn new(
        store: &'static Store,
        debug: Told,
        program: Option<[u8; 32]>,
        cargo_home: Option<String>,
        library: Option<super::LibrarySources>,
    ) -> Entries {
        let (entries, usable, types) = Default::default();
        Entries { store, debug, program, cargo_home, library, entries, usable, types }
    }

    /// Never use the entry again in this process: it did not fit the object it was read for.
    pub(super) fn ban(name: &str) {
        Self::banned().lock().unwrap_or_else(|e| e.into_inner()).insert(name.to_owned());
    }

    fn banned() -> &'static Mutex<FxHashSet<String>> {
        static BANNED: OnceLock<Mutex<FxHashSet<String>>> = OnceLock::new();
        BANNED.get_or_init(Mutex::default)
    }

    pub(super) fn get(&self, name: &str) -> Option<Arc<Entry>> {
        let cell = self.cell(name);
        let read = || {
            let key = Entry::key(name, self.debug, self.program.as_ref())?;
            self.arrived(&self.store.get(&key)?)
        };
        cell.get_or_init(|| {
            let tally = super::backend::Tally::global();
            let entry =
                (!Self::is_banned(name)).then(|| tally.step(super::backend::Step::StoreRead, read));
            entry.flatten().map(Arc::new)
        })
        .clone()
    }

    fn cell(&self, name: &str) -> Arc<OnceLock<Option<Arc<Entry>>>> {
        let mut entries = self.entries.shard(name);
        match entries.get(name) {
            Some(cell) => cell.clone(),
            None => entries.entry(Arc::from(name)).or_default().clone(),
        }
    }

    fn is_banned(name: &str) -> bool {
        Self::banned().lock().unwrap_or_else(|e| e.into_inner()).contains(name)
    }

    /// The entry the store keeps as `bytes`, as this build reads it.
    fn arrived(&self, bytes: &[u8]) -> Option<Entry> {
        let entry = Entry::decode(bytes)?;
        // The types are read when the function is described (`describe`), which a build
        // whose object is the one it has already never does.
        let stored = |key: &[u8; KEY]| self.store.has_used(key) || self.types(key).is_some();
        let debug = entry.debug.as_ref();
        if !debug.is_none_or(|it| it.stored_types.iter().all(stored)) {
            return None;
        }
        entry.arrived(self.cargo_home.as_deref(), self.library.as_ref())
    }

    /// Reads the entries of `names` and of everything they call, as [`Entries::get`] would
    /// read each, a wave of callees at a time, each wave from the store at once
    /// ([`Store::get_many`]) and decoded on every core. A build that asks for the code of every
    /// function of a program, as each program of a whole build does, read them one by one
    /// from every core in no order, and each read unpacked a block a hundred times the size
    /// of the entry: blaze's own build spent some 1000 s of its threads' time on it.
    pub(super) fn prefetch(&self, names: impl IntoIterator<Item = Arc<str>>) {
        let threads = std::thread::available_parallelism().map_or(1, |it| it.get());
        let tally = super::backend::Tally::global();
        let mut seen: FxHashSet<Arc<str>> = FxHashSet::default();
        let mut wave: Vec<Arc<str>> =
            names.into_iter().filter(|it| seen.insert(it.clone())).collect();
        while !wave.is_empty() {
            // What another thread read already, or is reading, is not read again.
            let cells: Vec<_> = (wave.iter())
                .map(|name| (name, self.cell(name)))
                .filter(|(_, cell)| cell.get().is_none())
                .collect();
            let keyed: Vec<(usize, [u8; KEY])> = (cells.iter().enumerate())
                .filter(|(_, (name, _))| !Self::is_banned(name))
                .filter_map(|(at, (name, _))| {
                    Some((at, Entry::key(name, self.debug, self.program.as_ref())?))
                })
                .collect();
            let keys: Vec<[u8; KEY]> = keyed.iter().map(|it| it.1).collect();
            let values =
                tally.step(super::backend::Step::StoreRead, || self.store.get_many(&keys, threads));
            let mut read: Vec<Option<Vec<u8>>> = vec![None; cells.len()];
            for ((at, _), value) in keyed.iter().zip(values) {
                read[*at] = value;
            }
            let next = std::sync::atomic::AtomicUsize::new(0);
            let decode = || {
                let mut called = Vec::new();
                loop {
                    let at = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some((_, cell)) = cells.get(at) else { break };
                    let entry = read[at].as_deref().and_then(|it| self.arrived(it));
                    _ = cell.set(entry.map(Arc::new));
                    if let Some(Some(entry)) = cell.get() {
                        called.extend(entry.named().map(Arc::<str>::from));
                    }
                }
                called
            };
            let threads = threads.clamp(1, cells.len() / 64 + 1);
            let called: Vec<Arc<str>> = match threads {
                1 => decode(),
                _ => std::thread::scope(|scope| {
                    let others: Vec<_> = (1..threads).map(|_| scope.spawn(decode)).collect();
                    let mut called = decode();
                    for other in others {
                        match other.join() {
                            Ok(theirs) => called.extend(theirs),
                            Err(panic) => std::panic::resume_unwind(panic),
                        }
                    }
                    called
                }),
            };
            wave = called.into_iter().filter(|it| seen.insert(it.clone())).collect();
        }
    }

    /// Gives what an entry tells a debugger the types of its variables, which [`Entries::get`]
    /// leaves to be read. A set the store no longer has leaves them all untold.
    pub(super) fn describe(&self, debug: &mut FunctionDebug) {
        if !debug.types.is_empty() || debug.stored_types.is_empty() {
            return;
        }
        let types = debug.stored_types.iter().map(|key| self.types(key));
        debug.types = types.collect::<Option<_>>().unwrap_or_default();
    }

    /// The set of types kept under `key`, which every entry with a variable of the type
    /// shares.
    fn types(&self, key: &[u8; KEY]) -> Option<TypeSet> {
        let cell = self.types.shard(key).entry(*key).or_default().clone();
        cell.get_or_init(|| {
            let types = self.store.get(key).and_then(|it| postcard::from_bytes::<Vec<_>>(&it).ok());
            types.map(TypeSet::from)
        })
        .clone()
    }

    fn settle(&self, name: Arc<str>, usable: bool) {
        self.usable.shard(&*name).insert(name, usable);
    }

    /// Whether there is an entry for `name` and for everything it calls, and what they import
    /// is in the program, as `defines` tells.
    pub(super) fn usable(&self, name: &str, defines: &dyn Fn(&str) -> bool) -> bool {
        let settled = |name: &str| self.usable.shard(name).get(name).copied();
        if let Some(usable) = settled(name) {
            return usable;
        }
        let name: Arc<str> = Arc::from(name);
        let Some(entry) = self.get(&name).filter(|it| it.links(defines)) else {
            self.settle(name, false);
            return false;
        };
        // Entries call each other in circles, so one is only known to be usable along with
        // the ones it leans on.
        let mut unsettled = vec![name.clone()];
        let mut leaning = FxHashMap::<Arc<str>, usize>::default();
        leaning.insert(name.clone(), 0);
        let mut path = vec![Visit { name, entry, seen: 0, leans_on: 0, below: 0 }];
        while !path.is_empty() {
            let depth = path.len() - 1;
            let visit = &mut path[depth];
            let entry = visit.entry.clone();
            let Some(callee) = entry.named().nth(visit.seen) else {
                let Some(done) = path.pop() else { break };
                if done.leans_on >= depth {
                    for name in unsettled.drain(done.below..) {
                        leaning.remove(&name);
                        self.settle(name, true);
                    }
                } else {
                    leaning.insert(done.name, done.leans_on);
                    let above = &mut path[depth - 1];
                    above.leans_on = above.leans_on.min(done.leans_on);
                }
                continue;
            };
            visit.seen += 1;
            if let Some(&on) = leaning.get(callee) {
                visit.leans_on = visit.leans_on.min(on);
                continue;
            }
            let found = match settled(callee) {
                Some(true) => continue,
                Some(false) => None,
                None => self.get(callee).filter(|it| it.links(defines)),
            };
            let callee: Arc<str> = Arc::from(callee);
            let Some(entry) = found else {
                self.settle(callee, false);
                for visit in path {
                    self.settle(visit.name, false);
                }
                return false;
            };
            leaning.insert(callee.clone(), depth + 1);
            unsettled.push(callee.clone());
            path.push(Visit {
                name: callee,
                entry,
                seen: 0,
                leans_on: depth + 1,
                below: unsettled.len() - 1,
            });
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use blaze_fuzz as fuzz;
    use cranelift_codegen::{
        ir::{AbiParam, types},
        isa::CallConv,
    };
    use cranelift_module::{DataId, FuncId};

    use super::{
        super::{
            Resolved, Target,
            debuginfo::{
                Aggregate, DebugContext, DebugType, Field, Place, Scope, VariableDebug,
                VariablePlace,
            },
        },
        *,
    };

    fn signature() -> Signature {
        let mut signature = Signature::new(CallConv::SystemV);
        signature.params.push(AbiParam::new(types::I64));
        signature.returns.push(AbiParam::new(types::I32));
        signature
    }

    fn reloc(offset: u32, namespace: u32, index: u32) -> CachedReloc {
        let name = ModuleRelocTarget::user(namespace, index);
        CachedReloc { offset, kind: Reloc::X86CallPCRel4, name, addend: -4 }
    }

    fn code(relocs: Vec<CachedReloc>) -> CachedCode {
        let lines = vec![LineRow { offset: 0, span: 0 }, LineRow { offset: 8, span: 3 }];
        CachedCode {
            signature: signature(),
            bytes: vec![0x90; 32],
            align: 16,
            relocs,
            unwind: None,
            lines,
            slots: SlotPlaces::default(),
            elf_types: Vec::new(),
        }
    }

    fn blob(relocs: Vec<(u32, DataId, i64)>, fn_relocs: Vec<(u32, FuncId)>) -> ConstBlob {
        ConstBlob { bytes: vec![0; 24], relocs, fn_relocs, align: 8, type_ids: Vec::new() }
    }

    /// Types each inside the one before, `depth` of them, by way of each kind of type that
    /// has one inside. The first is the outermost.
    fn nested(depth: u32) -> Vec<DebugType> {
        let ty = |at: u32| {
            let name = format!("T{at}");
            match depth - at {
                1 => DebugType::Base { name, size: 4, encoding: 7 },
                left if left % 3 == 0 => DebugType::Pointer { name, to: Some(at + 1) },
                left if left % 3 == 1 => DebugType::Array { of: at + 1, len: 2 },
                _ => {
                    let field = Field { name: "inside".to_owned(), offset: 0, ty: at + 1 };
                    let (fields, parameters) = (vec![field], Vec::new());
                    DebugType::Struct(Aggregate { name, size: 8, fields, parameters })
                }
            }
        };
        (0..depth).map(ty).collect()
    }

    fn debug(types: Vec<DebugType>) -> FunctionDebug {
        let mut debug =
            FunctionDebug::new("run".to_owned(), "_ZN4case3run17h0123456789abcdefE".to_owned());
        debug.files.push("src/lib.rs".to_owned());
        debug.declared = Some(Place { file: 0, line: 3, column: 1 });
        debug.rows.push((4, Place { file: 0, line: 4, column: 5 }));
        debug.types = vec![types.into()];
        debug.scopes.push(Scope { parent: None, ranges: vec![(0, 8)] });
        debug.frame_base = vec![cranelift_codegen::gimli::DW_OP_reg6.0];
        debug.variables.push(VariableDebug {
            name: "it".to_owned(),
            param: true,
            ty: 0,
            place: VariablePlace::Frame(-16),
            scope: Some(0),
        });
        debug
    }

    /// An entry with one of everything an entry has.
    fn entry() -> Entry {
        let import = CachedTarget::Import { symbol: "memcpy".to_owned(), checked: true };
        let targets = [
            import,
            CachedTarget::Named("c:1f:case::helper".to_owned()),
            CachedTarget::Root("case::edited".to_owned()),
            CachedTarget::TypeId(7 << 100),
            CachedTarget::Asm("nop".to_owned()),
            CachedTarget::Copy {
                name: "case::small".to_owned(),
                symbol: "_RNvCs1_4case5small".to_owned(),
            },
        ];
        let datas = vec![
            CachedData::Static { symbol: "ENVIRON".to_owned(), thread_local: false },
            CachedData::Blob { blob: blob(Vec::new(), Vec::new()), identity: DataIdentity::Shared },
            CachedData::Vtable {
                key: "dyn Run for Case".to_owned(),
                blob: blob(vec![(8, DataId::from_u32(1), 4)], vec![(16, FuncId::from_u32(4))]),
            },
            CachedData::OwnStatic("case::COUNT".to_owned()),
            CachedData::Location(Some(("src/lib.rs".to_owned(), 3, 9))),
            CachedData::FilePath("src/lib.rs".to_owned()),
        ];
        Entry {
            label: "case::run".to_owned(),
            funcs: targets
                .into_iter()
                .map(|target| CachedFunc { target, sig: signature() })
                .collect(),
            datas,
            code: code(vec![reloc(4, 0, 4), reloc(12, 1, 5)]),
            debug: Some(debug(nested(6))),
            tracked: Some(Tracked {
                mark: 7 << 100 | 3,
                home: Some([5; 32]),
                files: Some(vec!["src/lib.rs".to_owned()]),
                items: Some(vec![WrittenItem {
                    file: "src/lib.rs".to_owned(),
                    id: 9,
                    hash: 11,
                    at: Some((3, 1)),
                }]),
            }),
        }
    }

    /// An entry of a crate cargo fetched to `home`, with each kind of file name it can have.
    fn fetched_to(home: &str) -> Entry {
        let file =
            format!("{home}/registry/src/index.crates.io-1949cf8c6b5b557f/dep-0.1.0/src/lib.rs");
        let mut entry = entry();
        entry.datas.push(CachedData::Location(Some((file.clone(), 3, 9))));
        entry.datas.push(CachedData::FilePath(file.clone()));
        entry.debug.as_mut().unwrap().files.push(file);
        entry
    }

    #[test]
    fn a_fetched_file_is_stored_under_the_same_name_from_every_cargo_home() {
        let (one, other) = ("/home/one/.cargo", "/srv/another user/cargo");
        let mut stored = fetched_to(one);
        assert!(stored.leave(Some(one), None));
        let mut theirs = fetched_to(other);
        assert!(theirs.leave(Some(other), None));
        let bytes = stored.encode().unwrap();
        assert_eq!(bytes, theirs.encode().unwrap());
        assert!(!holds(&bytes, one) && !holds(&bytes, other));
        assert!(holds(&bytes, "/cargo/registry/src/"));
        // The files of the checkout are named as they were.
        assert!(stored.files().any(|it| it == "src/lib.rs"));
        let here = Entry::decode(&bytes).unwrap().arrived(Some(other), None).unwrap();
        assert_eq!(here.encode().unwrap(), fetched_to(other).encode().unwrap());
    }

    #[test]
    fn a_fetched_file_is_not_taken_where_cargo_has_no_directory() {
        let mut stored = fetched_to("/home/one/.cargo");
        assert!(stored.leave(Some("/home/one/.cargo"), None));
        assert!(Entry::decode(&stored.encode().unwrap()).unwrap().arrived(None, None).is_none());
        // An entry without one is taken anywhere.
        assert!(entry().arrived(None, None).is_some());
    }

    #[test]
    fn a_directory_named_as_cargos_stands_in_is_not_mistaken_for_it() {
        // A checkout that really is under `/cargo` is not stored as if cargo fetched it.
        assert!(!fetched_to("/cargo").leave(Some("/home/one/.cargo"), None));
        assert!(!fetched_to("/cargo").leave(None, None));
        // A cargo home that is `/cargo` already names its files as every machine does.
        let mut same = fetched_to("/cargo");
        assert!(same.leave(Some("/cargo"), None));
        assert_eq!(same.encode().unwrap(), fetched_to("/cargo").encode().unwrap());
        // Only whole components are the home.
        let mut beside = fetched_to("/home/one/.cargo-old");
        assert!(beside.leave(Some("/home/one/.cargo"), None));
        assert!(beside.files().any(|it| it.starts_with("/home/one/.cargo-old/")));
    }

    #[test]
    fn a_file_of_the_library_is_stored_as_built_and_read_back_as_installed() {
        let sources = |here: &str| super::super::LibrarySources {
            here: here.to_owned(),
            built_in: "/rustc/0123".to_owned(),
        };
        let with_file = |file: &str| {
            let mut entry = entry();
            entry.datas.push(CachedData::Location(Some((file.to_owned(), 3, 9))));
            entry
        };
        let (one, other) = (sources("/home/one/toolchain/src"), sources("/opt/rust/src"));
        let mut stored = with_file("/home/one/toolchain/src/library/core/src/option.rs");
        assert!(stored.leave(None, Some(&one)));
        assert!(stored.files().any(|it| it == "/rustc/0123/library/core/src/option.rs"));
        let here = stored.arrived(None, Some(&other)).unwrap();
        let theirs = with_file("/opt/rust/src/library/core/src/option.rs");
        assert_eq!(here.encode().unwrap(), theirs.encode().unwrap());
    }

    /// Does with an entry what stitching it does with the places it names.
    fn follow(entry: &Entry) {
        let mut ids = Resolved { funcs: Vec::new(), datas: Vec::new() };
        ids.funcs.extend((0..entry.funcs.len() as u32).map(FuncId::from_u32));
        for (at, data) in entry.datas.iter().enumerate() {
            if let CachedData::Blob { blob, .. } | CachedData::Vtable { blob, .. } = data {
                ids.blob(blob);
            }
            ids.datas.push(DataId::from_u32(at as u32));
        }
        for reloc in &entry.code.code().relocs {
            ids.reloc(reloc, FuncId::from_u32(0));
        }
    }

    #[test]
    fn a_crate_is_named_by_all_of_its_hash_and_a_key_by_all_of_a_name() {
        let (mut one, mut other) = ([0x5a; 32], [0x5a; 32]);
        // The same in the 64 bits an id was once cut to.
        (one[31], other[31]) = (1, 2);
        let (one, other) = (super::super::ContentId(one), super::super::ContentId(other));
        assert_eq!(one.to_string().len(), 64);
        assert_ne!(one.to_string(), other.to_string());
        let key = |name: &str, debug| Entry::key(name, debug, None).unwrap();
        let named = |id: &super::super::ContentId| key(&format!("~{id}:run"), Told::Nothing);
        assert_ne!(named(&one), named(&other));
        let name = format!("~{one}:run");
        let keys = [
            key(&name, Told::Nothing),
            key(&name, Told::Lines),
            key(&name, Told::Variables),
            key(&format!("{name}\0"), Told::Nothing),
        ];
        let distinct: std::collections::HashSet<_> = keys.iter().collect();
        assert_eq!(distinct.len(), keys.len());
    }

    /// The entry as it comes back from a store, which keeps its sets of types apart.
    fn read_back(written: &Entry) -> Entry {
        let mut read = Entry::decode(&written.encode().unwrap()).unwrap();
        if let (Some(read), Some(written)) = (&mut read.debug, &written.debug) {
            let stored = |it: &TypeSet| postcard::to_allocvec(&it.to_vec()).unwrap();
            let sets = written.types.iter().map(stored);
            read.types =
                sets.map(|it| postcard::from_bytes::<Vec<_>>(&it).unwrap().into()).collect();
        }
        read
    }

    #[test]
    fn an_entry_is_read_back_as_it_was_written() {
        let written = entry();
        let read = read_back(&written);
        assert_eq!(read, written);
        follow(&read);
    }

    #[test]
    fn an_entry_that_names_what_it_lacks_is_no_entry() {
        let broken: [fn(&mut Entry); 7] = [
            |it| it.code.relocs.push(reloc(0, 0, 6)),
            |it| it.code.relocs.push(reloc(0, 1, 6)),
            |it| it.code.relocs.push(reloc(32, 0, 0)),
            |it| it.code.align = 3,
            // A blob points to what is before it.
            |it| {
                it.datas[1] = CachedData::Blob {
                    blob: blob(vec![(0, DataId::from_u32(1), 0)], Vec::new()),
                    identity: DataIdentity::Own,
                }
            },
            |it| {
                it.datas[1] = CachedData::Blob {
                    blob: blob(Vec::new(), vec![(0, FuncId::from_u32(6))]),
                    identity: DataIdentity::Own,
                }
            },
            |it| {
                it.datas[1] = CachedData::Blob {
                    blob: blob(Vec::new(), vec![(24, FuncId::from_u32(0))]),
                    identity: DataIdentity::Own,
                }
            },
        ];
        for (which, change) in broken.iter().enumerate() {
            let mut entry = entry();
            change(&mut entry);
            assert!(Entry::decode(&entry.encode().unwrap()).is_none(), "{which}");
        }
    }

    /// The types of an entry are a list that names its own places, so reading one takes no
    /// more of the stack however many are inside each other. Telling a debugger of them
    /// does, and stops at [`DebugType::DEEPEST`]; a type or a scope inside itself, which no
    /// compiler writes and a store may hold, ends as well.
    #[test]
    fn types_and_scopes_inside_too_many_others_are_read_and_end() {
        let deep = Entry { debug: Some(debug(nested(40_000))), ..entry() };
        assert_eq!(read_back(&deep), deep);
        let mut ring = debug(vec![DebugType::Array { of: 0, len: 2 }]);
        ring.scopes[0].parent = Some(0);
        let mut chain = debug(nested(6));
        chain.scopes = (0..40_000u32)
            .map(|at| Scope { parent: at.checked_sub(1), ranges: vec![(0, 8)] })
            .collect();
        chain.variables[0].scope = Some(39_999);
        let isa = Target::selected().and_then(|it| it.isa("backtracking", false)).unwrap();
        // Only ELF and Mach-O have the tables.
        let Some(mut context) = DebugContext::new(&*isa, "/", true, None, false) else { return };
        for (at, debug) in [deep.debug.unwrap(), ring, chain].iter().enumerate() {
            context.add_function(FuncId::from_u32(at as u32), 8, debug);
        }
    }

    #[test]
    fn no_bytes_break_the_reading_of_an_entry() {
        let seeds = vec![
            entry().encode().unwrap(),
            Entry { debug: None, datas: Vec::new(), code: code(Vec::new()), ..entry() }
                .encode()
                .unwrap(),
        ];
        let (mut read, mut refused) = (0, 0);
        for bytes in fuzz::inputs(concat!(env!("CARGO_MANIFEST_DIR"), "/corpus/entry"), seeds) {
            let (largest, entry) = fuzz::largest_allocation(|| Entry::decode(&bytes));
            assert!(largest <= fuzz::allowed(bytes.len()), "{largest} bytes for {}", bytes.len());
            match entry {
                Some(entry) => {
                    follow(&entry);
                    read += 1;
                }
                None => refused += 1,
            }
        }
        eprintln!("entry: {read} read, {refused} refused");
        assert!(read > 2 && refused > 0);
    }

    #[cfg(feature = "llvm")]
    fn stored(relocs: Vec<CachedReloc>) -> StoredCode {
        let table = blob(vec![(0, DataId::from_u32(1), 0)], vec![(8, FuncId::from_u32(2))]);
        StoredCode { code: code(relocs), imports: vec!["memset".to_owned()], blobs: vec![table] }
    }

    #[cfg(feature = "llvm")]
    #[test]
    fn code_of_llvm_that_names_what_was_not_asked_for_is_not_restored() {
        use super::super::fragment::Mark;
        let asked = Mark { funcs: 3, datas: 2 };
        let fits = [reloc(0, 0, 2), reloc(4, 1, 1), reloc(8, 2, 0), reloc(12, 3, 0)];
        assert!(stored(fits.into()).sound(asked));
        for outside in [
            reloc(0, 0, 3),
            reloc(0, 1, 2),
            reloc(0, 2, 1),
            reloc(0, 3, 1),
            reloc(0, 4, 0),
            reloc(32, 0, 0),
        ] {
            assert!(!stored(vec![outside]).sound(asked));
        }
        assert!(!stored(Vec::new()).sound(Mark { funcs: 2, datas: 2 }));
        assert!(!stored(Vec::new()).sound(Mark { funcs: 3, datas: 1 }));
    }

    #[cfg(feature = "llvm")]
    #[test]
    fn no_bytes_break_the_reading_of_code_of_llvm() {
        use super::super::fragment::Mark;
        let code = LlvmEntry::Code(stored(vec![reloc(0, 0, 2), reloc(8, 2, 0)]));
        let left = LlvmEntry::LeftToCranelift("opcode `iadd`".to_owned());
        let seeds = vec![code.encode().unwrap(), left.encode().unwrap()];
        let (mut read, mut refused) = (0, 0);
        for bytes in fuzz::inputs(concat!(env!("CARGO_MANIFEST_DIR"), "/corpus/stored-code"), seeds)
        {
            let (largest, stored) = fuzz::largest_allocation(|| LlvmEntry::decode(&bytes));
            assert!(largest <= fuzz::allowed(bytes.len()), "{largest} bytes for {}", bytes.len());
            match stored {
                Some(LlvmEntry::Code(stored)) => {
                    stored.sound(Mark { funcs: 3, datas: 2 });
                    read += 1;
                }
                Some(LlvmEntry::LeftToCranelift(_)) => read += 1,
                None => refused += 1,
            }
        }
        eprintln!("stored code: {read} read, {refused} refused");
        assert!(read > 1 && refused > 0);
    }
}
