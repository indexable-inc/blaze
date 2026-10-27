//! Build scripts under WASI, an experiment beside the native sandbox (docs/cargo.md, "Build
//! scripts under WASI"). A script to run so is built a second time, for wasm32-wasip1 with
//! blaze's `std::process` (`ra-jit --run-tool`), beside where rustc links it ([`module_of`]).
//! When cargo runs it, Wasmtime runs the module in place of the executable
//! ([`blaze_wasi::run`]). It sees its package's directory, read-only, at its path and at `.`,
//! and its `OUT_DIR`, writable, and nothing else of the host; its environment is what cargo
//! documents for build scripts ([`documented`]); and a command it runs is run on the host, in
//! the sandbox the script would have run in, when it is the toolchain's `rustc`, a C compiler
//! or its archiver, or pkg-config ([`allowed`]), and refused, naming the program, when it is
//! anything else. Each command is written down beside `OUT_DIR` ([`TOOLS`]).
//!
//! - `RA_JIT_BUILD_SCRIPTS=wasi` runs every script so; `wasi-build-scripts` in
//!   `[workspace.metadata.blaze.sandbox]` runs those of the packages it lists (`"*"`, all).
//! - `RA_JIT_BUILD_SCRIPTS=wasi-compare` is for measuring: the module runs first and what it
//!   printed and wrote is set aside ([`ASIDE`]), then the script runs as without, for cargo;
//!   `harness/bswasi.py` compares the two.
//!
//! A script ra-jit can't build for wasm32-wasip1 is an error, as is a run that fails: the
//! native script does not run instead.

use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
};

use anyhow::Context as _;

use super::{own_script, sandboxed, script_sandbox};

/// The variable that runs build scripts under WASI: `wasi`, `wasi-compare` or `native`.
pub(crate) const VAR: &str = "RA_JIT_BUILD_SCRIPTS";

/// The key of [`sandboxed::WORKSPACE_TABLE`] that lists the packages whose scripts run under
/// WASI.
pub(crate) const LISTED: &str = "wasi-build-scripts";

/// The directory beside `OUT_DIR` where `wasi-compare` sets aside what the module did.
pub(crate) const ASIDE: &str = "ra-jit-wasi";

/// The file beside `OUT_DIR` with a line of JSON for each command the module asked the host to
/// run: the program, the one that ran, its arguments, where, and how it ended or why it was
/// refused.
pub(crate) const TOOLS: &str = "ra-jit-wasi-tools.jsonl";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Mode {
    /// The module runs for cargo.
    Wasi,
    /// The module runs and is set aside, and the script runs for cargo.
    Compare,
}

/// How the build script of `package`, in `cwd`, is built and run under WASI: as [`VAR`] says,
/// else as the workspace's [`LISTED`] does, `None` when it is not. `out_dir` is where cargo
/// builds or runs it, which finds the workspace of a package from a registry.
pub(crate) fn mode(
    package: &str,
    cwd: &Path,
    out_dir: Option<&Path>,
) -> anyhow::Result<Option<Mode>> {
    let (mode, why) = match std::env::var(VAR).as_deref() {
        Err(_) | Ok("" | "native") => (None, String::new()),
        Ok("wasi") => (Some(Mode::Wasi), format!("{VAR}=wasi")),
        Ok("wasi-compare") => (Some(Mode::Compare), format!("{VAR}=wasi-compare")),
        Ok(other) => {
            anyhow::bail!("{VAR}={other} is not known: `native`, `wasi` and `wasi-compare` are")
        }
    };
    let (mode, why) = match mode {
        Some(mode) => (Some(mode), why),
        None => match listed(package, cwd, out_dir)? {
            Some(manifest) => (Some(Mode::Wasi), format!("`{LISTED}` of {}", manifest.display())),
            None => (None, why),
        },
    };
    if mode.is_some() && !cfg!(feature = "wasi") {
        anyhow::bail!(
            "the build script of `{package}` is to run under WASI ({why}), and this ra-jit is \
             built without Wasmtime: build it with the feature `wasi` of the crate `ra-jit`"
        );
    }
    Ok(mode)
}

