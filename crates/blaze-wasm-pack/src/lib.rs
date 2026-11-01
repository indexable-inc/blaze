//! A cargo workspace as one file, for a compiler that has no cargo, no file system and no
//! process to run a proc macro in: the crates with what the workspace's load said of them,
//! their sources, and what every proc macro call in them expanded to.
//!
//! [`record`] makes one of a front end `blaze serve` lowered, its build scripts run and its
//! proc macros built in the sandbox; [`Pack::project`] gives a database what it holds,
//! anywhere. No codegen is linked here, so the daemon can depend on it without the
//! `wasm` feature of `hir-ty-codegen` reaching the native build.

#![forbid(unsafe_code)]

use std::sync::Arc;

use base_db::{
    AbsPathBuf, CrateDisplayName, CrateGraphBuilder, CrateName, CrateOrigin, CrateWorkspaceData,
    Db, DependencyBuilder, FileId, FileSet, LangCrateOrigin, SourceRoot, TargetKind, VfsPath,
    target::{Arch, TargetData},
};
use blaze_hash::HashMap;
use cfg::CfgAtom;
use hir_expand::proc_macro::{
    ProcMacro, ProcMacroCall, ProcMacroExpander, ProcMacroExpansionError, ProcMacroKind,
    ProcMacrosBuilder,
};
use intern::Symbol;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use span::Edition;

mod record;
#[cfg(test)]
mod tests;

pub use record::record;

#[derive(Default, Serialize, Deserialize)]
pub struct Pack {
    /// The file the program's crate starts at.
    pub main: u32,
    /// The target's tuple and data layout, as the workspace was loaded for it.
    pub tuple: String,
    pub data_layout: String,
    pub files: Vec<File>,
    pub roots: Vec<Root>,
    /// Dependencies before what depends on them.
    pub crates: Vec<Krate>,
    pub expansions: Vec<Expansion>,
}

#[derive(Serialize, Deserialize)]
pub struct File {
    pub id: u32,
    pub path: String,
    pub text: String,
}

#[derive(Serialize, Deserialize)]
pub struct Root {
    pub library: bool,
    pub files: Vec<u32>,
}

#[derive(Serialize, Deserialize)]
pub struct Krate {
    pub root: u32,
    pub edition: String,
    pub name: Option<String>,
    pub version: Option<String>,
    /// `key` or `key=value`.
    pub cfg: Vec<String>,
    pub env: Vec<(String, String)>,
    pub origin: Origin,
    pub attrs: Vec<String>,
    pub deps: Vec<Dep>,
    /// The macros of a proc macro crate, in the order it declares them.
    pub macros: Option<Vec<Macro>>,
}

#[derive(Serialize, Deserialize)]
pub enum Origin {
    Local,
    Library(String),
    Lang(String),
}

#[derive(Serialize, Deserialize)]
pub struct Dep {
    /// An index into [`Pack::crates`].
    pub krate: usize,
    pub name: String,
    pub prelude: bool,
    pub sysroot: bool,
}

#[derive(Serialize, Deserialize)]
pub struct Macro {
    pub name: String,
    pub kind: Kind,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Kind {
    Derive,
    Bang,
    Attr,
}

/// One call of a proc macro: what it was given, as text, and what it gave.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Call {
    pub krate: String,
    pub name: String,
    pub input: String,
    pub attr: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct Expansion {
    pub call: Call,
    pub output: Result<String, String>,
}

/// The expansion of a call by a proc macro the compiler has in it (`blaze_wasm::macros`), for
/// a call the pack has no expansion of: `None` when it has no such macro.
pub type Builtin = fn(&Call) -> Option<Result<String, String>>;

pub type Expansions = HashMap<Call, Result<String, String>>;

/// A proc macro that is the expansions it had when the pack was made.
#[derive(Debug)]
struct Replay {
    krate: String,
    name: String,
    expansions: Arc<Expansions>,
    builtin: Builtin,
    /// Calls there was no expansion for.
    missed: Arc<Mutex<Vec<Call>>>,
}

/// A call that panicked adds no miss: the misses kept are each whole.
impl std::panic::RefUnwindSafe for Replay {}

impl ProcMacroExpander for Replay {
    fn expand(&self, _: &Db, call: ProcMacroCall<'_>) -> Result<tt::TopSubtree, ProcMacroExpansionError> {
        let recorded = Call {
            krate: self.krate.clone(),
            name: self.name.clone(),
            input: call.subtree.to_string(),
            attr: call.attrs.map(ToString::to_string),
        };
        let output = match self.expansions.get(&recorded) {
            Some(output) => output.clone(),
            None => (self.builtin)(&recorded).unwrap_or_else(|| {
                let missed = format!("no expansion of `{}` was recorded for this input", recorded.name);
                self.missed.lock().push(recorded);
                Err(missed)
            }),
        };
        let output = output.map_err(ProcMacroExpansionError::Panic)?;
        syntax_bridge::parse_to_token_tree_static_span(Edition::CURRENT, call.call_site, &output)
            .ok_or_else(|| ProcMacroExpansionError::System("the expansion does not lex".into()))
    }

