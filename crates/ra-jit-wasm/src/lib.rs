//! ra-jit's compiler without the daemon around it: sources come from memory, and what is built
//! is a WebAssembly module instead of an object for the system linker. It builds for
//! `wasm32-wasip1` itself, which is how it gets into a browser.
//!
//! A [`Compiler`] holds the sources of one program and of the `core` it uses, both compiled
//! from source: there is no rustc to have built anything. Every [`Compiler::build`] gives a
//! [`link::Patch`]: a module with the functions and data that are new since the build before,
//! to instantiate over the memory and the function table of the modules before it.

pub mod bundle;
#[cfg(target_family = "wasm")]
mod exports;
pub mod host;
pub mod link;
pub mod macros;
mod names;
pub mod pack;
// The table `ra-jit` reads from the rlibs of a build, here made from the names in a module.
#[allow(dead_code)]
#[path = "../../ra-jit/src/symbols.rs"]
mod symbols;
// What the table reads the sources of a crate from.
#[allow(dead_code)]
#[path = "../../ra-jit/src/dep_info.rs"]
mod dep_info;

use std::time::Duration;

use cfg::{CfgAtom, CfgOptions};
use hir::{Crate, Module};
use hir_def::{
    AssocItemId, FunctionId, HasModule as _, ItemContainerId, Lookup, ModuleDefId, StaticId,
    signatures::FunctionSignature,
};
use hir_ty_codegen::{
    CodegenError, CodegenUnit, Rejections, Roots, SymbolResolver, exported_symbol,
};
use ide_db::{
    ChangeWithProcMacros, RootDatabase,
    base_db::{
        CrateGraphBuilder, CrateOrigin, CrateWorkspaceData, DependencyBuilder, Env,
        LangCrateOrigin, SourceDatabase as _, SourceRoot, TargetKind,
        target::{Arch, TargetData},
    },
};
use intern::Symbol;
use rustc_hash::{FxHashMap, FxHashSet};
use span::Edition;
use triomphe::Arc;
use vfs::{AbsPathBuf, FileId, VfsPath, file_set::FileSet};

use crate::link::{Linker, Patch};

/// Where the program's crate starts.
pub const MAIN: &str = "/app/main.rs";
/// Where `core` starts, when its source is there.
pub const CORE: &str = "/core/src/lib.rs";

/// The layout of `wasm32-unknown-unknown`.
const DATA_LAYOUT: &str = "e-m:e-p:32:32-p10:8:8-p20:8:8-i64:64-i128:128-n32:64-S128-ni:1:10:20";

pub struct Compiler {
    db: RootDatabase,
    files: FxHashMap<String, FileId>,
    /// Files added since the crate graph was last set.
    new_files: bool,
    known: Rejections,
    linker: Linker,
    /// The functions of `core` by symbol, listed once: its source does not change.
    library: Option<FxHashMap<String, FunctionId>>,
    /// The functions of the program in the last build.
    local: FxHashSet<FunctionId>,
    /// Functions of `core` that code generation calls by path, found by building without.
    by_path: Vec<FunctionId>,
    /// The object of the last build that was not a patch.
    base: Vec<u8>,
    /// Proc macro calls of a pack that it had no expansion for.
    missed: std::sync::Arc<std::sync::Mutex<Vec<pack::Call>>>,
    /// The module rustc built that the program is linked into, and the symbols it has.
    host: Option<(host::Host, symbols::SymbolTable)>,
}

/// What one build did.
pub struct Build {
    pub patch: Patch,
    /// Functions of the program that were not compiled, with why.
    pub skipped: Vec<(String, String)>,
    /// Functions in the build, and how many of them this build had to compile.
    pub bodies: usize,
    pub compiled: usize,
    pub listing: Duration,
    pub codegen: Duration,
    pub link: Duration,
}