/// The root manifest whose [`LISTED`] has `package`, or `"*"`.
fn listed(package: &str, cwd: &Path, out_dir: Option<&Path>) -> anyhow::Result<Option<PathBuf>> {
    let home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|it| Path::new(&it).join(".cargo")));
    let mut root = None;
    if !home.is_some_and(|it| cwd.starts_with(it)) {
        root = sandboxed::workspace_root(cwd)?;
    }
    if root.is_none()
        && let Some(dir) = out_dir.and_then(script_sandbox::target_of).and_then(Path::parent)
    {
        root = sandboxed::workspace_root(dir)?;
    }
    let Some(root) = root else { return Ok(None) };
    let out = sandboxed::workspace_opt_out(&root)?;
    let listed = &out.wasi_build_scripts;
    Ok((listed.contains(package) || listed.contains("*")).then_some(out.manifest))
}

/// Where the module of the script rustc links at `exe` is.
pub(crate) fn module_of(exe: &Path) -> PathBuf {
    let mut name = exe.as_os_str().to_owned();
    name.push(".wasm");
    PathBuf::from(name)
}

/// The file beside `module` that says why ra-jit did not build it, under `wasi-compare`.
fn not_built_of(module: &Path) -> PathBuf {
    let mut name = module.as_os_str().to_owned();
    name.push(".not-built");
    PathBuf::from(name)
}

/// Builds the module of the build script of `package`, which rustc links at `exe`, as cargo
/// asked rustc for it with `args` in `cwd` ([`own_script::compile_wasi`]), describing it in
/// `dir`. Under `wasi-compare` a module not built is written down for the run to set aside;
/// otherwise it is an error.
#[expect(clippy::too_many_arguments)]
pub(crate) fn compile(
    mode: Mode,
    package: &str,
    rustc: &OsStr,
    args: &[String],
    cwd: &Path,
    source: &Path,
    exe: &Path,
    dir: &Path,
) -> anyhow::Result<()> {
    let module = module_of(exe);
    let not_built = not_built_of(&module);
    _ = std::fs::remove_file(&not_built);
    let Err(why) = own_script::compile_wasi(rustc, args, cwd, source, &module, dir) else {
        return Ok(());
    };
    _ = std::fs::remove_file(&module);
    match mode {
        Mode::Compare => std::fs::write(&not_built, &why)
            .with_context(|| format!("writing {}", not_built.display())),
        Mode::Wasi => anyhow::bail!(
            "the build script of `{package}` is to run under WASI, and ra-jit could not build \
             it for {}: {why}",
            crate::RUN_TOOL_TARGET
        ),
    }
}

/// Whether cargo documents `name` as a variable it sets for build scripts (the Cargo Book,
/// "Environment variables Cargo sets for build scripts"), the package's (`CARGO_PKG_*`) with
/// them: all a script under WASI is given.
pub(crate) fn documented(name: &str) -> bool {
    const NAMES: [&str; 18] = [
        "CARGO",
        "CARGO_MANIFEST_DIR",
        "CARGO_MANIFEST_PATH",
        "CARGO_MANIFEST_LINKS",
        "CARGO_MAKEFLAGS",
        "CARGO_ENCODED_RUSTFLAGS",
        "OUT_DIR",
        "TARGET",
        "HOST",
        "NUM_JOBS",
        "OPT_LEVEL",
        "DEBUG",
        "PROFILE",
        "RUSTC",
        "RUSTDOC",
        "RUSTC_LINKER",
        "RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
    ];
    const PREFIXES: [&str; 4] = ["CARGO_FEATURE_", "CARGO_CFG_", "CARGO_PKG_", "DEP_"];
    NAMES.contains(&name) || PREFIXES.iter().any(|it| name.starts_with(it))
}

/// The programs a script under WASI may run besides the toolchain's `rustc`, by name, with or
/// without a target's prefix (`x86_64-linux-gnu-gcc`) or a version (`clang-19`): C and C++
/// compilers, the archivers `cc` makes libraries with, and pkg-config.
const TOOLS_ALLOWED: [&str; 13] = [
    "cc",
    "c++",
    "gcc",
    "g++",
    "clang",
    "clang++",
    "cpp",
    "ar",
    "ranlib",
    "llvm-ar",
    "llvm-ranlib",
    "pkg-config",
    "pkgconf",
];

