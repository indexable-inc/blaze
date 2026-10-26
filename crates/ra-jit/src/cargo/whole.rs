//! The cold build by blaze (docs/no-rustc.md, step K, #91): with `RA_JIT_WHOLE_BUILD=1`,
//! `cargo blaze build` and `run` from nothing build each program of the build whole, every
//! crate from source over the shipped standard library and linked by blaze (step G), and put
//! it where cargo would. No `cargo build` runs, and what blaze cannot compile that the program
//! reaches fails the build, with what it is.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Command,
    time::Instant,
};

use anyhow::Context as _;
use cargo_metadata::{Metadata, Package};

use crate::plan::{Plan, Selection};

use super::{
    FAILED,
    cli::{Invocation, Subcommand, worker_env},
    client::{self, Start},
    daemon::{Checkout, Reply, Request, WorkerConfig},
    plan::Plan as Linked,
    run, say,
    state::{
        as_built, configured_env, library_path, place, repository, run_env, sysroot_libraries,
    },
};

/// The setting that has a build from nothing be blaze's whole build.
const VARIABLE: &str = "RA_JIT_WHOLE_BUILD";

/// How many of the functions blaze could not compile a failed build names.
const SHOWN: usize = 8;

/// Whether the build `asked` for, from nothing, is blaze's whole build: `build` or `run`, of
/// the programs a build picks by default or one named with `--bin` or `--example`, for one
/// target at most.
pub(super) fn asked_for(asked: &Invocation) -> bool {
    let on = std::env::var_os(VARIABLE).is_some_and(|it| !it.is_empty() && it != "0");
    let flags = &asked.flags;
    let named = flags.bin.is_some() || flags.example.is_some();
    on && matches!(asked.subcommand, Subcommand::Build | Subcommand::Run)
        && (!flags.selects_targets || named)
        && !flags.cargos_plan
        && flags.targets.len() <= 1
}

/// A program of the build, as cargo plans it.
struct Program {
    /// The name of the executable.
    name: String,
    /// The root file of its crate.
    root: PathBuf,
    package: Package,
    /// Where cargo puts it.
    executable: PathBuf,
}

/// Builds the programs of `asked` whole and runs the one `run` asks for. `None` when the build
/// has no program to link, which is then built as without the setting.
pub(super) fn build(asked: &Invocation) -> anyhow::Result<Option<i32>> {
    let started = Instant::now();
    let cwd = std::env::current_dir()?;
    let metadata = super::state::metadata(asked);
    let programs = planned(asked, &cwd, &metadata)?;
    if programs.is_empty() {
        return Ok(None);
    }
    let dir = metadata.target_directory.as_std_path().join("ra-jit/whole");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    for program in &programs {
        let package = &program.package;
        let at = super::state::package_dir(package);
        say::status(
            asked.flags.quiet,
            "Compiling",
            &format!("{} v{} ({})", package.name, package.version, at.display()),
        );
    }
    if let Err(why) = whole(asked, &cwd, &metadata, &programs, &dir) {
        say::error(&why);
        return Ok(Some(FAILED));
    }
    let took = started.elapsed().as_secs_f64();
    let profile = asked.profile();
    let finished = format!("`{profile}` profile [ra-jit] target(s) in {took:.2}s");
    say::status(asked.flags.quiet, "Finished", &finished);
    if asked.subcommand != Subcommand::Run {
        return Ok(Some(0));
    }
    ran(asked, &cwd, &programs).map(Some)
}

