//! Git sources fetched with the `git` command from repositories made here, at cargo's paths.

use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
};

use blaze_registry::{
    Context,
    git::{self, Checkout},
    http::{Http, Response},
};
use blaze_source::{GitReference, SourceId};

struct NoNetwork;

impl Http for NoNetwork {
    fn get(&self, url: &str, _: &[(&str, &str)]) -> anyhow::Result<Response> {
        panic!("asked {url}")
    }
}

fn temporary(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("blaze-registry-git-{name}-{}", std::process::id()));
    _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[expect(clippy::disallowed_methods, reason = "every `git` is run in the repository it is about")]
fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args([
            "-c",
            "init.defaultBranch=main",
            "-c",
            "protocol.file.allow=always",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(["-c", "tag.gpgsign=false"])
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn commit(dir: &Path, files: &[(&str, &str)], message: &str) -> String {
    for (path, text) in files {
        std::fs::create_dir_all(dir.join(path).parent().unwrap()).unwrap();
        std::fs::write(dir.join(path), text).unwrap();
    }
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", message]);
    git(dir, &["rev-parse", "HEAD"])
}

fn url(dir: &Path) -> url::Url {
    url::Url::from_file_path(dir).unwrap()
}

struct Repos {
    work: PathBuf,
    upstream: PathBuf,
    first: String,
    dev: String,
    head: String,
}

/// A workspace repository: `a` at the root, `b` a member, `c` in a submodule that `b` depends
/// on by path; an annotated tag `v1` on the first commit, a branch `dev` and `main`.
fn repos(name: &str) -> Repos {
    let work = temporary(name);
    let sub = work.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    git(&sub, &["init", "-q"]);
    commit(
        &sub,
        &[("c/Cargo.toml", "[package]\nname = \"c\"\nversion = \"0.3.0\"\n"), ("c/src/lib.rs", "")],
        "c",
    );

    let upstream = work.join("upstream");
    std::fs::create_dir_all(&upstream).unwrap();
    git(&upstream, &["init", "-q"]);
    let first = commit(
        &upstream,
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"a\"\nversion = \"1.0.0\"\n\n[workspace]\nmembers = [\"b\"]\n",
            ),
            ("src/lib.rs", "pub fn first() {}\n"),
        ],
        "first",
    );
    git(&upstream, &["tag", "-a", "v1", "-m", "v1"]);
    git(&upstream, &["checkout", "-q", "-b", "dev"]);
    let dev = commit(&upstream, &[("src/lib.rs", "pub fn dev() {}\n")], "dev");
    git(&upstream, &["checkout", "-q", "main"]);
    git(&upstream, &["submodule", "add", "-q", url(&sub).as_str(), "vendor/sub"]);
    let head = commit(
        &upstream,
        &[
            (
                "b/Cargo.toml",
                "[package]\nname = \"b\"\nversion.workspace = true\n\n[dependencies]\nc = { path = \"../vendor/sub/c\" }\n",
            ),
            ("b/src/lib.rs", ""),
            ("target/Cargo.toml", "[package]\nname = \"ignored\"\n"),
            (".hidden/Cargo.toml", "[package]\nname = \"hidden\"\n"),
            ("broken/Cargo.toml", "[package\n"),
        ],
        "head",
    );
    Repos { work, upstream, first, dev, head }
}

fn context(home: &Path, offline: bool) -> Context {
    let mut context = Context::new(home.to_owned(), Arc::new(NoNetwork));
    context.net.offline = offline;
    context.net.retry.quiet = true;
    context
}

fn source(repos: &Repos, reference: GitReference) -> SourceId {
    SourceId::for_git(&url(&repos.upstream), reference).unwrap()
}

