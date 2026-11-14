//! The ways out of the sandbox a workspace's manifests name (docs/cargo.md, "Build scripts and
//! proc macros run in a sandbox"), read from the manifests the load already parsed: the
//! workspace's root manifest names packages by name (the only place one from a registry can be
//! named), and a member may take its own build script out and say how its tests run. Any other
//! key, or a value of another type, is an error. The workspace's `experimental` list is read
//! here too, for the `const-macros` experiment (and a package's `const-macros` file with it)
//! and for `sandboxed-tests`, the opt-in to sandboxed tests where they are not the default
//! ([`tests_sandboxed_by_default`], the one place that is decided) and a note where they are.

use std::path::Path;

use crate::query::{LoadError, TestSandbox};

/// The table of the workspace's root manifest.
pub(crate) const WORKSPACE_TABLE: &str = "[workspace.metadata.blaze.sandbox]";

/// The table of a package's own manifest.
pub(crate) const PACKAGE_TABLE: &str = "[package.metadata.blaze.sandbox]";

/// The key, in `[workspace.metadata.blaze]`, that lists the experiments a workspace opts into.
pub(crate) const EXPERIMENTAL: &str = "experimental";

/// A package's `[package.metadata.blaze] const-macros` file is a crate of macros the front end
/// runs (docs/future/const-macros.md); the key of that table too.
pub(crate) const CONST_MACROS: &str = "const-macros";

/// The workspace's opt-in to sandboxed tests: live where tests are not sandboxed by default
/// ([`tests_sandboxed_by_default`]); where they are, it does nothing and is said in a note,
/// never an error.
pub(crate) const SANDBOXED_TESTS: &str = "sandboxed-tests";

/// The experiments there are, as v1's `experimental::KNOWN`.
const KNOWN: &[&str] = &[CONST_MACROS, SANDBOXED_TESTS];

/// Whether a test `blaze test` runs on `host` (the plan's host triple, which is in the load's
/// key) is sandboxed when neither its package nor the command says: the one place the default
/// is decided.
///
/// Linux: yes. Its sandbox gives each test a PID namespace and `/proc` of its own, so a test
/// can't read another process's environment, where another launch's `BLAZE_INPROC` token is
/// (crates/blaze-inproc/src/host.rs, `Host::launch`).
///
/// macOS: no, only where the package (`tests = true`) or the workspace
/// (`experimental = ["sandboxed-tests"]`) opts in. Seatbelt does not keep a sandboxed process
/// from another of the same user's arguments and environment: a probe on macOS 27.0.1
/// (26A434) read a sibling's environment through `sysctl({CTL_KERN, KERN_PROCARGS2, pid})`
/// under `(allow default)(deny file-write*)(deny network*)`, and neither
/// `(deny process-info*)`, nor `(deny sysctl-read (sysctl-name "kern.procargs2"))` or its
/// prefix form, nor `(deny sysctl*)` stopped it. So a sandboxed test can read a second test
/// binary's launch token while that launch waits and take its grant; blaze-cargo's test
/// `sandbox::macos::tests::a_sandboxed_run_reads_another_processs_environment`
/// holds that this is so today. The difference goes when the channel to the daemon is a
/// descriptor the run inherits, with no token in its environment or on a path to read: then
/// this answers yes on every host and that test is inverted.
pub(crate) fn tests_sandboxed_by_default(host: &str) -> bool {
    !host.ends_with("-apple-darwin")
}

