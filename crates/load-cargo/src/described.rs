//! A proc-macro crate of a load and what it depends on, as a `rust-project.json`: what a
//! caller builds it from itself (blaze, as a WebAssembly module), whoever compiled its library.

use cfg::CfgAtom;
use ide_db::{
    FxHashMap, FxHashSet,
    base_db::{CrateBuilderId, CrateGraphBuilder, CrateOrigin, LangCrateOrigin},
};
use serde_json::{Value, json};
use vfs::{AbsPath, AbsPathBuf};

/// The description of `krate` and every crate it depends on but the sysroot's, `krate` first,
/// over the sysroot at `sysroot`. A proc macro it depends on is named by its library in
/// `libraries`. `None` when a crate's root is not a file of the machine.
pub(crate) fn description(
    graph: &CrateGraphBuilder,
    vfs: &vfs::Vfs,
    krate: CrateBuilderId,
    libraries: &FxHashMap<CrateBuilderId, AbsPathBuf>,
    sysroot: Option<&AbsPath>,
) -> Option<String> {
    let lang = |it: CrateBuilderId| matches!(graph[it].basic.origin, CrateOrigin::Lang(_));
    // What rustc sets for the target is in every crate's cfg; the description has the rest.
    let target: FxHashSet<CfgAtom> = graph
        .iter()
        .find(|it| matches!(graph[*it].basic.origin, CrateOrigin::Lang(LangCrateOrigin::Core)))
        .map(|it| graph[it].cfg_options.clone().into_iter().collect())
        .unwrap_or_default();
    let mut order = vec![krate];
    let mut at: FxHashMap<CrateBuilderId, usize> = FxHashMap::default();
    at.insert(krate, 0);
    let mut next = 0;
    while let Some(&it) = order.get(next) {
        next += 1;
        for dep in &graph[it].basic.dependencies {
            if !lang(dep.crate_id) && !at.contains_key(&dep.crate_id) {
                at.insert(dep.crate_id, order.len());
                order.push(dep.crate_id);
            }
        }
    }
    let mut crates = Vec::new();
    for (index, &it) in order.iter().enumerate() {
        let data = &graph[it];
        let root = vfs.file_path(data.basic.root_file_id);
        let root = root.as_path()?;
        let mut cfg: Vec<String> = data
            .cfg_options
            .clone()
            .into_iter()
            .filter(|atom| !target.contains(atom))
            .map(|atom| match atom {
                CfgAtom::Flag(name) => name.as_str().to_owned(),
                CfgAtom::KeyValue { key, value } => {
                    format!("{}=\"{}\"", key.as_str(), value.as_str())
                }
            })
            .collect();
        cfg.sort();
        let mut env: Vec<(&str, &str)> = data.env.iter().collect();
        env.sort();
        let deps: Vec<Value> = data
            .basic
            .dependencies
            .iter()
            .filter_map(|dep| {
                Some(json!({ "crate": at.get(&dep.crate_id)?, "name": dep.name.to_string() }))
            })
            .collect();
        let name = data.extra.display_name.as_ref().map(|it| it.crate_name().to_string());
        let proc_macro = data.basic.target_kind.is_proc_macro();
        crates.push(json!({
            "display_name": name,
            "root_module": root.to_string(),
            "edition": data.basic.edition.to_string(),
            "deps": deps,
            "cfg": cfg,
            "env": env.into_iter().map(|(k, v)| (k.to_owned(), json!(v))).collect::<serde_json::Map<_, _>>(),
            "is_workspace_member": index == 0,
            "is_proc_macro": proc_macro,
            "proc_macro_dylib_path": libraries.get(&it).filter(|_| index > 0 && proc_macro).map(|it| it.to_string()),
        }));
    }
    let mut out = json!({ "crates": crates });
    if let Some(sysroot) = sysroot {
        out["sysroot"] = json!(sysroot.to_string());
        // The library the graph was loaded with: `library/` above `core/src/lib.rs`, which is
        // blaze's own and not the toolchain's.
        let core = graph.iter().find(|it| {
            matches!(graph[*it].basic.origin, CrateOrigin::Lang(LangCrateOrigin::Core))
        });
        let library = core.and_then(|it| {
            let root = vfs.file_path(graph[it].basic.root_file_id);
            Some(root.as_path()?.parent()?.parent()?.parent()?.to_owned())
        });
        let library = library.unwrap_or_else(|| sysroot.join("lib/rustlib/src/rust/library"));
        out["sysroot_src"] = json!(library.to_string());
    }
    Some(out.to_string())
}
