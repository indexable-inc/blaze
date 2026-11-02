use std::{ffi::OsString, path::Path};

use super::*;

/// `w/` with `Cargo.toml`, a member `w/crates/a/` with its own and `src/deep/`, and a
/// `w/.cargo/config.toml`.
fn workspace(git: bool) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let w = dir.path().join("w");
    std::fs::create_dir_all(w.join("crates/a/src/deep")).unwrap();
    std::fs::create_dir_all(w.join(".cargo")).unwrap();
    std::fs::write(w.join("Cargo.toml"), "[workspace]\n").unwrap();
    std::fs::write(w.join("crates/a/Cargo.toml"), "[package]\n").unwrap();
    std::fs::write(w.join(".cargo/config.toml"), "").unwrap();
    if git {
        std::fs::create_dir(w.join(".git")).unwrap();
    }
    (dir, w)
}

#[test]
fn the_manifest_is_the_nearest_cargo_toml_up_from_cwd() {
    let (_dir, w) = workspace(false);
    let deep = w.join("crates/a/src/deep");
    assert_eq!(manifest(&deep, None, false).unwrap(), w.join("crates/a/Cargo.toml"));
    assert_eq!(manifest(&w, None, false).unwrap(), w.join("Cargo.toml"));
}

#[test]
fn the_root_is_the_repository_or_the_highest_manifest() {
    let (_git, w) = workspace(true);
    let place = find(&w.join("crates/a/src/deep"), None, false).unwrap();
    assert_eq!(place.manifest, w.join("crates/a/Cargo.toml"));
    assert_eq!(place.root, w);

    let (_plain, w) = workspace(false);
    let place = find(&w.join("crates/a/src"), None, false).unwrap();
    assert_eq!(place.root, w);

    // A repository inside the workspace's directory is the checkout of what is in it.
    std::fs::write(w.join("crates/a/.git"), "gitdir: ../../.git/modules/a\n").unwrap();
    assert_eq!(find(&w.join("crates/a/src"), None, false).unwrap().root, w.join("crates/a"));
}

#[test]
fn a_manifest_path_is_from_cwd() {
    let (dir, w) = workspace(true);
    let place = find(dir.path(), Some("w/crates/a/../a/Cargo.toml"), false).unwrap();
    assert_eq!(place.manifest, w.join("crates/a/Cargo.toml"));
    assert_eq!(place.root, w);
    assert_eq!(place.cwd, dir.path());
}

#[test]
fn cargos_errors_before_a_manifest() {
    let (dir, w) = workspace(false);
    assert_eq!(
        manifest(dir.path(), Some("w/crates"), false).unwrap_err(),
        "the manifest-path must be a path to a Cargo.toml file"
    );
    assert_eq!(
        manifest(dir.path(), Some("w/b/Cargo.toml"), false).unwrap_err(),
        "manifest path `w/b/Cargo.toml` does not exist"
    );
    std::fs::create_dir(w.join("crates/a/src/Cargo.toml")).unwrap();
    assert_eq!(
        manifest(dir.path(), Some("w/crates/a/src/Cargo.toml"), false).unwrap_err(),
        "manifest path `w/crates/a/src/Cargo.toml` is a directory but expected a file"
    );
    std::fs::write(w.join("hello.rs"), "fn main() {}\n").unwrap();
    let script = w.join("hello.rs");
    assert_eq!(
        manifest(&w, Some("hello.rs"), false).unwrap_err(),
        format!("embedded manifest `{}` requires `-Zscript`", script.display())
    );
    assert_eq!(manifest(&w, Some("hello.rs"), true).unwrap(), script);

    let none = tempfile::tempdir().unwrap();
    assert_eq!(
        manifest(none.path(), None, false).unwrap_err(),
        format!("could not find `Cargo.toml` in `{}` or any parent directory", none.path().display())
    );
}

#[test]
fn z_script() {
    let args = |it: &[&str]| it.iter().map(|it| it.to_string()).collect::<Vec<_>>();
    assert!(script(&args(&["-Zscript"])));
    assert!(script(&args(&["--release", "-Z", "script"])));
    assert!(!script(&args(&["-Zbuild-std"])));
    assert!(!script(&args(&["--features", "script"])));
}

