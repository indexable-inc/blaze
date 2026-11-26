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
