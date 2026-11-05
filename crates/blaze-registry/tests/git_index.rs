//! Git registry snapshots and cargo's index cache, without a network service or a checkout.
use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
};

use blaze_registry::{
    Context, Registry,
    http::{Http, Response},
    index::{Summaries, dep_path},
};
use blaze_source::SourceId;
use semver::VersionReq;

struct NoHttp;
impl Http for NoHttp {
    fn get(&self, url: &str, _: &[(&str, &str)]) -> anyhow::Result<Response> {
        panic!("unexpected HTTP request: {url}");
    }
}

struct Fixture {
    root: PathBuf,
    repo: PathBuf,
    home: PathBuf,
    source: SourceId,
}
impl Fixture {
    fn new(name: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("blaze-git-index-{name}-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&root);
        let repo = root.join("upstream");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        let source = SourceId::for_registry(&url::Url::from_file_path(&repo).unwrap()).unwrap();
        let fixture = Fixture { home: root.join("home"), root, repo, source };
        fixture.write("config.json", r#"{"dl":"https://example.invalid/{crate}/{version}"}"#);
        fixture.package("probe", "1.0.0");
        fixture.commit();
        fixture
    }
    fn write(&self, path: &str, text: &str) {
        let path = self.repo.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    fn package(&self, name: &str, version: &str) {
        self.write(&dep_path(name, false), &format!(
            "{{\"name\":\"{name}\",\"vers\":\"{version}\",\"deps\":[],\"cksum\":\"{}\",\"features\":{{}},\"yanked\":false}}\n", "1".repeat(64)));
    }
    fn commit(&self) -> String {
        git(&self.repo, &["add", "-A"]);
        git(&self.repo, &["commit", "-q", "--allow-empty", "-m", "index"]);
        git(&self.repo, &["rev-parse", "HEAD"])
    }
    fn registry(&self, offline: bool) -> Registry {
        let mut context = Context::new(self.home.clone(), Arc::new(NoHttp));
        context.net.offline = offline;
        context.net.retry.retries = 0;
        Registry::remote(self.source, Arc::new(context)).unwrap()
    }
    fn index(&self) -> PathBuf {
        self.home.join("registry/index").join(self.source.registry_dir_name())
    }
    fn cache(&self) -> PathBuf {
        self.index().join(".cache").join(dep_path("probe", false))
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        _ = std::fs::remove_dir_all(&self.root);
    }
}

#[expect(clippy::disallowed_methods, reason = "git operates only on this fixture")]
fn git(at: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(at)
        .args(["-c", "commit.gpgsign=false", "-c", "init.defaultBranch=main"])
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@localhost")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@localhost")
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}
fn versions(registry: &Registry, name: &str) -> Vec<String> {
    registry
        .query(name, &VersionReq::STAR)
        .unwrap()
        .into_iter()
        .map(|it| it.entry.version.to_string())
        .collect()
}

#[test]
fn cache_versions_are_blobs_and_old_commit_versions_are_read() {
    let f = Fixture::new("cache");
    let registry = f.registry(false);
    assert_eq!(versions(&registry, "probe"), ["1.0.0"]);
    let (summary, version) = Summaries::from_cache(std::fs::read(f.cache()).unwrap()).unwrap();
    let blob = git(&f.repo, &["rev-parse", "HEAD:pr/ob/probe"]);
    assert_eq!(version, blob);
    assert!(!f.index().join("pr").exists(), "nothing is checked out");
    let head = git(&f.repo, &["rev-parse", "HEAD"]);
    let old = summary.to_cache(&head);
    std::fs::write(f.cache(), &old).unwrap();
    assert_eq!(versions(&f.registry(true), "probe"), ["1.0.0"]);
    assert_eq!(std::fs::read(f.cache()).unwrap(), old, "old cargo's cache is already valid");
    f.write("unrelated", "another commit");
    f.commit();
    let registry = f.registry(false);
    registry.invalidate();
    assert_eq!(versions(&registry, "probe"), ["1.0.0"]);
    let (_, version) = Summaries::from_cache(std::fs::read(f.cache()).unwrap()).unwrap();
    assert_eq!(version, blob, "the file's identity survives unrelated commits");
}

#[test]
fn a_session_reads_one_snapshot_and_refreshes_missing_entries_once() {
    let f = Fixture::new("updates");
    assert_eq!(versions(&f.registry(false), "probe"), ["1.0.0"]);
    let registry = f.registry(false);
    assert_eq!(versions(&registry, "probe"), ["1.0.0"]);
    f.package("probe", "2.0.0");
    f.package("newcrate", "1.0.0");
    f.commit();
    assert_eq!(versions(&registry, "probe"), ["1.0.0"]);
    assert_eq!(versions(&f.registry(true), "newcrate"), Vec::<String>::new());
    assert_eq!(versions(&registry, "newcrate"), ["1.0.0"]);
    assert!(registry.is_updated());
    assert_eq!(versions(&registry, "probe"), ["2.0.0"]);
    std::fs::remove_file(f.repo.join("pr/ob/probe")).unwrap();
    f.commit();
    let registry = f.registry(false);
    registry.invalidate();
    assert!(versions(&registry, "probe").is_empty());
    assert!(!f.cache().exists(), "a removed index file removes its stale cache too");
}

#[test]
fn offline_empty_indexes_and_failed_updates_never_answer_an_old_tree() {
    let f = Fixture::new("offline");
    assert!(versions(&f.registry(true), "probe").is_empty());
    assert!(!f.index().exists(), "offline does not initialize or fetch a repository");
    assert_eq!(versions(&f.registry(false), "probe"), ["1.0.0"]);
    let moved = f.root.join("unavailable");
    std::fs::rename(&f.repo, &moved).unwrap();
    assert_eq!(versions(&f.registry(true), "probe"), ["1.0.0"]);
    let registry = f.registry(false);
    registry.invalidate();
    assert!(registry.query("probe", &VersionReq::STAR).is_err());
    assert!(!registry.is_updated());
    std::fs::rename(moved, &f.repo).unwrap();
    assert_eq!(versions(&registry, "probe"), ["1.0.0"], "a failed update can be retried");
}

#[test]
fn a_non_blob_index_path_is_an_error() {
    let f = Fixture::new("directory");
    std::fs::remove_file(f.repo.join("pr/ob/probe")).unwrap();
    f.write("pr/ob/probe/inside", "not an index file");
    f.commit();
    let error = f.registry(false).query("probe", &VersionReq::STAR).unwrap_err();
    assert!(format!("{error:#}").contains("not a blob"));
}
