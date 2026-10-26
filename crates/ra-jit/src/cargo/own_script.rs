//! Build scripts compiled and linked by ra-jit (docs/no-rustc.md, step I). When cargo asks
//! rustc for a build script, the wrapper describes the script and the libraries it links, from
//! the arguments cargo gave and the records of those libraries, as a `rust-project.json`, and
//! ra-jit builds it as it builds any program with nothing of rustc's: every crate from source,
//! over the standard library's unit from the store, linked by `cc`. It writes the executable
//! and the dep-info where rustc would have, with the files `include!` and its kin read and
//! the variables `env!` and `option_env!` read, and cargo runs the script as it would rustc's.
//! A script that read a variable cargo sets for rustc alone is left to rustc. A proc macro the
//! script links is expanded with the library compiled of it: rustc's by rust-analyzer's
//! server, ra-jit's (`own_macro.rs`) by the server that reads its list of macros.

use std::{
    collections::BTreeMap,
    ffi::OsStr,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use serde_json::{Value, json};

use super::units::Record;

/// The variable that has rustc compile the build scripts, as the harness's reference builds do.
pub(crate) const BY_RUSTC: &str = "RA_JIT_SCRIPTS_BY_RUSTC";

/// The name of the script's crate in the description: the one ra-jit is asked to build.
const SCRIPT: &str = "build_script_build";

/// The variables cargo sets for rustc that a crate's source reads with `env!`: those of its
/// package and the `OUT_DIR` of its build script.
pub(crate) fn read_by_crates(name: &str) -> bool {
    name.starts_with("CARGO_PKG_")
        || name.starts_with("CARGO_MANIFEST_")
        || matches!(name, "CARGO_CRATE_NAME" | "CARGO_PRIMARY_PACKAGE" | "OUT_DIR" | "CARGO")
}

/// A crate of the description, as rustc was told to compile it.
struct Described {
    name: String,
    root: PathBuf,
    edition: String,
    cfg: Vec<String>,
    env: BTreeMap<String, String>,
    /// The name each library is linked under and where it is in the description.
    deps: Vec<(String, usize)>,
    /// The library compiled of a proc macro, which a proc-macro server expands with.
    proc_macro: Option<PathBuf>,
    /// Whether the crate is a proc macro the build compiles.
    compiled_macro: bool,
    /// The dep-info of the library rustc compiled of it, and the directory its paths are from.
    inputs: Option<(PathBuf, PathBuf)>,
}

/// What a build makes of the first crate of its description.
pub(crate) struct Made<'a> {
    /// The crate's name in the description, which ra-jit is asked to build.
    pub(crate) name: &'a str,
    /// Whether the crate is a proc macro, linked as a shared library with its list of macros.
    pub(crate) proc_macro: bool,
    pub(crate) exe: PathBuf,
    pub(crate) dep_info: PathBuf,
}

/// The arguments of one run of rustc, as cargo gave them for a crate.
struct Asked<'a> {
    args: &'a [String],
    cwd: &'a Path,
}

impl Asked<'_> {
    fn value(&self, flag: &str) -> Option<&str> {
        let joined = format!("{flag}=");
        let mut args = self.args.iter();
        while let Some(arg) = args.next() {
            if arg == flag {
                return args.next().map(String::as_str);
            }
            if let Some(value) = arg.strip_prefix(&joined) {
                return Some(value);
            }
        }
        None
    }

    fn values(&self, flag: &str) -> Vec<&str> {
        self.args.windows(2).filter(|it| it[0] == flag).map(|it| it[1].as_str()).collect()
    }

    /// `-C name=value`, however cargo spelled it.
    fn codegen(&self, name: &str) -> Option<&str> {
        let joined = self.args.iter().filter_map(|it| it.strip_prefix("-C"));
        let split = self.values("-C").into_iter();
        joined.chain(split).filter(|it| !it.is_empty()).find_map(|it| {
            let (key, value) = it.split_once('=').unwrap_or((it, ""));
            (key == name).then_some(value)
        })
    }

    /// The cfgs rustc compiles the crate with beside the target's: `--cfg`, and
    /// `debug_assertions`, which rustc sets unless it optimizes or is told not to.
    fn cfg(&self) -> Vec<String> {
        let mut cfg: Vec<String> = self.values("--cfg").into_iter().map(str::to_owned).collect();
        let optimized = self.codegen("opt-level").is_some_and(|it| it != "0");
        let asserts = match self.codegen("debug-assertions") {
            Some(it) => matches!(it, "" | "y" | "yes" | "on" | "true"),
            None => !optimized,
        };
        if asserts {
            cfg.push("debug_assertions".to_owned());
        }
        cfg
    }

    fn edition(&self) -> String {
        self.value("--edition").unwrap_or("2015").to_owned()
    }

    /// The libraries it links: the name each is linked under and its file.
    fn externs(&self) -> Vec<(String, PathBuf)> {
        let named = self.values("--extern").into_iter().filter_map(|it| it.split_once('='));
        named.map(|(name, path)| (name.to_owned(), self.cwd.join(path))).collect()
    }
}