/// Has the daemon build `programs` whole into `dir`, and puts each where cargo would. A
/// worker of the daemon loads the workspace and links each program before it answers, and
/// builds them again on what it has loaded when asked next. `Err` says why not.
fn whole(
    asked: &Invocation,
    cwd: &Path,
    metadata: &Metadata,
    programs: &[Program],
    dir: &Path,
) -> Result<(), String> {
    let checkout = checkout(asked, cwd, metadata, programs, dir);
    let reply = client::ask(Request::Build(super::asking(&checkout)), Start::IfMissing)
        .map_err(|it| format!("{it:#}"))?;
    let built = match reply {
        Some(Reply::Synced { programs, .. }) => programs,
        Some(Reply::Cold { reason }) => return Err(not_loaded(cwd, &checkout, &reason)),
        None => return Err(client::why_none()),
        Some(other) => return Err(format!("the daemon answered {other:?}")),
    };
    for ((program, plan), built) in std::iter::zip(programs, &checkout.programs).zip(built) {
        if let Err(why) = built {
            return Err(format!("could not build `{}`: {}", program.name, why.why()));
        }
        let _unwritten = crate::splice::unwritten(&plan.binary);
        let stand_ins = crate::link::stand_ins(&plan.binary).map_err(|it| format!("{it:#}"))?;
        if !stand_ins.is_empty() {
            let mut reached: Vec<String> = stand_ins
                .iter()
                .take(SHOWN)
                .map(|it| it.trim().trim_start_matches(hir_ty_codegen::STAND_IN_SAYS))
                .map(without_hashes)
                .collect();
            if stand_ins.len() > SHOWN {
                reached.push(format!("and {} more", stand_ins.len() - SHOWN));
            }
            return Err(format!(
                "`{}` can reach functions blaze could not compile, so it is not built:\n  {}",
                program.name,
                reached.join("\n  ")
            ));
        }
        if let Some(parent) = program.executable.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|it| format!("creating {}: {it}", parent.display()))?;
        }
        place(&plan.binary, &program.executable)
            .map_err(|it| format!("installing {}: {it}", program.executable.display()))?;
    }
    Ok(())
}

/// The daemon's checkout of a whole build of `programs`, each linked into `dir`.
fn checkout(
    asked: &Invocation,
    cwd: &Path,
    metadata: &Metadata,
    programs: &[Program],
    dir: &Path,
) -> Checkout {
    let root = metadata.workspace_root.as_std_path();
    let asked_in = asked.flags.manifest_path.as_ref().map_or(cwd.to_owned(), |it| cwd.join(it));
    let project = programs.first().map(|it| super::state::package_dir(&it.package));
    let project = project.as_deref().unwrap_or(root);
    let config = WorkerConfig {
        repository: repository(root),
        project: project.strip_prefix(root).unwrap_or(Path::new("")).to_owned(),
        cfg_test: false,
        selection: Selection { asked_in: Some(asked_in), ..asked.selection() },
        env: worker_env(),
        whole: true,
    };
    let plans = programs.iter().map(|program| Linked {
        crates: vec![program.root.clone()],
        object: dir.join(format!("{}.o", program.name)),
        patch: dir.join(format!("{}.patch.o", program.name)),
        binary: dir.join(&program.name),
        rustc_binary: None,
        link_cmd: String::new(),
        symbol_files: Vec::new(),
        unit_test: false,
        linked_as_built: Vec::new(),
        copies: Vec::new(),
        package: None,
        stamp: 0,
        whole: true,
    });
    let members = metadata.workspace_packages();
    let manifests = members.iter().map(|it| it.manifest_path.as_std_path().to_owned());
    let lock = root.join("Cargo.lock");
    // A worker that loaded other manifests loads them again; one with the same is asked to
    // build again what was edited since.
    let generation = manifests.clone().chain([lock]).filter_map(|it| written_at(&it)).max();
    let sources = members.iter().flat_map(|it| sources_in(&super::state::package_dir(it)));
    Checkout {
        key: format!("{}-whole", asked.key(cwd)),
        generation: generation.unwrap_or(0),
        built_at: 0,
        config,
        root: root.to_owned(),
        programs: plans.collect(),
        sources: manifests.chain(sources).collect(),
        env: BTreeMap::new(),
        scripts: None,
    }
}