/// Whether `program`, as a script names it, is one it may run under WASI, `rustc` being the
/// `RUSTC` it was given.
pub(crate) fn allowed(program: &str, rustc: Option<&str>) -> bool {
    if Some(program) == rustc {
        return true;
    }
    let name = Path::new(program).file_name().and_then(OsStr::to_str).unwrap_or(program);
    let name = name.strip_suffix(".exe").unwrap_or(name);
    let unversioned = match name.rsplit_once('-') {
        Some((head, version)) if version.bytes().all(|it| it.is_ascii_digit() || it == b'.') => {
            head
        }
        _ => name,
    };
    unversioned == "rustc"
        || TOOLS_ALLOWED.iter().any(|tool| {
            unversioned == *tool
                || unversioned.strip_suffix(tool).is_some_and(|it| it.ends_with('-'))
        })
}

#[cfg(feature = "wasi")]
pub(crate) use running::as_cargo_does;

/// [`as_cargo_does`] of a ra-jit without Wasmtime: [`mode`] refuses first.
#[cfg(not(feature = "wasi"))]
pub(crate) fn as_cargo_does(
    _: &script_sandbox::Script<'_>,
    _: &script_sandbox::Plan,
    _: Mode,
    _: &super::script::Places,
) -> anyhow::Result<()> {
    anyhow::bail!("this ra-jit is built without Wasmtime (the feature `wasi` of `ra-jit`)")
}

#[cfg(feature = "wasi")]
mod running {
    use std::{
        ffi::OsString,
        io::Write as _,
        path::{Path, PathBuf},
        process::Stdio,
        sync::{Arc, Mutex},
        time::Instant,
    };

    use anyhow::Context as _;
    use blaze_wasi::{
        Request, Response,
        run::{Guest, Preopen},
    };
    use serde::Serialize;
    use sha2::{Digest as _, Sha256};

    use super::{ASIDE, Mode, TOOLS, allowed, documented, module_of, not_built_of};
    use crate::cargo::{
        directives, keyed,
        script::{self, Kept, Places},
        script_sandbox::{self, Plan, Script},
    };

    /// What the store keeps runs under WASI under, beside the native runs' ([`script::RUNS`]).
    /// The key is the Nix model's: everything a run is given, before it runs ([`run_key`]).
    const WASI_RUNS: &str = "build script runs under WASI 1";

    /// One command a module asked the host to run, as [`TOOLS`] has it.
    #[derive(Serialize)]
    struct Tool {
        program: String,
        /// The program that ran, as found on `PATH`; `None` when refused or not found.
        ran: Option<String>,
        args: Vec<String>,
        cwd: String,
        status: Option<i32>,
        refused: Option<String>,
        error: Option<String>,
        ms: u128,
    }

    /// How a run of a module went.
    struct Ran {
        status: i32,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
        trap: Option<String>,
        tools: Vec<Tool>,
    }