impl Build {
    /// What the page needs to know to instantiate the patch, as JSON.
    pub fn report(&self) -> String {
        let quote = |text: &str| {
            let mut quoted = String::from("\"");
            for c in text.chars() {
                match c {
                    '"' | '\\' => quoted.extend(['\\', c]),
                    c if c.is_control() => quoted.push_str(&format!("\\u{:04x}", c as u32)),
                    c => quoted.push(c),
                }
            }
            quoted + "\""
        };
        let list = |items: Vec<String>| items.join(",");
        let patch = &self.patch;
        let exports = patch.exports.iter().map(|(name, slot)| format!("{}:{slot}", quote(name)));
        let skipped = self
            .skipped
            .iter()
            .map(|(name, why)| format!("{{\"name\":{},\"why\":{}}}", quote(name), quote(why)));
        let functions = patch.functions.iter().map(|name| quote(name));
        let millis = |it: Duration| it.as_secs_f64() * 1000.0;
        format!(
            "{{\"table_len\":{},\"memory_len\":{},\"stack_top\":{},\"exports\":{{{}}},\
             \"functions\":[{}],\"data_len\":{},\"skipped\":[{}],\"bodies\":{},\
             \"compiled\":{},\"listing_ms\":{:.3},\"codegen_ms\":{:.3},\"link_ms\":{:.3},\
             \"host\":{{{}}},\"mismatched\":[{}]}}",
            patch.table_len,
            patch.memory_len,
            patch.stack_top,
            list(exports.collect()),
            list(functions.collect()),
            patch.data_len,
            list(skipped.collect()),
            self.bodies,
            self.compiled,
            millis(self.listing),
            millis(self.codegen),
            millis(self.link),
            list(patch.host.iter().map(|(name, slot)| format!("{}:{slot}", quote(name))).collect()),
            list(patch.mismatched.iter().map(|it| quote(it)).collect()),
        )
    }
}

impl Default for Compiler {
    fn default() -> Self {
        // The bodies of derived methods are compiled like any other.
        hir_def::nameres::EXPAND_BUILTIN_DERIVES.store(true, std::sync::atomic::Ordering::Relaxed);
        // MIR is lowered for the target's address size, which is said when it is selected.
        hir_ty_codegen::Target::current();
        Compiler {
            db: RootDatabase::new(None),
            files: FxHashMap::default(),
            new_files: false,
            known: Rejections::default(),
            linker: Linker::default(),
            library: None,
            local: FxHashSet::default(),
            by_path: Vec::new(),
            base: Vec::new(),
            missed: Default::default(),
            host: None,
        }
    }
}

impl Compiler {
    /// Sets the text of a file, new or not. The program is the crate at [`MAIN`], `core` the
    /// one at [`CORE`].
    pub fn set_file(&mut self, path: &str, text: String) {
        self.set_files([(path.to_owned(), text)]);
    }

    pub fn set_files(&mut self, files: impl IntoIterator<Item = (String, String)>) {
        let mut change = ChangeWithProcMacros::default();
        let mut changed = Vec::new();
        for (path, text) in files {
            let next = FileId::from_raw(self.files.len() as u32);
            let id = *self.files.entry(path).or_insert_with(|| {
                self.new_files = true;
                next
            });
            change.change_file(id, Some(text));
            changed.push(id);
        }
        if std::mem::take(&mut self.new_files) {
            let (roots, graph) = self.project();
            change.set_roots(roots);
            change.set_crate_graph(graph);
        }
        self.db.apply_change(change);
        // The build after this looks at what the changed files can have changed.
        for file in changed {
            let crates = ide_db::base_db::relevant_crates(&self.db, file);
            self.known.touch(&self.db, (!crates.is_empty()).then_some(crates));
        }
    }

    /// Sets the workspace of a pack as what there is to compile. The file its program starts
    /// at is [`MAIN`] from then on.
    pub fn set_pack(&mut self, pack: pack::Pack, fallback: pack::Fallback) -> Result<(), String> {
        let mut pack = pack;
        if let Some(host) = pack.host.take() {
            let functions = host.functions.iter().map(|it| it.0.as_str());
            let names = functions.chain(host.data.iter().map(|it| it.0.as_str()));
            let table = symbols::SymbolTable::of_names(names);
            self.host = Some((host, table));
        }
        let project = pack.project(fallback)?;
        let mut change = ChangeWithProcMacros::default();
        for (id, path, text) in project.files {
            let path = if id == project.main { MAIN.to_owned() } else { path };
            self.files.insert(path, id);
            change.change_file(id, Some(text));
        }
        change.set_roots(project.roots);
        change.set_crate_graph(project.graph);
        change.set_proc_macros(project.macros);
        self.db.apply_change(change);
        self.db.enable_proc_attr_macros();
        self.missed = project.missed;
        Ok(())
    }

