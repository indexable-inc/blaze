//! The standard library as a release of blaze ships it (docs/no-rustc.md, "The standard
//! library, shipped"). A build with nothing of rustc's starts from the standard library's unit
//! and the frozen maps of its crates, which take seconds to make on a machine of many cores
//! and most of a minute on a laptop. `blaze-engine std REMOTE TOOLCHAIN` makes them once, for the
//! toolchain and this build of blaze, and sends them to a remote as the project
//! `std-<target>-<build>`; the release workflow does that for each target and attaches what
//! it sent to the release. A build of a release fetches that project once a store, beside
//! the build, which does not wait for it: the build compiles what has not arrived, and what
//! has is found in the store. Only the standard library of build scripts' builds, which is
//! what it brings, is made once it is in. `BLAZE_STD_FROM` names another remote to fetch it from (`off` for
//! none), and `fetch-public = false` in `[workspace.metadata.blaze]` of the root manifest
//! turns the release's off.
//!
//! What is fetched is checked as everything a remote gives is (`remote.rs`): a pack is used
//! only when its bytes hash to the name its list gives it, and only by the build of blaze
//! and the toolchain that made it, which its keys say. Its list is not signed (`signed.rs`):
//! it is taken from the release the program came from, or from the remote the user named.
//! `BLAZE_REMOTE_TRUST` is for the lists of `BLAZE_REMOTE`.

use std::{
    path::Path,
    time::{Duration, Instant, SystemTime},
};

use anyhow::Context;

use crate::{cargo::own_script, remote};

/// Where the assets of the releases are.
const RELEASES: &str = "https://github.com/indexable-inc/blaze/releases/download";

/// How long after a fetch that failed the next one is tried.
const AGAIN_AFTER: Duration = Duration::from_secs(60 * 60);

/// The project the standard library for `target` is under on a remote, for the build of
/// blaze and the toolchain this process was told of.
fn project(target: &str) -> Option<String> {
    Some(format!("std-{target}-{}", blaze_store::build_name()?))
}

/// The key, in `[workspace.metadata.blaze]` of a workspace's root manifest, that turns off
/// fetching what is public (the standard library a release ships) when `false`.
pub(crate) const FETCH_PUBLIC: &str = "fetch-public";

/// Where the standard library is fetched from: `BLAZE_STD_FROM`, and without it the release
/// this build is, when it is one and the workspace at `root` does not say [`FETCH_PUBLIC`]
/// `= false`.
fn source(root: &Path) -> anyhow::Result<Option<String>> {
    match std::env::var("BLAZE_STD_FROM") {
        Ok(from) => Ok((!from.is_empty() && from != "off").then_some(from)),
        Err(_) if !fetch_public(root)? => Ok(None),
        Err(_) => {
            let release = option_env!("BLAZE_RELEASE").filter(|it| !it.is_empty());
            Ok(release.map(|tag| format!("{RELEASES}/{tag}")))
        }
    }
}

/// What [`FETCH_PUBLIC`] in the root manifest of the workspace `dir` is in says: `true` when
/// it has no manifest or does not say.
pub(crate) fn fetch_public(dir: &Path) -> anyhow::Result<bool> {
    let dir = if dir.is_file() { dir.parent().unwrap_or(dir) } else { dir };
    let root = crate::cargo::sandboxed::workspace_root(dir)?.unwrap_or_else(|| dir.to_owned());
    let manifest = root.join("Cargo.toml");
    let Ok(text) = std::fs::read_to_string(&manifest) else { return Ok(true) };
    let value: toml::Value =
        toml::from_str(&text).with_context(|| format!("reading {}", manifest.display()))?;
    let said = ["workspace", "metadata", "blaze", FETCH_PUBLIC]
        .iter()
        .try_fold(&value, |it, key| it.get(*key));
    match said {
        None => Ok(true),
        Some(said) => said.as_bool().with_context(|| {
            format!(
                "`{FETCH_PUBLIC}` in [workspace.metadata.blaze] of {} is not `true` or `false`",
                manifest.display()
            )
        }),
    }
}

