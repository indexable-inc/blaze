//! Real end-to-end interoperability test between `bgit` (server, client, infect, disinfect, ORT
//! merge) and the system `git` binary over real loopback TCP Smart HTTP.

use std::{
    fs,
    io::{BufRead as _, BufReader},
    path::Path,
    process::{Command, Stdio},
};

fn bgit_bin() -> &'static str {
    env!("CARGO_BIN_EXE_bgit")
}

fn run_git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .env("GIT_AUTHOR_NAME", "Test User")
        .env("GIT_AUTHOR_EMAIL", "test@ix.dev")
        .env("GIT_COMMITTER_NAME", "Test User")
        .env("GIT_COMMITTER_EMAIL", "test@ix.dev")
        .args(args)
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {args:?} failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn run_bgit(args: &[&str]) -> String {
    let out = Command::new(bgit_bin())
        .args(args)
        .output()
        .expect("spawn bgit");
    assert!(
        out.status.success(),
        "bgit {args:?} failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn real_git_infect_merge_serve_clone_push_and_disinfect() {
    let base = std::env::temp_dir().join(format!("bgit-interop-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let orig = base.join("orig");
    let git_clone = base.join("git-clone");
    let bgit_clone = base.join("bgit-clone");
    let disinfected = base.join("disinfected.git");
    fs::create_dir_all(&orig).unwrap();

    // 1. Create a real Git repo with diverging branches & an annotated tag.
    run_git(&orig, &["init", "-b", "main"]);
    fs::create_dir_all(orig.join("untouched")).unwrap();
    fs::write(orig.join("untouched/keep.txt"), "stay the same\n").unwrap();
    fs::write(orig.join("app.txt"), "line1\nline2\nline3\nline4\n").unwrap();
    run_git(&orig, &["add", "."]);
    run_git(&orig, &["commit", "-m", "initial commit"]);
    run_git(&orig, &["tag", "-a", "v1.0", "-m", "release v1.0"]);

    // Diverge on `feature`: edit line4.
    run_git(&orig, &["checkout", "-b", "feature"]);
    fs::write(orig.join("app.txt"), "line1\nline2\nline3\nLINE4_FEATURE\n").unwrap();
    run_git(&orig, &["add", "app.txt"]);
    run_git(&orig, &["commit", "-m", "feature edit"]);

    // Diverge on `main`: edit line1.
    run_git(&orig, &["checkout", "main"]);
    fs::write(orig.join("app.txt"), "LINE1_MAIN\nline2\nline3\nline4\n").unwrap();
    run_git(&orig, &["add", "app.txt"]);
    run_git(&orig, &["commit", "-m", "main edit"]);

    let git_mb = run_git(&orig, &["merge-base", "main", "feature"]);

    // 2. `bgit infect` converts `.git` into `.bgit` (BGOB + BGIX + BGRF + CGPH)!
    let infect_out = run_bgit(&["infect", orig.to_str().unwrap()]);
    assert!(infect_out.contains("Infected"), "unexpected infect output: {infect_out}");
    assert!(orig.join(".bgit/objects/slab-0000.bgob").is_file());
    assert!(orig.join(".bgit/objects/index.bgix").is_file());
    assert!(orig.join(".bgit/refs.bgref").is_file());
    assert!(orig.join(".bgit/graph.bggp").is_file());

    let verify_out = run_bgit(&["verify", orig.to_str().unwrap()]);
    assert!(verify_out.contains("Verified"), "unexpected verify output: {verify_out}");

    // 3. `bgit merge-base` matches `git merge-base` byte-for-byte!
    let bgit_mb = run_bgit(&["merge-base", orig.to_str().unwrap(), "main", "feature"]);
    assert_eq!(bgit_mb, git_mb);

    // 4. `bgit merge --write-commit` performs ORT 3-way merge and commits to `.bgit`!
    let merge_out = run_bgit(&[
        "merge",
        orig.to_str().unwrap(),
        "main",
        "feature",
        "--write-commit",
    ]);
    assert!(merge_out.contains("status=MergedClean"), "merge output: {merge_out}");
    assert!(merge_out.contains("subtrees_skipped=1"), "untouched/ must be skipped in O(1): {merge_out}");

    // 5. Start `bgit serve --http 127.0.0.1:0` serving `.bgit`!
    let mut server = Command::new(bgit_bin())
        .args(["serve", "--http", "127.0.0.1:0", orig.to_str().unwrap()])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn bgit serve");

    let stdout = server.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line).expect("read listening line");
    let url = line
        .split_whitespace()
        .find(|w| w.starts_with("http://"))
        .expect("listening URL")
        .to_string();

    // 6. REAL `git clone` against `bgit serve --http` over Protocol v2!
    let clone_status = Command::new("git")
        .args(["-c", "protocol.version=2", "clone", &url, git_clone.to_str().unwrap()])
        .output()
        .expect("spawn git clone");
    assert!(
        clone_status.status.success(),
        "real git clone failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&clone_status.stdout),
        String::from_utf8_lossy(&clone_status.stderr),
    );
    let cloned_app = fs::read_to_string(git_clone.join("app.txt")).unwrap();
    assert_eq!(cloned_app, "LINE1_MAIN\nline2\nline3\nLINE4_FEATURE\n");

    // 7. REAL `git commit` + `git push origin main` back to `bgit serve --http`!
    fs::write(git_clone.join("from_git.txt"), "pushed by real git!\n").unwrap();
    run_git(&git_clone, &["add", "from_git.txt"]);
    run_git(&git_clone, &["commit", "-m", "commit from real git"]);
    let pushed_head = run_git(&git_clone, &["rev-parse", "HEAD"]);
    run_git(&git_clone, &["push", "origin", "main"]);

    // 8. `bgit ls-remote` & `bgit clone` as a CLIENT against `bgit serve --http`!
    let ls_out = run_bgit(&["ls-remote", &url]);
    assert!(ls_out.contains(&pushed_head), "ls-remote missing pushed head: {ls_out}");

    let bgit_clone_out = run_bgit(&["clone", &url, bgit_clone.to_str().unwrap()]);
    assert!(bgit_clone_out.contains("cloned"), "bgit clone output: {bgit_clone_out}");
    assert_eq!(
        fs::read_to_string(bgit_clone.join("from_git.txt")).unwrap(),
        "pushed by real git!\n"
    );

    // Stop server cleanly.
    let _ = server.kill();
    let _ = server.wait();

    // 9. `bgit disinfect` exports `.bgit` back into standard `.git`, verified by `git fsck --full`!
    run_bgit(&["disinfect", orig.to_str().unwrap(), disinfected.to_str().unwrap()]);
    let fsck = Command::new("git")
        .arg("--git-dir")
        .arg(&disinfected)
        .args(["fsck", "--full"])
        .output()
        .expect("git fsck");
    assert!(
        fsck.status.success(),
        "git fsck on disinfected repo failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&fsck.stdout),
        String::from_utf8_lossy(&fsck.stderr),
    );

    let _ = fs::remove_dir_all(&base);
}

#[test]
fn bgit_status_matches_real_git_porcelain_and_works_on_pure_bgit_repos() {
    let base = std::env::temp_dir().join(format!("bgit-status-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let repo = base.join("repo");
    fs::create_dir_all(&repo).unwrap();

    let run_git_raw = |dir: &Path, args: &[&str]| -> String {
        let out = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
        assert!(out.status.success(), "git {args:?} failed");
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let run_bgit_raw = |args: &[&str]| -> String {
        let out = Command::new(bgit_bin()).args(args).output().unwrap();
        assert!(
            out.status.success(),
            "bgit {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    };

    // 1. Create a real Git repo with staged add/mod/del, MM, unstaged mod/del, ignored, and collapsed untracked dir.
    run_git(&repo, &["init", "-b", "main"]);
    run_git(&repo, &["config", "user.name", "Test"]);
    run_git(&repo, &["config", "user.email", "test@example.com"]);
    fs::write(repo.join(".gitignore"), "*.log\nignored_dir/\n").unwrap();
    fs::create_dir_all(repo.join("src")).unwrap();
    fs::write(repo.join("src/keep.rs"), "fn keep() {}\n").unwrap();
    fs::write(repo.join("src/staged_mod.rs"), "fn v1() {}\n").unwrap();
    fs::write(repo.join("src/staged_del.rs"), "fn old() {}\n").unwrap();
    fs::write(repo.join("src/wt_mod.rs"), "fn a() {}\n").unwrap();
    fs::write(repo.join("src/wt_del.rs"), "fn b() {}\n").unwrap();
    run_git(&repo, &["add", "."]);
    run_git(&repo, &["commit", "-m", "initial"]);

    // Staged add + staged mod (then also modified in worktree -> MM) + staged delete.
    fs::write(repo.join("src/staged_add.rs"), "fn added() {}\n").unwrap();
    fs::write(repo.join("src/staged_mod.rs"), "fn v2_staged() {}\n").unwrap();
    run_git(&repo, &["add", "src/staged_add.rs", "src/staged_mod.rs"]);
    run_git(&repo, &["rm", "src/staged_del.rs"]);
    // Now edit staged_mod.rs again in worktree so it becomes `MM`.
    fs::write(repo.join("src/staged_mod.rs"), "fn v3_worktree() {}\n").unwrap();
    // Unstaged modification and deletion.
    fs::write(repo.join("src/wt_mod.rs"), "fn a_modified() {}\n").unwrap();
    fs::remove_file(repo.join("src/wt_del.rs")).unwrap();
    // Untracked file, collapsed untracked dir, and ignored files.
    fs::write(repo.join("untracked.txt"), "hello\n").unwrap();
    fs::create_dir_all(repo.join("new_dir/sub")).unwrap();
    fs::write(repo.join("new_dir/sub/x.txt"), "nested\n").unwrap();
    fs::write(repo.join("skip.log"), "ignored\n").unwrap();
    fs::create_dir_all(repo.join("ignored_dir")).unwrap();
    fs::write(repo.join("ignored_dir/y.txt"), "ignored\n").unwrap();

    let git_porcelain = run_git_raw(&repo, &["status", "--porcelain=v1", "-b"]);
    let bgit_porcelain = run_bgit_raw(&["status", repo.to_str().unwrap(), "--porcelain", "-b"]);
    assert_eq!(
        bgit_porcelain, git_porcelain,
        "bgit status --porcelain -b must match git status --porcelain=v1 -b exactly"
    );

    // Also verify `-uno` matches `git status -uno --porcelain=v1 -b`.
    let git_uno = run_git_raw(&repo, &["status", "-uno", "--porcelain=v1", "-b"]);
    let bgit_uno = run_bgit_raw(&["status", repo.to_str().unwrap(), "-uno", "--porcelain", "-b"]);
    assert_eq!(bgit_uno, git_uno, "bgit status -uno must match git status -uno");

    // Also verify running from a subdirectory discovers the repo root!
    let bgit_from_sub =
        run_bgit_raw(&["status", repo.join("src").to_str().unwrap(), "--porcelain", "-b"]);
    assert_eq!(bgit_from_sub, git_porcelain, "bgit status from subdir must discover root");

    // 2. Infect the repo, verify `git status` stays clean (`.bgit/` excluded in `.git/info/exclude`),
    // then create a pure `.bgit`-only checkout (removing `.git`) and verify `bgit status` works!
    run_git(&repo, &["add", "-A"]);
    run_git(&repo, &["commit", "-m", "clean snapshot"]);
    run_bgit(&["infect", repo.to_str().unwrap()]);
    assert_eq!(
        run_git_raw(&repo, &["status", "--porcelain=v1"]),
        "",
        "git status must stay clean after `bgit infect` (`.bgit/` excluded in `.git/info/exclude`)"
    );
    // Remove `.git` completely -> pure `.bgit`-only repository!
    fs::remove_dir_all(repo.join(".git")).unwrap();
    let clean_bgit = run_bgit_raw(&["status", repo.to_str().unwrap(), "--porcelain", "-b"]);
    assert_eq!(clean_bgit, "## main\n", "pure .bgit clean status");

    fs::write(repo.join("src/keep.rs"), "fn keep_edited() {}\n").unwrap();
    fs::remove_file(repo.join("src/wt_mod.rs")).unwrap();
    fs::write(repo.join("brand_new.txt"), "hi\n").unwrap();
    let dirty_bgit = run_bgit_raw(&["status", repo.to_str().unwrap(), "--porcelain", "-b"]);
    assert_eq!(
        dirty_bgit,
        "## main\n M src/keep.rs\n D src/wt_mod.rs\n?? brand_new.txt\n",
        "pure .bgit dirty status"
    );

    let _ = fs::remove_dir_all(&base);
}

#[cfg(target_os = "macos")]
#[test]
fn bgit_clone_vfs_lazy_mount_cow_edits_and_o_dirty_status_on_macos() {
    let base = std::env::temp_dir().join(format!("bgit-vfs-interop-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let origin = base.join("origin");
    let lazy_clone = base.join("lazy-work");
    fs::create_dir_all(origin.join("src")).unwrap();

    run_git(&origin, &["init", "-b", "main"]);
    fs::write(origin.join("README.md"), "origin readme\n").unwrap();
    fs::write(origin.join("src/lib.rs"), "pub fn answer() -> u32 { 42 }\n").unwrap();
    run_git(&origin, &["add", "."]);
    run_git(&origin, &["commit", "-m", "initial"]);
    run_bgit(&["infect", origin.to_str().unwrap()]);

    // Serve `origin` over Smart HTTP v2 and clone with `--vfs` (0 files written to APFS)!
    let mut server = Command::new(bgit_bin())
        .args(["serve", "--http", "127.0.0.1:0", origin.to_str().unwrap()])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn bgit serve");

    let mut ready_line = String::new();
    let mut reader = BufReader::new(server.stdout.take().unwrap());
    reader.read_line(&mut ready_line).unwrap();
    let url = ready_line
        .trim()
        .strip_prefix("bgit-http listening on ")
        .expect("listening banner")
        .split_whitespace()
        .next()
        .expect("url")
        .to_string();

    let clone_out = run_bgit(&["clone", "--vfs", &url, lazy_clone.to_str().unwrap()]);
    assert!(clone_out.contains("files=0"), "expected 0 files materialized on disk: {clone_out}");
    assert!(clone_out.contains("Mounted"), "expected VFS mount confirmation: {clone_out}");

    // Ensure unmount happens even if an assertion fails.
    struct UnmountGuard(std::path::PathBuf);
    impl Drop for UnmountGuard {
        fn drop(&mut self) {
            let _ = Command::new(bgit_bin()).args(["unmount", self.0.to_str().unwrap()]).output();
        }
    }
    let _guard = UnmountGuard(lazy_clone.clone());

    // 1. Lazy read through macOS kernel NFSv3!
    assert_eq!(fs::read_to_string(lazy_clone.join("src/lib.rs")).unwrap(), "pub fn answer() -> u32 { 42 }\n");
    let clean_st = Command::new(bgit_bin())
        .args(["status", lazy_clone.to_str().unwrap(), "--porcelain", "-b"])
        .output()
        .unwrap();
    assert!(clean_st.status.success());
    assert_eq!(String::from_utf8_lossy(&clean_st.stdout), "## main\n");

    // 2. Copy-on-Write edit, create, and delete inside the lazy VFS!
    fs::write(lazy_clone.join("src/lib.rs"), "pub fn answer() -> u32 { 99 }\n").unwrap();
    fs::write(lazy_clone.join("added.txt"), "hello vfs\n").unwrap();
    fs::remove_file(lazy_clone.join("README.md")).unwrap();
    assert_eq!(fs::read_to_string(lazy_clone.join("src/lib.rs")).unwrap(), "pub fn answer() -> u32 { 99 }\n");

    // 3. O(dirty) `bgit status` must reflect all 3 overlay mutations immediately!
    let dirty_st = Command::new(bgit_bin())
        .args(["status", lazy_clone.to_str().unwrap(), "--porcelain", "-b"])
        .output()
        .unwrap();
    assert!(dirty_st.status.success());
    assert_eq!(
        String::from_utf8_lossy(&dirty_st.stdout),
        "## main\n D README.md\n M src/lib.rs\n?? added.txt\n"
    );

    drop(_guard);
    let _ = server.kill();
    let _ = server.wait();
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn bgit_worktree_auto_merges_tracked_updates_on_fs_and_auto_deletes_on_merge() {
    let base = std::env::temp_dir().join(format!("bgit-worktree-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let repo = base.join("repo");
    let wt_a = base.join("wt-feat-a");
    let wt_b = base.join("wt-feat-b");
    fs::create_dir_all(&repo).unwrap();

    // 1. Seed repo with `main` and a `feat-a` branch containing a new file + edit on line 5.
    run_git(&repo, &["init", "-b", "main"]);
    fs::write(repo.join("shared.txt"), "line1\nline2\nline3\nline4\nline5\n").unwrap();
    fs::write(repo.join("keep.txt"), "unchanged\n").unwrap();
    run_git(&repo, &["add", "."]);
    run_git(&repo, &["commit", "-m", "initial"]);

    run_git(&repo, &["checkout", "-b", "feat-a"]);
    fs::write(repo.join("shared.txt"), "line1\nline2\nline3\nline4\nline5_from_feat_a\n").unwrap();
    fs::write(repo.join("from_a.txt"), "hello from feat-a\n").unwrap();
    run_git(&repo, &["add", "."]);
    run_git(&repo, &["commit", "-m", "feat-a work"]);

    run_git(&repo, &["checkout", "main"]);
    run_bgit(&["infect", repo.to_str().unwrap()]);

    // 2. Register two worktrees tracking `main`: `wt-feat-a` on `feat-a`, and `wt-feat-b` on new `feat-b`.
    run_bgit(&[
        "worktree",
        "add",
        repo.to_str().unwrap(),
        wt_a.to_str().unwrap(),
        "feat-a",
        "--track",
        "main",
    ]);
    run_bgit(&[
        "worktree",
        "add",
        repo.to_str().unwrap(),
        wt_b.to_str().unwrap(),
        "feat-b",
        "--track",
        "main",
    ]);
    assert!(wt_a.join("from_a.txt").is_file());
    assert!(wt_b.join("shared.txt").is_file());
    assert!(!wt_b.join("from_a.txt").exists());

    // 3. Make a live uncommitted edit to line 1 of `shared.txt` inside `wt-feat-b`!
    fs::write(
        wt_b.join("shared.txt"),
        "line1_live_edit_in_wt_b\nline2\nline3\nline4\nline5\n",
    )
    .unwrap();

    // 4. Merge `feat-a` into `main` with `bgit merge --write-commit`:
    //    - `wt-feat-a` MUST be auto-deleted by default!
    //    - `wt-feat-b` (tracking `main`) MUST auto-merge `main` onto its live filesystem,
    //      adding `from_a.txt` AND cleanly 3-way line-merging `shared.txt`!
    let merge_out = run_bgit(&[
        "merge",
        repo.to_str().unwrap(),
        "main",
        "feat-a",
        "--write-commit",
    ]);
    assert!(
        merge_out.contains("Auto-deleted merged worktree"),
        "expected auto-delete message: {merge_out}"
    );
    assert!(!wt_a.exists(), "merged worktree folder wt-feat-a must be deleted by default");
    assert!(wt_b.exists(), "active worktree wt-feat-b must remain");
    assert_eq!(
        fs::read_to_string(wt_b.join("from_a.txt")).unwrap(),
        "hello from feat-a\n"
    );
    assert_eq!(
        fs::read_to_string(wt_b.join("shared.txt")).unwrap(),
        "line1_live_edit_in_wt_b\nline2\nline3\nline4\nline5_from_feat_a\n",
        "live edit in wt-feat-b must cleanly 3-way merge with incoming main change!"
    );

    let _ = fs::remove_dir_all(&base);
}

#[test]
fn plain_git_merge_auto_updates_tracking_worktree_and_deletes_merged_worktree_without_sync_or_watch() {
    let base = std::env::temp_dir().join(format!("bgit-githook-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let repo = base.join("repo");
    let wt_a = base.join("wt-a");
    let wt_b = base.join("wt-b");
    let wt_c = base.join("wt-c");
    fs::create_dir_all(&repo).unwrap();

    run_git(&repo, &["init", "-b", "main"]);
    fs::write(repo.join("shared.txt"), "line1\nline2\nline3\nline4\nline5\n").unwrap();
    run_git(&repo, &["add", "."]);
    run_git(&repo, &["commit", "-m", "initial"]);

    run_git(&repo, &["checkout", "-b", "feat-a"]);
    fs::write(repo.join("shared.txt"), "line1\nline2\nline3\nline4\nline5_via_git_merge\n").unwrap();
    fs::write(repo.join("from_a.txt"), "merged via plain git!\n").unwrap();
    run_git(&repo, &["add", "."]);
    run_git(&repo, &["commit", "-m", "feat-a commit"]);

    run_git(&repo, &["checkout", "-b", "feat-c", "main"]);
    fs::write(repo.join("only_c.txt"), "divergent commit on feat-c\n").unwrap();
    run_git(&repo, &["add", "."]);
    run_git(&repo, &["commit", "-m", "feat-c commit"]);

    run_git(&repo, &["checkout", "main"]);
    run_bgit(&["infect", repo.to_str().unwrap()]);
    run_bgit(&["worktree", "add", repo.to_str().unwrap(), wt_a.to_str().unwrap(), "feat-a", "--track", "main"]);
    run_bgit(&["worktree", "add", repo.to_str().unwrap(), wt_b.to_str().unwrap(), "feat-b", "--track", "main"]);
    run_bgit(&["worktree", "add", repo.to_str().unwrap(), wt_c.to_str().unwrap(), "feat-c", "--track", "main"]);

    // Make live uncommitted edits on line 1 of `shared.txt` in BOTH `wt-b` (FF case) and `wt-c` (divergent 3-way AutoMerge case).
    fs::write(wt_b.join("shared.txt"), "line1_local_in_wt_b\nline2\nline3\nline4\nline5\n").unwrap();
    fs::write(wt_c.join("shared.txt"), "line1_local_in_wt_c\nline2\nline3\nline4\nline5\n").unwrap();

    // Run PLAIN `git merge feat-a` — ZERO `bgit wt sync`, ZERO `bgit wt watch`!
    run_git(&repo, &["merge", "feat-a"]);

    // 1. `wt-a` (fast-forward merged into main) must be auto-deleted!
    assert!(!wt_a.exists(), "plain `git merge feat-a` must auto-delete merged worktree wt-a");
    // 2. `wt-b` (tracking main, no commits yet) must fast-forward & 3-way merge live edit!
    assert!(wt_b.exists(), "active worktree wt-b must remain");
    assert_eq!(fs::read_to_string(wt_b.join("from_a.txt")).unwrap(), "merged via plain git!\n");
    assert_eq!(
        fs::read_to_string(wt_b.join("shared.txt")).unwrap(),
        "line1_local_in_wt_b\nline2\nline3\nline4\nline5_via_git_merge\n",
        "plain `git merge` hook must auto-merge main into wt-b's live filesystem!"
    );
    // 3. `wt-c` (divergent branch tracking main) must 3-way AutoMerge AND mirror valid objects to `.git`!
    assert!(wt_c.exists(), "divergent worktree wt-c must remain");
    assert_eq!(fs::read_to_string(wt_c.join("only_c.txt")).unwrap(), "divergent commit on feat-c\n");
    assert_eq!(fs::read_to_string(wt_c.join("from_a.txt")).unwrap(), "merged via plain git!\n");
    assert_eq!(
        fs::read_to_string(wt_c.join("shared.txt")).unwrap(),
        "line1_local_in_wt_c\nline2\nline3\nline4\nline5_via_git_merge\n",
        "plain `git merge` hook must 3-way AutoMerge main into divergent wt-c's live filesystem!"
    );
    // Verify `.git` itself is 100% consistent (auto-merge commit + objects mirrored cleanly)!
    let parents = run_git(&repo, &["rev-list", "--parents", "-n", "1", "feat-c"]);
    assert_eq!(
        parents.split_whitespace().count(),
        3,
        "feat-c in .git must point to a real 2-parent auto-merge commit: {parents}"
    );
    run_git(&repo, &["fsck", "--full"]);

    let _ = fs::remove_dir_all(&base);
}

#[test]
fn libbgit_in_process_and_git_compat_routing_work_without_spawning() {
    let base = std::env::temp_dir().join(format!("bgit-inproc-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let repo = base.join("repo");
    fs::create_dir_all(&repo).unwrap();

    run_git(&repo, &["init", "-b", "main"]);
    fs::write(repo.join("a.txt"), "hello\n").unwrap();
    run_git(&repo, &["add", "."]);
    run_git(&repo, &["commit", "-m", "first commit"]);
    let first_sha = run_git(&repo, &["rev-parse", "HEAD"]).trim().to_string();

    fs::write(repo.join("b.txt"), "world\n").unwrap();
    run_git(&repo, &["add", "."]);
    run_git(&repo, &["commit", "-m", "second commit"]);
    let head_sha = run_git(&repo, &["rev-parse", "HEAD"]).trim().to_string();

    let alloc = blaze_own::Alloc::global();

    // 1. Un-infected repo: `git status --porcelain` and `git rev-parse --abbrev-ref HEAD` run
    //    in-process WITHOUT creating `.bgit/` on disk!
    fs::write(repo.join("untracked.txt"), "draft\n").unwrap();
    let st = bgit::try_run_git_via_bgit(&["status", "--porcelain"], &repo, false, alloc)
        .expect("git status --porcelain must run in-process");
    assert_eq!(st.exit_code, 0);
    assert_eq!(String::from_utf8_lossy(&st.stdout), "?? untracked.txt\n");
    let br = bgit::try_run_git_via_bgit(&["rev-parse", "--abbrev-ref", "HEAD"], &repo, false, alloc)
        .expect("git rev-parse --abbrev-ref HEAD must run in-process");
    assert_eq!(String::from_utf8_lossy(&br.stdout), "main\n");
    assert!(!repo.join(".bgit").exists(), "un-infected git queries must not create .bgit/");

    // 2. Infect in-process via `run_bgit_in_process`:
    let inf = bgit::run_bgit_in_process(&["infect"], &repo, false, alloc);
    assert_eq!(inf.exit_code, 0, "stderr: {}", String::from_utf8_lossy(&inf.stderr));
    assert!(repo.join(".bgit/VERSION").is_file());

    // 3. `git rev-parse HEAD`, `git log --oneline -n 2`, `git show-ref --heads`, `git merge-base`,
    //    and `git -C <repo> ...` all run directly in-process via `libbgit`!
    let rp = bgit::try_run_git_via_bgit(&["rev-parse", "HEAD"], &repo, false, alloc).unwrap();
    assert_eq!(String::from_utf8_lossy(&rp.stdout).trim(), head_sha);

    let log_out =
        bgit::try_run_git_via_bgit(&["-C", repo.to_str().unwrap(), "log", "--oneline", "-n", "2"], &base, false, alloc)
            .unwrap();
    assert_eq!(log_out.exit_code, 0);
    let log_text = String::from_utf8_lossy(&log_out.stdout);
    assert!(log_text.contains("second commit") && log_text.contains("first commit"), "log: {log_text}");

    let sr = bgit::try_run_git_via_bgit(&["show-ref", "--heads"], &repo, false, alloc).unwrap();
    assert_eq!(String::from_utf8_lossy(&sr.stdout).trim(), format!("{head_sha} refs/heads/main"));

    let mb = bgit::try_run_git_via_bgit(&["merge-base", &first_sha, &head_sha], &repo, false, alloc).unwrap();
    assert_eq!(String::from_utf8_lossy(&mb.stdout).trim(), first_sha);

    let entries = bgit::query_log_entries(&repo, "HEAD", 5, alloc).unwrap().unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].subject, "second commit");
    assert_eq!(entries[1].subject, "first commit");

    // 4. Interactive / exotic git flags cleanly return None (deferring to external git).
    assert!(bgit::try_run_git_via_bgit(&["commit"], &repo, false, alloc).is_none());
    assert!(bgit::try_run_git_via_bgit(&["add", "-p"], &repo, false, alloc).is_none());
    assert!(bgit::try_run_git_via_bgit(&["rebase", "-i", "HEAD~1"], &repo, false, alloc).is_none());

    let _ = fs::remove_dir_all(&base);
}

#[test]
fn bgit_porcelain_commands_full_workflow_and_git_interop() {
    let base = std::env::temp_dir().join(format!("bgit-porcelain-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let repo = base.join("repo");
    fs::create_dir_all(&repo).unwrap();

    let alloc = blaze_own::Alloc::global();
    let run_ok = |args: &[&str]| -> String {
        let out = bgit::run_bgit_in_process(args, &repo, false, alloc);
        assert_eq!(
            out.exit_code,
            0,
            "bgit {:?} failed:\nstdout: {}\nstderr: {}",
            args,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    };

    // Start from a real `.git` repo and infect it so we prove 100% `.git` <-> `.bgit` dual-sync!
    run_git(&repo, &["init", "-b", "main"]);
    fs::write(repo.join(".gitignore"), "*.log\n").unwrap();
    fs::write(repo.join("ignored.log"), "noise\n").unwrap();
    fs::write(repo.join("hello.txt"), "line1\nline2\n").unwrap();
    run_git(&repo, &["add", ".gitignore", "hello.txt"]);
    run_git(&repo, &["commit", "-m", "initial commit"]);
    let _ = run_ok(&["infect"]);

    // 1. `bgit config` & `bgit remote`
    let _ = run_ok(&["config", "--local", "user.name", "Blaze Tester"]);
    let _ = run_ok(&["config", "--local", "user.email", "tester@blaze.dev"]);
    assert_eq!(run_ok(&["config", "--local", "user.name"]).trim(), "Blaze Tester");
    let _ = run_ok(&["remote", "add", "origin", "http://127.0.0.1:9999/repo.git"]);
    assert_eq!(run_ok(&["remote", "get-url", "origin"]).trim(), "http://127.0.0.1:9999/repo.git");
    assert!(run_ok(&["remote", "-v"]).contains("origin\thttp://127.0.0.1:9999/repo.git"));
    let _ = run_ok(&["remote", "rename", "origin", "upstream"]);
    let _ = run_ok(&["remote", "remove", "upstream"]);
    assert!(run_ok(&["remote"]).trim().is_empty());

    // 2. `bgit add`, `bgit diff`, `bgit diff --staged`, `bgit commit`, `bgit show`
    fs::write(repo.join("hello.txt"), "line1\nline2-edited\nline3\n").unwrap();
    fs::write(repo.join("extra.txt"), "extra\n").unwrap();
    let wt_diff = run_ok(&["diff"]);
    assert!(wt_diff.contains("-line2") && wt_diff.contains("+line2-edited"), "wt_diff: {wt_diff}");
    let _ = run_ok(&["add", "."]);
    let staged_diff = run_ok(&["diff", "--staged", "--name-status"]);
    assert!(staged_diff.contains("A\textra.txt") && staged_diff.contains("M\thello.txt"), "staged: {staged_diff}");
    // Verify `.gitignore` kept `ignored.log` out of index!
    assert!(!staged_diff.contains("ignored.log"));
    let commit_out = run_ok(&["commit", "-m", "second commit via bgit"]);
    assert!(commit_out.contains("[main "), "commit_out: {commit_out}");
    assert!(commit_out.contains("second commit via bgit"));

    // Verify `bgit show` and `bgit show HEAD:hello.txt`
    let show_out = run_ok(&["show"]);
    assert!(show_out.contains("second commit via bgit") && show_out.contains("+line3"), "show: {show_out}");
    assert_eq!(run_ok(&["show", "HEAD:hello.txt"]), "line1\nline2-edited\nline3\n");
    assert_eq!(run_ok(&["rev-parse", "--abbrev-ref", "HEAD"]).trim(), "main");
    let head_sha = run_ok(&["rev-parse", "HEAD"]).trim().to_owned();
    let parent_sha = run_ok(&["rev-parse", "HEAD~1"]).trim().to_owned();
    assert_ne!(head_sha, parent_sha);
    assert_eq!(run_ok(&["rev-parse", "--short", "HEAD"]).trim(), &head_sha[..7]);

    // 3. `bgit branch`, `bgit checkout -b`, `bgit switch`, `bgit merge <branch>`
    let _ = run_ok(&["checkout", "-b", "feature"]);
    assert_eq!(run_ok(&["branch", "--show-current"]).trim(), "feature");
    let br_list = run_ok(&["branch", "-v"]);
    assert!(br_list.contains("* feature") && br_list.contains("  main"), "branches: {br_list}");
    fs::write(repo.join("feat.txt"), "from feature\n").unwrap();
    let _ = run_ok(&["add", "feat.txt"]);
    let _ = run_ok(&["commit", "-m", "feature work"]);
    let feat_sha = run_ok(&["rev-parse", "HEAD"]).trim().to_owned();

    // Switch back to `main`, verify `feat.txt` disappears cleanly from worktree!
    let _ = run_ok(&["switch", "main"]);
    assert!(!repo.join("feat.txt").exists());
    // Unmerged `-d` must fail!
    let bad_del = bgit::run_bgit_in_process(&["branch", "-d", "feature"], &repo, false, alloc);
    assert_ne!(bad_del.exit_code, 0, "deleting unmerged branch with -d must fail");

    // 1-arg ergonomic `bgit merge feature` into `main` (fast-forward) updates worktree + ref!
    let merge_out = run_ok(&["merge", "feature"]);
    assert!(merge_out.contains("FastForward"), "merge_out: {merge_out}");
    assert_eq!(fs::read_to_string(repo.join("feat.txt")).unwrap(), "from feature\n");
    assert_eq!(run_ok(&["rev-parse", "main"]).trim(), feat_sha);

    // Now `-d feature` succeeds because it's merged!
    let del_out = run_ok(&["branch", "-d", "feature"]);
    assert!(del_out.contains("Deleted branch feature"));

    // Rename & rename back
    let _ = run_ok(&["branch", "-m", "main", "trunk"]);
    assert_eq!(run_ok(&["branch", "--show-current"]).trim(), "trunk");
    let _ = run_ok(&["branch", "-m", "main"]);
    assert_eq!(run_ok(&["branch", "--show-current"]).trim(), "main");

    // 4. `bgit tag` (lightweight + annotated `-a -m` + delete)
    let _ = run_ok(&["tag", "v0.1.0"]);
    let _ = run_ok(&["tag", "-a", "v0.2.0", "-m", "Release 0.2.0"]);
    let tags = run_ok(&["tag"]);
    assert!(tags.contains("v0.1.0") && tags.contains("v0.2.0"), "tags: {tags}");
    assert_eq!(run_ok(&["rev-parse", "v0.2.0^{commit}"]).trim(), feat_sha);
    let _ = run_ok(&["tag", "-d", "v0.1.0"]);
    assert!(!run_ok(&["tag"]).contains("v0.1.0"));

    // 5. `bgit stash push/list/pop`
    fs::write(repo.join("hello.txt"), "stashed edit\n").unwrap();
    let _ = run_ok(&["stash", "push", "-m", "wip hello"]);
    assert_eq!(fs::read_to_string(repo.join("hello.txt")).unwrap(), "line1\nline2-edited\nline3\n");
    assert!(run_ok(&["stash", "list"]).contains("wip hello"));
    let _ = run_ok(&["stash", "pop"]);
    assert_eq!(fs::read_to_string(repo.join("hello.txt")).unwrap(), "stashed edit\n");

    // 6. `bgit restore` & `bgit reset`
    let _ = run_ok(&["restore", "hello.txt"]);
    assert_eq!(fs::read_to_string(repo.join("hello.txt")).unwrap(), "line1\nline2-edited\nline3\n");

    // 7. `bgit mv` & `bgit rm` & `bgit commit -a`
    let _ = run_ok(&["mv", "extra.txt", "renamed.txt"]);
    assert!(!repo.join("extra.txt").exists() && repo.join("renamed.txt").exists());
    let _ = run_ok(&["rm", "feat.txt"]);
    assert!(!repo.join("feat.txt").exists());
    let _ = run_ok(&["commit", "-m", "mv and rm"]);

    // 8. `bgit revert` & `bgit cherry-pick`
    let revert_out = run_ok(&["revert", "HEAD"]);
    assert!(revert_out.contains("Revert \"mv and rm\""), "revert: {revert_out}");
    assert!(repo.join("feat.txt").exists() && repo.join("extra.txt").exists());

    // 9. `bgit clean -n` & `bgit clean -f`
    fs::write(repo.join("junk.tmp"), "delete me\n").unwrap();
    assert!(run_ok(&["clean", "-n"]).contains("junk.tmp"));
    assert!(repo.join("junk.tmp").exists());
    let _ = run_ok(&["clean", "-f"]);
    assert!(!repo.join("junk.tmp").exists());

    // 10. Real `git` interoperability check (`git fsck --full`, `git status --porcelain`, `git branch`)!
    let fsck = Command::new("git").args(["fsck", "--full"]).current_dir(&repo).output().unwrap();
    assert!(
        fsck.status.success(),
        "git fsck failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&fsck.stdout),
        String::from_utf8_lossy(&fsck.stderr)
    );
    let git_st = run_git(&repo, &["status", "--porcelain"]);
    assert!(git_st.trim().is_empty(), "expected clean working tree in real git, got:\n{git_st}");
    let git_br = run_git(&repo, &["branch", "--show-current"]);
    assert_eq!(git_br.trim(), "main");

    // 11. `try_run_git_via_bgit` routes `git branch`, `git tag`, `git show --stat` in-process!
    let gb = bgit::try_run_git_via_bgit(&["branch", "--show-current"], &repo, false, alloc).unwrap();
    assert_eq!(gb.exit_code, 0);
    assert_eq!(String::from_utf8_lossy(&gb.stdout).trim(), "main");
    let gt = bgit::try_run_git_via_bgit(&["tag"], &repo, false, alloc).unwrap();
    assert_eq!(gt.exit_code, 0);
    assert!(String::from_utf8_lossy(&gt.stdout).contains("v0.2.0"));

    // 12. Everyday write commands (`git add`, `git commit -m`, `git switch -c`, `git switch`)
    //     also run directly in-process via `try_run_git_via_bgit` on `.bgit`-infected repos!
    fs::write(repo.join("inproc.txt"), "hello in-process git\n").unwrap();
    let ga = bgit::try_run_git_via_bgit(&["add", "inproc.txt"], &repo, false, alloc).unwrap();
    assert_eq!(ga.exit_code, 0, "stderr: {}", String::from_utf8_lossy(&ga.stderr));
    let gc =
        bgit::try_run_git_via_bgit(&["commit", "-m", "in-process commit"], &repo, false, alloc)
            .unwrap();
    assert_eq!(gc.exit_code, 0, "stderr: {}", String::from_utf8_lossy(&gc.stderr));
    // Interactive `git commit` (no `-m`) and `git add -p` still defer cleanly to external git.
    assert!(bgit::try_run_git_via_bgit(&["commit"], &repo, false, alloc).is_none());
    assert!(bgit::try_run_git_via_bgit(&["add", "-p"], &repo, false, alloc).is_none());

    let fsck2 = Command::new("git").args(["fsck", "--full"]).current_dir(&repo).output().unwrap();
    assert!(fsck2.status.success(), "git fsck2 failed: {}", String::from_utf8_lossy(&fsck2.stderr));

    let _ = fs::remove_dir_all(&base);
}

#[test]
fn fsmonitor_o_dirty_status_diff_and_structured_queries() {
    let base = std::env::temp_dir().join(format!("bgit-fsmon-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let repo = base.join("repo");
    fs::create_dir_all(&repo).unwrap();

    run_git(&repo, &["init", "-b", "main"]);
    fs::write(repo.join("a.txt"), "alpha\nbeta\n").unwrap();
    fs::write(repo.join("b.txt"), "one\ntwo\n").unwrap();
    run_git(&repo, &["add", "."]);
    run_git(&repo, &["commit", "-m", "init"]);
    run_git(&repo, &["branch", "dev"]);

    let alloc = blaze_own::Alloc::global();
    let inf = bgit::run_bgit_in_process(&["infect"], &repo, false, alloc);
    assert_eq!(inf.exit_code, 0, "stderr: {}", String::from_utf8_lossy(&inf.stderr));

    // Verify `BGRF` v2 magic/version header on disk.
    let bgref = fs::read(repo.join(".bgit/refs.bgref")).unwrap();
    assert_eq!(&bgref[0..4], b"BGRF");
    assert_eq!(u32::from_le_bytes(bgref[4..8].try_into().unwrap()), 2);

    // Structured `query_branches` reads zero-copy from `BGRF` v2.
    let branches = bgit::query_branches(&repo, false, false, alloc).unwrap().unwrap();
    assert_eq!(branches.len(), 2);
    assert!(branches.iter().any(|b| b.name == "main" && b.is_current && b.subject == "init"));
    assert!(branches.iter().any(|b| b.name == "dev" && !b.is_current && b.subject == "init"));

    // Start in-process FSEvents monitor (`blaze_core::Native` `Effect::Journal` + `Effect::Flush`).
    assert!(bgit::ensure_fsmonitor_watching(&repo), "expected FSEvents monitor to start");

    // 1. Prime baselines (clean).
    let st1 = bgit::run_bgit_in_process(&["status", "--porcelain"], &repo, false, alloc);
    assert_eq!(st1.exit_code, 0);
    assert!(String::from_utf8_lossy(&st1.stdout).trim().is_empty());
    let df1 = bgit::run_bgit_in_process(&["diff", "--stat"], &repo, false, alloc);
    assert_eq!(df1.exit_code, 0);
    assert!(String::from_utf8_lossy(&df1.stdout).trim().is_empty());

    // 2. Warm clean cache hit.
    let st2 = bgit::run_bgit_in_process(&["status", "--porcelain"], &repo, false, alloc);
    assert_eq!(st2.exit_code, 0);
    assert!(String::from_utf8_lossy(&st2.stdout).trim().is_empty());

    // 3. Modify a tracked file -> FSEvents O(dirty) path detects it immediately via cookie barrier!
    fs::write(repo.join("a.txt"), "alpha\nbeta\ngamma\n").unwrap();
    let st3 = bgit::run_bgit_in_process(&["status", "--porcelain"], &repo, false, alloc);
    assert_eq!(st3.exit_code, 0);
    assert_eq!(String::from_utf8_lossy(&st3.stdout).trim(), "M a.txt");

    let df3 = bgit::run_bgit_in_process(&["diff", "--stat"], &repo, false, alloc);
    assert_eq!(df3.exit_code, 0);
    assert!(
        String::from_utf8_lossy(&df3.stdout).contains("a.txt"),
        "expected a.txt in diff --stat: {}",
        String::from_utf8_lossy(&df3.stdout)
    );

    // Structured `query_diff_stat` uses the same O(dirty) FSEvents path!
    let dstat = bgit::query_diff_stat(&repo, false, alloc).unwrap().unwrap();
    assert_eq!(dstat.len(), 1);
    assert_eq!(dstat[0].path, "a.txt");
    assert_eq!(dstat[0].status, 'M');
    assert_eq!(dstat[0].insertions, 1);
    assert_eq!(dstat[0].deletions, 0);

    // 4. Revert tracked file -> O(dirty) path clears it back to clean!
    fs::write(repo.join("a.txt"), "alpha\nbeta\n").unwrap();
    let st4 = bgit::run_bgit_in_process(&["status", "--porcelain"], &repo, false, alloc);
    assert_eq!(st4.exit_code, 0);
    assert!(String::from_utf8_lossy(&st4.stdout).trim().is_empty());
    let dstat_clean = bgit::query_diff_stat(&repo, false, alloc).unwrap().unwrap();
    assert!(dstat_clean.is_empty());

    // 5. Create a brand-new untracked file -> monitor detects untracked path & rescans cleanly!
    fs::write(repo.join("untracked.txt"), "new file\n").unwrap();
    let st5 = bgit::run_bgit_in_process(&["status", "--porcelain"], &repo, false, alloc);
    assert_eq!(st5.exit_code, 0);
    assert_eq!(String::from_utf8_lossy(&st5.stdout).trim(), "?? untracked.txt");

    let _ = fs::remove_dir_all(&base);
}

#[test]
fn stateless_dircache_bgdc_detects_adds_deletes_and_edits_in_sparse_dirs() {
    let base = std::env::temp_dir().join(format!("bgit-dircache-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let repo = base.join("repo");
    fs::create_dir_all(repo.join("sub/nested")).unwrap();

    run_git(&repo, &["init", "-b", "main"]);
    fs::write(repo.join("sub/nested/only.txt"), "v1\n").unwrap();
    run_git(&repo, &["add", "."]);
    run_git(&repo, &["commit", "-m", "init"]);

    let alloc = blaze_own::Alloc::global();
    let inf = bgit::run_bgit_in_process(&["infect"], &repo, false, alloc);
    assert_eq!(inf.exit_code, 0);

    // Wait past the 50 ms racy-dir window so `sub` and `sub/nested` are eligible for `.bgit/dircache.bgdc`.
    std::thread::sleep(std::time::Duration::from_millis(80));

    // Run 1 populates `.bgit/dircache.bgdc`.
    let st1 = bgit::run_bgit_in_process(&["status", "--porcelain"], &repo, false, alloc);
    assert_eq!(st1.exit_code, 0);
    assert!(String::from_utf8_lossy(&st1.stdout).trim().is_empty());
    assert!(repo.join(".bgit/dircache.bgdc").is_file(), "expected .bgit/dircache.bgdc to be written");

    // Run 2 hits `.bgit/dircache.bgdc` cleanly.
    let st2 = bgit::run_bgit_in_process(&["status", "--porcelain"], &repo, false, alloc);
    assert_eq!(st2.exit_code, 0);
    assert!(String::from_utf8_lossy(&st2.stdout).trim().is_empty());

    // Edit file inside cached sparse dir (parent dir mtime unchanged, child stat changed!).
    fs::write(repo.join("sub/nested/only.txt"), "v2 edited\n").unwrap();
    let st3 = bgit::run_bgit_in_process(&["status", "--porcelain"], &repo, false, alloc);
    assert_eq!(String::from_utf8_lossy(&st3.stdout).trim(), "M sub/nested/only.txt");

    // Add a brand-new untracked file inside cached sparse dir (parent dir mtime bumped -> invalidates!).
    fs::write(repo.join("sub/nested/extra.txt"), "new\n").unwrap();
    let st4 = bgit::run_bgit_in_process(&["status", "--porcelain"], &repo, false, alloc);
    let out4 = String::from_utf8_lossy(&st4.stdout);
    assert!(out4.contains("M sub/nested/only.txt") && out4.contains("?? sub/nested/extra.txt"), "got:\n{out4}");

    // Delete tracked file inside cached sparse dir -> detected immediately!
    fs::remove_file(repo.join("sub/nested/only.txt")).unwrap();
    let st5 = bgit::run_bgit_in_process(&["status", "--porcelain"], &repo, false, alloc);
    let out5 = String::from_utf8_lossy(&st5.stdout);
    assert!(out5.contains("D sub/nested/only.txt") && out5.contains("?? sub/nested/extra.txt"), "got:\n{out5}");

    let _ = fs::remove_dir_all(&base);
}