/// Compiles and links the build script cargo asked rustc for with `args`, in `cwd`, into `exe`
/// with its dep-info beside it, as rustc would have, describing it in `dir`. `rustc` is the
/// compiler cargo named, whose toolchain has the standard library's source. `Err` is why rustc
/// is to compile it.
pub(crate) fn compile(
    rustc: &OsStr,
    args: &[String],
    cwd: &Path,
    source: &Path,
    exe: &Path,
    dir: &Path,
) -> Result<(), String> {
    let made = Made {
        name: SCRIPT,
        proc_macro: false,
        exe: exe.to_owned(),
        dep_info: exe.with_extension("d"),
    };
    compile_as(rustc, args, cwd, source, &made, dir)
}

/// [`compile`] of the crate cargo asked rustc for, made as `made` says.
pub(crate) fn compile_as(
    rustc: &OsStr,
    args: &[String],
    cwd: &Path,
    source: &Path,
    made: &Made<'_>,
    dir: &Path,
) -> Result<(), String> {
    let rustc = crate::answers::toolchain_rustc_of(rustc).ok_or("its toolchain is not known")?;
    let sysroot = rustc.parent().and_then(Path::parent).ok_or("its toolchain is not known")?;
    let env: BTreeMap<String, String> =
        std::env::vars().filter(|(name, _)| read_by_crates(name)).collect();
    let asked = Asked { args, cwd };
    compile_in(sysroot, &asked, source, env, made, dir, &|name| std::env::var(name).ok())
}

/// [`compile_as`] of the crate rustc compiled as `record` says, over the standard library of
/// `sysroot`, with the variables cargo set for it then.
pub(crate) fn compile_record(
    sysroot: &Path,
    record: &Record,
    made: &Made<'_>,
    dir: &Path,
) -> Result<(), String> {
    let asked = Asked { args: &record.args, cwd: &record.cwd };
    let env = record.crate_env.clone();
    let rustc_had = |name: &str| record.crate_env.get(name).cloned().or(std::env::var(name).ok());
    compile_in(sysroot, &asked, &record.source, env, made, dir, &rustc_had)
}

/// [`compile_as`] of the crate rustc was `asked` for, the variables its source reads by `env!`
/// being `env` and the others what `rustc_had` says.
fn compile_in(
    sysroot: &Path,
    asked: &Asked<'_>,
    source: &Path,
    env: BTreeMap<String, String>,
    made: &Made<'_>,
    dir: &Path,
    rustc_had: &dyn Fn(&str) -> Option<String>,
) -> Result<(), String> {
    if let Some(why) = crate::unstable::refusal(asked.args) {
        return Err(format!("it is built with {why}"));
    }
    let library = sysroot.join("lib/rustlib/src/rust/library");
    if !library.is_dir() {
        return Err("the toolchain has no source of the standard library".to_owned());
    }
    let mut crates = Vec::new();
    let first = Described {
        name: made.name.to_owned(),
        root: source.to_owned(),
        edition: asked.edition(),
        cfg: asked.cfg(),
        env,
        deps: Vec::new(),
        proc_macro: None,
        compiled_macro: made.proc_macro,
        inputs: None,
    };
    crates.push(first);
    let mut linked = BTreeMap::new();
    let deps = asked.externs();
    let deps =
        deps.iter().map(|(name, path)| Ok((name.clone(), add(&mut crates, &mut linked, path)?)));
    crates[0].deps = deps.collect::<Result<_, String>>()?;
    let described = description(sysroot, &crates);
    // A proc macro's library is kept in the store, by its description.
    let key = made.proc_macro.then(|| super::own_macro::library_key(&described));
    if !key.is_some_and(|key| super::own_macro::restored(&key, made)) {
        let said = build(&described, dir, made, super::daemon::store(), true)?;
        if let Some(line) =
            said.lines().find(|it| it.contains(" functions in it ra-jit could not compile"))
            && !line.ends_with(", 0 functions in it ra-jit could not compile")
        {
            return Err(format!("it calls functions ra-jit could not compile ({line})"));
        }
        if let Some(key) = key {
            let inputs = crates.iter().filter_map(|it| it.inputs.as_ref());
            super::own_macro::keep(&key, made, inputs.map(|(info, cwd)| (&**info, &**cwd)));
        }
    }
    let read = std::fs::read_to_string(&made.dep_info).map_err(|it| it.to_string())?;
    read_otherwise(&crate::dep_info::DepInfo::parse(&read), rustc_had)
}

