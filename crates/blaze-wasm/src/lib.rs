//! blaze's compiler without the daemon around it: sources come from memory, and what is built
//! is a WebAssembly module instead of an image for the system. It builds for `wasm32-wasip1`
//! itself, which is how it gets into a browser.
//!
//! A [`Compiler`] holds the sources of one program and of the `core` it uses, both compiled
//! from source: there is no rustc to have built anything. A build is v2's per-function build
//! ([`blaze_codegen::driver::Build`] over [`hir_ty_codegen::v2::HirFrontend`]), its templates
//! kept in the compiler's memo and its puts in the compiler's own store between builds, so an
//! edit compiles only what it changed. Every [`Compiler::build`] gives a [`link::Patch`]: a
//! module with the functions and data that are new since the build before, to instantiate
//! over the memory and the function table of the modules before it.

pub mod bundle;
#[cfg(target_family = "wasm")]
mod exports;
pub mod link;
pub mod macros;
pub use blaze_wasm_pack as pack;

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use base_db::{
    AbsPathBuf, CrateGraphBuilder, CrateOrigin, CrateWorkspaceData, Db, DependencyBuilder, Env, FileId,
    FileSet, LangCrateOrigin, SourceRoot, TargetKind, VfsPath,
    target::{Arch, TargetData},
};
use blaze_codegen::{
    driver::{self, Lookup},
    memo::Memo,
    plan::{Mode, OptLevel, Origin, Override, Package, Policy, Profile},
    settings::{DebugInfo, Panic, Pgo, Sanitize, Toolchain},
};
use blaze_hash::HashMap;
use cfg::{CfgAtom, CfgOptions};
use hir::{Crate, Module};
use hir_def::{AssocItemId, FunctionId, ItemContainerId, Lookup as _, ModuleDefId};
use hir_expand::change::ChangeWithProcMacros;
use hir_ty_codegen::v2::{Backends, BuildTime, Databases, HashSource, HirFrontend, ItemHashes, RootKind, Runs};
use intern::Symbol;
use parking_lot::Mutex;
use span::Edition;
use triomphe::Arc as TArc;

use crate::link::{Linker, Patch};

/// Where the program's crate starts.
pub const MAIN: &str = "/app/main.rs";
/// Where `core` starts, when its source is there.
pub const CORE: &str = "/core/src/lib.rs";

/// What the page's code is compiled for.
const TARGET: &str = "wasm32-unknown-unknown";
/// The layout of `wasm32-unknown-unknown`.
const DATA_LAYOUT: &str = "e-m:e-p:32:32-p10:8:8-p20:8:8-i64:64-i128:128-n32:64-S128-ni:1:10:20";

pub struct Compiler {
    db: Db,
    files: HashMap<String, FileId>,
    /// Files added since the crate graph was last set.
    new_files: bool,
    linker: Linker,
    /// The code generator's templates by shape, kept from one build to the next.
    memo: Memo,
    /// What the builds put in the store, which the next reads: the page has no other.
    store: Stored,
    /// Proc macro calls of a pack that it had no expansion for.
    missed: Arc<Mutex<Vec<pack::Call>>>,
}

/// The store of a compiler with no disk: every put of its builds, by key.
#[derive(Default)]
struct Stored(HashMap<blaze_cas::Key, Vec<u8>>);

impl Lookup for Stored {
    fn read(&self, key: &blaze_cas::Key) -> Result<Option<&[u8]>, String> {
        Ok(self.0.get(key).map(Vec::as_slice))
    }
}

/// What one build did.
pub struct Build {
    pub patch: Patch,
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
        let functions = patch.functions.iter().map(|name| quote(name));
        let millis = |it: Duration| it.as_secs_f64() * 1000.0;
        format!(
            "{{\"table_len\":{},\"memory_len\":{},\"stack_top\":{},\"exports\":{{{}}},\
             \"functions\":[{}],\"data_len\":{},\"bodies\":{},\"compiled\":{},\
             \"listing_ms\":{:.3},\"codegen_ms\":{:.3},\"link_ms\":{:.3}}}",
            patch.table_len,
            patch.memory_len,
            patch.stack_top,
            list(exports.collect()),
            list(functions.collect()),
            patch.data_len,
            self.bodies,
            self.compiled,
            millis(self.listing),
            millis(self.codegen),
            millis(self.link),
        )
    }
}

