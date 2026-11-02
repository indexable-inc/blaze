//! The ways out of the sandbox a workspace's manifests name (docs/cargo.md, "Build scripts and
//! proc macros run in a sandbox"), read from the manifests the load already parsed: the
//! workspace's root manifest names packages by name (the only place one from a registry can be
//! named), and a member may take its own build script out and say how its tests run. Any other
//! key, or a value of another type, is an error. The workspace's `experimental` list is read
//! here too, for the `sandboxed-tests` and `const-macros` experiments, and a package's
//! `const-macros` file with it.

use std::path::Path;

use crate::query::{LoadError, TestSandbox};

/// The table of the workspace's root manifest.
pub(crate) const WORKSPACE_TABLE: &str = "[workspace.metadata.blaze.sandbox]";

/// The table of a package's own manifest.
pub(crate) const PACKAGE_TABLE: &str = "[package.metadata.blaze.sandbox]";

/// The key, in `[workspace.metadata.blaze]`, that lists the experiments a workspace opts into.
pub(crate) const EXPERIMENTAL: &str = "experimental";

/// Every test of the workspace runs in a sandbox.
pub(crate) const SANDBOXED_TESTS: &str = "sandboxed-tests";

/// A package's `[package.metadata.blaze] const-macros` file is a crate of macros the front end
/// runs (docs/future/const-macros.md); the key of that table too.
pub(crate) const CONST_MACROS: &str = "const-macros";

/// The experiments there are, as v1's `experimental::KNOWN`.
const KNOWN: &[&str] = &[SANDBOXED_TESTS, CONST_MACROS];

/// What the workspace's root manifest says in [`WORKSPACE_TABLE`] and [`EXPERIMENTAL`] that the
/// load hands on.
#[derive(Default, Debug, PartialEq, Eq)]
pub(crate) struct WorkspaceOptOut {
    /// `unsandboxed-build-scripts`: packages by name, sorted.
    pub build_scripts: Vec<String>,
    /// `build-script-link-dirs`: absolute (a relative one is under the workspace's root),
    /// normal, sorted; or `${NAME}/rest` as written, `NAME` one `build-script-env` lists.
    pub link_dirs: Vec<String>,
    /// [`EXPERIMENTAL`] lists [`SANDBOXED_TESTS`].
    pub sandboxed_tests: bool,
}

impl WorkspaceOptOut {
    pub fn unsandboxed(&self, package: &str) -> bool {
        self.build_scripts.binary_search_by(|it| it.as_str().cmp(package)).is_ok()
    }
}

/// What a member's [`PACKAGE_TABLE`] says.
#[derive(Default, Debug, PartialEq, Eq)]
pub(crate) struct PackageOptOut {
    /// `build-script = false`.
    pub build_script_outside: bool,
    /// `tests` and `unsandboxed-tests`.
    pub tests: TestSandbox,
}

/// `metadata`'s `blaze` table: none when there is no such table.
fn blaze_table(metadata: Option<&toml::Value>) -> Option<&toml::Table> {
    metadata?.get("blaze")?.as_table()
}

/// `metadata`'s `blaze.sandbox` table: none when there is no such table.
fn sandbox_table(metadata: Option<&toml::Value>) -> Option<&toml::Table> {
    blaze_table(metadata)?.get("sandbox")?.as_table()
}

fn wrong(table: &str, manifest: &Path, what: &str) -> LoadError {
    LoadError { what: format!("{table} in {}: {what}", manifest.display()), at: Some(manifest.display().to_string()) }
}