    /// Where what is linked into the pack's module goes: the first slot of its table and
    /// the first byte of its memory that are free.
    pub fn set_start(&mut self, table_start: u32, memory_start: u64) {
        if let Some((host, _)) = &self.host {
            self.linker.set_host(host, table_start, memory_start);
        }
    }

    /// Resolves the names of every crate and infers the types of the program, for how long
    /// each takes: (the number of crates, name resolution, inference).
    pub fn analyze(&self) -> (usize, Duration, Duration) {
        let db = &self.db;
        let start = stdx::time::Instant::now();
        let crates = ide_db::base_db::all_crates(db);
        for &krate in crates.iter() {
            hir_def::nameres::crate_def_map(db, krate);
        }
        let resolved = start.elapsed();
        hir::attach_db(db, || {
            for &krate in crates.iter().filter(|it| it.data(db).origin.is_local()) {
                for function in functions_of(db, krate.into()) {
                    hir_ty::InferenceResult::of(db, hir_def::DefWithBodyId::from(function));
                }
            }
        });
        (crates.len(), resolved, start.elapsed() - resolved)
    }

    /// The bodies of the program's functions as the compiler sees them, macros expanded.
    pub fn expanded(&self) -> String {
        let db = &self.db;
        let mut text = String::new();
        for &krate in ide_db::base_db::all_crates(db).iter() {
            if krate.data(db).origin.is_local() {
                for function in functions_of(db, krate.into()) {
                    text.push_str(
                        &hir::DefWithBody::from(hir::Function::from(function)).debug_hir(db),
                    );
                    text.push('\n');
                }
            }
        }
        text
    }

    /// Proc macro calls that had no recorded expansion, as `crate::name(input)`.
    pub fn missed(&self) -> Vec<String> {
        let missed = self.missed.lock().unwrap_or_else(|it| it.into_inner());
        missed.iter().map(|it| format!("{}::{}({})", it.krate, it.name, it.input)).collect()
    }

    /// The source roots and crates of the files there are: the program, and `core` under it.
    fn project(&self) -> (Vec<SourceRoot>, CrateGraphBuilder) {
        let (mut app, mut core) = (FileSet::default(), FileSet::default());
        for (path, &id) in &self.files {
            let set = if path.starts_with("/app/") { &mut app } else { &mut core };
            set.insert(id, VfsPath::new_virtual_path(path.clone()));
        }
        let roots = vec![SourceRoot::new_local(app), SourceRoot::new_library(core)];

        let mut cfg = CfgOptions::default();
        let key = |key: &str, value: &str| CfgAtom::KeyValue {
            key: Symbol::intern(key),
            value: Symbol::intern(value),
        };
        cfg.extend([
            key("target_arch", "wasm32"),
            key("target_pointer_width", "32"),
            key("target_endian", "little"),
            key("target_os", "unknown"),
            key("target_family", "wasm"),
            key("panic", "abort"),
            CfgAtom::Flag(Symbol::intern("debug_assertions")),
        ]);
        for width in ["8", "16", "32", "64", "ptr"] {
            cfg.extend([
                key("target_has_atomic", width),
                key("target_has_atomic_load_store", width),
            ]);
        }
        let workspace = Arc::new(CrateWorkspaceData {
            target: Ok(TargetData { data_layout: DATA_LAYOUT.into(), arch: Arch::Wasm32 }),
            toolchain: None,
        });
        let cwd = Arc::new(AbsPathBuf::assert("/".into()));
        let mut graph = CrateGraphBuilder::default();
        let mut add = |root: FileId, name: &str, origin: CrateOrigin| {
            graph.add_crate_root(
                root,
                Edition::CURRENT,
                Some(ide_db::base_db::CrateName::new(name).expect("a crate name").into()),
                None,
                cfg.clone(),
                None,
                Env::default(),
                origin,
                Vec::new(),
                TargetKind::Lib { is_proc_macro: false },
                cwd.clone(),
                workspace.clone(),
            )
        };
        let core = self
            .files
            .get(CORE)
            .map(|&root| add(root, "core", CrateOrigin::Lang(LangCrateOrigin::Core)));
        let app = self
            .files
            .get(MAIN)
            .map(|&root| add(root, "app", CrateOrigin::Local { repo: None, name: None }));
        if let (Some(app), Some(core)) = (app, core) {
            let name = ide_db::base_db::CrateName::new("core").expect("a crate name");
            let dep = DependencyBuilder::with_prelude(name, core, true, true);
            graph.add_dep(app, dep).expect("`core` depends on nothing");
        }
        (roots, graph)
    }

