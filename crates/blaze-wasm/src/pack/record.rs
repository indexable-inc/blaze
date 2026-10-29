//! Making a [`Pack`] of a cargo workspace, with the toolchain there to load it.

use std::{
    path::Path,
    sync::{Arc, Mutex},
};

use cfg::CfgAtom;
use cfg::CfgDiff;
use hir_expand::proc_macro::{
    CrateProcMacros, ProcMacro, ProcMacroExpander, ProcMacroExpansionError, ProcMacroKind,
    ProcMacros,
};
use ide_db::{
    RootDatabase,
    base_db::{Crate, CrateOrigin, Env, SourceDatabase, salsa::Setter as _},
};
use intern::Symbol;
use load_cargo::{LoadCargoConfig, ProcMacroServerChoice, load_workspace};
use project_model::{CargoConfig, CfgOverrides, ProjectManifest, ProjectWorkspace, RustLibSource};
use rustc_hash::{FxHashMap, FxHashSet};
use span::Span;
use vfs::AbsPathBuf;

use super::{Call, Dep, Expansion, Expansions, File, Kind, Krate, Macro, Origin, Pack, Root};

/// A proc macro that keeps what it was called with and what it gave.
#[derive(Debug)]
struct Recorder {
    krate: String,
    name: String,
    inner: Arc<dyn ProcMacroExpander>,
    expansions: Arc<Mutex<Expansions>>,
}

impl ProcMacroExpander for Recorder {
    fn inherits_process_env(&self) -> bool {
        self.inner.inherits_process_env()
    }

    fn inherited_env(&self) -> Option<&[(std::ffi::OsString, std::ffi::OsString)]> {
        self.inner.inherited_env()
    }

    fn requires_session(&self) -> bool {
        self.inner.requires_session()
    }

    fn expand(
        &self,
        db: &dyn SourceDatabase,
        subtree: &tt::TopSubtree,
        attrs: Option<&tt::TopSubtree>,
        env: &Env,
        def_site: Span,
        call_site: Span,
        mixed_site: Span,
        current_dir: String,
    ) -> Result<tt::TopSubtree, ProcMacroExpansionError> {
        let output = self.inner.expand(
            db,
            subtree,
            attrs,
            env,
            def_site,
            call_site,
            mixed_site,
            current_dir,
        );
        self.record(subtree, attrs, &output);
        output
    }

    fn expand_in(
        &self,
        db: &dyn SourceDatabase,
        calling_crate: ide_db::base_db::Crate,
        subtree: &tt::TopSubtree,
        attrs: Option<&tt::TopSubtree>,
        env: &Env,
        def_site: Span,
        call_site: Span,
        mixed_site: Span,
        current_dir: String,
    ) -> Result<tt::TopSubtree, ProcMacroExpansionError> {
        let output = self.inner.expand_in(
            db,
            calling_crate,
            subtree,
            attrs,
            env,
            def_site,
            call_site,
            mixed_site,
            current_dir,
        );
        self.record(subtree, attrs, &output);
        output
    }

    fn eq_dyn(&self, other: &dyn ProcMacroExpander) -> bool {
        (other as &dyn std::any::Any)
            .downcast_ref::<Self>()
            .is_some_and(|other| self.name == other.name && self.inner.eq_dyn(&*other.inner))
    }
}

impl Recorder {
    fn record(
        &self,
        subtree: &tt::TopSubtree,
        attrs: Option<&tt::TopSubtree>,
        output: &Result<tt::TopSubtree, ProcMacroExpansionError>,
    ) {
        let call = Call {
            krate: self.krate.clone(),
            name: self.name.clone(),
            input: subtree.to_string(),
            attr: attrs.map(ToString::to_string),
        };
        let recorded = match output {
            Ok(output) => Ok(output.to_string()),
            Err(ProcMacroExpansionError::Panic(why) | ProcMacroExpansionError::System(why)) => {
                Err(why.clone())
            }
        };
        self.expansions.lock().unwrap_or_else(|it| it.into_inner()).insert(call, recorded);
    }
}

fn name(db: &RootDatabase, krate: Crate) -> Option<String> {
    krate.extra_data(db).display_name.as_ref().map(|it| it.to_string())
}