/// What the workspace's root manifest says in [`WORKSPACE_TABLE`] and [`EXPERIMENTAL`] that the
/// load hands on.
#[derive(Default, Debug, PartialEq, Eq)]
pub(crate) struct WorkspaceOptOut {
    /// `unsandboxed-build-scripts`: packages by name, sorted.
    pub build_scripts: Vec<String>,
    /// `build-script-link-dirs`: absolute (a relative one is under the workspace's root),
    /// normal, sorted; or `${NAME}/rest` as written, `NAME` one `build-script-env` lists.
    pub link_dirs: Vec<String>,
    /// [`EXPERIMENTAL`] lists [`SANDBOXED_TESTS`]: every member's tests run sandboxed on a host
    /// that does not by default ([`tests_sandboxed_by_default`]).
    pub sandboxed_tests: bool,
    /// The load's notes: [`SANDBOXED_TESTS`] listed for a host that sandboxes tests by
    /// default, where it does nothing ([`crate::query::Units::notes`]).
    pub notes: Vec<String>,
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
    /// `tests = true`: an opt-in on a host that does not sandbox tests by default
    /// ([`tests_sandboxed_by_default`]); the default said out loud on one that does.
    pub tests_opted_in: bool,
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
    LoadError {
        what: format!("{table} in {}: {what}", manifest.display()),
        at: Some(manifest.display().to_string()),
    }
}