    /// Runs `script`'s module for cargo (`Mode::Wasi`), or runs it and sets aside what it did
    /// (`Mode::Compare`), in place of what `plan` says of the native script. Under `Mode::Wasi`
    /// it ends the process as the run ended; under `Mode::Compare` it returns, the native script
    /// to run next.
    pub(crate) fn as_cargo_does(
        script: &Script<'_>,
        plan: &Plan,
        mode: Mode,
        places: &Places,
    ) -> anyhow::Result<()> {
        let package = script.env.iter().find(|it| it.0 == "CARGO_PKG_NAME").map_or("?", |it| &it.1);
        let module = module_of(&linked_of(script.real));
        if mode == Mode::Compare {
            let ran = run(script, plan, &module).map_err(|it| format!("{it:#}"));
            return set_aside(ran, script.out_dir, script.scratch);
        }
        ra_jit_store::compiles_against(script::RUNS);
        let store = ra_jit_store::Store::global_or(crate::cargo::daemon::store().as_deref());
        let key = store.zip(run_key(&module, script, plan, places));
        let checking = keyed::check();
        let found = key.as_ref().and_then(|(store, key)| {
            let (bytes, _) = keyed::find(store, key, &keyed::digest)?;
            Some(Kept::read(&bytes)?.placed(places))
        });
        if let Some(kept) = &found
            && !checking
        {
            kept.restore(script.out_dir)?;
            if std::env::var_os("RA_JIT_STATS").is_some() {
                eprintln!("ra-jit: a run under WASI the store kept");
            }
            std::io::stdout().write_all(&kept.stdout)?;
            std::io::stderr().write_all(&kept.stderr)?;
            std::process::exit(0);
        }
        let ran = run(script, plan, &module)?;
        std::io::stdout().write_all(&ran.stdout)?;
        std::io::stderr().write_all(&ran.stderr)?;
        if let Some(trap) = &ran.trap {
            eprintln!("note: the build script of `{package}` under WASI ended with a trap: {trap}");
        }
        let refused: Vec<&str> = ran.tools.iter().filter_map(|it| it.refused.as_deref()).collect();
        if ran.status != 0 && !refused.is_empty() {
            eprintln!("note: under WASI, {}", refused.join("; "));
        }
        if ran.status != 0 {
            std::process::exit(ran.status);
        }
        let link_dirs = match plan {
            Plan::Sandboxed { link_dirs, .. } => link_dirs.clone(),
            Plan::Outside(..) => Vec::new(),
        };
        let directives =
            directives::Ran { package_dir: script.cwd, out_dir: script.out_dir, link_dirs: &link_dirs };
        let refused = directives::refused(&ran.stdout, &directives);
        if !refused.is_empty() {
            eprintln!("error: {}", directives::refused_error(package, &refused));
            std::process::exit(1);
        }
        let made = Kept::of(&ran.stdout, &ran.stderr, script.out_dir);
        if checking
            && let (Some(kept), Some(made)) = (&found, &made)
            && kept != made
        {
            eprintln!(
                "error: {}=check: the build script of `{package}` under WASI did something else \
                 than the run the store kept of it with the same inputs ({})",
                keyed::CHECK,
                script.out_dir.display()
            );
            std::process::exit(1);
        }
        if let (Some((store, key)), Some(made)) = (key, made.and_then(|it| it.unplaced(places))) {
            store.settled();
            keyed::keep(store, &key, &[], &keyed::digest, &made.bytes());
            store.flush();
        }
        std::process::exit(0)
    }

    /// The name rustc linked a script at, `real` being where it is now.
    fn linked_of(real: &Path) -> PathBuf {
        real.to_str()
            .and_then(|it| it.strip_suffix(".real"))
            .map_or_else(|| real.to_owned(), PathBuf::from)
    }