    // dyn: hir-expand's `ProcMacroExpander::eq_dyn`, when a crate's macros are set.
    fn eq_dyn(&self, other: &dyn ProcMacroExpander) -> bool {
        // dyn: as above, the one way to compare across expander types.
        (other as &dyn std::any::Any).downcast_ref::<Self>().is_some_and(|other| {
            self.krate == other.krate
                && self.name == other.name
                && Arc::ptr_eq(&self.expansions, &other.expansions)
        })
    }
}

/// What a database is given to hold the workspace of a pack.
pub struct Project {
    pub files: Vec<(FileId, String, String)>,
    pub roots: Vec<SourceRoot>,
    pub graph: CrateGraphBuilder,
    pub macros: ProcMacrosBuilder,
    pub missed: Arc<Mutex<Vec<Call>>>,
    pub main: FileId,
}

impl Kind {
    fn of(kind: ProcMacroKind) -> Kind {
        match kind {
            ProcMacroKind::CustomDerive => Kind::Derive,
            ProcMacroKind::Bang => Kind::Bang,
            ProcMacroKind::Attr => Kind::Attr,
        }
    }

    fn proc_macro_kind(self) -> ProcMacroKind {
        match self {
            Kind::Derive => ProcMacroKind::CustomDerive,
            Kind::Bang => ProcMacroKind::Bang,
            Kind::Attr => ProcMacroKind::Attr,
        }
    }
}

impl Pack {
    pub fn project(self, builtin: Builtin) -> Result<Project, String> {
        let paths: HashMap<u32, &str> = self.files.iter().map(|it| (it.id, &*it.path)).collect();
        let roots = self
            .roots
            .iter()
            .map(|root| {
                let mut set = FileSet::default();
                for &file in &root.files {
                    let path = paths.get(&file).ok_or("a root with a file there is not")?;
                    set.insert(FileId::from_raw(file), VfsPath::new_virtual_path(path.to_string()));
                }
                Ok(if root.library { SourceRoot::new_library(set) } else { SourceRoot::new_local(set) })
            })
            .collect::<Result<Vec<_>, &str>>()?;

        let workspace = triomphe::Arc::new(CrateWorkspaceData {
            target: Ok(TargetData {
                tuple: Symbol::intern(&self.tuple),
                data_layout: self.data_layout.into(),
                arch: Arch::Wasm32,
                has_thumb_interworking: false,
            }),
            toolchain: None,
            const_eval: Default::default(),
        });
        let cwd = triomphe::Arc::new(AbsPathBuf::assert("/".into()));
        let expansions: Arc<Expansions> =
            Arc::new(self.expansions.into_iter().map(|it| (it.call, it.output)).collect());
        let missed = Arc::new(Mutex::new(Vec::new()));
        let mut graph = CrateGraphBuilder::default();
        let mut macros = ProcMacrosBuilder::default();
        let mut ids = Vec::new();
        for krate in &self.crates {
            let name = |name: &str| CrateName::new(name).map_err(|_| "a bad crate name");
            let cfg = krate.cfg.iter().map(|atom| match atom.split_once('=') {
                Some((key, value)) => CfgAtom::KeyValue { key: Symbol::intern(key), value: Symbol::intern(value) },
                None => CfgAtom::Flag(Symbol::intern(atom)),
            });
            let origin = match &krate.origin {
                Origin::Local => CrateOrigin::Local { repo: None, name: None },
                Origin::Library(name) => CrateOrigin::Library { repo: None, name: Symbol::intern(name) },
                Origin::Lang(name) => CrateOrigin::Lang(LangCrateOrigin::from(&**name)),
            };
            let display = krate.name.as_deref().map(CrateDisplayName::from_canonical_name);
            let edition: Edition = krate.edition.parse().map_err(|_| "a bad edition")?;
            let id = graph.add_crate_root(
                FileId::from_raw(krate.root),
                edition,
                display,
                krate.version.clone(),
                cfg.collect(),
                None,
                krate.env.iter().cloned().collect(),
                origin,
                krate.attrs.clone(),
                TargetKind::Lib { is_proc_macro: krate.macros.is_some() },
                cwd.clone(),
                workspace.clone(),
            );
            for dep in &krate.deps {
                let to = *ids.get(dep.krate).ok_or("a dependency after what depends on it")?;
                let dep = DependencyBuilder::with_prelude(name(&dep.name)?, to, dep.prelude, dep.sysroot);
                graph.add_dep(id, dep).map_err(|_| "a dependency cycle")?;
            }
            if let Some(list) = &krate.macros {
                let list = list.iter().map(|it| ProcMacro {
                    name: Symbol::intern(&it.name),
                    kind: it.kind.proc_macro_kind(),
                    expander: Arc::new(Replay {
                        krate: krate.name.clone().unwrap_or_default(),
                        name: it.name.clone(),
                        expansions: expansions.clone(),
                        builtin,
                        missed: missed.clone(),
                    }),
                    disabled: false,
                });
                macros.insert(id, Ok(list.collect()));
            }
            ids.push(id);
        }
        let files = self.files.into_iter().map(|it| (FileId::from_raw(it.id), it.path, it.text)).collect();
        Ok(Project { files, roots, graph, macros, missed, main: FileId::from_raw(self.main) })
    }
}
