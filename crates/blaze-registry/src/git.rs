//! Git sources as cargo 1.99 fetches them (`src/sources/git/`), with the `git` command as cargo
//! runs it under `net.git-fetch-with-cli`: a bare database per repository at
//! `$CARGO_HOME/git/db/<name>-<hash>`, and a checkout per commit at
//! `$CARGO_HOME/git/checkouts/<name>-<hash>/<short id>`, with its submodules from databases of
//! their own, marked done by `.cargo-ok`. gix in the process, in place of the command, is the
//! later path.
//!
//! A `git fetch` runs with `--progress` and is watched as blaze's HTTP downloads are: it is in
//! blaze-fetch's downloads (the status line, `blaze status`, the build's stall bound) with the
//! bytes git says it received, and a fetch that prints nothing for
//! [`blaze_fetch::progress::STALL`] is killed and fails as [`Stalled`].

use std::{
    collections::VecDeque,
    io::{self, Read as _},
    path::{Path, PathBuf},
    process::{Child, ChildStderr, Command, Stdio},
    time::Instant,
};

use anyhow::{Context as _, bail};
use crossbeam_channel::{RecvTimeoutError, Sender};
use url::Url;

use blaze_fetch::{downloads::Download, progress::STALL};
use blaze_source::{GitReference, SourceId, SourceKind};

use crate::{
    Context,
    http::{Stalled, Transport},
};

const CHECKOUT_READY_LOCK: &str = ".cargo-ok";
/// cargo's `__CARGO_PACKFILE_LIMIT`: a database with more packs than this is `git gc --auto`ed.
const PACKFILE_LIMIT: usize = 100;

/// A commit of a git source, checked out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checkout {
    pub root: PathBuf,
    /// The commit, in full.
    pub rev: String,
}

/// The commit `precise` of the git source `id` (the lock file's), or what its reference names
/// now when there is none, fetched into its database when the database does not have it, and
/// checked out with its submodules.
pub fn fetch(context: &Context, id: SourceId, precise: Option<&str>) -> anyhow::Result<Checkout> {
    let SourceKind::Git(reference) = id.kind() else { bail!("{id} is not a git source") };
    let git = context.cargo_home.join("git");
    std::fs::create_dir_all(&git).with_context(|| format!("failed to create {}", git.display()))?;
    exclude_from_backups(&git);
    let ident = id.git_ident();
    let (db, rev) = fetch_db(context, id.url().as_str(), &ident, reference, precise, false)?;
    let short = short_id(&db, &rev)?;
    let root = git.join("checkouts").join(&ident).join(short);
    copy_to(context, &db, &rev, &root, id.url().as_str())?;
    Ok(Checkout { root, rev })
}

/// `CACHEDIR.TAG`, as cargo's `exclude_from_backups_and_indexing`.
fn exclude_from_backups(dir: &Path) {
    let tag = dir.join("CACHEDIR.TAG");
    if !tag.exists() {
        _ = std::fs::write(
            tag,
            "Signature: 8a477f597d28d172789f06886806bc55\n\
             # This file is a cache directory tag created by cargo.\n\
             # For information about cache directory tags see https://bford.info/cachedir/\n",
        );
    }
}

fn is_full_oid(rev: &str) -> bool {
    matches!(rev.len(), 40 | 64) && rev.bytes().all(|it| it.is_ascii_hexdigit())
}

/// cargo's `GitSource::fetch_db`: the database and the commit.
fn fetch_db(
    context: &Context,
    url: &str,
    ident: &str,
    reference: &GitReference,
    precise: Option<&str>,
    is_submodule: bool,
) -> anyhow::Result<(PathBuf, String)> {
    let db = context.cargo_home.join("git").join("db").join(ident);
    let exists = is_repo(&db);
    let locked = precise.filter(|it| is_full_oid(it));
    if let Some(oid) = locked
        && exists
        && contains(&db, oid)
    {
        return Ok((db, oid.to_owned()));
    }
    if locked.is_none() && exists && context.net.offline {
        let rev = resolve(
            &db,
            precise.map_or(reference.clone(), |it| GitReference::Rev(it.to_owned())),
        )
        .context(
            "failed to lookup reference in preexisting repository, and can't check for updates in \
                 offline mode (--offline)",
        )?;
        return Ok((db, rev));
    }
    if context.net.offline {
        bail!("can't checkout from '{url}': you are in the offline mode (--offline)");
    }
    if !context.net.retry.quiet {
        let scope = if is_submodule { "submodule" } else { "repository" };
        // The address as said, never as asked: it may carry a token (AGENTS.md, Security).
        eprintln!("    Updating git {scope} `{}`", blaze_fetch::redact(url));
    }
    let locked = precise.map_or(reference.clone(), |it| GitReference::Rev(it.to_owned()));
    let rev = checkout_db(context, url, &db, exists, reference, &locked)?;
    Ok((db, rev))
}