    /// Runs `module`, `script`'s, under Wasmtime, its commands run as `plan` says of the
    /// script, each written down in [`TOOLS`].
    fn run(script: &Script<'_>, plan: &Plan, module: &Path) -> anyhow::Result<Ran> {
        let package = script.env.iter().find(|it| it.0 == "CARGO_PKG_NAME").map_or("?", |it| &it.1);
        if !module.exists() {
            match std::fs::read_to_string(not_built_of(module)) {
                Ok(why) => anyhow::bail!(
                    "ra-jit did not build the build script of `{package}` for {}: {why}",
                    crate::RUN_TOOL_TARGET
                ),
                Err(_) => anyhow::bail!(
                    "the build script of `{package}` has no build for {} ({}): it was built \
                     before it was to run under WASI; `cargo clean -p {package}` builds it again",
                    crate::RUN_TOOL_TARGET,
                    module.display()
                ),
            }
        }
        let compiled = blaze_wasi::run::compile(module)?;
        let env: Vec<(String, String)> =
            plan.env().iter().filter(|it| documented(&it.0)).cloned().collect();
        let text = |path: &Path| path.to_str().map(str::to_owned).context("a path that is not text");
        let preopens = [
            Preopen { host: script.cwd.to_owned(), guest: text(script.cwd)?, writable: false },
            Preopen { host: script.cwd.to_owned(), guest: ".".to_owned(), writable: false },
            Preopen {
                host: script.out_dir.to_owned(),
                guest: text(script.out_dir)?,
                writable: true,
            },
        ];
        let mut args = vec![text(&linked_of(script.real))?];
        args.extend(script.args.iter().map(|it| it.to_string_lossy().into_owned()));
        let tools = Arc::new(Mutex::new(Vec::new()));
        let host = Host {
            plan: plan.clone(),
            package: package.to_owned(),
            package_dir: script.cwd.to_owned(),
            out_dir: script.out_dir.to_owned(),
            scratch: script.scratch.join("ra-jit-wasi-tool"),
            rustc: env.iter().find(|it| it.0 == "RUSTC").map(|it| it.1.clone()),
            tools: tools.clone(),
        };
        let guest = Guest { module, args: &args, env: &env, preopens: &preopens };
        let exited = blaze_wasi::run::run(&guest, &compiled, move |request| host.answer(request))?;
        let tools = std::mem::take(&mut *crate::lock(&tools));
        let mut lines = Vec::new();
        for tool in &tools {
            lines.extend(serde_json::to_vec(tool)?);
            lines.push(b'\n');
        }
        let record = script.scratch.join(TOOLS);
        std::fs::write(&record, lines).with_context(|| format!("writing {}", record.display()))?;
        Ok(Ran {
            status: exited.status,
            stdout: exited.stdout,
            stderr: exited.stderr,
            trap: exited.trap,
            tools,
        })
    }

    /// What answers a module's commands.
    struct Host {
        plan: Plan,
        package: String,
        package_dir: PathBuf,
        out_dir: PathBuf,
        scratch: PathBuf,
        rustc: Option<String>,
        tools: Arc<Mutex<Vec<Tool>>>,
    }

    impl Host {
        fn answer(&self, request: Request) -> Response {
            let text = |bytes: &[u8]| String::from_utf8_lossy(bytes).into_owned();
            let program = text(&request.program);
            let args: Vec<String> = request.args.iter().map(|it| text(it)).collect();
            let cwd = match &request.cwd {
                Some(cwd) => self.package_dir.join(text(cwd)),
                None => self.package_dir.clone(),
            };
            let started = Instant::now();
            let mut tool = Tool {
                program: program.clone(),
                ran: None,
                args: args.clone(),
                cwd: cwd.display().to_string(),
                status: None,
                refused: None,
                error: None,
                ms: 0,
            };
            let response = if !allowed(&program, self.rustc.as_deref()) {
                let why = format!(
                    "the build script of `{}` ran `{program}`, and a script under WASI runs only \
                     the toolchain's rustc, a C compiler or its archiver, and pkg-config",
                    self.package
                );
                tool.refused = Some(why.clone());
                Response::Refused(why)
            } else {
                match self.run(&program, &args, &cwd, &request) {
                    Ok((ran, output)) => {
                        tool.ran = Some(ran.display().to_string());
                        let status = output.status.code().unwrap_or(-1);
                        tool.status = Some(status);
                        Response::Ran { status, stdout: output.stdout, stderr: output.stderr }
                    }
                    Err(error) => {
                        let error = format!("{error:#}");
                        tool.error = Some(error.clone());
                        Response::NotStarted(error)
                    }
                }
            };
            tool.ms = started.elapsed().as_millis();
            crate::lock(&self.tools).push(tool);
            response
        }