/// What `[workspace.metadata]` (`metadata`) of the root manifest at `manifest` says in
/// [`WORKSPACE_TABLE`] and [`EXPERIMENTAL`].
pub(crate) fn workspace(metadata: Option<&toml::Value>, manifest: &Path) -> Result<WorkspaceOptOut, LoadError> {
    let mut out = WorkspaceOptOut { sandboxed_tests: sandboxed_tests(metadata, manifest)?, ..WorkspaceOptOut::default() };
    let Some(table) = sandbox_table(metadata) else { return Ok(out) };
    let wrong = |what: &str| wrong(WORKSPACE_TABLE, manifest, what);
    let root = manifest.parent().unwrap_or(Path::new("/"));
    let mut env: Vec<&str> = Vec::new(); // alloc: a manifest's few names, once a load
    let mut under_vars: Vec<(&str, &str)> = Vec::new(); // alloc: as above
    for (key, value) in table {
        let names = || -> Result<Vec<&str>, LoadError> {
            let list = value.as_array().ok_or_else(|| wrong(&format!("`{key}` is a list")))?;
            list.iter().map(|it| it.as_str().ok_or_else(|| wrong(&format!("`{key}` lists names, as strings")))).collect()
        };
        match key.as_str() {
            "unsandboxed-build-scripts" => out.build_scripts = names()?.into_iter().map(str::to_owned).collect(),
            "build-script-link-dirs" => {
                for dir in names()? {
                    // Expanded from the script's environment when it runs (`blaze_cargo::directives::link_dirs`).
                    if let Some(rest) = dir.strip_prefix("${") {
                        let name = rest.split_once('}').map(|(name, _)| name).filter(|it| !it.is_empty());
                        let name = name.ok_or_else(|| {
                            wrong(&format!("`{dir}` of `build-script-link-dirs` starts `${{` with no `NAME}}` after it"))
                        })?;
                        under_vars.push((dir, name));
                        out.link_dirs.push(dir.to_owned());
                        continue;
                    }
                    let absolute = super::files::normal(&root.join(dir))
                        .ok_or_else(|| wrong(&format!("`{dir}` of `build-script-link-dirs` is not a UTF-8 path")))?;
                    out.link_dirs.push(absolute);
                }
            }
            // Every build script is given the command's whole environment, these variables too.
            "build-script-env" => env = names()?,
            "unsandboxed-proc-macros" => {
                return Err(wrong(&format!(
                    "`unsandboxed-proc-macros` lists {:?}, but no proc macro can run outside: blaze runs \
                     every proc macro on its own evaluator, which has no way out (docs/cargo.md, \"Proc \
                     macros run on blaze's evaluator\"); remove the key",
                    names()?
                )));
            }
            "wasi-build-scripts" => {
                return Err(wrong(&format!(
                    "`wasi-build-scripts` lists {:?}, but blaze runs every build script natively in its \
                     sandbox, none under WASI; remove the key",
                    names()?
                )));
            }
            other => {
                return Err(wrong(&format!(
                    "`{other}` is not known (`unsandboxed-build-scripts`, `unsandboxed-proc-macros`, \
                     `build-script-env`, `build-script-link-dirs` and `wasi-build-scripts` are)"
                )));
            }
        }
    }
    // A dir under a variable the workspace does not declare its scripts read would hang on
    // whatever the shell happens to set.
    if let Some((dir, name)) = under_vars.iter().find(|(_, name)| !env.contains(name)) {
        return Err(wrong(&format!(
            "`{dir}` of `build-script-link-dirs` is under `{name}`, which `build-script-env` does not list"
        )));
    }
    out.build_scripts.sort_unstable();
    out.build_scripts.dedup();
    out.link_dirs.sort_unstable();
    out.link_dirs.dedup();
    Ok(out)
}

/// Whether [`EXPERIMENTAL`] in `[workspace.metadata.blaze]` of the root manifest at `manifest`
/// lists [`SANDBOXED_TESTS`]. A name not [`KNOWN`], or a value that is not a list of strings, is
/// an error worded as v1's.
fn sandboxed_tests(metadata: Option<&toml::Value>, manifest: &Path) -> Result<bool, LoadError> {
    let Some(listed) = blaze_table(metadata).and_then(|it| it.get(EXPERIMENTAL)) else { return Ok(false) };
    let wrong = |what: &str| LoadError {
        what: format!("`{EXPERIMENTAL}` in [workspace.metadata.blaze] of {}: {what}", manifest.display()),
        at: Some(manifest.display().to_string()),
    };
    // alloc: the known names, only for an error
    let known = || KNOWN.iter().map(|it| format!("`{it}`")).collect::<Vec<_>>().join(", ");
    let names = listed.as_array().ok_or_else(|| wrong(&format!("a list of experiments' names (known: {})", known())))?;
    let mut on = false;
    for name in names {
        let name = name
            .as_str()
            .ok_or_else(|| wrong(&format!("a list of experiments' names, as strings (known: {})", known())))?;
        if !KNOWN.contains(&name) {
            return Err(wrong(&format!("`{name}` is not known (known: {})", known())));
        }
        on |= name == SANDBOXED_TESTS;
    }
    Ok(on)
}