/// cargo's `GitRemote::checkout`: fetch into the database there is, else into a new one.
fn checkout_db(
    context: &Context,
    url: &str,
    db: &Path,
    exists: bool,
    manifest: &GitReference,
    locked: &GitReference,
) -> anyhow::Result<String> {
    if exists {
        fetch_into(context, db, url, manifest, locked)
            .with_context(|| format!("failed to fetch into: {}", db.display()))?;
        if let Ok(rev) = resolve(db, locked.clone()) {
            return Ok(rev);
        }
    }
    if db.exists() {
        std::fs::remove_dir_all(db)
            .with_context(|| format!("failed to remove {}", db.display()))?;
    }
    std::fs::create_dir_all(db).with_context(|| format!("failed to create {}", db.display()))?;
    git(db, &["init", "--bare", "--quiet", "--template="])?;
    fetch_into(context, db, url, manifest, locked)
        .with_context(|| format!("failed to clone into: {}", db.display()))?;
    resolve(db, locked.clone())
}

enum FastPath {
    UpToDate,
    NeedsFetch(String),
    Indeterminate,
}

/// cargo's `fetch` in `src/sources/git/utils.rs`, the reference turned into refspecs.
fn fetch_into(
    context: &Context,
    db: &Path,
    url: &str,
    manifest: &GitReference,
    locked: &GitReference,
) -> anyhow::Result<()> {
    if context.net.offline {
        bail!("attempting to update a git repository, but --offline was specified");
    }
    let oid_to_fetch = match github_fast_path(context, db, url, locked) {
        Ok(FastPath::UpToDate) => return Ok(()),
        Ok(FastPath::NeedsFetch(oid)) => Some(oid),
        Ok(FastPath::Indeterminate) | Err(_) => None,
    };
    if count_packs(db) > PACKFILE_LIMIT {
        _ = git(db, &["gc", "--auto"]);
    }
    let mut refspecs = Vec::new();
    let mut tags = false;
    let mut fast_path_rev = false;
    match locked {
        GitReference::Branch(b) => {
            refspecs.push(format!("+refs/heads/{b}:refs/remotes/origin/{b}"))
        }
        GitReference::Tag(t) => {
            refspecs.push(format!("+refs/tags/{t}:refs/remotes/origin/tags/{t}"))
        }
        GitReference::DefaultBranch => refspecs.push("+HEAD:refs/remotes/origin/HEAD".to_owned()),
        GitReference::Rev(rev) if rev.starts_with("refs/") => {
            refspecs.push(format!("+{rev}:{rev}"))
        }
        GitReference::Rev(_) => match (&oid_to_fetch, manifest) {
            (Some(oid), _) => {
                fast_path_rev = true;
                refspecs.push(format!("+{oid}:refs/commit/{oid}"));
            }
            (None, GitReference::Rev(rev)) if rev.starts_with("refs/") => {
                refspecs.push(format!("+{rev}:{rev}"))
            }
            (None, _) => {
                refspecs.push("+refs/heads/*:refs/remotes/origin/*".to_owned());
                refspecs.push("+HEAD:refs/remotes/origin/HEAD".to_owned());
                tags = true;
            }
        },
    }
    let result = context.net.retry.run(|| fetch_with_cli(db, url, &refspecs, tags));
    match oid_to_fetch {
        Some(oid) if fast_path_rev => result.with_context(|| format!("revision {oid} not found")),
        _ => result,
    }
}

/// `git fetch --progress` of `refspecs` from `url` into `db`, watched by [`watch`]: in
/// blaze-fetch's downloads as `git fetch of <url>` while it runs, failed as a [`Transport`]
/// error (tried again) when git fails, and as [`Stalled`] (not tried again, as a stalled HTTP
/// download is not) when git prints nothing for [`STALL`].
fn fetch_with_cli(db: &Path, url: &str, refspecs: &[String], tags: bool) -> anyhow::Result<()> {
    let mut command = command(db);
    command
        .args(["-c", "core.fsmonitor=false", "fetch"])
        .arg(if tags { "--tags" } else { "--no-tags" })
        // `--progress` even with no terminal: its lines are what tells a live fetch from a
        // stuck one.
        .args(["--progress", "--force", "--update-head-ok", url])
        .args(refspecs)
        .env("GIT_DIR", db)
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    // The address as said, never as asked: it may carry a token (AGENTS.md, Security).
    let said = blaze_fetch::redact(url);
    // alloc: one per `git fetch`, the command as its error says it.
    let shown = format!("git fetch {said} {}", refspecs.join(" "));
    let download = blaze_fetch::downloads::Downloads::process().begin(
        // alloc: one per `git fetch`, what the status line names.
        &format!("git fetch of {said}"),
        url,
        None,
    );
    watch(&mut command, &download, &said, &shown)
}