#[test]
fn paths_normalized_as_cargo_does() {
    assert_eq!(normalize_path(Path::new("/w/./a/../b/Cargo.toml")), Path::new("/w/b/Cargo.toml"));
    assert_eq!(normalize_path(Path::new("/w/a/..")), Path::new("/w"));
}

#[test]
fn config_files_outside_the_checkout() {
    let (dir, w) = workspace(true);
    std::fs::create_dir(dir.path().join(".cargo")).unwrap();
    std::fs::write(dir.path().join(".cargo/config.toml"), "[build]\njobs = 2\n").unwrap();
    let home = dir.path().join("home/.cargo");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("config.toml"), "[net]\noffline = true\n").unwrap();
    std::fs::write(home.join("credentials.toml"), "[registry]\ntoken = \"x\"\n").unwrap();
    std::fs::write(dir.path().join("extra.toml"), "[env]\n").unwrap();
    let place = find(&w.join("crates/a"), None, false).unwrap();
    let configs = ["../../../extra.toml".to_owned(), "build.jobs=4".to_owned(), "../../.cargo/config.toml".to_owned()];
    let files = config_files(&place, &configs, Some(&home)).unwrap();
    let named: Vec<&str> = files.iter().map(|(path, _)| path.as_str()).collect();
    let utf8 = |path: PathBuf| path.to_str().unwrap().to_owned();
    assert_eq!(named, [
        utf8(dir.path().join(".cargo/config.toml")),
        utf8(home.join("config.toml")),
        utf8(dir.path().join("extra.toml")),
    ]);
    assert_eq!(files[1].1, b"[net]\noffline = true\n");
}

#[test]
fn the_environment_sorted_and_never_shown() {
    let vars = [("B", "2"), ("A", "1")].map(|(name, value)| (OsString::from(name), OsString::from(value)));
    assert_eq!(env(vars.into_iter()).unwrap(), [("A".to_owned(), "1".to_owned()), ("B".to_owned(), "2".to_owned())]);
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt as _;
        let secret = OsString::from_vec(b"hunter\xff2".to_vec());
        let refused = env([(OsString::from("TOKEN"), secret)].into_iter()).unwrap_err();
        assert!(refused.contains("`TOKEN`"), "{refused}");
        assert!(!refused.contains("hunter"), "{refused}");
    }
}

#[test]
fn color_as_cargo_decides() {
    let vars = |set: &'static [(&'static str, &'static str)]| {
        move |name: &str| set.iter().find(|(it, _)| *it == name).map(|(_, value)| value.to_string())
    };
    let term = &[("TERM", "xterm-256color")];
    assert_eq!(color(Some("always"), vars(&[]), false), Ok(true));
    assert_eq!(color(Some("never"), vars(&[("CARGO_TERM_COLOR", "always")]), true), Ok(false));
    assert_eq!(color(None, vars(&[("CARGO_TERM_COLOR", "always")]), false), Ok(true));
    assert_eq!(color(None, vars(term), true), Ok(true));
    assert_eq!(color(None, vars(term), false), Ok(false));
    assert_eq!(color(Some("auto"), vars(&[("TERM", "xterm"), ("NO_COLOR", "1")]), true), Ok(false));
    assert_eq!(color(None, vars(&[("CLICOLOR_FORCE", "1")]), false), Ok(true));
    assert_eq!(color(None, vars(&[("TERM", "xterm"), ("CLICOLOR", "0")]), true), Ok(false));
    #[cfg(not(windows))]
    assert_eq!(color(None, vars(&[("TERM", "dumb")]), true), Ok(false));
    let refused = color(None, vars(&[("CARGO_TERM_COLOR", "sometimes")]), true).unwrap_err();
    assert_eq!(refused, "argument for --color must be auto, always, or never, but found `sometimes`");
    // clap took `--color` in any case; cargo's `ColorChoice` takes lowercase only.
    let refused = color(Some("ALWAYS"), vars(&[]), true).unwrap_err();
    assert_eq!(refused, "argument for --color must be auto, always, or never, but found `ALWAYS`");
}