/// What `[package.metadata]` (`metadata`) of the member whose manifest is `manifest` says in
/// [`PACKAGE_TABLE`]: `build-script = false`, and how its tests run (v1's `sandboxed::opt_out`).
pub(crate) fn package(metadata: Option<&toml::Value>, manifest: &Path) -> Result<PackageOptOut, LoadError> {
    let mut out = PackageOptOut::default();
    let Some(table) = sandbox_table(metadata) else { return Ok(out) };
    let wrong = |what: &str| wrong(PACKAGE_TABLE, manifest, what);
    for (key, value) in table {
        match key.as_str() {
            "tests" => {
                let tests = value.as_bool().ok_or_else(|| wrong("`tests` is true or false"))?;
                (out.tests.all, out.tests.opted_in) = (!tests, tests);
            }
            "unsandboxed-tests" => {
                let names = value.as_array().ok_or_else(|| wrong("`unsandboxed-tests` is a list"))?;
                out.tests.tests = names
                    .iter()
                    .map(|it| {
                        it.as_str()
                            // alloc: a test's name, once per name the member lists
                            .map(str::to_owned)
                            .ok_or_else(|| wrong("`unsandboxed-tests` lists tests by their names, as strings"))
                    })
                    .collect::<Result<_, _>>()?;
            }
            "build-script" => {
                out.build_script_outside = !value.as_bool().ok_or_else(|| wrong("`build-script` is true or false"))?;
            }
            other => {
                return Err(wrong(&format!(
                    "`{other}` is not known (`tests`, `unsandboxed-tests` and `build-script` are)"
                )));
            }
        }
    }
    out.tests.tests.sort_unstable();
    out.tests.tests.dedup();
    Ok(out)
}

/// Whether [`EXPERIMENTAL`] in the root manifest's `[workspace.metadata.blaze]` (`metadata`)
/// lists [`CONST_MACROS`]; the list itself is checked by [`workspace`].
pub(crate) fn const_macros_on(metadata: Option<&toml::Value>) -> bool {
    let listed = blaze_table(metadata).and_then(|it| it.get(EXPERIMENTAL)).and_then(|it| it.as_array());
    listed.is_some_and(|names| names.iter().any(|it| it.as_str() == Some(CONST_MACROS)))
}

