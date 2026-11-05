//! `BLAZE_VERSION` and `BLAZE_VERSION_LINE`: which blaze this is (see [`version`]);
//! `BLAZE_CODEGEN_CODE`, the code identity of its code generator, which stored templates are
//! keyed by; `BLAZE_FRONT_END_CODE`, that of its front end, which images, stored goals and
//! bodies and kept expansions are keyed by; and `BLAZE_RECORD_CODE`, that of what makes a unit's
//! record, which records and the standard library's shared directory are keyed by with the other
//! two. All three are worked out in one walk of the workspace (`blaze_codegen_code::emit_all`).

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    seed();
    version();
    blaze_codegen_code::emit_all(&[
        &blaze_codegen_code::CODEGEN,
        &blaze_codegen_code::FRONT_END,
        &blaze_codegen_code::RECORD,
    ]);
}

/// The `seed` feature is the seed profile, `--no-default-features --features seed`
/// (docs/future/bootstrap.md): no C, C++ or LLVM. With a feature that brings them back (`default`
/// among them, which is on unless `--no-default-features` is given) it is refused, naming both,
/// never built as a seed that isn't one.
fn seed() {
    let on = |feature: &str| {
        std::env::var_os(format!("CARGO_FEATURE_{}", feature.to_uppercase().replace('-', "_")))
            .is_some()
    };
    if !on("seed") {
        return;
    }
    let not_in_seed = ["default", "native"];
    let also: Vec<&str> = not_in_seed.into_iter().filter(|it| on(it)).collect();
    if !also.is_empty() {
        panic!(
            "blaze-engine's `seed` feature is the seed profile, which has no C, C++ or LLVM, and \
             `{}` {} on too: build the seed with `--no-default-features --features seed` and no \
             other feature",
            also.join("`, `"),
            if also.len() == 1 { "is" } else { "are" }
        );
    }
}

/// Which blaze this is, from the one place it is said: `BLAZE_RELEASE`, the tag a release is
/// built as (`v0.YYYYMMDD.N`), which only release.yaml sets. The workspace's own version in
/// Cargo.toml stays `0.0.0` on main, so that no release changes the identity of every crate
/// (and every crate's place in blaze's store); only this crate, which changes with every commit
/// anyway, is told the release. Sets
///
/// - `BLAZE_VERSION`: the release's `0.YYYYMMDD.N`; for a build of the source tree (Cargo.toml's
///   `0.0.0`, no `BLAZE_RELEASE`) `0.0.0-dev`, which is no release and which no pin names; for
///   a crate published with its version written in, that version.
/// - `BLAZE_VERSION_LINE`, what `--version` prints first: `blaze 0.20261028.9 (fb82b09
///   2026-10-28)` for a release, as rustc says its own, and `blaze 0.0.0-dev (fb82b09)` for a
///   build of the tree. The commit is `BLAZE_GIT_SHA` and `BLAZE_GIT_DATE` (release.yaml and the
///   flake set them, where there may be no `.git`), else the checkout's HEAD, else none. A
///   release must be told both; the version stays field 2 (.github/actions/blaze reads it).
///
/// The build runs again when HEAD moves (its reflog grows with each commit and checkout).
fn version() {
    for name in ["BLAZE_RELEASE", "BLAZE_GIT_SHA", "BLAZE_GIT_DATE"] {
        println!("cargo:rerun-if-env-changed={name}");
    }
    let given = |name: &str| std::env::var(name).ok().filter(|it| !it.is_empty());
    let package = std::env::var("CARGO_PKG_VERSION").expect("cargo sets CARGO_PKG_VERSION");
    let release = given("BLAZE_RELEASE");
    let version = match &release {
        Some(tag) => {
            let dated = tag.strip_prefix("v0.").and_then(|it| it.split_once('.')).is_some_and(
                |(day, n)| {
                    day.len() == 8
                        && day.bytes().all(|b| b.is_ascii_digit())
                        && !n.is_empty()
                        && n.bytes().all(|b| b.is_ascii_digit())
                        && (n == "0" || !n.starts_with('0'))
                },
            );
            assert!(dated, "BLAZE_RELEASE is {tag:?}, not a release's tag v0.YYYYMMDD.N");
            let version = tag[1..].to_owned();
            assert!(
                package == "0.0.0" || package == version,
                "BLAZE_RELEASE is {tag}, and the workspace's version is {package}: a release's is \
                 either the tree's 0.0.0 or its own"
            );
            version
        }
        None if package == "0.0.0" => "0.0.0-dev".to_owned(),
        None => package,
    };
    // TZ: the date is UTC's, as release.yaml's `BLAZE_GIT_DATE`.
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .env("TZ", "UTC0")
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .output()
            .ok()
            .filter(|out| out.status.success())?;
        String::from_utf8(out.stdout)
            .ok()
            .map(|it| it.trim().to_owned())
            .filter(|it| !it.is_empty())
    };
    let (sha, date) = match (given("BLAZE_GIT_SHA"), given("BLAZE_GIT_DATE")) {
        (Some(sha), date) => (Some(sha), date),
        (None, _) => {
            for path in ["HEAD", "logs/HEAD"] {
                if let Some(file) =
                    git(&["rev-parse", "--path-format=absolute", "--git-path", path])
                {
                    println!("cargo:rerun-if-changed={file}");
                }
            }
            let sha = git(&["rev-parse", "--short=7", "HEAD"]);
            let date = sha.as_ref().and_then(|_| {
                git(&["show", "-s", "--date=format-local:%Y-%m-%d", "--format=%cd", "HEAD"])
            });
            (sha, date)
        }
    };
    let line = match (&release, sha, date) {
        (Some(_), Some(sha), Some(date)) => format!("blaze {version} ({sha} {date})"),
        (Some(tag), sha, date) => panic!(
            "BLAZE_RELEASE is {tag}, and a release names its commit and the commit's date: \
             BLAZE_GIT_SHA is {sha:?} and BLAZE_GIT_DATE is {date:?}"
        ),
        (None, Some(sha), _) => format!("blaze {version} ({sha})"),
        (None, None, _) => format!("blaze {version}"),
    };
    println!("cargo:rustc-env=BLAZE_VERSION={version}");
    println!("cargo:rustc-env=BLAZE_VERSION_LINE={line}");
}