/// `command` (a `git fetch` whose stderr is piped, `shown` as the error says it) run to its end:
/// its stderr read as it comes, on a thread of its own that hands each piece through a
/// crossbeam channel to this one, which waits on it with `recv_deadline` (as `blaze test`'s
/// runner waits on its executables' output); this is a plain thread, not a fiber, as the
/// `output()` it replaces was. Each piece is progress: `download` is told the bytes its
/// `Receiving objects` lines say came (and of a piece with none, so the build's stall bound
/// counts a line as progress). It ends in one of three ways: git's exit, which is an error
/// naming its status and the last [`TAIL`] lines of its stderr unless it succeeded; a read of
/// its stderr that fails; or [`STALL`] with nothing said, when git is killed and reaped and the
/// error is [`Stalled`] at the repository `said`, naming how long and how many bytes had come.
fn watch(
    command: &mut Command,
    download: &Download<'_>,
    said: &str,
    shown: &str,
) -> anyhow::Result<()> {
    let _waiting = tracing::info_span!(target: "blaze", "git fetch", repo = %said).entered();
    let mut child = command.spawn().context("could not run `git`")?;
    let stderr = child.stderr.take().expect("a piped stderr");
    let (send, pieces) = crossbeam_channel::unbounded();
    let reader = std::thread::Builder::new()
        .name("blaze git fetch stderr".into())
        .spawn(move || read_stderr(stderr, &send));
    if let Err(error) = reader {
        stop(&mut child);
        return Err(error)
            .with_context(|| format!("could not start a thread to read the stderr of `{shown}`"));
    }
    let mut stderr = Stderr::new();
    let mut last = Instant::now();
    loop {
        match pieces.recv_deadline(last + STALL) {
            Ok(Ok(piece)) => {
                last = Instant::now();
                download.got(stderr.push(&piece));
            }
            Ok(Err(error)) => {
                stop(&mut child);
                return Err(error).with_context(|| {
                    format!(
                        "could not read the stderr of `{shown}`, which was killed\n--- stderr\n{}",
                        stderr.tail()
                    )
                });
            }
            Err(RecvTimeoutError::Timeout) => {
                let idle = last.elapsed();
                stop(&mut child);
                return Err(anyhow::Error::new(Stalled {
                    url: said.to_owned(),
                    idle,
                    line: download.line(),
                })
                .context(format!(
                    "`{shown}` went silent and was killed\n--- stderr\n{}",
                    stderr.tail()
                )));
            }
            // Its stderr closed: git and every process it started that held it have ended.
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    let status = child.wait().with_context(|| format!("could not wait for `{shown}`"))?;
    if !status.success() {
        return Err(anyhow::Error::new(Transport(format!(
            "process didn't exit successfully: `{shown}` ({status})\n--- stderr\n{}",
            stderr.tail()
        ))));
    }
    Ok(())
}

/// A child killed and reaped. Either may fail only when it has already exited (and so was
/// reaped by `wait` or is about to be): nothing is left running either way.
fn stop(child: &mut Child) {
    _ = child.kill();
    _ = child.wait();
}

/// `stderr` read until it closes, each piece sent as it comes; a failed read is sent and ends
/// it, as does a receiver that has gone (the watcher gave up and killed git).
fn read_stderr(mut stderr: ChildStderr, send: &Sender<io::Result<Vec<u8>>>) {
    let mut chunk = [0; 8192];
    loop {
        let read = match stderr.read(&mut chunk) {
            Ok(0) => return,
            // alloc: one per read of git's stderr, a few a second while it fetches.
            Ok(n) => Ok(chunk[..n].to_vec()),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => Err(error),
        };
        let failed = read.is_err();
        if send.send(read).is_err() || failed {
            return;
        }
    }
}

/// How many of git's last stderr lines an error says.
const TAIL: usize = 40;

/// The longest stderr line kept; past it a line is cut.
const LINE: usize = 4096;

/// What git has said on its stderr: its last [`TAIL`] lines, a progress line it redraws in
/// place (ended by `\r`) kept only as last drawn, and the bytes its `Receiving objects` lines
/// say came.
struct Stderr {
    /// Each line, and whether it ended in `\r` (to be drawn over).
    lines: VecDeque<(String, bool)>,
    /// The line being said, not yet ended.
    open: Vec<u8>,
    received: u64,
}

impl Stderr {
    fn new() -> Stderr {
        // alloc: one per `git fetch`, its last lines for an error.
        Stderr { lines: VecDeque::with_capacity(TAIL), open: Vec::new(), received: 0 }
    }

    /// `piece` more of git's stderr: how many more bytes git now says it received.
    fn push(&mut self, piece: &[u8]) -> u64 {
        let mut more = 0;
        for &byte in piece {
            match byte {
                b'\r' | b'\n' => more += self.end_line(byte == b'\r'),
                _ if self.open.len() < LINE => self.open.push(byte),
                _ => {}
            }
        }
        more
    }

    fn end_line(&mut self, redrawn: bool) -> u64 {
        // alloc: one per line of git's stderr, the last `TAIL` kept.
        let text = String::from_utf8_lossy(&self.open).into_owned();
        self.open.clear();
        let more = received(&text).map_or(0, |now| {
            let more = now.saturating_sub(self.received);
            self.received = self.received.max(now);
            more
        });
        if text.trim().is_empty() {
            return more;
        }
        if matches!(self.lines.back(), Some((_, true))) {
            self.lines.pop_back();
        }
        if self.lines.len() == TAIL {
            self.lines.pop_front();
        }
        self.lines.push_back((text, redrawn));
        more
    }

    /// The lines kept, and the one not yet ended, for an error.
    fn tail(&self) -> String {
        let open = String::from_utf8_lossy(&self.open);
        let lines = self.lines.iter().map(|(it, _)| it.as_str());
        // alloc: an error path.
        let all: Vec<&str> = lines.chain((!open.trim().is_empty()).then_some(&*open)).collect();
        all.join("\n")
    }
}

/// The bytes a `Receiving objects:  45% (1234/2742), 12.30 MiB | 1.20 MiB/s` line says came:
/// the size before its ` | `, as git's `strbuf_humanise_bytes` says one (`N bytes`, `X.XX KiB`,
/// `MiB`, `GiB`). Read by its shape rather than its words, so a git that speaks another
/// language is read too (a `,` taken as the decimal point as well). `None` for any other line.
fn received(line: &str) -> Option<u64> {
    let (before, _) = line.split_once(" | ")?;
    let size = before.rsplit(", ").next()?.trim();
    let (number, unit) = size.split_once(' ')?;
    let scale: u64 = match unit {
        "byte" | "bytes" => 1,
        "KiB" => 1 << 10,
        "MiB" => 1 << 20,
        "GiB" => 1 << 30,
        "TiB" => 1 << 40,
        _ => return None,
    };
    let (whole, fraction) = number.split_once(['.', ',']).unwrap_or((number, ""));
    let whole: u64 = whole.parse().ok()?;
    let parts = match fraction {
        "" => 0,
        digits if digits.len() <= 4 && digits.bytes().all(|it| it.is_ascii_digit()) => {
            digits.parse::<u64>().ok()? * scale / 10u64.pow(digits.len() as u32)
        }
        _ => return None,
    };
    whole.checked_mul(scale)?.checked_add(parts)
}

/// cargo's `github_fast_path`: whether GitHub says the database is up to date, or which commit
/// to fetch, without fetching.
fn github_fast_path(
    context: &Context,
    db: &Path,
    url: &str,
    reference: &GitReference,
) -> anyhow::Result<FastPath> {
    let url = Url::parse(url)?;
    if url.host_str() != Some("github.com") {
        return Ok(FastPath::Indeterminate);
    }
    let local = resolve(db, reference.clone()).ok();
    let name = match reference {
        GitReference::Branch(it) | GitReference::Tag(it) => it.as_str(),
        GitReference::DefaultBranch => "HEAD",
        GitReference::Rev(rev) if rev.starts_with("refs/") => rev,
        GitReference::Rev(rev)
            if rev.len() >= 7 && rev.bytes().all(|it| it.is_ascii_hexdigit()) =>
        {
            if let Some(local) = &local
                && local.get(..rev.len()).is_some_and(|it| it.eq_ignore_ascii_case(rev))
            {
                return Ok(FastPath::UpToDate);
            }
            if is_full_oid(rev) {
                return Ok(FastPath::NeedsFetch(rev.to_lowercase()));
            }
            rev
        }
        GitReference::Rev(_) => return Ok(FastPath::Indeterminate),
    };
    let mut pieces = url.path_segments().context("no path segments on url")?;
    let user = pieces.next().context("couldn't find username")?;
    let repository = pieces.next().context("couldn't find repository name")?;
    if pieces.next().is_some() {
        bail!("too many segments on URL");
    }
    let repository = repository.strip_suffix(".git").unwrap_or(repository);
    let api = format!("https://api.github.com/repos/{user}/{repository}/commits/{name}");
    let condition = local.as_ref().map(|it| format!("\"{it}\""));
    let mut headers = vec![("accept", "application/vnd.github.3.sha")];
    if let Some(condition) = &condition {
        headers.push(("if-none-match", condition));
    }
    let response = context.http.get(&api, &headers)?;
    Ok(match response.status {
        304 => FastPath::UpToDate,
        200 => match std::str::from_utf8(&response.body) {
            Ok(body) if is_full_oid(body) => FastPath::NeedsFetch(body.to_owned()),
            _ => FastPath::Indeterminate,
        },
        _ => FastPath::Indeterminate,
    })
}

fn count_packs(db: &Path) -> usize {
    std::fs::read_dir(db.join("objects/pack")).map_or(0, |it| {
        it.filter_map(Result::ok)
            .filter(|it| it.path().extension().is_some_and(|e| e == "pack"))
            .count()
    })
}

/// cargo's `resolve_ref`: the commit a reference names in the database.
fn resolve(db: &Path, reference: GitReference) -> anyhow::Result<String> {
    match reference {
        GitReference::Tag(tag) => {
            rev_parse(db, &format!("refs/remotes/origin/tags/{tag}^{{commit}}"))
                .with_context(|| format!("failed to find tag `{tag}`"))
        }
        GitReference::Branch(branch) => rev_parse(db, &format!("refs/remotes/origin/{branch}"))
            .with_context(|| format!("failed to find branch `{branch}`")),
        GitReference::DefaultBranch => rev_parse(db, "refs/remotes/origin/HEAD^{commit}"),
        GitReference::Rev(rev) => {
            if rev.starts_with('-') {
                bail!("revspec '{rev}' not found");
            }
            let oid = rev_parse(db, &rev)?;
            // An annotated tag is taken for its target, one level down, as libgit2's
            // `revparse_single` and `Tag::target_id`.
            if git(db, &["cat-file", "-t", &oid])?.trim() == "tag" {
                let tag = git(db, &["cat-file", "tag", &oid])?;
                let target = tag
                    .lines()
                    .find_map(|it| it.strip_prefix("object "))
                    .context("a tag object")?;
                return Ok(target.to_owned());
            }
            Ok(oid)
        }
    }
}

fn rev_parse(db: &Path, spec: &str) -> anyhow::Result<String> {
    Ok(git(db, &["rev-parse", "--verify", "--quiet", "--end-of-options", spec])
        .with_context(|| format!("revspec '{spec}' not found"))?
        .trim()
        .to_owned())
}

fn contains(db: &Path, oid: &str) -> bool {
    git(db, &["cat-file", "-e", oid]).is_ok()
}

/// A commit's hex, seven digits or more where seven would name more than one object (cargo's
/// `to_short_id`, which ignores `core.abbrev`).
fn short_id(db: &Path, rev: &str) -> anyhow::Result<String> {
    Ok(git(db, &["rev-parse", "--short=7", rev])?.trim().to_owned())
}

fn is_repo(dir: &Path) -> bool {
    dir.exists() && git(dir, &["rev-parse", "--git-dir"]).is_ok()
}

/// cargo's `GitDatabase::copy_to`: the checkout as it is when its `HEAD` is `rev` and it has
/// `.cargo-ok`, else cloned again from the database.
fn copy_to(context: &Context, db: &Path, rev: &str, dest: &Path, url: &str) -> anyhow::Result<()> {
    let fresh = dest.join(CHECKOUT_READY_LOCK).exists()
        && local(dest, &["rev-parse", "--verify", "--quiet", "HEAD"])
            .is_ok_and(|it| it.trim() == rev);
    if fresh {
        return Ok(());
    }
    let parent = dest.parent().expect("a checkout is in a directory");
    std::fs::create_dir_all(parent)
        .with_context(|| format!("failed to create {}", parent.display()))?;
    if dest.exists() {
        std::fs::remove_dir_all(dest)
            .with_context(|| format!("failed to remove {}", dest.display()))?;
    }
    let from = db.to_str().context("a database path is UTF-8")?;
    let into = dest.to_str().context("a checkout path is UTF-8")?;
    local(parent, &["clone", "--local", "--no-checkout", "--quiet", "--template=", from, into])?;
    local(dest, &["config", "core.autocrlf", "false"])?;
    local(dest, &["reset", "--hard", "--quiet", rev])?;
    update_submodules(context, dest, url)?;
    std::fs::File::create(dest.join(CHECKOUT_READY_LOCK)).with_context(|| {
        format!("failed to create {}", dest.join(CHECKOUT_READY_LOCK).display())
    })?;
    Ok(())
}

/// cargo's `GitCheckout::update_submodules`: each submodule of the checkout fetched into a
/// database of its own and checked out where the checkout has it, recursively.
fn update_submodules(context: &Context, repo: &Path, parent_url: &str) -> anyhow::Result<()> {
    if !repo.join(".gitmodules").exists() {
        return Ok(());
    }
    let listed =
        local(repo, &["config", "-f", ".gitmodules", "--get-regexp", r"^submodule\..*\.path$"])
            .unwrap_or_default();
    for line in listed.lines() {
        let Some((key, path)) = line.split_once(' ') else { continue };
        let name = &key["submodule.".len()..key.len() - ".path".len()];
        update_submodule(context, repo, name, path, parent_url)
            .with_context(|| format!("failed to update submodule `{name}`"))?;
    }
    Ok(())
}

fn update_submodule(
    context: &Context,
    repo: &Path,
    name: &str,
    path: &str,
    parent_url: &str,
) -> anyhow::Result<()> {
    let key = |it: &str| format!("submodule.{name}.{it}");
    let url = local(repo, &["config", "-f", ".gitmodules", "--get", &key("url")])
        .map(|it| it.trim().to_owned())
        .map_err(|_| anyhow::anyhow!("unable to update submodule `{name}` without a path"))?;
    if local(repo, &["config", "-f", ".gitmodules", "--get", &key("update")])
        .is_ok_and(|it| it.trim() == "none")
    {
        if !context.net.retry.quiet {
            eprintln!(
                "    Skipping git submodule `{}` due to update strategy in .gitmodules",
                blaze_fetch::redact(&url)
            );
        }
        return Ok(());
    }
    let child_url = absolute_submodule_url(parent_url, &url)?;
    let listed = local(repo, &["ls-tree", "HEAD", "--", path])?;
    let Some(head) = listed
        .split_whitespace()
        .collect::<Vec<_>>()
        .chunks(4)
        .find(|it| it.len() == 4 && it[1] == "commit")
        .map(|it| it[2].to_owned())
    else {
        return Ok(());
    };
    let dest = repo.join(path);
    let id = SourceId::for_git(
        &Url::parse(&child_url).with_context(|| format!("invalid url `{child_url}`"))?,
        GitReference::Rev(head.clone()),
    )?;
    let (db, rev) = fetch_db(
        context,
        &child_url,
        &id.git_ident(),
        &GitReference::Rev(head.clone()),
        Some(&head),
        true,
    )
    .with_context(|| format!("failed to fetch submodule `{name}` from {child_url}"))?;
    copy_to(context, &db, &rev, &dest, &child_url)
}

/// cargo's `absolute_submodule_url`: a submodule's URL that starts with `./` or `../` taken
/// against its parent's.
fn absolute_submodule_url(base: &str, submodule: &str) -> anyhow::Result<String> {
    if !submodule.starts_with("./") && !submodule.starts_with("../") {
        return Ok(submodule.to_owned());
    }
    Ok(match Url::parse(base) {
        Ok(mut base) => {
            let path = base.path().to_owned();
            if !path.ends_with('/') {
                base.set_path(&format!("{path}/"));
            }
            base.join(submodule)
                .with_context(|| {
                    format!(
                        "failed to parse relative child submodule url `{submodule}` using parent base url `{base}`"
                    )
                })?
                .to_string()
        }
        Err(_) => {
            let mut url = base.to_owned();
            if !url.ends_with('/') {
                url.push('/');
            }
            url + submodule
        }
    })
}

#[expect(clippy::disallowed_methods, reason = "every `git` is run in the repository it is about")]
fn command(dir: &Path) -> Command {
    let mut command = Command::new("git");
    command.current_dir(dir);
    command
}

/// The git index at cargo's `registry/index/<name>/.git`, read without a checkout.
pub(crate) fn registry_command(db: &Path) -> Command {
    let mut command = command(db);
    command
        .env("GIT_DIR", db)
        // Reading a partial index must not implicitly fetch missing objects offline.
        .env("GIT_NO_LAZY_FETCH", "1")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .env("GIT_COMMITTER_NAME", "blaze")
        .env("GIT_COMMITTER_EMAIL", "blaze@localhost");
    command
}

/// The git index `url`'s default branch fetched into `db` (made when there is none), unless
/// GitHub says it is up to date: a `git fetch` watched as [`fetch_with_cli`] watches one.
pub(crate) fn fetch_registry(context: &Context, url: &str, db: &Path) -> anyhow::Result<()> {
    if !db.join("HEAD").exists() {
        std::fs::create_dir_all(db)?;
        run(registry_command(db).args(["init", "--quiet", "--template="]))?;
    }
    if matches!(
        github_fast_path(context, db, url, &GitReference::DefaultBranch)?,
        FastPath::UpToDate
    ) {
        return Ok(());
    }
    context
        .net
        .retry
        .run(|| fetch_with_cli(db, url, &["+HEAD:refs/remotes/origin/HEAD".to_owned()], false))
}

/// `git` in a database: the user's configuration counts, for credentials and `insteadOf`.
fn git(db: &Path, args: &[&str]) -> anyhow::Result<String> {
    run(command(db).env("GIT_DIR", db).args(args))
}

/// `git` on a checkout, with none of the user's or the system's configuration, as cargo clones
/// and resets with an empty configuration: no filter driver (Git LFS) changes a file.
fn local(dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    let mut command = command(dir);
    command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", if cfg!(windows) { "NUL" } else { "/dev/null" })
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        // With no configuration there is no name or email for the reflog, and git, making one
        // up, asks DNS for the machine's domain name. Nothing here is committed.
        .env("GIT_COMMITTER_NAME", "blaze")
        .env("GIT_COMMITTER_EMAIL", "blaze@localhost");
    run(command.args(args))
}

fn run(command: &mut Command) -> anyhow::Result<String> {
    let output = command.output().context("could not run `git`")?;
    if !output.status.success() {
        bail!(
            "`git {}` failed ({}): {}",
            command.get_args().map(|it| it.to_string_lossy()).collect::<Vec<_>>().join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim_end()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// A package of a checkout: its name, its version when the manifest writes it out, and its
/// directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub name: String,
    pub version: Option<String>,
    pub dir: PathBuf,
}

impl Checkout {
    /// The packages of the checkout as cargo's `read_packages` finds them: every directory
    /// with a `Cargo.toml` that has a `[package]`, but directories whose names start with `.`,
    /// submodules, and `target` beside a manifest, and the `path` dependencies of each, which
    /// may be in a submodule. A manifest that does not parse is skipped, as cargo skips it.
    pub fn packages(&self) -> anyhow::Result<Vec<Found>> {
        let mut found = Vec::new();
        let mut visited = blaze_hash::HashSet::new_in(std::alloc::Global);
        walk(&self.root, &self.root, &mut found, &mut visited)?;
        Ok(found)
    }

    /// The directory of the package `name` at `version`: the first package of that name whose
    /// version is that or is inherited from its workspace.
    pub fn package(&self, name: &str, version: &semver::Version) -> anyhow::Result<PathBuf> {
        let version = version.to_string();
        let found = self.packages()?;
        let named = |exact: bool| {
            found.iter().find(|it| {
                it.name == name
                    && if exact {
                        it.version.as_deref() == Some(&version)
                    } else {
                        it.version.is_none()
                    }
            })
        };
        match named(true).or_else(|| named(false)) {
            Some(it) => Ok(it.dir.clone()),
            None => bail!("no package `{name} v{version}` in {}", self.root.display()),
        }
    }
}

fn walk(
    root: &Path,
    dir: &Path,
    found: &mut Vec<Found>,
    visited: &mut blaze_hash::HashSet<PathBuf, std::alloc::Global>,
) -> anyhow::Result<()> {
    if dir != root {
        if dir.file_name().and_then(|it| it.to_str()).is_some_and(|it| it.starts_with('.')) {
            return Ok(());
        }
        if dir.join(".git").exists() {
            return Ok(());
        }
    }
    if dir.file_name().is_some_and(|it| it == "target")
        && dir.parent().is_some_and(|it| it.join("Cargo.toml").is_file())
    {
        return Ok(());
    }
    if dir.join("Cargo.toml").is_file() {
        read_nested(dir, found, visited);
    }
    let mut entries: Vec<_> = match std::fs::read_dir(dir) {
        Ok(it) => it.filter_map(Result::ok).collect(),
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => return Ok(()),
        Err(err) => {
            return Err(err)
                .with_context(|| format!("failed to read directory `{}`", dir.display()));
        }
    };
    entries.sort_by_key(|it| it.file_name());
    for entry in entries {
        if entry.file_type()?.is_dir() {
            walk(root, &entry.path(), found, visited)?;
        }
    }
    Ok(())
}

fn read_nested(
    dir: &Path,
    found: &mut Vec<Found>,
    visited: &mut blaze_hash::HashSet<PathBuf, std::alloc::Global>,
) {
    if !visited.insert(dir.to_owned()) {
        return;
    }
    let Ok(text) = std::fs::read_to_string(dir.join("Cargo.toml")) else { return };
    let Ok(manifest) = toml::from_str::<toml::Table>(&text) else { return };
    let Some(package) = manifest.get("package").and_then(|it| it.as_table()) else { return };
    let Some(name) = package.get("name").and_then(|it| it.as_str()) else { return };
    let version = match package.get("version") {
        None => Some("0.0.0".to_owned()),
        Some(toml::Value::String(it)) => Some(it.clone()),
        Some(_) => None,
    };
    found.push(Found { name: name.to_owned(), version, dir: dir.to_owned() });
    for path in nested_paths(&manifest) {
        read_nested(&blaze_manifest::paths::normalize_path(&dir.join(path)), found, visited);
    }
}

/// The `path` of every dependency the manifest writes out (not one inherited from its
/// workspace), in every dependency table and every `[target.<cfg>]`.
fn nested_paths(manifest: &toml::Table) -> Vec<String> {
    const TABLES: [&str; 5] = [
        "dependencies",
        "build-dependencies",
        "build_dependencies",
        "dev-dependencies",
        "dev_dependencies",
    ];
    let mut tables: Vec<&toml::Table> =
        TABLES.iter().filter_map(|it| manifest.get(*it).and_then(|it| it.as_table())).collect();
    for target in
        manifest.get("target").and_then(|it| it.as_table()).into_iter().flat_map(|it| it.values())
    {
        tables.extend(TABLES.iter().filter_map(|it| target.get(*it).and_then(|it| it.as_table())));
    }
    tables
        .into_iter()
        .flat_map(|it| it.values())
        .filter_map(|dep| dep.as_table())
        .filter(|dep| dep.get("workspace").is_none())
        .filter_map(|dep| dep.get("path").and_then(|it| it.as_str()).map(str::to_owned))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The copy this file had, which dropped a leading `..` of a relative path; kept to check
    /// cargo-util's rule agrees on the absolute paths `read_nested` is given.
    fn old_normalize(path: &Path) -> PathBuf {
        let mut out = PathBuf::new();
        for part in path.components() {
            match part {
                std::path::Component::CurDir => {}
                std::path::Component::ParentDir => {
                    out.pop();
                }
                other => out.push(other),
            }
        }
        out
    }

    #[cfg(unix)]
    #[test]
    fn a_nested_path_is_normal_as_it_was() {
        let parts = ["a", ".", "..", "b"];
        let mut paths = vec![PathBuf::from("/w")];
        for _ in 0..4 {
            let longer: Vec<PathBuf> =
                paths.iter().flat_map(|it| parts.iter().map(move |part| it.join(part))).collect();
            paths.extend(longer);
        }
        paths.push(PathBuf::from("/w/a").join("/x/../y"));
        for path in &paths {
            assert_eq!(
                blaze_manifest::paths::normalize_path(path),
                old_normalize(path),
                "{path:?}"
            );
        }
    }

    /// git's progress as it says it: the bytes received from `Receiving objects` lines only,
    /// counted as they grow; a line redrawn with `\r` kept once, as last drawn; other lines
    /// kept for the error.
    #[test]
    fn git_progress_reads_as_bytes_and_a_tail() {
        let mib = 1u64 << 20;
        let line = "Receiving objects:  45% (1234/2742), 12.30 MiB | 1.20 MiB/s";
        assert_eq!(received(line), Some(12 * mib + 30 * mib / 100));
        let line = "Receiving objects: 100% (9/9), 512 bytes | 512.00 KiB/s, done.";
        assert_eq!(received(line), Some(512));
        assert_eq!(received("remote: Counting objects:  50% (5/10)"), None);
        let mut stderr = Stderr::new();
        let mut got = stderr.push(b"remote: Enumerating objects: 10, done.\n");
        got += stderr.push(b"Receiving objects:  10% (1/9), 1.00 KiB | 1.00");
        got += stderr.push(b" KiB/s\rReceiving objects:  50% (4/9), 3.00 KiB | 2.00 KiB/s\r");
        assert_eq!(got, 3 << 10);
        got += stderr.push(b"Receiving objects: 100% (9/9), 3.00 KiB | 2.00 KiB/s, done.\n");
        got += stderr.push(b"fatal: gone");
        assert_eq!(got, 3 << 10, "a size said again is no more bytes");
        assert_eq!(
            stderr.tail(),
            "remote: Enumerating objects: 10, done.\n\
             Receiving objects: 100% (9/9), 3.00 KiB | 2.00 KiB/s, done.\nfatal: gone"
        );
    }

    #[test]
    fn submodule_urls() {
        let base = "https://github.com/o/r";
        assert_eq!(absolute_submodule_url(base, "../s").unwrap(), "https://github.com/o/s");
        assert_eq!(absolute_submodule_url(base, "./s").unwrap(), "https://github.com/o/r/s");
        assert_eq!(absolute_submodule_url(base, "https://x.org/y").unwrap(), "https://x.org/y");
    }
}
