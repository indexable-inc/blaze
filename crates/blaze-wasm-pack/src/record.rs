//! Making a [`Pack`] of a workspace's front end: the database `blaze serve` lowered for a job
//! (`blaze_lower::Lowered::served_db`), its build scripts run and its proc macros built and
//! loaded in the sandbox by the daemon. Nothing here loads a workspace or runs anything.

use std::sync::Arc;

use base_db::{Crate, CrateOrigin, Db};
use blaze_hash::{HashMap, HashSet};
use cfg::CfgAtom;
use hir_expand::proc_macro::{ProcMacro, ProcMacroCall, ProcMacroExpander, ProcMacroExpansionError, ProcMacros};
use intern::sym;
use parking_lot::Mutex;

use super::{Call, Dep, Expansion, Expansions, File, Kind, Krate, Macro, Origin, Pack, Root};

/// A proc macro that keeps what it was called with and what it gave.
#[derive(Debug)]
struct Recorder {
    krate: String,
    name: String,
    // dyn: the proc macro it records, as `ProcMacro::expander` holds it: the edge to user code.
    inner: Arc<dyn ProcMacroExpander>,
    expansions: Arc<Mutex<Expansions>>,
}

/// An expansion that panicked records nothing: the expansions kept are each whole.
impl std::panic::RefUnwindSafe for Recorder {}

impl ProcMacroExpander for Recorder {
    fn requires_session(&self) -> bool {
        self.inner.requires_session()
    }

    fn code(&self) -> Option<base_db::ContentId> {
        self.inner.code()
    }

    fn expand(&self, db: &Db, call: ProcMacroCall<'_>) -> Result<tt::TopSubtree, ProcMacroExpansionError> {
        let output = self.inner.expand(db, call);
        self.record(&call, &output);
        output
    }

    fn expand_in(
        &self,
        db: &Db,
        calling_crate: Crate,
        call: ProcMacroCall<'_>,
    ) -> Result<tt::TopSubtree, ProcMacroExpansionError> {
        let output = self.inner.expand_in(db, calling_crate, call);
        self.record(&call, &output);
        output
    }

    // dyn: hir-expand's `ProcMacroExpander::eq_dyn`, when a crate's macros are set.
    fn eq_dyn(&self, other: &dyn ProcMacroExpander) -> bool {
        // dyn: as above, the one way to compare across expander types.
        (other as &dyn std::any::Any)
            .downcast_ref::<Self>()
            .is_some_and(|other| self.name == other.name && self.inner.eq_dyn(&*other.inner))
    }
}

impl Recorder {
    fn record(&self, call: &ProcMacroCall<'_>, output: &Result<tt::TopSubtree, ProcMacroExpansionError>) {
        let call = Call {
            krate: self.krate.clone(),
            name: self.name.clone(),
            input: call.subtree.to_string(),
            attr: call.attrs.map(ToString::to_string),
        };
        let recorded = match output {
            Ok(output) => Ok(output.to_string()),
            Err(ProcMacroExpansionError::Panic(why) | ProcMacroExpansionError::System(why)) => Err(why.clone()),
        };
        self.expansions.lock().insert(call, recorded);
    }
}

fn name(db: &Db, krate: Crate) -> Option<String> {
    krate.extra_data(db).display_name.as_ref().map(|it| it.to_string())
}

/// The crates `program` is made of, dependencies first. Of a proc macro crate only what its
/// macros expand to is needed, so none of its dependencies are.
fn closure(db: &Db, program: Crate) -> Vec<Crate> {
    fn visit(db: &Db, krate: Crate, seen: &mut HashSet<Crate>, out: &mut Vec<Crate>) {
        if seen.insert(krate) {
            let data = krate.data(db);
            if !data.target_kind.is_proc_macro() {
                for dep in data.dependencies.iter() {
                    visit(db, dep.crate_id, seen, out);
                }
            }
            out.push(krate);
        }
    }
    let mut crates = Vec::new();
    visit(db, program, &mut HashSet::default(), &mut crates);
    crates
}