    /// Compiles what changed since the last build and links it into a module.
    pub fn build(&mut self) -> Result<Build, String> {
        let start = stdx::time::Instant::now();
        let db = &self.db;
        let mut entries = Vec::new();
        // What the module linked into has is not compiled: nothing is found by path.
        let host = self.host.as_ref();
        let library = self.library.get_or_insert_with(|| {
            let mut library = FxHashMap::default();
            if host.is_some() {
                return library;
            }
            let names = Names { db, library: &library };
            let mut named = Vec::new();
            for &krate in ide_db::base_db::all_crates(db).iter() {
                if !krate.data(db).origin.is_local() {
                    let functions = functions_of(db, krate.into());
                    named.extend(
                        functions.into_iter().filter_map(|it| Some((names.function(it)?, it))),
                    );
                }
            }
            library.extend(named);
            library
        });
        for &krate in ide_db::base_db::all_crates(db).iter() {
            if krate.data(db).origin.is_local() {
                entries.extend(functions_of(db, krate.into()));
            }
        }
        // This is built without unwinding, so nothing may ask about a function that is gone.
        let local: FxHashSet<FunctionId> = entries.iter().copied().collect();
        if !self.local.is_subset(&local) {
            self.known.forget_base();
        }
        self.local = local;
        // In the order a build that found them left them in, for the next to be a patch.
        entries.extend(&self.by_path);
        let on_demand: Vec<FunctionId> = library.values().copied().collect();
        let listing = start.elapsed();
        let (paths, rustc);
        let resolver: &dyn SymbolResolver = match host {
            Some((_, table)) => {
                rustc = names::Resolver { db, table };
                &rustc
            }
            None => {
                paths = Names { db, library };
                &paths
            }
        };
        let known = &mut self.known;
        // What the code generator asks for by path has to be among the roots, and which paths
        // those are shows in what a build leaves undefined.
        known.patching = true;
        let unit = loop {
            let roots = Roots {
                entries: &entries,
                on_demand: &on_demand,
                start: None,
                shared: false,
                whole: false,
                kept: &[],
                offered: &[],
            };
            let unit = hir::attach_db(db, || {
                CodegenUnit::compile(db, resolver, roots, &Default::default(), known)
            })
            .map_err(|e| e.to_string())?;
            let before = entries.len();
            for object in unit.patch.iter().chain(&unit.object) {
                for symbol in Linker::undefined(object)? {
                    if let Some(&func) = library.get(&symbol)
                        && !entries.contains(&func)
                        && !self.linker.defines(&symbol)
                    {
                        entries.push(func);
                        self.by_path.push(func);
                    }
                }
            }
            if entries.len() == before {
                break unit;
            }
        };
        let codegen = start.elapsed() - listing;
        // A patch amends the last object built in full.
        if let Some(object) = unit.object {
            self.base = object;
        }
        let objects: Vec<&[u8]> =
            unit.patch.iter().map(Vec::as_slice).chain([self.base.as_slice()]).collect();
        let exports = unit.compiled.iter().map(|(_, symbol)| symbol.as_str());
        let patch = self.linker.link(&objects, exports)?;
        let skipped = unit
            .skipped
            .iter()
            // A generic function has no code of its own to be missing, only that of its uses.
            .filter(|(_, why)| !why.no_symbol())
            .filter(|(func, _)| func.krate(db).data(db).origin.is_local())
            .map(|&(func, ref why)| {
                let name = resolver
                    .function(func)
                    .unwrap_or_else(|| FunctionSignature::of(db, func).name.as_str().to_owned());
                (name, why.to_string())
            })
            .collect();
        Ok(Build {
            patch,
            skipped,
            bodies: known.stats.bodies,
            compiled: known.stats.compiled,
            listing,
            codegen,
            link: start.elapsed() - listing - codegen,
        })
    }
}

