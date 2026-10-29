//! The names blaze had before it was renamed from ra-jit (#270), which it no longer reads:
//! tools/rename.py leaves this file as it is.

/// The prefix of the settings' names before the rename, now `BLAZE_`.
const OLD: &str = "RA_JIT_";

/// The program cargo ran as `cargo blaze`, which `blaze enable` linked `cargo fmt` and
/// `cargo clippy` to before `blaze` was the only command.
pub const OLD_LAUNCHER: &str = "cargo-blaze";

/// Ends this process when a setting is named as it was before the rename, with a line for
/// each saying what it is called now: a setting blaze does not read would change nothing
/// without a word.
pub fn refuse_old_settings() {
    let mut old: Vec<String> = std::env::vars_os()
        .filter_map(|(name, _)| name.to_str().filter(|it| it.starts_with(OLD)).map(str::to_owned))
        .collect();
    if old.is_empty() {
        return;
    }
    old.sort();
    for name in &old {
        eprintln!("{name} was renamed BLAZE_{}", &name[OLD.len()..]);
    }
    std::process::exit(1);
}

/// The directories blaze kept its daemon, its store and its answers in before the rename,
/// in their default places, as are there. They are not read: what was in them is made again
/// under the new name.
fn old_dirs() -> Vec<std::path::PathBuf> {
    use std::path::PathBuf;
    let var = |name| std::env::var_os(name).filter(|it| !it.is_empty()).map(PathBuf::from);
    let home = var("HOME").or_else(|| var("USERPROFILE"));
    let mut dirs: Vec<PathBuf> = [
        var("XDG_RUNTIME_DIR").map(|it| it.join("ra-jit")),
        var("XDG_CACHE_HOME").map(|it| it.join("ra-jit")),
        home.map(|it| it.join(".cache").join("ra-jit")),
    ]
    .into_iter()
    .flatten()
    .filter(|it| it.is_dir())
    .collect();
    dirs.dedup();
    dirs
}

/// How many bytes the files under `dir` hold, links not followed.
fn bytes_under(dir: &std::path::Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    entries
        .flatten()
        .map(|entry| match entry.file_type() {
            Ok(kind) if kind.is_dir() => bytes_under(&entry.path()),
            Ok(kind) if kind.is_file() => entry.metadata().map_or(0, |it| it.len()),
            _ => 0,
        })
        .sum()
}

/// On the first command under the new name, before the daemon's directory `home` is made:
/// a line for each directory of blaze from before the rename that is still there, with its
/// size, saying it can be deleted.
pub fn say_old_dirs(home: &std::path::Path) {
    if home.exists() {
        return;
    }
    let old = old_dirs();
    for dir in &old {
        let mib = bytes_under(dir) as f64 / 1048576.0;
        crate::cargo::say::note(&format!(
            "{} ({mib:.1} MiB) is blaze's from before it was renamed from ra-jit; nothing reads it now, and it can be deleted",
            dir.display()
        ));
    }
    // Said once: the next command finds `home`.
    if !old.is_empty() {
        let _ = std::fs::create_dir_all(home);
    }
}