/// Fetches the standard library for `target` into the store on a thread of its own, when
/// there is somewhere to fetch it from ([`source`] of the workspace at `root`) and this store
/// has not fetched it before. The build does not wait for it: it compiles what it needs from
/// the start, and once the fetch is in, the store reads it, so what the build asks for after
/// that is found rather than compiled. The standard library of the scripts' builds waits for it
/// ([`own_script::fetching`]), being what it brings. A fetch the build did not need is kept for
/// the next one.
pub(crate) fn fetch_ahead(target: &str, root: &Path) -> anyhow::Result<()> {
    let Some(from) = source(root)? else { return Ok(()) };
    let (Some(project), Some(store)) = (project(target), crate::cargo::daemon::store()) else {
        return Ok(());
    };
    let fetching = own_script::fetching();
    std::thread::spawn(move || {
        let _fetching = fetching;
        match fetch(&store, &from, &project) {
            // A store opened before the toolchain is known is none for good; one opened after
            // reads what was fetched when it opens.
            Ok(()) => {
                if blaze_store::toolchain().is_some()
                    && let Some(store) = blaze_store::Store::global()
                {
                    store.refresh();
                }
            }
            Err(e) => {
                let why = format!("{e:#}");
                let why = why.lines().next().unwrap_or_default();
                eprintln!("standard library: not fetched from {from}, built here: {why}");
            }
        }
    });
    Ok(())
}

/// Fetches `project` from `from` into `store` unless the store did, or tried within
/// [`AGAIN_AFTER`]. Builds that start together fetch it once: the others wait for it.
fn fetch(store: &Path, from: &str, project: &str) -> anyhow::Result<()> {
    std::fs::create_dir_all(store)?;
    let mut options = std::fs::OpenOptions::new();
    let lock = options.create(true).truncate(false).write(true);
    let lock = lock.open(store.join(format!("fetching-{project}")))?;
    lock.lock()?;
    let fetched = store.join(format!("fetched-{project}"));
    if let Ok(said) = std::fs::read_to_string(&fetched) {
        let modified = std::fs::metadata(&fetched).and_then(|it| it.modified());
        let since = modified.ok().and_then(|it| SystemTime::now().duration_since(it).ok());
        if said != "failed" || since.is_some_and(|it| it < AGAIN_AFTER) {
            return Ok(());
        }
    }
    let start = Instant::now();
    let done = remote::runtime()?.block_on(async {
        let remote = remote::Remote::open(from, store).await?.as_release();
        remote::pull(store, &remote, project).await
    });
    std::fs::write(&fetched, if done.is_ok() { "fetched" } else { "failed" })?;
    eprintln!("standard library: {} from {from} in {:.1?}", done?, start.elapsed());
    Ok(())
}

/// Builds a program that prints a line, whole, with `profile` and the toolchain at `sysroot`,
/// in `dir` and into `store`: a program keys the standard library's unit by the debug info
/// its profile asks for, which a build script's does not ask for, so the shipped store has
/// the unit of each profile a whole build takes.
fn program_unit(sysroot: &Path, dir: &Path, store: &Path, profile: &str) -> anyhow::Result<String> {
    std::fs::create_dir_all(dir.join("src"))?;
    let manifest =
        "[package]\nname = \"hello\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n";
    std::fs::write(dir.join("Cargo.toml"), manifest)?;
    std::fs::write(dir.join("src/main.rs"), "fn main() {\n    println!(\"hello\");\n}\n")?;
    let path = std::env::var_os("PATH").unwrap_or_default();
    let path = std::env::join_paths(
        std::iter::once(sysroot.join("bin")).chain(std::env::split_paths(&path)),
    )?;
    let mut blaze = std::process::Command::new(std::env::current_exe()?);
    blaze.arg(dir).arg("--own-target").args(["--profile", profile, "--crate", "src/main.rs"]);
    blaze.arg("--out").arg(dir.join(format!("{profile}.o")));
    blaze.arg("--exe").arg(dir.join(format!("hello-{profile}")));
    blaze.env("PATH", path).env("BLAZE_CACHE", store).env("BLAZE_STD_FROM", "off");
    blaze.env_remove("RUSTUP_TOOLCHAIN").env_remove("CARGO_TARGET_DIR");
    blaze.current_dir(dir).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null());
    let built = blaze.output().context("running blaze")?;
    let said = String::from_utf8_lossy(&built.stderr).into_owned();
    std::fs::write(dir.join(format!("blaze-{profile}.log")), &said)?;
    if !built.status.success() {
        let last = said.lines().rev().find(|it| !it.trim().is_empty()).unwrap_or("");
        anyhow::bail!("blaze did not build a `{profile}` program ({}): {last}", built.status);
    }
    Ok(said)
}