/// When `file` was last written, as a stamp.
fn written_at(file: &Path) -> Option<u128> {
    let written = std::fs::metadata(file).and_then(|it| it.modified()).ok()?;
    Some(written.duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos())
}

/// The Rust files of the package in `dir`, but not those of a target directory or of a
/// package within it.
fn sources_in(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut left = vec![dir.to_owned()];
    while let Some(dir) = left.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_dir() {
                let hidden = name.to_string_lossy().starts_with('.');
                if !hidden && name != "target" && !path.join("Cargo.toml").exists() {
                    left.push(path);
                }
            } else if path.extension().is_some_and(|it| it == "rs") {
                found.push(path);
            }
        }
    }
    found
}

/// Why the daemon did not load the workspace, from what its worker said: the functions blaze
/// could not compile, or its last error.
fn not_loaded(cwd: &Path, checkout: &Checkout, reason: &str) -> String {
    let named = checkout.programs.iter().map(|it| it.binary.file_name().unwrap_or_default());
    let named: Vec<String> = named.map(|it| format!("`{}`", it.to_string_lossy())).collect();
    let named = named.join(", ");
    let Some(log) = worker_log(&checkout.root.join(&checkout.config.project)) else {
        return format!("could not build {named}: {reason}");
    };
    let said = std::fs::read_to_string(&log).unwrap_or_default();
    let shown = log.strip_prefix(cwd).unwrap_or(&log).display().to_string();
    // A function of the program's own crates blaze cannot compile is left out of the object,
    // and the link then fails on what it would have defined.
    let unsupported: Vec<String> = said
        .lines()
        .filter_map(|it| it.trim().strip_prefix("left to rustc: "))
        .map(without_hashes)
        .collect();
    if !unsupported.is_empty() {
        return format!(
            "{named} has functions blaze could not compile, so it is not built:\n  {}\n(what blaze said is in {shown})",
            unsupported.join("\n  ")
        );
    }
    let last = said.lines().rev().find(|it| it.starts_with("Error"));
    let last = last.map_or(reason, |it| it.strip_prefix("Error: ").unwrap_or(it));
    format!("could not build {named}: {last} (what blaze said is in {shown})")
}

/// The log of the daemon's worker for the package in `project`.
fn worker_log(project: &Path) -> Option<PathBuf> {
    let Ok(Some(Reply::Status(status))) = client::ask(Request::Status, Start::Never) else {
        return None;
    };
    status.workers.into_iter().find(|it| it.project == project).map(|it| it.log)
}