/// Loads the cargo workspace at `project` for `target`, and packs what `main`, the name of one
/// of its crates, is made of. `expand` runs over the loaded workspace with the proc macros
/// recording: every expansion the pack is to have has to happen in it.
pub fn record(
    project: &Path,
    target: &str,
    main: &str,
    expand: impl FnOnce(&RootDatabase, &[Crate]),
) -> Result<Pack, Box<dyn std::error::Error>> {
    // The program links against what cargo built, so its crates are configured as cargo's.
    project_model::SYSROOT_AS_BUILT.store(true, std::sync::atomic::Ordering::Relaxed);
    let root = AbsPathBuf::assert_utf8(std::env::current_dir()?.join(project));
    let manifest = ProjectManifest::discover_single(&root)?;
    let cargo_config = CargoConfig {
        sysroot: Some(RustLibSource::Discover),
        target: Some(target.to_owned()),
        // The page links against a release build.
        cfg_overrides: CfgOverrides {
            global: CfgDiff::new(
                Vec::new(),
                vec![CfgAtom::Flag(Symbol::intern("debug_assertions"))],
            ),
            ..Default::default()
        },
        ..Default::default()
    };
    let load_config = LoadCargoConfig {
        load_out_dirs_from_check: true,
        with_proc_macro_server: ProcMacroServerChoice::Sysroot,
        prefill_caches: false,
        num_worker_threads: 1,
        proc_macro_processes: 1,
    };
    let mut workspace = ProjectWorkspace::load(manifest, &cargo_config, &|_| {})?;
    let scripts = workspace.run_build_scripts(&cargo_config, &|_| {})?;
    workspace.set_build_scripts(scripts);
    let (mut db, vfs, _server) = load_workspace(workspace, &cargo_config.extra_env, &load_config)?;

    // The crates of the program, dependencies first.
    let all = ide_db::base_db::all_crates(&db);
    let program = *all
        .iter()
        .find(|&&it| it.data(&db).origin.is_local() && name(&db, it).as_deref() == Some(main))
        .ok_or_else(|| format!("no crate `{main}` in the workspace"))?;
    let mut crates = Vec::new();
    let mut seen = FxHashSet::default();
    fn visit(db: &RootDatabase, krate: Crate, seen: &mut FxHashSet<Crate>, out: &mut Vec<Crate>) {
        if seen.insert(krate) {
            // Of a proc macro crate only what its macros expand to is needed.
            let deps = &krate.data(db).dependencies;
            for dep in deps.iter().filter(|_| !krate.data(db).target_kind.is_proc_macro()) {
                visit(db, dep.crate_id, seen, out);
            }
            out.push(krate);
        }
    }
    visit(&db, program, &mut seen, &mut crates);

    let expansions = Arc::new(Mutex::new(Expansions::default()));
    let loaded = ProcMacros::get(&db).by_crate(&db).clone();
    let recording: FxHashMap<_, _> = loaded
        .iter()
        .map(|(&krate, macros)| {
            let macros = macros.macros().map(|macros| {
                macros
                    .iter()
                    .map(|it| ProcMacro {
                        expander: Arc::new(Recorder {
                            krate: name(&db, krate).unwrap_or_default(),
                            name: it.name.as_str().to_owned(),
                            inner: it.expander.clone(),
                            expansions: expansions.clone(),
                        }),
                        ..it.clone()
                    })
                    .collect()
            });
            (
                krate,
                triomphe::Arc::new(CrateProcMacros::new(
                    macros.ok_or_else(|| ide_db::base_db::ProcMacroLoadingError::NotYetBuilt),
                )),
            )
        })
        .collect();
    let macros = ProcMacros::get(&db);
    macros.set_by_crate(&mut db).to(recording);
    expand(&db, &crates);

    let index: FxHashMap<Crate, usize> =
        crates.iter().enumerate().map(|(at, &it)| (it, at)).collect();
    let mut pack = Pack { main: program.data(&db).root_file_id.index(), ..Pack::default() };
    let mut roots = FxHashSet::default();
    let mut modules = FxHashSet::default();
    for &krate in &crates {
        let mut pending = vec![hir::Crate::from(krate).root_module(&db)];
        while let Some(module) = pending.pop() {
            pending.extend(module.children(&db));
            modules.extend(module.as_source_file_id(&db).map(|it| it.file_id(&db)));
        }
    }
    for &krate in &crates {
        let data = krate.data(&db);
        if pack.data_layout.is_empty()
            && let Ok(target) = &krate.workspace_data(&db).target
        {
            pack.data_layout = target.data_layout.to_string();
        }
        let root = db.file_source_root(data.root_file_id).source_root_id(&db);
        if data.target_kind.is_proc_macro() {
            pack.files.push(File {
                id: data.root_file_id.index(),
                path: format!("/proc-macro/{}.rs", pack.crates.len()),
                // What it declares its macros with, helper attributes and all.
                text: db.file_text(data.root_file_id).text(&db).to_string(),
            });
            pack.roots.push(Root { library: true, files: vec![data.root_file_id.index()] });
        } else if roots.insert(root) {
            let root = db.source_root(root).source_root(&db);
            let mut files = Vec::new();
            for file in root.iter() {
                let Some(path) = root.path_for_file(&file) else { continue };
                // Rust that is no module of a crate is a test, an example, or another target's.
                let rust = path.name_and_extension().is_some_and(|(_, ext)| ext == Some("rs"));
                if rust && !modules.contains(&file) {
                    continue;
                }
                files.push(file.index());
                let text = db.file_text(file).text(&db).to_string();
                pack.files.push(File { id: file.index(), path: path.to_string(), text });
            }
            pack.roots.push(Root { library: root.is_library, files });
        }
        let macros = loaded.get(&krate).and_then(|it| it.macros()).map(|macros| {
            let kind = |kind| match kind {
                ProcMacroKind::CustomDerive => Kind::Derive,
                ProcMacroKind::Bang => Kind::Bang,
                ProcMacroKind::Attr => Kind::Attr,
            };
            macros
                .iter()
                .map(|it| Macro { name: it.name.as_str().to_owned(), kind: kind(it.kind) })
                .collect()
        });
        pack.crates.push(Krate {
            root: data.root_file_id.index(),
            edition: data.edition.to_string(),
            name: name(&db, krate),
            version: krate.extra_data(&db).version.clone(),
            cfg: krate
                .cfg_options(&db)
                .into_iter()
                .map(|atom| match atom {
                    CfgAtom::Flag(name) => name.as_str().to_owned(),
                    CfgAtom::KeyValue { key, value } => format!("{key}={value}"),
                })
                .collect(),
            env: krate
                .env(&db)
                .iter()
                .map(|(key, value)| (key.to_owned(), value.to_owned()))
                .collect(),
            origin: match &data.origin {
                CrateOrigin::Local { .. } => Origin::Local,
                CrateOrigin::Library { name, .. } | CrateOrigin::Rustc { name } => {
                    Origin::Library(name.as_str().to_owned())
                }
                CrateOrigin::Lang(lang) => Origin::Lang(lang.to_string()),
            },
            attrs: data.crate_attrs.iter().map(|it| it.to_string()).collect(),
            deps: data
                .dependencies
                .iter()
                .filter(|_| !data.target_kind.is_proc_macro())
                .map(|dep| Dep {
                    krate: index[&dep.crate_id],
                    name: dep.name.to_string(),
                    prelude: dep.is_prelude(),
                    sysroot: dep.is_sysroot(),
                })
                .collect(),
            macros: data.target_kind.is_proc_macro().then(|| macros.unwrap_or_default()),
        });
    }
    drop(vfs);
    let expansions = std::mem::take(&mut *expansions.lock().unwrap_or_else(|it| it.into_inner()));
    pack.expansions =
        expansions.into_iter().map(|(call, output)| Expansion { call, output }).collect();
    pack.expansions.sort_by(|a, b| {
        (&a.call.krate, &a.call.name, &a.call.input).cmp(&(
            &b.call.krate,
            &b.call.name,
            &b.call.input,
        ))
    });
    Ok(pack)
}