/// What `[workspace.metadata]` (`metadata`) of the root manifest at `manifest` says in
/// [`WORKSPACE_TABLE`] and [`EXPERIMENTAL`], for a build on `host` (its triple).
pub(crate) fn workspace(
    metadata: Option<&toml::Value>,
    manifest: &Path,
    host: &str,
) -> Result<WorkspaceOptOut, LoadError> {
    let (sandboxed_tests, notes) = experiments(metadata, manifest, host)?;
    let mut out = WorkspaceOptOut { sandboxed_tests, notes, ..WorkspaceOptOut::default() };
    let Some(table) = sandbox_table(metadata) else { return Ok(out) };
    let wrong = |what: &str| wrong(WORKSPACE_TABLE, manifest, what);
    let root = manifest.parent().unwrap_or(Path::new("/"));
    let mut env: Vec<&str> = Vec::new(); // alloc: a manifest's few names, once a load
    let mut under_vars: Vec<(&str, &str)> = Vec::new(); // alloc: as above
    for (key, value) in table {
        let names = || -> Result<Vec<&str>, LoadError> {
            let list = value.as_array().ok_or_else(|| wrong(&format!("`{key}` is a list")))?;
            list.iter()
                .map(|it| {
                    it.as_str().ok_or_else(|| wrong(&format!("`{key}` lists names, as strings")))
                })
                .collect()
        };
        match key.as_str() {
            "unsandboxed-build-scripts" => {
                out.build_scripts = names()?.into_iter().map(str::to_owned).collect()
            }
            "build-script-link-dirs" => {
                for dir in names()? {
                    // Expanded from the script's environment when it runs (`blaze_cargo::directives::link_dirs`).
                    if let Some(rest) = dir.strip_prefix("${") {
                        let name =
                            rest.split_once('}').map(|(name, _)| name).filter(|it| !it.is_empty());
                        let name = name.ok_or_else(|| {
                            wrong(&format!("`{dir}` of `build-script-link-dirs` starts `${{` with no `NAME}}` after it"))
                        })?;
                        under_vars.push((dir, name));
                        out.link_dirs.push(dir.to_owned());
                        continue;
                    }
                    let absolute = super::files::normal(&root.join(dir)).ok_or_else(|| {
                        wrong(&format!("`{dir}` of `build-script-link-dirs` is not a UTF-8 path"))
                    })?;
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
/// lists [`SANDBOXED_TESTS`], and the notes of the load: that name, once, where `host`
/// sandboxes tests by default and it does nothing. A name not [`KNOWN`], or a value that is
/// not a list of strings, is an error worded as v1's.
fn experiments(
    metadata: Option<&toml::Value>,
    manifest: &Path,
    host: &str,
) -> Result<(bool, Vec<String>), LoadError> {
    let mut notes = Vec::new_in(std::alloc::Global);
    let mut sandboxed_tests = false;
    let Some(listed) = blaze_table(metadata).and_then(|it| it.get(EXPERIMENTAL)) else {
        return Ok((sandboxed_tests, notes));
    };
    let wrong = |what: &str| LoadError {
        what: format!(
            "`{EXPERIMENTAL}` in [workspace.metadata.blaze] of {}: {what}",
            manifest.display()
        ),
        at: Some(manifest.display().to_string()),
    };
    // alloc: the known names, only for an error
    let known = || KNOWN.iter().map(|it| format!("`{it}`")).collect::<Vec<_>>().join(", ");
    let names = listed
        .as_array()
        .ok_or_else(|| wrong(&format!("a list of experiments' names (known: {})", known())))?;
    for name in names {
        let name = name.as_str().ok_or_else(|| {
            wrong(&format!("a list of experiments' names, as strings (known: {})", known()))
        })?;
        if !KNOWN.contains(&name) {
            return Err(wrong(&format!("`{name}` is not known (known: {})", known())));
        }
        // Where tests are sandboxed by default the opt-in asked for exactly that: not refused
        // (that would break the workspaces that were ahead), nor taken in silence.
        if name == SANDBOXED_TESTS && !sandboxed_tests {
            sandboxed_tests = true;
            if tests_sandboxed_by_default(host) {
                notes.push(format!(
                    "`{name}` in `{EXPERIMENTAL}` under [workspace.metadata.blaze] of {} does \
                     nothing on {host}: every test `blaze test` runs here is sandboxed unless its \
                     package or the command takes it out (docs/cargo.md, \"Sandboxed tests\"); \
                     delete it from the list once no macOS machine builds the workspace, where \
                     it is still the opt-in",
                    manifest.display()
                ));
            }
        }
    }
    Ok((sandboxed_tests, notes))
}

/// The tests of a member as its plan holds them on `host`: `tests` as its manifest says, and
/// whether it runs them outside for want of an opt-in (`opted_in`: its `tests = true` or the
/// workspace's [`SANDBOXED_TESTS`]) on a host that sandboxes tests only so
/// ([`tests_sandboxed_by_default`]).
pub(crate) fn tests_on(mut tests: TestSandbox, opted_in: bool, host: &str) -> TestSandbox {
    tests.outside_by_host = !tests.all && !opted_in && !tests_sandboxed_by_default(host);
    tests
}

/// What `[package.metadata]` (`metadata`) of the member whose manifest is `manifest` says in
/// [`PACKAGE_TABLE`]: `build-script = false`, and how its tests run (v1's `sandboxed::opt_out`).
pub(crate) fn package(
    metadata: Option<&toml::Value>,
    manifest: &Path,
) -> Result<PackageOptOut, LoadError> {
    let mut out = PackageOptOut::default();
    let Some(table) = sandbox_table(metadata) else { return Ok(out) };
    let wrong = |what: &str| wrong(PACKAGE_TABLE, manifest, what);
    for (key, value) in table {
        match key.as_str() {
            // `true`: every test sandboxed but those `unsandboxed-tests` names; the default
            // said out loud where tests are sandboxed by default, the opt-in where not.
            "tests" => {
                let tests = value.as_bool().ok_or_else(|| wrong("`tests` is true or false"))?;
                out.tests.all = !tests;
                out.tests_opted_in = tests;
            }
            "unsandboxed-tests" => {
                let names =
                    value.as_array().ok_or_else(|| wrong("`unsandboxed-tests` is a list"))?;
                out.tests.tests = names
                    .iter()
                    .map(|it| {
                        it.as_str()
                            // alloc: a test's name, once per name the member lists
                            .map(str::to_owned)
                            .ok_or_else(|| {
                                wrong("`unsandboxed-tests` lists tests by their names, as strings")
                            })
                    })
                    .collect::<Result<_, _>>()?;
            }
            "build-script" => {
                out.build_script_outside =
                    !value.as_bool().ok_or_else(|| wrong("`build-script` is true or false"))?;
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
    let listed =
        blaze_table(metadata).and_then(|it| it.get(EXPERIMENTAL)).and_then(|it| it.as_array());
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
    let Some(value) = blaze_table(metadata).and_then(|it| it.get(CONST_MACROS)) else {
        return Ok(None);
    };
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

    /// A host that sandboxes tests by default, and one that does only on an opt-in.
    const LINUX: &str = "x86_64-unknown-linux-gnu";
    const MAC: &str = "aarch64-apple-darwin";

    #[test]
    fn the_workspace_names_packages_and_link_dirs() {
        let manifest = Path::new("/w/Cargo.toml");
        let value = metadata(
            "[blaze.sandbox]\nunsandboxed-build-scripts = [\"z-sys\", \"a-sys\"]\n\
             build-script-link-dirs = [\"/opt/lib\", \"vendor/../lib\"]\nbuild-script-env = [\"OPENSSL_DIR\"]\n",
        );
        let out = workspace(Some(&value), manifest, LINUX).unwrap();
        assert_eq!(out.build_scripts, ["a-sys", "z-sys"]);
        assert_eq!(out.link_dirs, ["/opt/lib", "/w/lib"]);
        assert!(out.unsandboxed("z-sys") && !out.unsandboxed("z"));
        assert!(out.notes.is_empty());
        assert_eq!(workspace(None, manifest, LINUX).unwrap(), WorkspaceOptOut::default());
        assert_eq!(
            workspace(Some(&metadata("[other]\nx = 1\n")), manifest, LINUX).unwrap(),
            WorkspaceOptOut::default()
        );
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
        let out = workspace(Some(&declared), manifest, LINUX).unwrap();
        assert_eq!(out.link_dirs, ["${LLVM_SYS_231_PREFIX}/lib", "/opt/lib"]);

        let undeclared = metadata(
            "[blaze.sandbox]\nbuild-script-link-dirs = [\"${LLVM_SYS_231_PREFIX}/lib\"]\n",
        );
        let error = workspace(Some(&undeclared), manifest, LINUX).unwrap_err();
        assert!(
            error.what.ends_with(
                "is under `LLVM_SYS_231_PREFIX`, which `build-script-env` does not list"
            ),
            "{}",
            error.what
        );
        let unclosed = metadata("[blaze.sandbox]\nbuild-script-link-dirs = [\"${LLVM/lib\"]\n");
        let error = workspace(Some(&unclosed), manifest, LINUX).unwrap_err();
        assert!(error.what.contains("with no `NAME}` after it"), "{}", error.what);
    }

    #[test]
    fn a_table_said_otherwise_is_an_error() {
        let manifest = Path::new("/w/Cargo.toml");
        let unknown =
            workspace(Some(&metadata("[blaze.sandbox]\nbuild-scripts = []\n")), manifest, LINUX)
                .unwrap_err();
        assert!(
            unknown.what.starts_with(
                "[workspace.metadata.blaze.sandbox] in /w/Cargo.toml: `build-scripts`"
            ),
            "{}",
            unknown.what
        );
        let not_list = workspace(
            Some(&metadata("[blaze.sandbox]\nunsandboxed-build-scripts = \"a\"\n")),
            manifest,
            LINUX,
        )
        .unwrap_err();
        assert!(
            not_list.what.ends_with("`unsandboxed-build-scripts` is a list"),
            "{}",
            not_list.what
        );
        for key in ["unsandboxed-proc-macros", "wasi-build-scripts"] {
            let asked = workspace(
                Some(&metadata(&format!("[blaze.sandbox]\n{key} = [\"m\"]\n"))),
                manifest,
                LINUX,
            )
            .unwrap_err();
            assert!(asked.what.contains(&format!("`{key}` lists [\"m\"]")), "{}", asked.what);
            assert!(asked.what.ends_with("remove the key"), "{}", asked.what);
        }
        let member = Path::new("/w/a/Cargo.toml");
        let not_bool = package(Some(&metadata("[blaze.sandbox]\nbuild-script = \"no\"\n")), member)
            .unwrap_err();
        assert!(not_bool.what.ends_with("`build-script` is true or false"), "{}", not_bool.what);
        let not_names =
            package(Some(&metadata("[blaze.sandbox]\nunsandboxed-tests = [1]\n")), member)
                .unwrap_err();
        assert!(
            not_names.what.ends_with("`unsandboxed-tests` lists tests by their names, as strings"),
            "{}",
            not_names.what
        );
        let not_tests =
            package(Some(&metadata("[blaze.sandbox]\ntests = \"yes\"\n")), member).unwrap_err();
        assert!(not_tests.what.ends_with("`tests` is true or false"), "{}", not_tests.what);
    }

    #[test]
    fn a_member_takes_its_build_script_out() {
        let member = Path::new("/w/a/Cargo.toml");
        assert!(
            package(Some(&metadata("[blaze.sandbox]\nbuild-script = false\n")), member)
                .unwrap()
                .build_script_outside
        );
        assert!(
            !package(
                Some(&metadata("[blaze.sandbox]\nbuild-script = true\ntests = false\n")),
                member
            )
            .unwrap()
            .build_script_outside
        );
        assert_eq!(package(None, member).unwrap(), PackageOptOut::default());
    }

    /// The invariant: a member's manifest says how its tests run: nothing said is the host's
    /// default; `tests = true` is every test sandboxed (an opt-in where the host's default is
    /// outside); `tests = false` is none; `unsandboxed-tests` is all but those named.
    #[test]
    fn a_member_says_how_its_tests_run() {
        let member = Path::new("/w/a/Cargo.toml");
        let nothing = package(None, member).unwrap();
        assert_eq!(nothing.tests, TestSandbox::default());
        assert!(!nothing.tests_opted_in);
        assert!(!nothing.tests.all && !nothing.tests.outside("a"), "nothing said");
        let said_true = package(Some(&metadata("[blaze.sandbox]\ntests = true\n")), member).unwrap();
        assert_eq!(said_true.tests, TestSandbox::default());
        assert!(said_true.tests_opted_in);
        let named = package(
            Some(&metadata(
                "[blaze.sandbox]\ntests = true\nunsandboxed-tests = [\"b\", \"a::c\", \"b\"]\n",
            )),
            member,
        )
        .unwrap()
        .tests;
        assert_eq!(
            named,
            TestSandbox { tests: vec!["a::c".to_owned(), "b".to_owned()], ..TestSandbox::default() }
        );
        assert!(named.outside("b") && named.outside("a::c") && !named.outside("a"));
        let out = package(Some(&metadata("[blaze.sandbox]\ntests = false\n")), member).unwrap();
        assert_eq!(out.tests, TestSandbox { all: true, ..TestSandbox::default() });
        assert!(!out.tests_opted_in);
        let only_named =
            package(Some(&metadata("[blaze.sandbox]\nunsandboxed-tests = [\"t\"]\n")), member)
                .unwrap()
                .tests;
        assert_eq!(only_named, TestSandbox { tests: vec!["t".to_owned()], ..TestSandbox::default() });
    }

    /// The invariant: on a host that sandboxes tests by default a member's tests run sandboxed
    /// unless `tests = false`; on one that does not (macOS, `tests_sandboxed_by_default`) only
    /// with an opt-in, else outside by the host; `tests = false` is said alone either way.
    #[test]
    fn whether_a_members_tests_are_sandboxed_by_default_is_the_hosts() {
        assert!(tests_sandboxed_by_default(LINUX));
        assert!(tests_sandboxed_by_default("aarch64-unknown-linux-gnu"));
        assert!(!tests_sandboxed_by_default(MAC));
        assert!(!tests_sandboxed_by_default("x86_64-apple-darwin"));
        let plain = TestSandbox::default();
        let off = TestSandbox { all: true, ..TestSandbox::default() };
        assert!(!tests_on(plain.clone(), false, LINUX).outside_by_host);
        assert!(tests_on(plain.clone(), false, MAC).outside_by_host, "the control");
        assert!(!tests_on(plain, true, MAC).outside_by_host, "opted in");
        assert!(!tests_on(off.clone(), false, MAC).outside_by_host);
        assert_eq!(tests_on(off.clone(), false, LINUX), off);
    }

    /// `sandboxed-tests` is the workspace's opt-in on a host that does not sandbox tests by
    /// default, taken in silence; on one that does it is still taken (never refused), and each
    /// load says in one note that it does nothing there; a name never known is an error.
    #[test]
    fn sandboxed_tests_is_an_opt_in_on_macos_and_a_note_on_linux() {
        let manifest = Path::new("/w/Cargo.toml");
        let listed = "[blaze]\nexperimental = [\"sandboxed-tests\", \"sandboxed-tests\"]\n";
        let mac = workspace(Some(&metadata(listed)), manifest, MAC).unwrap();
        assert!(mac.sandboxed_tests && mac.notes.is_empty(), "{mac:?}");
        let notes =
            |text: &str| workspace(Some(&metadata(text)), manifest, LINUX).map(|it| it.notes);
        assert_eq!(notes("[blaze]\nnative-proc-macros = []\n"), Ok(Vec::new()));
        assert_eq!(notes("[blaze]\nexperimental = [\"const-macros\"]\n"), Ok(Vec::new()));
        let linux = workspace(Some(&metadata(listed)), manifest, LINUX).unwrap();
        assert!(linux.sandboxed_tests);
        let said = linux.notes;
        assert_eq!(said.len(), 1, "once: {said:?}");
        assert!(
            said[0].starts_with(
                "`sandboxed-tests` in `experimental` under [workspace.metadata.blaze] of \
                 /w/Cargo.toml does nothing on x86_64-unknown-linux-gnu: every test"
            ),
            "{}",
            said[0]
        );
        assert!(said[0].ends_with("where it is still the opt-in"), "{}", said[0]);
        assert_eq!(
            notes(
                "[blaze]\nexperimental = [\"sandboxed-tests\"]\n[blaze.sandbox]\nunsandboxed-build-scripts = []\n"
            ),
            Ok(said.clone())
        );
        let on = |text: &str| workspace(Some(&metadata(text)), manifest, LINUX);
        let unknown = on("[blaze]\nexperimental = [\"sandboxed-test\"]\n").unwrap_err();
        assert_eq!(
            unknown.what,
            "`experimental` in [workspace.metadata.blaze] of /w/Cargo.toml: `sandboxed-test` is not known \
             (known: `const-macros`, `sandboxed-tests`)"
        );
        let not_list = on("[blaze]\nexperimental = \"sandboxed-tests\"\n").unwrap_err();
        assert!(
            not_list.what.contains("Cargo.toml: a list of experiments' names"),
            "{}",
            not_list.what
        );
        let not_strings = on("[blaze]\nexperimental = [true]\n").unwrap_err();
        assert!(not_strings.what.contains("as strings"), "{}", not_strings.what);
    }

    #[test]
    fn a_package_names_its_const_macros_once_the_workspace_turns_them_on() {
        let (root, member) = (Path::new("/w/Cargo.toml"), Path::new("/w/a/Cargo.toml"));
        assert!(const_macros_on(Some(&metadata(
            "[blaze]\nexperimental = [\"sandboxed-tests\", \"const-macros\"]\n"
        ))));
        assert!(!const_macros_on(Some(&metadata(
            "[blaze]\nexperimental = [\"sandboxed-tests\"]\n"
        ))));
        assert!(!const_macros_on(None));
        let named = metadata("[blaze]\nconst-macros = \"src/../macros/derive.rs\"\n");
        assert_eq!(
            const_macros(Some(&named), member, root, true),
            Ok(Some("/w/a/macros/derive.rs".to_owned()))
        );
        assert_eq!(const_macros(None, member, root, true), Ok(None));
        assert_eq!(
            const_macros(Some(&metadata("[blaze.sandbox]\ntests = true\n")), member, root, false),
            Ok(None)
        );
        let off = const_macros(Some(&named), member, root, false).unwrap_err();
        assert_eq!(
            off.what,
            "`const-macros` under [package.metadata.blaze] in /w/a/Cargo.toml is an experiment: \
             `experimental = [\"const-macros\"]` under [workspace.metadata.blaze] in /w/Cargo.toml turns it on"
        );
        let not_path =
            const_macros(Some(&metadata("[blaze]\nconst-macros = 1\n")), member, root, true)
                .unwrap_err();
        assert!(not_path.what.ends_with("is a path, as a string"), "{}", not_path.what);
    }
}