        /// Runs `program` as the script's sandbox would have, with the environment the module
        /// gave it over what the sandbox gives that is not cargo's: `PATH`, the C compiler's
        /// variables, Nix's wrappers'.
        fn run(
            &self,
            program: &str,
            args: &[String],
            cwd: &Path,
            request: &Request,
        ) -> anyhow::Result<(PathBuf, std::process::Output)> {
            let text = |bytes: &[u8]| String::from_utf8_lossy(bytes).into_owned();
            let mut env: Vec<(String, String)> =
                self.plan.env().iter().filter(|it| !documented(&it.0)).cloned().collect();
            for (name, value) in &request.env {
                let name = text(name);
                env.retain(|it| it.0 != name);
                env.push((name, text(value)));
            }
            let path = env.iter().find(|it| it.0 == "PATH").map(|it| it.1.clone());
            let found = found(program, cwd, path.as_deref())
                .with_context(|| format!("`{program}` is not found"))?;
            std::fs::create_dir_all(&self.scratch)?;
            let stdin = match request.stdin.is_empty() {
                true => Stdio::null(),
                false => {
                    let file = self.scratch.join("stdin");
                    std::fs::write(&file, &request.stdin)?;
                    Stdio::from(std::fs::File::open(&file)?)
                }
            };
            let plan = script_sandbox::for_tool(&self.plan, env.clone(), cwd, &self.scratch);
            let args: Vec<OsString> = args.iter().map(OsString::from).collect();
            let tool = Script {
                real: &found,
                args: &args,
                cwd,
                env: &env,
                out_dir: &self.out_dir,
                scratch: &self.scratch,
            };
            let ran = script_sandbox::run(&tool, &plan, stdin)?;
            Ok((found, ran.output))
        }
    }

    /// The file `program` names, as `Command` finds it: a path from `cwd`, or a name on `path`.
    fn found(program: &str, cwd: &Path, path: Option<&str>) -> Option<PathBuf> {
        if program.contains('/') {
            let it = cwd.join(program);
            return it.is_file().then_some(it);
        }
        path?.split(':').map(|dir| Path::new(dir).join(program)).find(|it| it.is_file())
    }

    /// Sets aside what a run under `wasi-compare` did: what it printed and how it ended in
    /// [`ASIDE`] beside `out_dir`, and the files it wrote, moved from `out_dir` to its `out`,
    /// which leaves `out_dir` as it was for the native script.
    fn set_aside(ran: Result<Ran, String>, out_dir: &Path, scratch: &Path) -> anyhow::Result<()> {
        let aside = scratch.join(ASIDE);
        _ = std::fs::remove_dir_all(&aside);
        let out = aside.join("out");
        std::fs::create_dir_all(&out).with_context(|| format!("making {}", out.display()))?;
        let result = match &ran {
            Ok(ran) => {
                std::fs::write(aside.join("stdout"), &ran.stdout)?;
                std::fs::write(aside.join("stderr"), &ran.stderr)?;
                serde_json::json!({
                    "status": ran.status,
                    "trap": ran.trap,
                    "tools": ran.tools,
                })
            }
            Err(error) => serde_json::json!({ "error": error }),
        };
        std::fs::write(aside.join("result.json"), serde_json::to_vec_pretty(&result)?)?;
        for entry in std::fs::read_dir(out_dir)?.flatten() {
            std::fs::rename(entry.path(), out.join(entry.file_name()))
                .with_context(|| format!("setting aside {}", entry.path().display()))?;
        }
        Ok(())
    }