/// Packs what `program`, a crate of `db`, is made of. `expand` runs over the database with its
/// proc macros recording: every expansion the pack is to have has to happen in it. The
/// database's proc macros are the recorders' from then on.
pub fn record(db: &mut Db, program: Crate, expand: impl FnOnce(&Db, &[Crate])) -> Result<Pack, String> {
    let reading: &Db = db;
    let crates = closure(reading, program);

    let expansions = Arc::new(Mutex::new(Expansions::default()));
    let loaded = ProcMacros::get(reading).by_crate(reading).clone();
    let recording: Vec<_> = loaded
        .iter()
        .map(|(&krate, macros)| {
            let macros = macros.macros().map(|macros| {
                macros
                    .iter()
                    .map(|it| ProcMacro {
                        expander: Arc::new(Recorder {
                            krate: name(reading, krate).unwrap_or_default(),
                            name: it.name.as_str().to_owned(),
                            inner: it.expander.clone(),
                            expansions: expansions.clone(),
                        }),
                        ..it.clone()
                    })
                    .collect()
            });
            (krate, macros.ok_or(base_db::ProcMacroLoadingError::NotYetBuilt))
        })
        .collect(); // alloc: once per pack, each loaded crate's recording expanders
    ProcMacros::give(db, recording);
    let db = &*db;
    expand(db, &crates);

    let index: HashMap<Crate, usize> = crates.iter().enumerate().map(|(at, &it)| (it, at)).collect();
    let mut pack = Pack { main: program.data(db).root_file_id.index(), ..Pack::default() };
    let mut roots = HashSet::default();
    let mut modules = HashSet::default();
    for &krate in &crates {
        let mut pending = vec![hir::Crate::from(krate).root_module(db)];
        while let Some(module) = pending.pop() {
            pending.extend(module.children(db));
            modules.extend(module.as_source_file_id(db).map(|it| it.file_id(db)));
        }
    }
    for &krate in &crates {
        let data = krate.data(db);
        if pack.data_layout.is_empty()
            && let Ok(target) = &krate.workspace_data(db).target
        {
            pack.data_layout = target.data_layout.to_string();
            pack.tuple = target.tuple.as_str().to_owned();
        }
        let root = db.file_source_root(data.root_file_id).source_root_id(db);
        if data.target_kind.is_proc_macro() {
            pack.files.push(File {
                id: data.root_file_id.index(),
                path: format!("/proc-macro/{}.rs", pack.crates.len()),
                // What it declares its macros with, helper attributes and all.
                text: db.file_text(data.root_file_id).text(db).to_string(),
            });
            pack.roots.push(Root { library: true, files: vec![data.root_file_id.index()] });
        } else if roots.insert(root) {
            let root = db.source_root(root).source_root(db);
            let mut files = Vec::new();
            for file in root.iter() {
                let Some(path) = root.path_for_file(&file) else { continue };
                // Rust that is no module of a crate is a test, an example, or another target's.
                let rust = path.name_and_extension().is_some_and(|(_, ext)| ext == Some("rs"));
                if rust && !modules.contains(&file) {
                    continue;
                }
                files.push(file.index());
                let text = db.file_text(file).text(db).to_string();
                pack.files.push(File { id: file.index(), path: path.to_string(), text });
            }
            pack.roots.push(Root { library: root.is_library, files });
        }
        let macros = loaded.get(&krate).and_then(|it| it.macros()).map(|macros| {
            macros.iter().map(|it| Macro { name: it.name.as_str().to_owned(), kind: Kind::of(it.kind) }).collect()
        });
        let mut deps = Vec::new();
        if !data.target_kind.is_proc_macro() {
            for dep in data.dependencies.iter() {
                let krate = *index.get(&dep.crate_id).ok_or("a dependency outside the program's crates")?;
                deps.push(Dep {
                    krate,
                    name: dep.name.to_string(),
                    prelude: dep.is_prelude(),
                    sysroot: dep.is_sysroot(),
                });
            }
        }
        pack.crates.push(Krate {
            root: data.root_file_id.index(),
            edition: data.edition.to_string(),
            name: name(db, krate),
            version: krate.extra_data(db).version.clone(),
            cfg: krate
                .cfg_options(db)
                .into_iter()
                // Every `CfgOptions` holds `true`; it is not a cfg the crate was given.
                .filter(|atom| !matches!(atom, CfgAtom::Flag(it) if *it == sym::true_))
                .map(|atom| match atom {
                    CfgAtom::Flag(name) => name.as_str().to_owned(),
                    CfgAtom::KeyValue { key, value } => format!("{key}={value}"),
                })
                .collect(),
            env: krate.env(db).iter().map(|(key, value)| (key.to_owned(), value.to_owned())).collect(),
            origin: match &data.origin {
                CrateOrigin::Local { .. } => Origin::Local,
                CrateOrigin::Library { name, .. } | CrateOrigin::Rustc { name } => {
                    Origin::Library(name.as_str().to_owned())
                }
                CrateOrigin::Lang(lang) => Origin::Lang(lang.to_string()),
            },
            attrs: data.crate_attrs.iter().map(|it| it.to_string()).collect(),
            deps,
            macros: data.target_kind.is_proc_macro().then(|| macros.unwrap_or_default()),
        });
    }
    let expansions = std::mem::take(&mut *expansions.lock());
    pack.expansions = expansions.into_iter().map(|(call, output)| Expansion { call, output }).collect();
    pack.expansions.sort_by(|a, b| {
        (&a.call.krate, &a.call.name, &a.call.input, &a.call.attr).cmp(&(
            &b.call.krate,
            &b.call.name,
            &b.call.input,
            &b.call.attr,
        ))
    });
    Ok(pack)
}
