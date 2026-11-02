use std::{
    os::unix::fs::{PermissionsExt, symlink},
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};

use super::*;

static REPOS: AtomicU64 = AtomicU64::new(0);

/// A repository in a temporary directory, removed when dropped.
struct Repo {
    dir: PathBuf,
}

impl Repo {
    fn new(name: &str) -> Repo {
        let unique = REPOS.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("blaze-ci-{name}-{}-{unique}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let repo = Repo { dir };
        repo.git(&["init", "-q"]);
        repo
    }

    fn git(&self, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.dir)
            .args(["-c", "user.name=blaze", "-c", "user.email=blaze@example.com", "-c", "commit.gpgsign=false"])
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("HOME", &self.dir)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn write(&self, path: &str, bytes: &[u8]) {
        let path = self.dir.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    fn mode(&self, path: &str, mode: u32) {
        std::fs::set_permissions(self.dir.join(path), std::fs::Permissions::from_mode(mode)).unwrap();
    }

    fn commit(&self, message: &str) -> String {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "--allow-empty", "-m", message]);
        self.git(&["rev-parse", "HEAD"])
    }

    fn clone_of(&self) -> Git {
        Git { dir: self.dir.clone(), remote: "https://github.com/indexable-inc/blaze.git".to_owned() }
    }
}

impl Drop for Repo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn write(path: &str, bytes: &[u8]) -> Op {
    write_as(path, bytes, Mode::Regular)
}

fn write_as(path: &str, bytes: &[u8], mode: Mode) -> Op {
    Op::Write { path: path.to_owned(), bytes: bytes.to_vec(), mode }
}

const RUN: &[u8] = b"#!/bin/sh\necho hi\n";

fn delete(path: &str) -> Op {
    Op::Delete { path: path.to_owned() }
}

/// Three files, one of them executable, then a change, a deletion and an addition.
fn history(repo: &Repo) -> (String, String) {
    repo.write("a.txt", b"one\n");
    repo.write("dir/b.rs", b"fn main() {}\n");
    repo.write("run.sh", b"#!/bin/sh\necho hi\n");
    repo.mode("run.sh", 0o755);
    let first = repo.commit("first");
    repo.write("a.txt", b"one more\n");
    std::fs::remove_file(repo.dir.join("dir/b.rs")).unwrap();
    repo.write("c.txt", b"\0binary\xff\n");
    repo.mode("run.sh", 0o644);
    let second = repo.commit("second");
    (first, second)
}

#[test]
fn needs_git_has_says_whether_the_clone_has_a_commit() {
    let repo = Repo::new("has");
    let (first, second) = history(&repo);
    let git = repo.clone_of();
    assert_eq!(git.has(&first), Ok(true));
    assert_eq!(git.has(&second), Ok(true));
    assert_eq!(git.has("0123456789abcdef0123456789abcdef01234567"), Ok(false));
    let tree = repo.git(&["rev-parse", &format!("{first}^{{tree}}")]);
    assert_eq!(git.has(&tree), Ok(false), "a tree is no commit");
    assert!(git.has("-c").is_err());
    let nowhere = Git { dir: repo.dir.join("no-such-clone"), remote: git.remote.clone() };
    assert!(nowhere.has(&first).is_err(), "no clone is an error, not a missing commit");
}

#[test]
fn split_fields_take_exactly_their_count() {
    assert_eq!(fields::<3>("100644 blob abc", ' '), Some(["100644", "blob", "abc"]));
    assert_eq!(fields::<3>("100644 blob", ' '), None);
    assert_eq!(fields::<3>("100644 blob abc more", ' '), None);
    assert_eq!(fields::<2>("abc missing", ' '), Some(["abc", "missing"]));
}

#[test]
fn needs_git_ops_from_nothing_write_every_file_sorted() {
    let repo = Repo::new("from-nothing");
    let (first, _) = history(&repo);
    let ops = repo.clone_of().ops(None, &first).unwrap();
    assert_eq!(
        ops,
        vec![write("a.txt", b"one\n"), write("dir/b.rs", b"fn main() {}\n"), write_as("run.sh", RUN, Mode::Executable)],
    );
}

#[test]
fn needs_git_ops_between_commits_write_changes_and_delete_what_went() {
    let repo = Repo::new("between");
    let (first, second) = history(&repo);
    let git = repo.clone_of();
    assert_eq!(
        git.ops(Some(&first), &second).unwrap(),
        vec![
            write("a.txt", b"one more\n"),
            write("c.txt", b"\0binary\xff\n"),
            delete("dir/b.rs"),
            write("run.sh", RUN),
        ],
        "run.sh lost its executable bit: the same bytes, written with the new mode",
    );
    assert_eq!(
        git.ops(Some(&second), &first).unwrap(),
        vec![
            write("a.txt", b"one\n"),
            delete("c.txt"),
            write("dir/b.rs", b"fn main() {}\n"),
            write_as("run.sh", RUN, Mode::Executable),
        ],
    );
    assert_eq!(git.ops(Some(&second), &second).unwrap(), Vec::<Op>::new());
}