    /// The key of a run of `script`'s `module`, the Nix model's: what the run is given, before
    /// it runs. The module, by its bytes, which are the same on every host; the package's
    /// files; the environment the run and its commands are given, with what names the checkout
    /// and the target directory unplaced ([`Places`]); its arguments; and the programs it may
    /// run, each found as it would be: the toolchain's identity for `rustc`, and for the
    /// others the file `PATH` finds, by its Nix store path or its bytes. `None` when one of
    /// them can't be read.
    fn run_key(module: &Path, script: &Script<'_>, plan: &Plan, places: &Places) -> Option<[u8; 32]> {
        let mut key = keyed::Key::new(WASI_RUNS);
        key.field(&Sha256::digest(std::fs::read(module).ok()?));
        key.field(&tree_of(script.cwd)?);
        key.field(&places.unplace(script.cwd.as_os_str().as_encoded_bytes()));
        for arg in script.args {
            key.field(&places.unplace(arg.as_encoded_bytes()));
        }
        let mut env: Vec<&(String, String)> =
            plan.env().iter().filter(|it| !script::UNREAD.contains(&it.0.as_str())).collect();
        env.sort();
        for (name, value) in &env {
            key.field(name.as_bytes()).field(&places.unplace(value.as_bytes()));
        }
        let sysroot = script::sysroot_of(plan.env());
        key.field(sysroot.as_deref().map(keyed::toolchain).unwrap_or_default().as_bytes());
        let path = plan.env().iter().find(|it| it.0 == "PATH").map(|it| it.1.as_str());
        let var = |name: &str| plan.env().iter().find(|it| it.0 == name).map(|it| it.1.clone());
        let named = ["CC", "CXX", "AR", "PKG_CONFIG"].into_iter().filter_map(var);
        let tools = super::TOOLS_ALLOWED.iter().map(|it| (*it).to_owned()).chain(named);
        for tool in tools {
            key.field(tool.as_bytes());
            let Some(file) = found(&tool, script.cwd, path) else {
                key.field(b"-");
                continue;
            };
            let file = std::fs::canonicalize(&file).unwrap_or(file);
            let text = file.to_str()?;
            let identity = match text.strip_prefix("/nix/store/") {
                Some(rest) => rest.split('/').next().unwrap_or_default().to_owned(),
                None => format!("{text} {}", keyed::digest(text)),
            };
            key.field(identity.as_bytes());
        }
        Some(key.done())
    }

    /// The digest of the files of the package at `dir`, by their paths in it, but for its
    /// `target` directory and `.git`: none of a script's inputs.
    fn tree_of(dir: &Path) -> Option<[u8; 32]> {
        let mut files = Vec::new();
        let mut pending = vec![dir.to_owned()];
        while let Some(at) = pending.pop() {
            for entry in std::fs::read_dir(&at).ok()?.flatten() {
                let path = entry.path();
                let name = path.strip_prefix(dir).ok()?.to_str()?.to_owned();
                if name == "target" || entry.file_name() == ".git" {
                    continue;
                }
                let kind = entry.file_type().ok()?;
                if kind.is_dir() {
                    pending.push(path);
                } else if kind.is_symlink() {
                    let to = std::fs::read_link(&path).ok()?;
                    files.push((name, b'l', to.as_os_str().as_encoded_bytes().to_vec()));
                } else {
                    let executable = script::is_executable(&entry.metadata().ok()?);
                    let bytes = Sha256::digest(std::fs::read(&path).ok()?).to_vec();
                    files.push((name, if executable { b'x' } else { b'f' }, bytes));
                }
            }
        }
        files.sort();
        let mut key = keyed::Key::new("package files");
        for (name, kind, bytes) in files {
            key.field(name.as_bytes()).field(&[kind]).field(&bytes);
        }
        Some(key.done())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_toolchain_c_compilers_and_pkg_config_are_allowed_and_nothing_else() {
        let rustc = Some("/t/ra-jit-rustc/rustc");
        for it in [
            "/t/ra-jit-rustc/rustc",
            "rustc",
            "cc",
            "/usr/bin/cc",
            "c++",
            "clang-19",
            "x86_64-linux-gnu-gcc",
            "aarch64-unknown-linux-gnu-g++",
            "ar",
            "llvm-ar",
            "pkg-config",
            "x86_64-w64-mingw32-pkg-config",
        ] {
            assert!(allowed(it, rustc), "{it}");
        }
        for it in ["git", "sh", "/bin/sh", "make", "cmake", "python3", "nasm", "ld", "rustc-x"] {
            assert!(!allowed(it, rustc), "{it}");
        }
    }

    #[test]
    fn a_script_is_given_cargo_s_variables_alone() {
        for it in ["OUT_DIR", "TARGET", "CARGO_CFG_TARGET_OS", "CARGO_FEATURE_STD", "DEP_Z_INCLUDE"] {
            assert!(documented(it), "{it}");
        }
        for it in ["PATH", "HOME", "CC", "NIX_CFLAGS_COMPILE", "PKG_CONFIG_PATH", "RA_JIT_CACHE"] {
            assert!(!documented(it), "{it}");
        }
    }
}
