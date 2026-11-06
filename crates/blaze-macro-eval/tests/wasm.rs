//! The wasm host against the evaluator: every call of serde's derives and thiserror's in the
//! fixture workspace expanded by each proc-macro crate's `wasm32-wasip1` module
//! (`blaze_macro_eval::load_wasm`) and on the evaluator (`load`), the two outputs the same in
//! every token, delimiter, spacing, literal and span (`corpus/difference.rs`). Both engines run
//! one crate: the evaluator the macro crate as every build plans it, for `wasm32-wasip1` with
//! wasm32's cfg, which is the crate the module is built from.
//!
//! What it needs is `BLAZE_MACRO_CORPUS` (`corpus/config.rs`) with `wasm`: each crate's module
//! as blaze built it for `wasm32-wasip1` with the bridge's glue (`guest/entry.rs`).

#[allow(dead_code, reason = "shared with tests/corpus.rs, which uses other parts of it")]
#[path = "corpus/config.rs"]
mod config;
#[path = "corpus/difference.rs"]
mod difference;
#[allow(dead_code, reason = "shared with tests/corpus.rs, which uses other parts of it")]
#[path = "corpus/load.rs"]
mod load;

use std::{any::Any, collections::BTreeMap, fmt, sync::Arc};

use base_db::{ContentId, Crate, Db};
use hir_def::nameres::crate_def_map;
use hir_expand::proc_macro::{
    Expanded, ProcMacro, ProcMacroCall, ProcMacroExpander, ProcMacroExpansionError,
};
use blaze_core::lock::Lock;
use tt::TopSubtree;

use config::Corpus;

/// The crates each call of which is expanded both ways, and their macros.
const COMPARED: &[(&str, &[&str])] =
    &[("serde_derive", &["Serialize", "Deserialize"]), ("thiserror_impl", &["Error"])];

/// One call both ways: which macro, and the first difference of the module's output from the
/// evaluator's, if any.
struct Compared {
    krate: Box<str>,
    name: Box<str>,
    difference: Option<String>,
}

type Comparisons = Arc<Lock<Vec<Compared>>>;

/// A macro that expands each call on the evaluator and in its module, noting how the two
/// compare, and gives the evaluator's output.
struct Twin {
    krate: Box<str>,
    name: Box<str>,
    evaluated: Arc<dyn ProcMacroExpander>,
    module: Arc<dyn ProcMacroExpander>,
    compared: Comparisons,
}

impl std::panic::RefUnwindSafe for Twin {}

fn outcome(result: &Result<Expanded, ProcMacroExpansionError>) -> Result<&TopSubtree, String> {
    match result {
        Ok(expanded) => Ok(&expanded.tree),
        Err(error) => Err(format!("{error:?}")),
    }
}

impl Twin {
    /// The evaluator's expansion, its reads and state as it stated them, compared with the
    /// module's.
    fn run(
        &self,
        db: &Db,
        calling: Crate,
        call: ProcMacroCall<'_>,
    ) -> Result<Expanded, ProcMacroExpansionError> {
        let evaluated = self.evaluated.expand_in(db, calling, call);
        let module = self.module.expand_in(db, calling, call);
        let difference = match (outcome(&evaluated), outcome(&module)) {
            (Ok(ours), Ok(theirs)) => difference::difference(theirs, ours),
            (ours, theirs) => (ours.as_ref().err() != theirs.as_ref().err())
                .then(|| format!("the evaluator gave {ours:?}, the module {theirs:?}")),
        };
        self.compared.lock().push(Compared {
            krate: self.krate.clone(),
            name: self.name.clone(),
            difference,
        });
        evaluated
    }
}

impl fmt::Debug for Twin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Twin")
            .field("krate", &self.krate)
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl ProcMacroExpander for Twin {
    fn requires_session(&self) -> bool {
        self.evaluated.requires_session()
    }

    fn code(&self) -> Option<ContentId> {
        self.evaluated.code()
    }

    fn expand(
        &self,
        _db: &Db,
        _call: ProcMacroCall<'_>,
    ) -> Result<Expanded, ProcMacroExpansionError> {
        Err(ProcMacroExpansionError::System(
            "the corpus calls every macro from a calling crate".to_owned(),
        ))
    }

    fn expand_in(
        &self,
        db: &Db,
        calling: Crate,
        call: ProcMacroCall<'_>,
    ) -> Result<Expanded, ProcMacroExpansionError> {
        self.run(db, calling, call)
    }

    fn eq_dyn(&self, other: &dyn ProcMacroExpander) -> bool {
        (other as &dyn Any).downcast_ref::<Twin>().is_some_and(|other| {
            Arc::ptr_eq(&self.compared, &other.compared)
                && self.krate == other.krate
                && self.name == other.name
        })
    }
}

