//! `blaze doc` ([`crate::serve::work::Front::doc`]): the documentation `cargo doc` writes, made
//! by `blaze-doc` from the job's front end once its check is built (its build scripts run and
//! its proc macros loaded by the daemon in the sandbox, as any check's are). No rustdoc runs.
//! The files go back to the client, which writes them under [`Documented::dir`].

use blaze_api::{ApiError, Diagnostic, Documented, DocumentedCrate, Level, Response};
use blaze_doc::{Asked, TestOnly, html::Pages, model::Warning};
use blaze_hash::HashMap;
use blaze_lower::Lowered;
use blaze_units::{
    query::{PackagePlan, Units},
    schema::Mode,
};

use super::Job;

/// The documentation of the check's root units, and unless `no_deps` of every library they
/// depend on: what is documented and where, each file, then the warnings rustdoc would give of
/// the crates asked for.
pub(super) fn doc(job: &Job, lowered: &mut Lowered, no_deps: bool, private: bool) -> Result<Vec<Response>, ApiError> {
    let units = &job.input.units;
    let asked = asked(units, lowered, private)?;
    let packages = packages(units, lowered);
    // The check's units are of the dependencies it has for its libraries and binaries: none of
    // a member's dev-dependencies is in the crate graph to leave out.
    let test_only = TestOnly::default();
    let db = lowered.served_db();
    let crates = hir_ty::attach_db(db, || blaze_doc::crates_to_document(db, &asked, no_deps, &test_only));
    if crates.is_empty() {
        return Err(ApiError::new("nothing to document", None));
    }
    let platform = units.graph.roots.first().and_then(|&root| units.graph.units[root].platform.as_deref());
    let dir = match platform {
        Some(triple) => format!("{}/{triple}/doc", units.target_dir),
        None => format!("{}/doc", units.target_dir),
    };
    let documented: Vec<DocumentedCrate> = crates
        .iter()
        .map(|it| {
            let krate = base_db::Crate::from(it.krate);
            let package = packages.get(&krate).map_or_else(|| it.name.clone(), |plan| described(plan));
            let asked = asked.iter().any(|one| one.krate == it.krate);
            DocumentedCrate { name: it.name.clone(), package, asked }
        })
        .collect();
    blaze_doc::set_cfg_doc(db, &crates);
    let db = &*db;
    let model = hir_ty::attach_db(db, || blaze_doc::model(db, crates, test_only));
    let Pages { files, written } = blaze_doc::html::pages(&model);
    let mut out = Vec::with_capacity(files.len() + model.warnings.len() + 2); // alloc: once per documentation
    out.push(Response::Documented(Documented {
        dir,
        crates: documented,
        pages: written.pages as u64,
        source_pages: written.source_pages as u64,
    }));
    out.extend(files.into_iter().map(|page| Response::DocPage { path: page.path, page: page.bytes }));
    out.extend(warnings(&model.warnings, &job.input.config.invocation.cwd).into_iter().map(Response::Diagnostic));
    Ok(out)
}

/// The crates of the check's root units, as `cargo doc` documents them: a library with its
/// items public unless `private`, any other target with its private items too, as cargo passes
/// rustdoc `--document-private-items` for a binary.
fn asked(units: &Units, lowered: &Lowered, private: bool) -> Result<Vec<Asked>, ApiError> {
    let mut asked = Vec::new(); // alloc: once per documentation
    for &root in &units.graph.roots {
        let unit = &units.graph.units[root];
        if unit.mode != Mode::Check || unit.checked_as_test {
            continue;
        }
        let Some(krate) = lowered.krate(root) else {
            // alloc: an error, once
            let what = format!("the crate of `{}` was not lowered", unit.target.name);
            return Err(ApiError::new(what, Some(unit.target.src_path.display().to_string())));
        };
        let lib = unit.target.kind.iter().any(|it| {
            matches!(it.as_str(), "lib" | "rlib" | "dylib" | "cdylib" | "staticlib" | "proc-macro")
        });
        asked.push(Asked { krate: hir::Crate::from(krate), private: private || !lib });
    }
    if asked.is_empty() {
        return Err(ApiError::new("no library or binary to document in the packages asked for", None));
    }
    Ok(asked)
}

/// Each lowered crate's package.
fn packages<'a>(units: &'a Units, lowered: &Lowered) -> HashMap<base_db::Crate, &'a PackagePlan> {
    let by_id: HashMap<&str, &PackagePlan> = units.packages.iter().map(|it| (it.id.as_str(), it)).collect();
    let mut out = HashMap::default();
    for (at, unit) in units.graph.units.iter().enumerate() {
        if let (Some(krate), Some(plan)) = (lowered.krate(at), by_id.get(unit.pkg_id.as_str())) {
            out.entry(krate).or_insert(*plan);
        }
    }
    out
}

/// `serde v1.0.219`, then `(/abs/path)` for a path package, as cargo's `Documenting` names it.
fn described(plan: &PackagePlan) -> String {
    match &plan.source {
        Some(source) => format!("{} v{} {source}", plan.name, plan.version),
        None => format!("{} v{}", plan.name, plan.version),
    }
}

/// rustdoc's warnings as cargo prints them, each crate's followed by the count cargo says after
/// them: `` `name` (lib doc) generated 2 warnings ``. Paths are from `cwd` where under it.
fn warnings(warnings: &[Warning], cwd: &str) -> Vec<Diagnostic> {
    let mut out = Vec::new(); // alloc: once per documentation
    let mut counts: Vec<(&str, usize)> = Vec::new(); // alloc: once per documentation
    for warning in warnings {
        let mut rendered = format!("warning: {}\n", warning.message);
        if let Some(at) = &warning.at {
            let at = at.strip_prefix(cwd).and_then(|it| it.strip_prefix('/')).unwrap_or(at);
            rendered.push_str(&format!("  --> {at}\n"));
        }
        rendered.push('\n');
        out.push(warned(warning.message.clone(), rendered));
        match counts.iter_mut().find(|(name, _)| *name == warning.krate) {
            Some((_, count)) => *count += 1,
            None => counts.push((&warning.krate, 1)),
        }
    }
    for (name, count) in counts {
        let plural = if count == 1 { "warning" } else { "warnings" };
        let message = format!("`{name}` (lib doc) generated {count} {plural}");
        let rendered = format!("warning: {message}\n");
        out.push(warned(message, rendered));
    }
    out
}

fn warned(message: String, rendered: String) -> Diagnostic {
    Diagnostic {
        code: None,
        unit: None,
        facts: Vec::new(),
        level: Level::Warning,
        spans: Vec::new(),
        message,
        children: Vec::new(),
        rendered,
        explanation: None,
    }
}