/// The `rust-project.json` of `crates` over the standard library of `sysroot`.
fn description(sysroot: &Path, crates: &[Described]) -> String {
    json!({
        "sysroot": sysroot,
        "sysroot_src": sysroot.join("lib/rustlib/src/rust/library"),
        "crates": crates.iter().enumerate().map(|(at, it)| described(it, at == 0)).collect::<Vec<_>>(),
    })
    .to_string()
}

/// Builds the first crate of `described`, a [`description`], as `made` says, describing them
/// in `dir`, with `store` as the store: what the build said, or why it did not build. With
/// `shipped` it fetches the standard library as builds do (`shipped.rs`).
fn build(
    described: &str,
    dir: &Path,
    made: &Made<'_>,
    store: Option<PathBuf>,
    shipped: bool,
) -> Result<String, String> {
    std::fs::create_dir_all(dir).map_err(|it| it.to_string())?;
    let project = dir.join("rust-project.json");
    std::fs::write(&project, described).map_err(|it| it.to_string())?;
    let mut ra_jit = Command::new(std::env::current_exe().map_err(|it| it.to_string())?);
    ra_jit.arg(&project).args(["--crate", made.name]).arg("--out").arg(dir.join("built.o"));
    ra_jit.arg("--exe").arg(&made.exe).arg("--dep-info").arg(&made.dep_info);
    if made.proc_macro {
        ra_jit.args(["--crate", "proc_macro", "--shared"]);
    }
    // What cargo set for the script's rustc is not the build's: the build ra-jit runs is its
    // own, which asks cargo nothing through a wrapper and is no doctest.
    for (name, _) in std::env::vars_os() {
        let text = name.to_string_lossy();
        let cargos = text.starts_with("CARGO_") && text != "CARGO_HOME";
        if cargos || text.starts_with("RUSTC_") || text.starts_with("RA_JIT_") || text == "CARGO" {
            ra_jit.env_remove(&name);
        }
    }
    for kept in ["RA_JIT_HOME", "RA_JIT_BACKEND", "RA_JIT_STD_FROM", super::own_macro::SERVER] {
        if let Some(value) = std::env::var_os(kept) {
            ra_jit.env(kept, value);
        }
    }
    if let Some(store) = store {
        ra_jit.env("RA_JIT_CACHE", store);
    }
    if !shipped {
        ra_jit.env("RA_JIT_STD_FROM", "off");
    }
    ra_jit.current_dir(dir).stdin(Stdio::null()).stdout(Stdio::null());
    let built = ra_jit.output().map_err(|it| format!("ra-jit did not run: {it}"))?;
    let said = String::from_utf8_lossy(&built.stderr).into_owned();
    std::fs::write(dir.join("ra-jit.log"), &said).map_err(|it| it.to_string())?;
    if !built.status.success() {
        let last = said.lines().rev().find(|it| !it.trim().is_empty()).unwrap_or("");
        return Err(format!("ra-jit did not build it ({}): {last}", built.status));
    }
    Ok(said)
}

/// Builds a script that does nothing over the standard library of `sysroot` in `dir`, into
/// the store `store`: what the store then has is what any build script's build takes from
/// it before its own crates, the standard library's unit and the frozen maps of its crates.
pub(crate) fn build_nothing(sysroot: &Path, dir: &Path, store: &Path) -> Result<String, String> {
    let source = dir.join("build.rs");
    std::fs::create_dir_all(dir).map_err(|it| it.to_string())?;
    std::fs::write(&source, "fn main() {}\n").map_err(|it| it.to_string())?;
    let script = Described {
        name: SCRIPT.to_owned(),
        root: source,
        edition: "2021".to_owned(),
        cfg: vec!["debug_assertions".to_owned()],
        env: BTreeMap::new(),
        deps: Vec::new(),
        proc_macro: None,
        compiled_macro: false,
        inputs: None,
    };
    let exe = dir.join("build-script-build");
    let made = Made { name: SCRIPT, proc_macro: false, dep_info: exe.with_extension("d"), exe };
    let described = description(sysroot, &[script]);
    build(&described, dir, &made, Some(store.to_owned()), false)
}

/// Why rustc is to compile the script when it read a variable as another than rustc would,
/// `rustc_has` saying what rustc would: one cargo sets for rustc alone, which ra-jit's build
/// is not given (`RUSTC_*` and the like).
fn read_otherwise(
    info: &crate::dep_info::DepInfo,
    rustc_has: impl Fn(&str) -> Option<String>,
) -> Result<(), String> {
    match info.env.iter().find(|(name, value)| rustc_has(name) != *value) {
        Some((name, _)) => Err(format!("it reads `{name}`, which ra-jit's build is not given")),
        None => Ok(()),
    }
}