/// `name` with the `[0123456789abcdef]` after each crate's name, and the `ra-jit.<hash>.` of
/// a symbol, taken out.
fn without_hashes(name: &str) -> String {
    let is_hash = |it: &str| it.len() == 16 && it.bytes().all(|it| it.is_ascii_hexdigit());
    let mut name = name.to_owned();
    while let Some(at) = name.find("ra-jit.")
        && let Some(hash) = name.get(at + 7..at + 23)
        && is_hash(hash)
        && name[at + 23..].starts_with('.')
    {
        name.replace_range(at..at + 24, "");
    }
    let mut out = String::with_capacity(name.len());
    let mut rest = name.as_str();
    while let Some(at) = rest.find('[') {
        let hash = rest[at + 1..].split_once(']').map(|(it, _)| it);
        out.push_str(&rest[..at]);
        match hash {
            Some(hash) if is_hash(hash) => {
                rest = &rest[at + hash.len() + 2..];
            }
            _ => {
                out.push('[');
                rest = &rest[at + 1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Runs the program `asked` names, as `cargo run` does.
fn ran(asked: &Invocation, cwd: &Path, programs: &[Program]) -> anyhow::Result<i32> {
    let named = asked.flags.bin.as_ref().or(asked.flags.example.as_ref());
    let mut chosen: Vec<&Program> =
        programs.iter().filter(|it| named.is_none_or(|name| it.name == *name)).collect();
    if chosen.len() > 1 {
        let default = |it: &&Program| it.package.default_run.as_deref() == Some(it.name.as_str());
        chosen.retain(default);
    }
    let [program] = chosen[..] else {
        let names: Vec<&str> = programs.iter().map(|it| it.name.as_str()).collect();
        say::error(&format!(
            "`cargo run` could not determine which binary to run. Use the `--bin` option to specify a binary, \
             or the `default-run` manifest key.\navailable binaries: {}",
            names.join(", ")
        ));
        return Ok(FAILED);
    };
    let shown =
        program.executable.strip_prefix(cwd).unwrap_or(&program.executable).display().to_string();
    let args = asked.program_args().map(|it| format!(" {it}")).collect::<String>();
    say::status(asked.flags.quiet, "Running", &format!("`{shown}{args}`"));
    let mut command = Command::new(&program.executable);
    for it in configured_env(cwd) {
        if it.force || std::env::var_os(&it.name).is_none() {
            command.env(&it.name, &it.value);
        }
    }
    command.args(asked.program_args()).envs(run_env(&program.package, &[]));
    command.envs(library_path(&program.executable, sysroot_libraries().as_deref()));
    as_built(&mut command)?;
    let status = command.status().with_context(|| format!("could not execute {shown}"))?;
    Ok(run::exit_code(status))
}

/// The programs `asked` builds, each executable root of the build, as blaze plans it with its
/// own resolution, or as cargo's unit graph has them where blaze cannot.
fn planned(asked: &Invocation, cwd: &Path, metadata: &Metadata) -> anyhow::Result<Vec<Program>> {
    let manifest = asked.flags.manifest_path.as_ref().map(|it| cwd.join(it));
    let dir = manifest.as_deref().and_then(Path::parent).unwrap_or(cwd);
    let mut targets = Vec::new();
    if let Some(bin) = &asked.flags.bin {
        targets.extend(["--bin".to_owned(), bin.clone()]);
    }
    if let Some(example) = &asked.flags.example {
        targets.extend(["--example".to_owned(), example.clone()]);
    }
    let selection = asked.selection();
    let named = blaze_metadata::Targets {
        bin: asked.flags.bin.as_deref(),
        example: asked.flags.example.as_deref(),
    };
    let own = super::metadata::programs(&selection, dir, manifest.as_deref(), named);
    let planned = match own {
        Some(Ok(planned)) => planned
            .into_iter()
            .map(|it| crate::plan::Program {
                package: it.package,
                name: it.name,
                root: it.root,
                example: it.example,
                platform: selection.target.clone(),
            })
            .collect(),
        Some(Err(why)) => {
            eprintln!("blaze could not plan the build itself, so cargo does: {why:#}");
            Plan::programs(dir, manifest.as_deref(), &selection, &targets)?
        }
        None => Plan::programs(dir, manifest.as_deref(), &selection, &targets)?,
    };
    // cargo's directory of a profile: `dev`'s is `debug`.
    let profile = match selection.profile.as_str() {
        "dev" => "debug",
        other => other,
    };
    let target = metadata.target_directory.as_std_path();
    let mut programs = Vec::new();
    for program in planned {
        let Some(package) = metadata.packages.iter().find(|it| it.id.repr == program.package)
        else {
            anyhow::bail!(
                "the plan of the build names {}, which the workspace does not",
                program.package
            );
        };
        let mut dir = target.to_owned();
        if let Some(triple) = &program.platform {
            dir.push(triple);
        }
        dir.push(profile);
        if program.example {
            dir.push("examples");
        }
        let name = format!("{}{}", program.name, std::env::consts::EXE_SUFFIX);
        programs.push(Program {
            executable: dir.join(&name),
            name: program.name,
            root: program.root,
            package: package.clone(),
        });
    }
    Ok(programs)
}