#[test]
fn needs_git_ops_from_nothing_of_a_later_commit_are_its_whole_tree() {
    let repo = Repo::new("same-tree");
    let (_, second) = history(&repo);
    let ops = repo.clone_of().ops(None, &second).unwrap();
    assert_eq!(
        ops,
        vec![write("a.txt", b"one more\n"), write("c.txt", b"\0binary\xff\n"), write("run.sh", RUN)],
    );
}

#[test]
fn needs_git_a_symlink_is_written_as_one_with_its_target() {
    let repo = Repo::new("symlink");
    let (_, second) = history(&repo);
    symlink("a.txt", repo.dir.join("dir-link")).unwrap();
    let third = repo.commit("a symlink");
    let git = repo.clone_of();
    let link = write_as("dir-link", b"a.txt", Mode::Symlink);
    assert_eq!(git.ops(Some(&second), &third).unwrap(), vec![link.clone()]);
    assert_eq!(
        git.ops(None, &third).unwrap(),
        vec![write("a.txt", b"one more\n"), write("c.txt", b"\0binary\xff\n"), link, write("run.sh", RUN)],
    );
}

#[test]
fn needs_git_a_submodule_is_refused_naming_its_path() {
    let repo = Repo::new("submodule");
    let (first, second) = history(&repo);
    repo.git(&["update-index", "--add", "--cacheinfo", &format!("160000,{first},vendor/sub")]);
    repo.git(&["commit", "-q", "-m", "a submodule"]);
    let third = repo.git(&["rev-parse", "HEAD"]);
    let git = repo.clone_of();
    for error in [git.ops(Some(&second), &third).unwrap_err(), git.ops(None, &third).unwrap_err()] {
        assert_eq!(error.at.as_deref(), Some("vendor/sub"), "{error}");
        assert!(error.what.contains("submodule"), "{error}");
    }
}

#[test]
fn needs_git_a_commit_the_clone_lacks_fails_naming_the_command() {
    let repo = Repo::new("missing");
    let (first, _) = history(&repo);
    let git = repo.clone_of();
    let absent = "0123456789abcdef0123456789abcdef01234567";
    let error = git.ops(None, absent).unwrap_err();
    assert!(error.at.as_deref().is_some_and(|at| at.starts_with("git ls-tree")), "{error}");
    assert!(!error.what.is_empty());
    let error = git.ops(Some(&first), absent).unwrap_err();
    assert!(error.at.as_deref().is_some_and(|at| at.starts_with("git diff-tree")), "{error}");
}

#[test]
fn needs_git_fetch_from_a_local_path_is_refused_and_shows_no_token() {
    let origin = Repo::new("origin");
    let (first, _) = history(&origin);
    let clone = Repo::new("clone");
    let git = Git { dir: clone.dir.clone(), remote: origin.dir.to_str().unwrap().to_owned() };
    let secret = "ghs_0123456789abcdefTESTTOKEN";
    let error = git.fetch(&[first.clone()], Some(&Token::new(secret.to_owned()))).unwrap_err();
    let shown = format!("{error} {error:?}");
    assert!(!shown.contains(secret), "{shown}");
    assert!(!shown.contains(&STANDARD.encode(format!("x-access-token:{secret}"))), "{shown}");
    assert!(error.at.as_deref().is_some_and(|at| at.starts_with("git fetch")), "{error}");
    assert!(clone.clone_of().ops(None, &first).is_err(), "nothing was fetched");
}

#[test]
fn arguments_git_would_take_as_options_are_refused() {
    let git = Git { dir: Path::new("/nonexistent").to_path_buf(), remote: "origin".to_owned() };
    assert!(git.ops(None, "--output=/tmp/x").is_err());
    assert!(git.ops(Some("-p"), "HEAD").is_err());
    assert!(git.fetch(&["--upload-pack=touch /tmp/x".to_owned()], None).is_err());
    let flagged = Git { dir: git.dir.clone(), remote: "--upload-pack=x".to_owned() };
    assert!(flagged.fetch(&["main".to_owned()], None).is_err());
    assert_eq!(git.fetch(&[], None), Ok(()));
}

#[test]
fn a_pull_requests_head_ref() {
    assert_eq!(pull_ref(277), "refs/pull/277/head");
}

#[test]
fn a_shown_command_hides_a_urls_user_and_password() {
    assert_eq!(
        shown(&["fetch", "https://x-access-token:abc@github.com/o/r.git", "main"]),
        "git fetch https://***@github.com/o/r.git main",
    );
    assert_eq!(shown(&["fetch", "https://github.com/o/r.git"]), "git fetch https://github.com/o/r.git");
}

#[test]
fn a_batch_answer_that_is_not_the_blob_asked_is_an_error() {
    let blob = "0123456789abcdef0123456789abcdef01234567";
    let good = format!("{blob} blob 3\nabc\n");
    assert_eq!(batch(&mut good.as_bytes(), &[blob]).unwrap(), vec![b"abc".to_vec()]);
    let missing = format!("{blob} missing\n");
    assert!(batch(&mut missing.as_bytes(), &[blob]).unwrap_err().what.contains("no blob"));
    let other = format!("{blob} tree 3\nabc\n");
    assert!(batch(&mut other.as_bytes(), &[blob]).is_err());
    let short = format!("{blob} blob 30\nabc\n");
    assert!(batch(&mut short.as_bytes(), &[blob]).is_err());
}