#[test]
fn checks_out_as_cargo() {
    let repos = repos("checkout");
    let home = repos.work.join("home");
    let context = context(&home, false);
    let id = source(&repos, GitReference::DefaultBranch);
    let ident = id.git_ident();
    assert!(ident.starts_with("upstream-"), "{ident}");

    let checkout = git::fetch(&context, id, None).unwrap();
    let short = &repos.head[..7];
    assert_eq!(
        checkout,
        Checkout {
            root: home.join("git/checkouts").join(&ident).join(short),
            rev: repos.head.clone()
        }
    );
    assert!(home.join("git/db").join(&ident).join("HEAD").exists());
    assert!(home.join("git/CACHEDIR.TAG").exists());
    assert_eq!(std::fs::read(checkout.root.join(".cargo-ok")).unwrap(), b"");
    assert!(checkout.root.join("vendor/sub/c/src/lib.rs").exists());
    assert!(checkout.root.join("vendor/sub/.cargo-ok").exists());
    let dbs = std::fs::read_dir(home.join("git/db")).unwrap().count();
    assert_eq!(dbs, 2, "the submodule has a database of its own");

    let mut names: Vec<_> =
        checkout.packages().unwrap().into_iter().map(|it| (it.name, it.version)).collect();
    names.sort();
    assert_eq!(
        names,
        [
            ("a".to_owned(), Some("1.0.0".to_owned())),
            ("b".to_owned(), None),
            ("c".to_owned(), Some("0.3.0".to_owned()))
        ]
    );
    let version = |it: &str| semver::Version::parse(it).unwrap();
    assert_eq!(checkout.package("b", &version("1.0.0")).unwrap(), checkout.root.join("b"));
    assert_eq!(
        checkout.package("c", &version("0.3.0")).unwrap(),
        checkout.root.join("vendor/sub/c")
    );
    assert!(checkout.package("ignored", &version("0.0.0")).is_err());

    let tag =
        git::fetch(&context, source(&repos, GitReference::Tag("v1".to_owned())), None).unwrap();
    assert_eq!(tag.rev, repos.first);
    let branch =
        git::fetch(&context, source(&repos, GitReference::Branch("dev".to_owned())), None).unwrap();
    assert_eq!(branch.rev, repos.dev);
    assert_eq!(
        std::fs::read_to_string(branch.root.join("src/lib.rs")).unwrap(),
        "pub fn dev() {}\n"
    );
    let short_rev =
        git::fetch(&context, source(&repos, GitReference::Rev(repos.first[..10].to_owned())), None)
            .unwrap();
    assert_eq!(short_rev.rev, repos.first);
    let annotated =
        git::fetch(&context, source(&repos, GitReference::Rev("v1".to_owned())), None).unwrap();
    assert_eq!(annotated.rev, repos.first, "an annotated tag is its commit");

    // The lock file's commit is what is checked out, whatever the branch says now.
    let locked = git::fetch(&context, id, Some(&repos.dev)).unwrap();
    assert_eq!(locked.rev, repos.dev);

    // A checkout that was not finished is made again.
    std::fs::remove_file(checkout.root.join(".cargo-ok")).unwrap();
    std::fs::write(checkout.root.join("src/lib.rs"), "edited").unwrap();
    git::fetch(&context, id, Some(&repos.head)).unwrap();
    assert_eq!(
        std::fs::read_to_string(checkout.root.join("src/lib.rs")).unwrap(),
        "pub fn first() {}\n"
    );
}

#[test]
fn offline_uses_what_is_there() {
    let repos = repos("offline");
    let home = repos.work.join("home");
    let id = source(&repos, GitReference::Branch("dev".to_owned()));
    git::fetch(&context(&home, false), id, None).unwrap();
    std::fs::rename(&repos.upstream, repos.work.join("gone")).unwrap();

    let offline = context(&home, true);
    assert_eq!(git::fetch(&offline, id, Some(&repos.dev)).unwrap().rev, repos.dev);
    assert_eq!(git::fetch(&offline, id, None).unwrap().rev, repos.dev);
    let error = format!("{:#}", git::fetch(&offline, id, Some(&repos.head)).unwrap_err());
    assert!(error.starts_with("can't checkout from '"), "{error}");
    assert!(error.ends_with("': you are in the offline mode (--offline)"), "{error}");
}