/// The file `[package.metadata.blaze] const-macros` (`metadata`) of the package whose manifest is
/// `manifest` names, absolute and normal; none when it names none. Named while the workspace
/// (`root_manifest`) has not turned the experiment on (`on`), or as anything but a path, it is
/// an error worded as v1's. Whether the file is there is the front end's to say: it reads the
/// build's files, and this the manifests.
pub(crate) fn const_macros(
    metadata: Option<&toml::Value>,
    manifest: &Path,
    root_manifest: &Path,
    on: bool,
) -> Result<Option<String>, LoadError> {
    let Some(value) = blaze_table(metadata).and_then(|it| it.get(CONST_MACROS)) else { return Ok(None) };
    let wrong = |what: String| LoadError { what, at: Some(manifest.display().to_string()) };
    let Some(path) = value.as_str() else {
        return Err(wrong(format!(
            "`{CONST_MACROS}` under [package.metadata.blaze] in {} is a path, as a string",
            manifest.display()
        )));
    };
    if !on {
        return Err(wrong(format!(
            "`{CONST_MACROS}` under [package.metadata.blaze] in {} is an experiment: \
             `experimental = [\"{CONST_MACROS}\"]` under [workspace.metadata.blaze] in {} turns it on",
            manifest.display(),
            root_manifest.display()
        )));
    }
    let dir = manifest.parent().unwrap_or(Path::new("/"));
    let file = super::files::normal(&dir.join(path)).ok_or_else(|| {
        wrong(format!(
            "`{CONST_MACROS}` under [package.metadata.blaze] in {} names `{path}`, which is not a UTF-8 path",
            manifest.display()
        ))
    })?;
    Ok(Some(file))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata(text: &str) -> toml::Value {
        toml::Value::Table(toml::from_str(text).unwrap())
    }

    #[test]
    fn the_workspace_names_packages_and_link_dirs() {
        let manifest = Path::new("/w/Cargo.toml");
        let value = metadata(
            "[blaze.sandbox]\nunsandboxed-build-scripts = [\"z-sys\", \"a-sys\"]\n\
             build-script-link-dirs = [\"/opt/lib\", \"vendor/../lib\"]\nbuild-script-env = [\"OPENSSL_DIR\"]\n",
        );
        let out = workspace(Some(&value), manifest).unwrap();
        assert_eq!(out.build_scripts, ["a-sys", "z-sys"]);
        assert_eq!(out.link_dirs, ["/opt/lib", "/w/lib"]);
        assert!(out.unsandboxed("z-sys") && !out.unsandboxed("z"));
        assert!(!out.sandboxed_tests);
        assert_eq!(workspace(None, manifest).unwrap(), WorkspaceOptOut::default());
        assert_eq!(workspace(Some(&metadata("[other]\nx = 1\n")), manifest).unwrap(), WorkspaceOptOut::default());
    }

    /// A link dir under a variable is kept as written, for the run to expand; its variable must
    /// be one `build-script-env` lists, whichever key comes first.
    #[test]
    fn a_link_dir_under_a_variable_is_kept_for_the_run_and_its_variable_is_declared() {
        let manifest = Path::new("/w/Cargo.toml");
        let declared = metadata(
            "[blaze.sandbox]\nbuild-script-link-dirs = [\"${LLVM_SYS_231_PREFIX}/lib\", \"/opt/lib\"]\n\
             build-script-env = [\"LLVM_SYS_231_PREFIX\"]\n",
        );
        let out = workspace(Some(&declared), manifest).unwrap();
        assert_eq!(out.link_dirs, ["${LLVM_SYS_231_PREFIX}/lib", "/opt/lib"]);

        let undeclared = metadata("[blaze.sandbox]\nbuild-script-link-dirs = [\"${LLVM_SYS_231_PREFIX}/lib\"]\n");
        let error = workspace(Some(&undeclared), manifest).unwrap_err();
        assert!(error.what.ends_with("is under `LLVM_SYS_231_PREFIX`, which `build-script-env` does not list"), "{}", error.what);
        let unclosed = metadata("[blaze.sandbox]\nbuild-script-link-dirs = [\"${LLVM/lib\"]\n");
        let error = workspace(Some(&unclosed), manifest).unwrap_err();
        assert!(error.what.contains("with no `NAME}` after it"), "{}", error.what);
    }

    #[test]
    fn a_table_said_otherwise_is_an_error() {
        let manifest = Path::new("/w/Cargo.toml");
        let unknown = workspace(Some(&metadata("[blaze.sandbox]\nbuild-scripts = []\n")), manifest).unwrap_err();
        assert!(unknown.what.starts_with("[workspace.metadata.blaze.sandbox] in /w/Cargo.toml: `build-scripts`"), "{}", unknown.what);
        let not_list = workspace(Some(&metadata("[blaze.sandbox]\nunsandboxed-build-scripts = \"a\"\n")), manifest).unwrap_err();
        assert!(not_list.what.ends_with("`unsandboxed-build-scripts` is a list"), "{}", not_list.what);
        for key in ["unsandboxed-proc-macros", "wasi-build-scripts"] {
            let asked = workspace(Some(&metadata(&format!("[blaze.sandbox]\n{key} = [\"m\"]\n"))), manifest).unwrap_err();
            assert!(asked.what.contains(&format!("`{key}` lists [\"m\"]")), "{}", asked.what);
            assert!(asked.what.ends_with("remove the key"), "{}", asked.what);
        }
        let member = Path::new("/w/a/Cargo.toml");
        let not_bool = package(Some(&metadata("[blaze.sandbox]\nbuild-script = \"no\"\n")), member).unwrap_err();
        assert!(not_bool.what.ends_with("`build-script` is true or false"), "{}", not_bool.what);
        let not_names = package(Some(&metadata("[blaze.sandbox]\nunsandboxed-tests = [1]\n")), member).unwrap_err();
        assert!(not_names.what.ends_with("`unsandboxed-tests` lists tests by their names, as strings"), "{}", not_names.what);
        let not_tests = package(Some(&metadata("[blaze.sandbox]\ntests = \"yes\"\n")), member).unwrap_err();
        assert!(not_tests.what.ends_with("`tests` is true or false"), "{}", not_tests.what);
    }

    #[test]
    fn a_member_takes_its_build_script_out() {
        let member = Path::new("/w/a/Cargo.toml");
        assert!(package(Some(&metadata("[blaze.sandbox]\nbuild-script = false\n")), member).unwrap().build_script_outside);
        assert!(!package(Some(&metadata("[blaze.sandbox]\nbuild-script = true\ntests = false\n")), member).unwrap().build_script_outside);
        assert_eq!(package(None, member).unwrap(), PackageOptOut::default());
    }

    #[test]
    fn a_member_says_how_its_tests_run() {
        let member = Path::new("/w/a/Cargo.toml");
        let opted = package(Some(&metadata("[blaze.sandbox]\ntests = true\nunsandboxed-tests = [\"b\", \"a::c\", \"b\"]\n")), member)
            .unwrap()
            .tests;
        assert_eq!(opted, TestSandbox { all: false, opted_in: true, tests: vec!["a::c".to_owned(), "b".to_owned()] });
        assert!(opted.outside("b") && opted.outside("a::c") && !opted.outside("a"));
        let out = package(Some(&metadata("[blaze.sandbox]\ntests = false\n")), member).unwrap().tests;
        assert_eq!(out, TestSandbox { all: true, opted_in: false, tests: Vec::new() });
        let named = package(Some(&metadata("[blaze.sandbox]\nunsandboxed-tests = [\"t\"]\n")), member).unwrap().tests;
        assert_eq!(named, TestSandbox { all: false, opted_in: false, tests: vec!["t".to_owned()] });
    }

    #[test]
    fn a_workspace_opts_into_sandboxed_tests() {
        let manifest = Path::new("/w/Cargo.toml");
        let on = |text: &str| workspace(Some(&metadata(text)), manifest).map(|it| it.sandboxed_tests);
        assert_eq!(on("[blaze]\nnative-proc-macros = []\n"), Ok(false));
        assert_eq!(on("[blaze]\nexperimental = [\"const-macros\"]\n"), Ok(false));
        assert_eq!(on("[blaze]\nexperimental = [\"sandboxed-tests\"]\n"), Ok(true));
        assert_eq!(
            on("[blaze]\nexperimental = [\"sandboxed-tests\"]\n[blaze.sandbox]\nunsandboxed-build-scripts = []\n"),
            Ok(true)
        );
        let unknown = on("[blaze]\nexperimental = [\"sandboxed-test\"]\n").unwrap_err();
        assert_eq!(
            unknown.what,
            "`experimental` in [workspace.metadata.blaze] of /w/Cargo.toml: `sandboxed-test` is not known \
             (known: `sandboxed-tests`, `const-macros`)"
        );
        let not_list = on("[blaze]\nexperimental = \"sandboxed-tests\"\n").unwrap_err();
        assert!(not_list.what.contains("Cargo.toml: a list of experiments' names"), "{}", not_list.what);
        let not_strings = on("[blaze]\nexperimental = [true]\n").unwrap_err();
        assert!(not_strings.what.contains("as strings"), "{}", not_strings.what);
    }

    #[test]
    fn a_package_names_its_const_macros_once_the_workspace_turns_them_on() {
        let (root, member) = (Path::new("/w/Cargo.toml"), Path::new("/w/a/Cargo.toml"));
        assert!(const_macros_on(Some(&metadata("[blaze]\nexperimental = [\"sandboxed-tests\", \"const-macros\"]\n"))));
        assert!(!const_macros_on(Some(&metadata("[blaze]\nexperimental = [\"sandboxed-tests\"]\n"))));
        assert!(!const_macros_on(None));
        let named = metadata("[blaze]\nconst-macros = \"src/../macros/derive.rs\"\n");
        assert_eq!(const_macros(Some(&named), member, root, true), Ok(Some("/w/a/macros/derive.rs".to_owned())));
        assert_eq!(const_macros(None, member, root, true), Ok(None));
        assert_eq!(const_macros(Some(&metadata("[blaze.sandbox]\ntests = true\n")), member, root, false), Ok(None));
        let off = const_macros(Some(&named), member, root, false).unwrap_err();
        assert_eq!(
            off.what,
            "`const-macros` under [package.metadata.blaze] in /w/a/Cargo.toml is an experiment: \
             `experimental = [\"const-macros\"]` under [workspace.metadata.blaze] in /w/Cargo.toml turns it on"
        );
        let not_path = const_macros(Some(&metadata("[blaze]\nconst-macros = 1\n")), member, root, true).unwrap_err();
        assert!(not_path.what.ends_with("is a path, as a string"), "{}", not_path.what);
    }
}