/// What every crate's macros load with: the corpus's module paths, the daemon's state of both
/// engines, and where the twins' comparisons go.
struct Loading<'a> {
    modules: &'a BTreeMap<String, String>,
    macros: &'a Arc<blaze_macro_eval::Macros>,
    loaded: &'a blaze_macro_eval::Modules,
    compared: &'a Comparisons,
}

/// The macros of `krate`, named `name`: each a [`Twin`] when the crate is [`COMPARED`].
fn twins(
    db: &Db,
    krate: Crate,
    toolchain: ContentId,
    name: &str,
    loading: &Loading<'_>,
) -> Result<Vec<ProcMacro>, String> {
    let Loading { modules, macros, loaded, compared } = *loading;
    let evaluated = blaze_macro_eval::load(db, krate, toolchain, macros)
        .map_err(|error| format!("load of `{name}`: {error}"))?;
    if !COMPARED.iter().any(|(it, _)| *it == name) {
        return Ok(evaluated);
    }
    let path = modules
        .get(name)
        .ok_or_else(|| format!("the corpus config's `wasm` has no module of `{name}`"))?;
    let bytes = std::fs::read(path).map_err(|error| format!("{path}: {error}"))?;
    let modules = blaze_macro_eval::load_wasm(db, krate, toolchain, macros, loaded, &bytes)
        .map_err(|error| format!("load_wasm of `{name}`: {error}"))?;
    let mut twins = Vec::new();
    for eval in evaluated {
        let module = modules
            .iter()
            .find(|it| it.name == eval.name && it.kind == eval.kind)
            .ok_or_else(|| format!("`{name}`'s module has no macro `{}`", eval.name.as_str()))?;
        let twin = Twin {
            krate: name.into(),
            name: eval.name.as_str().into(),
            evaluated: eval.expander.clone(),
            module: module.expander.clone(),
            compared: compared.clone(),
        };
        twins.push(ProcMacro {
            name: eval.name,
            kind: eval.kind,
            expander: Arc::new(twin),
            disabled: eval.disabled,
        });
    }
    Ok(twins)
}

#[test]
#[ignore = "needs BLAZE_MACRO_CORPUS with `wasm`: each compared crate built by blaze for wasm32-wasip1"]
fn a_modules_expansion_is_the_evaluators_in_every_token() {
    let config = Corpus::read().unwrap_or_else(|why| panic!("{why}"));
    let modules =
        config.wasm.clone().unwrap_or_else(|| panic!("{}: no `wasm` modules", config::VAR));
    let compared: Comparisons = Arc::new(Lock::new("macro.test.wasm.compared", Vec::new()));
    let loaded = blaze_macro_eval::Modules::new().unwrap_or_else(|why| panic!("{why}"));
    // Each call runs on both engines to compare them, so none is kept or found.
    let macros = blaze_macro_eval::Macros::nothing("each call is compared on both engines");
    let loading =
        Loading { modules: &modules, macros: &macros, loaded: &loaded, compared: &compared };
    let workspace = load::lower_with(
        &config.workspace,
        &config.cargo_home,
        config.homes(),
        |db, krate, toolchain, name| twins(db, krate, toolchain, name, &loading),
    )
    .unwrap_or_else(|why| panic!("{why}"));
    let calls = workspace.krate("corpus-calls").unwrap_or_else(|why| panic!("{why}"));
    crate_def_map(workspace.lowered.db(), calls);

    let compared = std::mem::take(&mut *compared.lock());
    for (krate, names) in COMPARED {
        for name in *names {
            let calls =
                compared.iter().filter(|it| &*it.krate == *krate && &*it.name == *name).count();
            assert!(calls > 0, "no call of `{krate}::{name}` was expanded both ways");
        }
    }
    let differences: Vec<String> = compared
        .iter()
        .filter_map(|it| {
            it.difference.as_ref().map(|why| format!("`{}::{}`: {why}", it.krate, it.name))
        })
        .collect();
    assert!(
        differences.is_empty(),
        "{} of {} calls differ:\n{}",
        differences.len(),
        compared.len(),
        differences.join("\n")
    );
}