impl Default for Compiler {
    fn default() -> Self {
        // The bodies of derived methods are compiled like any other.
        hir_def::nameres::EXPAND_BUILTIN_DERIVES.store(true, std::sync::atomic::Ordering::Relaxed);
        Compiler {
            db: Db::new(),
            files: HashMap::default(),
            new_files: false,
            linker: Linker::default(),
            memo: Memo::new(),
            store: Stored::default(),
            missed: Default::default(),
        }
    }
}

/// How the page's code is compiled: as a dev build that takes edits, its members' functions
/// reached through their slots, so that an edit swaps one slot.
fn policy(toolchain: Toolchain) -> Policy {
    Policy {
        toolchain,
        profile: Profile {
            opt_level: OptLevel::O0,
            overflow_checks: true,
            debug_assertions: true,
            debuginfo: DebugInfo::None,
            panic: Panic::Abort,
            packages: BTreeMap::new(),
            build_override: Override::default(),
        },
        mode: Mode::Live,
        fold: true,
        sanitize: Sanitize::default(),
        pgo: Pgo::None,
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
        for (path, text) in files {
            let next = FileId::from_raw(self.files.len() as u32);
            let id = *self.files.entry(path).or_insert_with(|| {
                self.new_files = true;
                next
            });
            change.change_file(id, Some(text));
        }
        if std::mem::take(&mut self.new_files) {
            let (roots, graph) = self.project();
            change.set_roots(roots);
            change.set_crate_graph(graph);
        }
        change.apply(&mut self.db);
    }

    /// Sets the workspace of a pack as what there is to compile. The file its program starts
    /// at is [`MAIN`] from then on.
    pub fn set_pack(&mut self, pack: pack::Pack, builtin: pack::Builtin) -> Result<(), String> {
        let project = pack.project(builtin)?;
        let mut change = ChangeWithProcMacros::default();
        for (id, path, text) in project.files {
            let path = if id == project.main { MAIN.to_owned() } else { path };
            self.files.insert(path, id);
            change.change_file(id, Some(text));
        }
        change.set_roots(project.roots);
        change.set_crate_graph(project.graph);
        change.set_proc_macros(project.macros);
        change.apply(&mut self.db);
        self.missed = project.missed;
        Ok(())
    }

    /// Where the program goes when it lives in another module's memory and table: the first
    /// slot of its table and the first byte of its memory that are free.
    pub fn set_start(&mut self, table_start: u32, memory_start: u64) -> Result<(), String> {
        self.linker.set_start(table_start, memory_start)
    }

    /// Resolves the names of every crate and infers the types of the program, for how long
    /// each takes: (the number of crates, name resolution, inference).
    pub fn analyze(&self) -> (usize, Duration, Duration) {
        let db = &self.db;
        let start = stdx::time::Instant::now();
        let crates = base_db::all_crates(db);
        for &krate in crates.iter() {
            hir_def::nameres::crate_def_map(db, krate);
        }
        let resolved = start.elapsed();
        hir_ty::attach_db(db, || {
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
        for &krate in base_db::all_crates(db).iter() {
            if krate.data(db).origin.is_local() {
                for function in functions_of(db, krate.into()) {
                    text.push_str(&hir::DefWithBody::from(hir::Function::from(function)).debug_hir(db));
                    text.push('\n');
                }
            }
        }
        text
    }

    /// Proc macro calls that had no recorded expansion, as `crate::name(input)`.
    pub fn missed(&self) -> Vec<String> {
        let missed = self.missed.lock();
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
        let key = |key: &str, value: &str| CfgAtom::KeyValue { key: Symbol::intern(key), value: Symbol::intern(value) };
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
            cfg.extend([key("target_has_atomic", width), key("target_has_atomic_load_store", width)]);
        }
        let workspace = TArc::new(CrateWorkspaceData {
            target: Ok(TargetData {
                tuple: Symbol::intern(TARGET),
                data_layout: DATA_LAYOUT.into(),
                arch: Arch::Wasm32,
                has_thumb_interworking: false,
            }),
            toolchain: None,
            const_eval: Default::default(),
        });
        let cwd = TArc::new(AbsPathBuf::assert("/".into()));
        let mut graph = CrateGraphBuilder::default();
        let mut add = |root: FileId, name: &str, origin: CrateOrigin, attrs: &[&str]| {
            graph.add_crate_root(
                root,
                Edition::CURRENT,
                Some(base_db::CrateName::new(name).expect("a crate name").into()),
                None,
                cfg.clone(),
                None,
                Env::default(),
                origin,
                attrs.iter().map(|&it| it.to_owned()).collect(),
                TargetKind::Lib { is_proc_macro: false },
                cwd.clone(),
                workspace.clone(),
            )
        };
        // The graph has no std: without `no_std` the app's prelude is std's, a crate that can't
        // be found (E0463). `core`'s own source says `#![no_core]`.
        let core = self.files.get(CORE).map(|&root| add(root, "core", CrateOrigin::Lang(LangCrateOrigin::Core), &[]));
        let local = CrateOrigin::Local { repo: None, name: None };
        let app = self.files.get(MAIN).map(|&root| add(root, "app", local, &["no_std"]));
        if let (Some(app), Some(core)) = (app, core) {
            let name = base_db::CrateName::new("core").expect("a crate name");
            let dep = DependencyBuilder::with_prelude(name, core, true, true);
            graph.add_dep(app, dep).expect("`core` depends on nothing");
        }
        (roots, graph)
    }

    /// Compiles what changed since the last build and links it into a module. Any function
    /// the code generator can't compile stops the build, naming it.
    pub fn build(&mut self) -> Result<Build, String> {
        let start = stdx::time::Instant::now();
        let toolchain = hir_ty_codegen::v2::toolchain(TARGET, &[], Runs::Elsewhere)?;
        hir_ty_codegen::v2::select(&toolchain)?;
        let policy = policy(toolchain);
        let db = &self.db;
        let main = *self.files.get(MAIN).ok_or_else(|| format!("there is no program: no file is at {MAIN}"))?;
        let app = base_db::all_crates(db)
            .iter()
            .copied()
            .find(|it| it.data(db).root_file_id == main)
            .ok_or_else(|| format!("no crate starts at {MAIN}"))?;
        let package = app.extra_data(db).display_name.as_ref().map_or_else(|| "app".to_owned(), |it| it.to_string());
        let paths: HashMap<FileId, String> = self.files.iter().map(|(path, &id)| (id, path.clone())).collect();
        let names = move |file: FileId| paths.get(&file).cloned();
        let frontend = HirFrontend::new(
            Databases::new(db),
            HashSource::Items(ItemHashes::new(Box::new(names))),
            Arc::new(BuildTime::default()),
        )
        .with_policy(&policy);
        let roots = frontend.roots(app, RootKind::Shared).map_err(|it| it.what)?;
        let origin = Origin::Member(Package::new(&package));
        let asked: Vec<_> = roots.roots.iter().map(|it| (it.clone(), origin)).collect();
        let listing = start.elapsed();

        let backends = Backends::default();
        let built = driver::Build::new(&backends, &frontend, &policy, &self.memo)
            .run_with_statics(&asked, &roots.statics, &self.store)
            .map_err(|errors| errors.into_iter().map(|it| it.what).collect::<Vec<_>>().join("\n"))?;
        let side = frontend.link_inputs(&built).map_err(|it| it.what)?;
        let codegen = start.elapsed() - listing;

        let patch = self.linker.link(&built, &side.spans)?;
        for put in built.puts {
            self.store.0.insert(put.key, put.bytes);
        }
        Ok(Build {
            patch,
            bodies: built.stats.instances,
            compiled: built.stats.compiled,
            listing,
            codegen,
            link: start.elapsed() - listing - codegen,
        })
    }
}

/// The functions of a crate's modules and `impl`s that have a body.
// FIXME: those declared inside a body are not listed.
pub fn functions_of(db: &Db, krate: Crate) -> Vec<FunctionId> {
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