/// The functions of a crate's modules and `impl`s that have a body.
// FIXME: those declared inside a body are not listed, as `ra-jit` lists them.
pub fn functions_of(db: &RootDatabase, krate: Crate) -> Vec<FunctionId> {
    let mut modules: Vec<Module> = vec![krate.root_module(db)];
    let mut functions = Vec::new();
    while let Some(module) = modules.pop() {
        modules.extend(module.children(db));
        let id = hir_def::ModuleId::from(module);
        let scope = id.def_map(db).module(db, id).scope(db);
        for def in scope.declarations() {
            if let ModuleDefId::FunctionId(id) = def
                && !matches!(id.lookup(db).container, ItemContainerId::ExternBlockId(_))
            {
                functions.push(id);
            }
        }
        for imp in scope.impls() {
            for &(_, item) in imp.impl_items(db).items.iter() {
                if let AssocItemId::FunctionId(id) = item {
                    functions.push(id);
                }
            }
        }
    }
    functions
}

/// Names for what rustc would have named, had it built anything: the path of the item.
struct Names<'a> {
    db: &'a RootDatabase,
    library: &'a FxHashMap<String, FunctionId>,
}

impl Names<'_> {
    fn module_path(&self, module: Module) -> Option<String> {
        // FIXME: items declared in a body have no name here.
        if module.nearest_non_block_module(self.db) != module {
            return None;
        }
        let krate = module.krate(self.db).display_name(self.db)?;
        let mut path = krate.crate_name().as_str().to_owned();
        for segment in module.path_segments(self.db) {
            path.push_str("::");
            path.push_str(segment.as_str());
        }
        Some(path)
    }
}

impl SymbolResolver for Names<'_> {
    fn function(&self, func: FunctionId) -> Option<String> {
        if let Some(symbol) = exported_symbol(self.db, func) {
            return Some(symbol);
        }
        let generic = |def: hir::GenericDef| !def.type_or_const_params(self.db).is_empty();
        let function = hir::Function::from(func);
        if generic(function.into()) {
            return None;
        }
        let module = self.module_path(function.module(self.db))?;
        let name = function.name(self.db);
        match func.lookup(self.db).container {
            ItemContainerId::ModuleId(_) => Some(format!("{module}::{}", name.as_str())),
            ItemContainerId::ImplId(imp) if !generic(hir::Impl::from(imp).into()) => {
                // The `impl`s of a module are told apart by their place among its items.
                let mut place = rustc_hash::FxHasher::default();
                std::hash::Hash::hash(&imp.lookup(self.db).id, &mut place);
                let place = std::hash::Hasher::finish(&place) as u32;
                Some(format!("{module}::{{impl#{place:x}}}::{}", name.as_str()))
            }
            ItemContainerId::ImplId(_)
            | ItemContainerId::TraitId(_)
            | ItemContainerId::ExternBlockId(_) => None,
        }
    }

    fn static_(&self, id: StaticId) -> Option<String> {
        let it = hir::Static::from(id);
        Some(format!("{}::{}", self.module_path(it.module(self.db))?, it.name(self.db).as_str()))
    }

    fn path(&self, path: &str) -> Option<String> {
        self.library.contains_key(path).then(|| path.to_owned())
    }

    fn item(&self, item: ModuleDefId) -> Option<String> {
        let item = hir::ModuleDef::from(item);
        let module = self.module_path(item.module(self.db)?)?;
        Some(format!("{module}::{}", item.name(self.db)?.as_str()))
    }

    fn source_path(&self, file: FileId) -> Option<String> {
        let root = self.db.file_source_root(file).source_root_id(self.db);
        let root = self.db.source_root(root).source_root(self.db);
        Some(root.path_for_file(&file)?.to_string())
    }

    fn stable_crate_id(&self, krate: ide_db::base_db::Crate) -> Option<u64> {
        // Nothing here is linked with what another compiler named.
        let name = Crate::from(krate).display_name(self.db)?;
        Some(name.crate_name().as_str().bytes().fold(0xcbf29ce484222325, |hash, byte| {
            (hash ^ byte as u64).wrapping_mul(0x100000001b3)
        }))
    }
}