const USAGE: &str = "usage: blaze-engine std REMOTE TOOLCHAIN\n\n\
                     Builds the standard library of TOOLCHAIN (the directory `rustc --print \
                     sysroot` names) for a build with nothing of rustc's, and sends it to \
                     REMOTE for this build of blaze to fetch with BLAZE_STD_FROM=REMOTE. \
                     Nothing is built when REMOTE has it.";

/// `blaze-engine std REMOTE TOOLCHAIN`: builds the standard library for this build of blaze, the
/// toolchain and the target of this machine into a store of its own, and sends that to
/// `REMOTE` under the project a build fetches it as.
pub(crate) fn command(mut args: impl Iterator<Item = String>) -> anyhow::Result<()> {
    let mut words = args.by_ref().filter(|it| !it.starts_with('-'));
    let (Some(to), Some(sysroot)) = (words.next(), words.next()) else { anyhow::bail!(USAGE) };
    let sysroot = std::path::absolute(sysroot)?;
    let sysroot = sysroot.as_path();
    if !sysroot.join("lib/rustlib").is_dir() {
        anyhow::bail!("{} is not a toolchain: it has no lib/rustlib", sysroot.display());
    }
    crate::cargo::toolchain::library::fetch().map_err(anyhow::Error::msg)?;
    blaze_store::compiles_against(&crate::source_identity().join(" "));
    let target = hir_ty_codegen::Target::selected().map_err(anyhow::Error::msg)?;
    let project = project(target.triple()).context("this program cannot be read")?;
    let dir = std::env::temp_dir().join(format!("blaze-std-{}", std::process::id()));
    let store = dir.join("store");
    std::fs::create_dir_all(&dir)?;
    let runtime = remote::runtime()?;
    let remote = runtime.block_on(remote::Remote::open(&to, &dir))?.as_release();
    if runtime.block_on(remote::lists(&remote, &project))? {
        std::fs::remove_dir_all(&dir)?;
        println!("{project}: {to} has it already");
        return Ok(());
    }
    let start = Instant::now();
    let built = own_script::build_nothing(sysroot, &dir.join("build"), &store, false);
    let said = built.map_err(|e| anyhow::anyhow!("{e} (the log is in {})", dir.display()))?;
    let unit = said.lines().find(|it| it.starts_with("standard library:")).unwrap_or_default();
    eprintln!("{unit}; built in {:.1?}", start.elapsed());
    // Without them a program's build compiles its unit itself, as before: what a build
    // script's build takes is still sent.
    for profile in ["dev", "release"] {
        let start = Instant::now();
        match program_unit(sysroot, &dir.join("program"), &store, profile) {
            Ok(said) => {
                let unit = said.lines().find(|it| it.starts_with("standard library:"));
                let unit = unit.unwrap_or_default();
                eprintln!("{unit}, for a `{profile}` program; built in {:.1?}", start.elapsed());
            }
            Err(e) => eprintln!("standard library for a `{profile}` program: not built: {e:#}"),
        }
    }
    let start = Instant::now();
    let sent = runtime.block_on(remote::push(&store, &remote, &project))?;
    std::fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))?;
    println!("{project}: {sent} in {:.1?}", start.elapsed());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_workspace_turns_fetching_what_is_public_off_in_its_root_manifest() {
        let dir = std::env::temp_dir().join(format!("blaze-fetch-public-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("member/src")).unwrap();
        let write = |text: &str| std::fs::write(dir.join("Cargo.toml"), text).unwrap();
        std::fs::write(dir.join("member/Cargo.toml"), "[package]\nname = \"m\"\n").unwrap();
        write("[workspace]\nmembers = [\"member\"]\n");
        assert!(fetch_public(&dir.join("member")).unwrap());
        write("[workspace]\n[workspace.metadata.blaze]\nfetch-public = false\n");
        assert!(!fetch_public(&dir.join("member")).unwrap());
        assert!(!fetch_public(&dir.join("member/Cargo.toml")).unwrap());
        assert!(!fetch_public(&dir).unwrap());
        write("[workspace]\n[workspace.metadata.blaze]\nfetch-public = \"no\"\n");
        assert!(fetch_public(&dir).is_err());
        _ = std::fs::remove_dir_all(&dir);
    }
}