/// Adds the library rustc wrote at `path`, and what it links, to `crates` from their records:
/// where it is in them. `linked` has where each file already is.
fn add(
    crates: &mut Vec<Described>,
    linked: &mut BTreeMap<PathBuf, usize>,
    path: &Path,
) -> Result<usize, String> {
    // A library is named by its metadata to a crate rustc pipelines and by itself to the others.
    let path = &match path.extension().is_some_and(|it| it == "rmeta") {
        true => path.with_extension("rlib"),
        false => path.to_owned(),
    };
    if let Some(&at) = linked.get(path) {
        return Ok(at);
    }
    let name = path.file_name().and_then(|it| it.to_str()).unwrap_or_default();
    let library = name
        .strip_prefix("lib")
        .and_then(|it| it.strip_suffix(".rlib").or(it.strip_suffix(".rmeta")));
    let (prefix, suffix) = (std::env::consts::DLL_PREFIX, std::env::consts::DLL_SUFFIX);
    let proc_macro = name.strip_prefix(prefix).and_then(|it| it.strip_suffix(suffix));
    let Some(stem) = library.or(proc_macro) else {
        return Err(format!("it links {name}, which is not a library ra-jit knows"));
    };
    let parent = path.parent().ok_or("a library without a directory")?;
    let unit = &super::layout::unit_of_file(stem, parent);
    let dir = Record::dir_for(parent);
    let record = Record::described(&dir, unit).ok_or_else(|| {
        format!("`{unit}`, which it links, was compiled before ra-jit described it")
    })?;
    let asked = Asked { args: &record.args, cwd: &record.cwd };
    let at = crates.len();
    linked.insert(path.to_owned(), at);
    let extra = record.codegen("extra-filename").unwrap_or_default();
    let info = path.with_file_name(format!("{}{extra}.d", record.crate_name));
    crates.push(Described {
        name: record.crate_name.clone(),
        root: record.source.clone(),
        edition: asked.edition(),
        cfg: asked.cfg(),
        env: record.crate_env.clone(),
        deps: Vec::new(),
        proc_macro: library.is_none().then(|| path.to_owned()),
        compiled_macro: false,
        inputs: Some((info, record.cwd.clone())),
    });
    // A proc macro's libraries are described too, though the library of it is what
    // expands: what the store keeps of the script is keyed by them.
    let mut deps = Vec::new();
    for (name, path) in asked.externs() {
        deps.push((name, add(crates, linked, &path)?));
    }
    crates[at].deps = deps;
    Ok(at)
}

fn described(krate: &Described, member: bool) -> Value {
    json!({
        "display_name": krate.name,
        "root_module": krate.root,
        "edition": krate.edition,
        "deps": krate.deps.iter().map(|(name, at)| json!({ "crate": at, "name": name })).collect::<Vec<_>>(),
        "cfg": krate.cfg,
        "env": krate.env,
        "is_workspace_member": member,
        "is_proc_macro": krate.proc_macro.is_some() || krate.compiled_macro,
        "proc_macro_dylib_path": krate.proc_macro,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_variable_read_as_rustc_would_read_it() {
        let info = crate::dep_info::DepInfo {
            files: Vec::new(),
            env: vec![("A".to_owned(), Some("1".to_owned())), ("B".to_owned(), None)],
        };
        let rustc_has = |a: Option<&'static str>| {
            move |name: &str| if name == "A" { a.map(str::to_owned) } else { None }
        };
        assert_eq!(read_otherwise(&info, rustc_has(Some("1"))), Ok(()));
        assert!(read_otherwise(&info, rustc_has(Some("2"))).is_err());
        assert!(read_otherwise(&info, rustc_has(None)).is_err());
    }

    #[test]
    fn debug_assertions_unless_optimized() {
        let cwd = Path::new("/");
        let args = |it: &[&str]| it.iter().map(|it| (*it).to_owned()).collect::<Vec<_>>();
        let plain = args(&["--cfg", "feature=\"std\""]);
        assert_eq!(Asked { args: &plain, cwd }.cfg(), ["feature=\"std\"", "debug_assertions"]);
        let optimized = args(&["-C", "opt-level=3"]);
        assert!(Asked { args: &optimized, cwd }.cfg().is_empty());
        let told = args(&["-C", "opt-level=3", "-Cdebug-assertions=on"]);
        assert_eq!(Asked { args: &told, cwd }.cfg(), ["debug_assertions"]);
    }
}
