//! `cargo-features` read as the pinned nightly's cargo reads them: a listed unstable feature is
//! activated and what it gates is accepted, an unlisted one is refused with nightly cargo's
//! message.

use std::path::{Path, PathBuf};

use blaze_manifest::metadata::root_manifest;
use blaze_manifest::profiles::Profiles;
use blaze_manifest::files::Disk;
use blaze_manifest::{Config, Context, Workspace};

/// A fresh directory holding `files`, each a path under it and its text.
fn tree(name: &str, files: &[(&str, &str)]) -> PathBuf {
    let root = std::env::temp_dir()
        .join(format!("blaze-manifest-cargo-features-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    for (path, text) in files {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    std::fs::create_dir_all(root.join("cargo-home")).unwrap();
    // As cargo is given it: not the `\\?\` form `canonicalize` gives on Windows, which a
    // path read back from a `file:` URL is not.
    dunce::canonicalize(root).unwrap()
}

fn context(root: &Path) -> Context<Disk> {
    let env = [("CARGO_HOME".to_owned(), root.join("cargo-home").display().to_string())];
    Context::new(Config::load(root, env, &[]).unwrap())
}

/// The workspace at `root`, or the error reading it, with its causes.
fn workspace<'c>(ctx: &'c Context<Disk>) -> anyhow::Result<Workspace<'c, Disk>> {
    let manifest = root_manifest(None, ctx)?;
    Workspace::new(&manifest, ctx)
}

const PROFILES: &str = r#"
[package]
name = "a"
version = "0.1.0"
edition = "2024"

[profile.release]
rustflags = ["-Ctarget-cpu=native"]

[profile.max]
inherits = "release"
rustflags = ["-Ctarget-cpu=native", "-Zshare-generics=y"]
trim-paths = "all"
"#;

#[test]
fn a_listed_unstable_feature_is_activated() {
    let manifest = format!("cargo-features = [\"profile-rustflags\"]\n{PROFILES}");
    let root = tree("listed", &[("Cargo.toml", &manifest), ("src/lib.rs", "")]);
    let ctx = context(&root);
    let ws = workspace(&ctx).unwrap();
    assert_eq!(ws.unstable_features().activated(), ["profile-rustflags"]);
    let host = "aarch64-apple-darwin";
    for (profile, flags) in [
        ("release", &["-Ctarget-cpu=native"][..]),
        ("max", &["-Ctarget-cpu=native", "-Zshare-generics=y"][..]),
    ] {
        let profiles =
            Profiles::new(ws.profiles(), ws.unstable_features(), &ctx.config, profile, host)
                .unwrap();
        assert_eq!(profiles.base_profile().rustflags, flags, "profile `{profile}`");
    }
}

#[test]
fn an_unlisted_unstable_feature_is_refused() {
    let root = tree("unlisted", &[("Cargo.toml", PROFILES), ("src/lib.rs", "")]);
    let ctx = context(&root);
    let err = format!("{:#}", workspace(&ctx).err().expect("profile-rustflags is not listed"));
    assert!(err.contains("feature `profile-rustflags` is required"), "{err}");
    assert!(
        err.contains("Consider adding `cargo-features = [\"profile-rustflags\"]` to the top of"),
        "{err}"
    );
}

#[test]
fn unknown_and_repeated_features_are_refused() {
    let cases = [
        ("cargo-features = [\"no-such-thing\"]", "unknown Cargo.toml feature `no-such-thing`"),
        ("cargo-features = [\"profile_rustflags\"]", "Feature names must use '-' instead of '_'."),
        ("cargo-features = [\"build-std\"]", "This feature can be enabled via -Zbuild-std"),
        (
            "cargo-features = [\"path-bases\", \"path-bases\"]",
            "the cargo feature `path-bases` has already been activated",
        ),
        ("cargo-features = [\"publish-lockfile\"]", "has been removed in the 1.37 release"),
    ];
    for (i, (features, expected)) in cases.into_iter().enumerate() {
        let manifest = format!(
            "{features}\n[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n"
        );
        let root = tree(&format!("bad-{i}"), &[("Cargo.toml", &manifest), ("src/lib.rs", "")]);
        let ctx = context(&root);
        let err = format!("{:#}", workspace(&ctx).err().expect(features));
        assert!(err.contains(expected), "{features}: {err}");
    }
}

#[test]
fn a_path_base_joins_the_workspace_root() {
    let root = tree(
        "path-bases",
        &[
            ("Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\nresolver = \"3\"\n"),
            (
                "crates/a/Cargo.toml",
                "cargo-features = [\"path-bases\"]\n[package]\nname = \"a\"\nversion = \"0.1.0\"\n\
                 edition = \"2024\"\n[dependencies]\nb = { base = \"workspace\", path = \"crates/b\" }\n",
            ),
            ("crates/a/src/lib.rs", ""),
            (
                "crates/b/Cargo.toml",
                "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
            ),
            ("crates/b/src/lib.rs", ""),
        ],
    );
    let ctx = context(&root);
    let ws = workspace(&ctx).unwrap();
    let json = serde_json::to_value(blaze_manifest::metadata::export_no_deps(&ws)).unwrap();
    let a = json["packages"].as_array().unwrap().iter().find(|p| p["name"] == "a").unwrap();
    let dep = &a["dependencies"][0];
    assert_eq!(dep["path"].as_str().unwrap(), root.join("crates").join("b").display().to_string());
}
