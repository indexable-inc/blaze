//! The lock policy (docs/v2/plan/design/lock-policy.md, docs/cargo.md "Lock files"): whether a build may
//! change its lock files, what it says when it does, and `blaze.lock`, which pins what blaze
//! builds a workspace with besides the crates `Cargo.lock` pins.
//!
//! - [`policy`]: the decision table, a pure function of the command line (`--locked`,
//!   `--frozen`, `--no-locked`), `CI` and the workspace's `lock = "required"`.
//! - [`BlazeLock`]: `blaze.lock`, read ([`BlazeLock::parse`]) and written ([`BlazeLock::render`]);
//!   what blaze writes reads back to the same value and renders to the same bytes.
//! - [`moves`] and [`pin_moves`]: what a build would change of each lock file, each line naming
//!   the package or pin, old and new, and why.
//! - [`decide`]: the policy applied to those changes: nothing to do, the files to write with
//!   what to say, or the refusal.
//!
//! - [`after_resolve`]: all of it for one resolve, the one place a command's lock files are
//!   decided (a build's load, blaze-metadata's commands).
//!
//! Nothing here reads a clock, and files only through the [`Files`] it is handed.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use blaze_manifest::Workspace;
use blaze_manifest::files::Files;
use blaze_resolve::lockfile::{self, PathDeps};
use blaze_resolve::{PackageId, Resolve, RustVersion};

use crate::query::{LoadError, LockFile, LockWrite};

/// Where `blaze.lock` is: beside `Cargo.lock`, at the workspace's root.
pub const BLAZE_LOCK: &str = "blaze.lock";

/// The Rust this blaze builds with, as `harness/toolchain` names it (`nightly-2026-10-27`).
const TOOLCHAIN: &str = include_str!("../../../harness/toolchain");

/// [`TOOLCHAIN`] without its newline.
pub fn toolchain() -> &'static str {
    TOOLCHAIN.trim()
}

/// Whether `version` is a release's version, `0.YYYYMMDD.N`: `0`, a date of eight digits and a
/// number, so that it names a directory and an asset and nothing else, and its date is always
/// there to read. A build of the tree (`0.0.0-dev`) is none. The one rule for it: the launcher's
/// pin (blaze-engine's `pin.rs`) and `blaze.lock` both ask it.
pub fn is_version(version: &str) -> bool {
    let digits = |it: &str| !it.is_empty() && it.bytes().all(|b| b.is_ascii_digit());
    let mut parts = version.split('.');
    let (Some("0"), Some(date), Some(n), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    date.len() == 8 && digits(date) && digits(n)
}

/// Whether the environment says this runs on CI, the one rule for it (`--locked` by default,
/// `blaze test`'s live view off): `CI` set to anything but empty, `0` or `false` (any case;
/// GitHub Actions, GitLab, Buildkite, CircleCI, Travis and most others set it), or `TF_BUILD`
/// set (Azure Pipelines, which sets no `CI`).
pub fn ci_says(ci: Option<&str>, tf_build: Option<&str>) -> bool {
    let ci = ci.map(str::trim).is_some_and(|it| !(it.is_empty() || it == "0" || it.eq_ignore_ascii_case("false")));
    ci || tf_build.is_some_and(|it| !it.is_empty())
}

/// What the command line and the environment say of the lock files.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct LockAsk {
    /// `--locked` or `--frozen`, as typed (`--frozen` wins).
    pub flag: Option<&'static str>,
    /// `--no-locked`: this build may change the lock files whatever `CI` or the workspace says.
    pub no_locked: bool,
    /// The build runs on CI ([`ci_says`]).
    pub ci: bool,
    /// `BLAZE_LOCK=write` ([`LOCK_ENV`]): the environment says every build under it may write
    /// its lock files, as `--no-locked` would, except that a typed `--locked` or `--frozen`
    /// wins. A harness that makes throwaway workspaces with no lock says so once, where it
    /// starts its children, instead of passing `--no-locked` to each command.
    pub env_writes: bool,
}

/// The variable a harness sets to `write` so the builds under it may write their lock files
/// ([`LockAsk::env_writes`]).
pub const LOCK_ENV: &str = "BLAZE_LOCK";

/// What `BLAZE_LOCK` says: unset or empty is nothing, `write` lets builds write their lock
/// files, and anything else is an error naming the variable and its one value.
pub fn env_writes(value: Option<&str>) -> Result<bool, String> {
    match value.map(str::trim) {
        None | Some("") => Ok(false),
        Some("write") => Ok(true),
        Some(other) => Err(format!("{LOCK_ENV}={other:?}: the one value it takes is `write` (docs/cargo.md, \"Lock files\")")),
    }
}

/// Why a build may not change its lock files.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// `--locked` or `--frozen`, as typed.
    Flag(&'static str),
    /// `CI` is set, which makes `--locked` the default.
    Ci,
    /// The workspace says `lock = "required"` under `[workspace.metadata.blaze]`.
    Required,
}

impl Refusal {
    /// Why, in a clause: "`CI` is set, which makes --locked the default".
    fn because(self, manifest: &str) -> String {
        match self {
            Refusal::Flag(flag) => format!("{flag} was passed"),
            Refusal::Ci => "`CI` is set, which makes --locked the default".to_owned(),
            Refusal::Required => format!("{manifest} says `lock = \"required\"` under [workspace.metadata.blaze]"),
        }
    }
}

/// What a build may do with its lock files.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Policy {
    /// Write what changes, and say what it wrote.
    Write,
    /// Change nothing: a change is an error naming what would move.
    Refuse(Refusal),
}

/// The decision table (docs/cargo.md, "Lock files"), `required` being the workspace's
/// `lock = "required"`:
///
/// | `--locked`/`--frozen` | `--no-locked` or `BLAZE_LOCK=write` | CI  | required | policy                |
/// |-----------------------|-------------------------------------|-----|----------|-----------------------|
/// | yes                   | `--no-locked`                       | any | any      | error: opposite flags |
/// | yes                   | `BLAZE_LOCK=write` or neither       | any | any      | refuse (the flag)     |
/// | no                    | either                              | any | any      | write                 |
/// | no                    | neither                             | yes | any      | refuse (CI)           |
/// | no                    | neither                             | no  | yes      | refuse (required)     |
/// | no                    | neither                             | no  | no       | write                 |
pub fn policy(ask: LockAsk, required: bool) -> Result<Policy, String> {
    match (ask.flag, ask.no_locked) {
        (Some(flag), true) => Err(format!("{flag} and --no-locked say opposite things of the lock files: pass one of them")),
        (Some(flag), false) => Ok(Policy::Refuse(Refusal::Flag(flag))),
        (None, true) => Ok(Policy::Write),
        (None, false) if ask.env_writes => Ok(Policy::Write),
        (None, false) if ask.ci => Ok(Policy::Refuse(Refusal::Ci)),
        (None, false) if required => Ok(Policy::Refuse(Refusal::Required)),
        (None, false) => Ok(Policy::Write),
    }
}

/// The key under `[workspace.metadata.blaze]` that makes a workspace's lock files required.
pub const REQUIRED_KEY: &str = "lock";

/// Whether the root manifest at `manifest` (its `[workspace.metadata]` is `metadata`) says
/// `lock = "required"`; `"write"` is the default said out loud. Anything else is an error
/// naming the key and the manifest.
pub fn required(metadata: Option<&toml::Value>, manifest: &str) -> Result<bool, String> {
    let Some(value) = metadata.and_then(|it| it.get("blaze")).and_then(|it| it.get(REQUIRED_KEY)) else {
        return Ok(false);
    };
    match value.as_str() {
        Some("required") => Ok(true),
        Some("write") => Ok(false),
        _ => Err(format!(
            "`{REQUIRED_KEY}` under [workspace.metadata.blaze] in {manifest} is {value}: it is \"required\" (a build \
             may not change the lock files) or \"write\" (the default: a build writes them and says so)"
        )),
    }
}

/// Which blaze built a workspace: the release and the Rust it ships.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    /// `0.YYYYMMDD.N`.
    pub release: String,
    /// As `harness/toolchain` names it: `nightly-2026-10-27`.
    pub toolchain: String,
}

/// One pinned native input (`[workspace.metadata.blaze.native.<name>]`), every target its pin
/// file names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativePin {
    pub name: String,
    /// The pin file, relative to the workspace's root (absolute when it is outside it).
    pub pin: String,
    /// Each target triple's archive sha256, sorted by triple.
    pub targets: Vec<(String, String)>,
}

/// `blaze.lock`: what blaze pinned to build a workspace besides its crates. Every part is
/// optional; a lock with no part is never written.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct BlazeLock {
    /// The release that wrote it, when that was a release run as the workspace pins it.
    pub blaze: Option<Identity>,
    /// The newest publish time (`yyyy-mm-ddThh:mm:ssZ`) of any package a resolve took from a
    /// registry's index: the index was at least that new when the crates were locked.
    pub index: Option<String>,
    /// Sorted by name.
    pub native: Vec<NativePin>,
}

/// The `version` of the format [`BlazeLock::render`] writes and [`BlazeLock::parse`] reads.
const FORMAT: i64 = 1;

const HEADER: &str = "# blaze.lock: what blaze builds this workspace with besides the crates in Cargo.lock.\n\
                      # blaze writes it (`blaze lock`); commit it. docs/cargo.md, \"Lock files\".\n";

/// A TOML string, as the `toml` crate quotes it.
fn string(text: &str) -> String {
    toml::Value::String(text.to_owned()).to_string()
}

/// A TOML key: bare when it can be, else quoted.
fn key(text: &str) -> String {
    let bare = !text.is_empty() && text.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if bare { text.to_owned() } else { string(text) }
}

impl BlazeLock {
    /// Whether it pins nothing, and so is not written.
    pub fn is_empty(&self) -> bool {
        self.blaze.is_none() && self.index.is_none() && self.native.is_empty()
    }

    /// The text of `blaze.lock`: the header, `version`, then `[blaze]`, `[index]` and each
    /// `[native.<name>]` with its `targets`, each part only when it is there, in that order,
    /// inputs by name and targets by triple.
    pub fn render(&self) -> String {
        let mut out = String::from(HEADER); // alloc: the lock file's text, once per write
        let _ = writeln!(out, "version = {FORMAT}");
        if let Some(blaze) = &self.blaze {
            out.push_str("\n[blaze]\n");
            let _ = writeln!(out, "release = {}", string(&blaze.release));
            let _ = writeln!(out, "toolchain = {}", string(&blaze.toolchain));
        }
        if let Some(newest) = &self.index {
            out.push_str("\n[index]\n");
            let _ = writeln!(out, "newest = {}", string(newest));
        }
        let mut native: Vec<&NativePin> = self.native.iter().collect(); // alloc: a few inputs, once per write
        native.sort_by(|a, b| a.name.cmp(&b.name));
        for input in native {
            let name = key(&input.name);
            let _ = write!(out, "\n[native.{name}]\npin = {}\n", string(&input.pin));
            let _ = write!(out, "\n[native.{name}.targets]\n");
            let mut targets: Vec<&(String, String)> = input.targets.iter().collect(); // alloc: as above
            targets.sort();
            for (target, sha256) in targets {
                let _ = writeln!(out, "{} = {}", key(target), string(sha256));
            }
        }
        out
    }

    /// `blaze.lock` read from `text`, the file at `path` (for errors). Every key is known and
    /// every value of its type; anything else is an error naming the file and the key, with
    /// the fix. A lock of another format version is refused, never guessed at.
    pub fn parse(text: &str, path: &str) -> Result<BlazeLock, String> {
        let bad = |what: String| format!("{path} {what}; `blaze lock` writes it again");
        let table: toml::Table = toml::from_str(text).map_err(|error| bad(format!("is not TOML: {error}")))?;
        let mut lock = BlazeLock::default();
        let mut version = None;
        let strings = |table: &toml::Value, at: &str| -> Result<Vec<(String, String)>, String> {
            let table = table.as_table().ok_or_else(|| bad(format!("has `{at}`, which is not a table")))?;
            table
                .iter()
                .map(|(name, value)| match value.as_str() {
                    Some(text) => Ok((name.clone(), text.to_owned())),
                    None => Err(bad(format!("has `{at}.{name}`, which is not a string"))),
                })
                .collect()
        };
        for (name, value) in &table {
            match name.as_str() {
                "version" => version = Some(value.as_integer().ok_or_else(|| bad("has a `version` that is not a number".to_owned()))?),
                "blaze" => {
                    let (mut release, mut toolchain) = (None, None);
                    for (name, text) in strings(value, "blaze")? {
                        match name.as_str() {
                            "release" => release = Some(text),
                            "toolchain" => toolchain = Some(text),
                            other => return Err(bad(format!("has `blaze.{other}`, which is not known (`release` and `toolchain` are)"))),
                        }
                    }
                    let (Some(release), Some(toolchain)) = (release, toolchain) else {
                        return Err(bad("has a `[blaze]` without both `release` and `toolchain`".to_owned()));
                    };
                    lock.blaze = Some(Identity { release, toolchain });
                }
                "index" => {
                    for (name, text) in strings(value, "index")? {
                        match name.as_str() {
                            "newest" => lock.index = Some(text),
                            other => return Err(bad(format!("has `index.{other}`, which is not known (`newest` is)"))),
                        }
                    }
                }
                "native" => {
                    let inputs = value.as_table().ok_or_else(|| bad("has `native`, which is not a table".to_owned()))?;
                    for (input, fields) in inputs {
                        let fields = fields.as_table().ok_or_else(|| bad(format!("has `native.{input}`, which is not a table")))?;
                        let (mut pin, mut targets) = (None, None);
                        for (field, value) in fields {
                            match field.as_str() {
                                "pin" => {
                                    let text = value.as_str().ok_or_else(|| bad(format!("has `native.{input}.pin`, which is not a string")))?;
                                    pin = Some(text.to_owned());
                                }
                                "targets" => targets = Some(strings(value, &format!("native.{input}.targets"))?),
                                other => {
                                    return Err(bad(format!(
                                        "has `native.{input}.{other}`, which is not known (`pin` and `targets` are)"
                                    )));
                                }
                            }
                        }
                        let (Some(pin), Some(mut targets)) = (pin, targets) else {
                            return Err(bad(format!("has a `[native.{input}]` without both `pin` and `targets`")));
                        };
                        targets.sort();
                        lock.native.push(NativePin { name: input.clone(), pin, targets });
                    }
                }
                other => {
                    return Err(bad(format!("has `{other}`, which is not known (`version`, `blaze`, `index` and `native` are)")));
                }
            }
        }
        match version {
            Some(FORMAT) => {}
            Some(other) => return Err(bad(format!("is of format version {other}, and this blaze reads version {FORMAT}"))),
            None => return Err(bad("has no `version`".to_owned())),
        }
        lock.native.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(lock)
    }
}

/// A package as a move names it: its name, and its source unless that is crates.io.
fn package(id: PackageId) -> String {
    match id.source_id().is_crates_io() {
        true => id.name().to_string(),
        false => format!("{} ({})", id.name(), id.source_id().without_precise()),
    }
}

/// A package's version as a move names it, with its commit for a git package.
fn version(id: PackageId) -> String {
    match id.source_id().precise_git_fragment() {
        Some(commit) => format!("{} ({})", id.version(), &commit[..commit.len().min(8)]),
        None => id.version().to_string(),
    }
}

/// Each package of `resolve` that depends on `target`, sorted, with each requirement of that
/// dependency as its manifest writes it.
fn asked_by(resolve: &Resolve, target: PackageId) -> Vec<(PackageId, Vec<String>)> {
    let mut parents: Vec<PackageId> = resolve.iter().collect(); // alloc: once per moved package
    parents.sort();
    let mut found = Vec::new(); // alloc: as above
    for parent in parents {
        for (dep, deps) in resolve.deps(parent) {
            if dep != target {
                continue;
            }
            let mut reqs: Vec<String> = deps.iter().map(|it| it.version_req().to_string()).collect();
            reqs.sort();
            reqs.dedup();
            found.push((parent, reqs));
        }
    }
    found
}

/// `asked_by`'s packages in a clause: "`a v0.1.0` asks `^1.2`", at most two, then how many
/// more.
fn askers(found: &[&(PackageId, Vec<String>)]) -> String {
    let mut out = String::new(); // alloc: a move's reason, once per move
    for (at, (parent, reqs)) in found.iter().take(2).enumerate() {
        if at > 0 {
            out.push_str(" and ");
        }
        let reqs: Vec<String> = reqs.iter().map(|it| format!("`{it}`")).collect();
        let _ = write!(out, "`{parent}` asks {}", reqs.join(" and "));
    }
    if found.len() > 2 {
        let _ = write!(out, ", and {} more", found.len() - 2);
    }
    out
}

/// Why `new` holds `id`, which the lock file did not: who asks for it.
fn why_added(new: &Resolve, id: PackageId) -> String {
    let found = asked_by(new, id);
    let found: Vec<&(PackageId, Vec<String>)> = found.iter().collect();
    match found.is_empty() {
        true => "a member of the workspace".to_owned(),
        false => askers(&found),
    }
}

/// Why `new` holds `id` where the lock file held `old`: the requirements `old` does not meet,
/// when there are; else who asks for it.
fn why_changed(new: &Resolve, id: PackageId, old: PackageId) -> String {
    let found = asked_by(new, id);
    let unmet: Vec<&(PackageId, Vec<String>)> = found
        .iter()
        .filter(|(_, reqs)| {
            reqs.iter().any(|req| semver::VersionReq::parse(req).is_ok_and(|req| !req.matches(old.version())))
        })
        .collect();
    if !unmet.is_empty() {
        return format!("{}, which {} does not match", askers(&unmet), version(old));
    }
    let all: Vec<&(PackageId, Vec<String>)> = found.iter().collect();
    match all.is_empty() {
        true => "its manifest's version".to_owned(),
        false => format!("{}, and the resolve moved it to meet another requirement", askers(&all)),
    }
}

/// What a resolve changed of the lock file's packages, one line each, sorted by package:
/// `+ name new (why)`, `- name old (why)`, `~ name old → new (why)`. A package of one version
/// before and after that moved is one `~`; one of several versions is told by each version
/// added and removed. `old` is the lock file's resolve, none when there was no lock file.
pub fn moves(old: Option<&Resolve>, new: &Resolve) -> Vec<String> {
    // By name and source (without a git commit): the versions before, and after.
    type Versions = BTreeMap<String, PackageId>;
    let mut by: BTreeMap<(String, String), (Versions, Versions)> = BTreeMap::new();
    let key = |id: PackageId| (id.name().to_string(), id.source_id().without_precise().as_url().to_string());
    for id in old.into_iter().flat_map(|it| it.iter()) {
        by.entry(key(id)).or_default().0.insert(version(id), id);
    }
    for id in new.iter() {
        by.entry(key(id)).or_default().1.insert(version(id), id);
    }
    let mut lines = Vec::new(); // alloc: the moves, once per lock change
    for (was, now) in by.values() {
        if was.keys().eq(now.keys()) {
            continue;
        }
        if let (1, 1, Some((old_version, &old)), Some((new_version, &id))) =
            (was.len(), now.len(), was.iter().next(), now.iter().next())
        {
            lines.push(format!("~ {} {old_version} → {new_version} ({})", package(id), why_changed(new, id, old)));
            continue;
        }
        for (old_version, &old) in was.iter().filter(|(it, _)| !now.contains_key(*it)) {
            lines.push(format!("- {} {old_version} (nothing depends on it now)", package(old)));
        }
        for (new_version, &id) in now.iter().filter(|(it, _)| !was.contains_key(*it)) {
            lines.push(format!("+ {} {new_version} ({})", package(id), why_added(new, id)));
        }
    }
    lines
}

/// The first eight characters of a digest, as a move shows it.
fn short(sha256: &str) -> &str {
    &sha256[..sha256.len().min(8)]
}

/// What `new` changes of `old` (none: there was no `blaze.lock`), one line each, in the file's
/// order.
pub fn pin_moves(old: Option<&BlazeLock>, new: &BlazeLock) -> Vec<String> {
    let empty = BlazeLock::default();
    let old = old.unwrap_or(&empty);
    let mut lines = Vec::new(); // alloc: the moves, once per lock change
    match (&old.blaze, &new.blaze) {
        (None, Some(new)) => lines.push(format!("+ blaze {} on {}", new.release, new.toolchain)),
        (Some(old), None) => lines.push(format!("- blaze {} on {}", old.release, old.toolchain)),
        (Some(old), Some(new)) => {
            if old.release != new.release {
                lines.push(format!("~ blaze {} → {}", old.release, new.release));
            }
            if old.toolchain != new.toolchain {
                lines.push(format!("~ toolchain {} → {}", old.toolchain, new.toolchain));
            }
        }
        (None, None) => {}
    }
    match (&old.index, &new.index) {
        (None, Some(new)) => lines.push(format!("+ index as of {}", minute(new))),
        (Some(old), None) => lines.push(format!("- index as of {}", minute(old))),
        (Some(old), Some(new)) if old != new => lines.push(format!("~ index as of {} → {}", minute(old), minute(new))),
        _ => {}
    }
    let named = |lock: &BlazeLock| -> BTreeMap<String, NativePin> {
        lock.native.iter().map(|it| (it.name.clone(), it.clone())).collect()
    };
    let (was, now) = (named(old), named(new));
    let names: std::collections::BTreeSet<&String> = was.keys().chain(now.keys()).collect();
    for name in names {
        match (was.get(name), now.get(name)) {
            (None, Some(new)) => {
                lines.push(format!("+ native input `{name}` ({}, {} targets)", new.pin, new.targets.len()));
            }
            (Some(_), None) => lines.push(format!("- native input `{name}`")),
            (Some(old), Some(new)) => {
                if old.pin != new.pin {
                    lines.push(format!("~ native input `{name}` pinned in {} → {}", old.pin, new.pin));
                }
                let (a, b): (BTreeMap<&str, &str>, BTreeMap<&str, &str>) = (
                    old.targets.iter().map(|(t, s)| (t.as_str(), s.as_str())).collect(),
                    new.targets.iter().map(|(t, s)| (t.as_str(), s.as_str())).collect(),
                );
                let targets: std::collections::BTreeSet<&str> = a.keys().chain(b.keys()).copied().collect();
                for target in targets {
                    match (a.get(target), b.get(target)) {
                        (None, Some(new)) => lines.push(format!("+ native `{name}` for {target}: {}", short(new))),
                        (Some(old), None) => lines.push(format!("- native `{name}` for {target}: {}", short(old))),
                        (Some(old), Some(new)) if old != new => {
                            lines.push(format!("~ native `{name}` for {target}: {} → {}", short(old), short(new)));
                        }
                        _ => {}
                    }
                }
            }
            (None, None) => {}
        }
    }
    lines
}

/// A publish time to the minute: `2026-11-04T10:12Z` of `2026-11-04T10:12:00Z`.
fn minute(time: &str) -> String {
    match time.get(..16) {
        Some(head) if time.len() == 20 => format!("{head}Z"),
        _ => time.to_owned(),
    }
}

/// One lock file as the build found it and would leave it.
pub struct Change {
    /// `Cargo.lock` or `blaze.lock`.
    pub name: &'static str,
    /// Absolute.
    pub path: String,
    /// Whether the file is there.
    pub existed: bool,
    /// The whole new text, when the file changes.
    pub text: Option<String>,
    /// What moves, one line each ([`moves`], [`pin_moves`]).
    pub moves: Vec<String>,
}

/// What a build's headline says of its lock files when it writes them.
pub struct Summary<'a> {
    /// How many packages the new `Cargo.lock` holds.
    pub packages: usize,
    /// The new `blaze.lock`.
    pub pins: &'a BlazeLock,
}

/// What the load does with its lock files.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Both are as they would be written.
    Same,
    Write(LockWrite),
    /// The policy forbids the change: the error, naming each move and the fix.
    Refused(String),
}

/// `policy` applied to `changes` (`Cargo.lock`'s first): nothing when none changes; else the
/// files to write and one line saying so, with each move of a file that was there under it
/// (a new file is told by its size, not each package); or, refused, the error naming every
/// move, why the build may not make it, and the fix. `manifest` is the root manifest, which a
/// refusal by `lock = "required"` names.
pub fn decide(policy: Policy, manifest: &str, changes: &[Change], summary: &Summary<'_>) -> Outcome {
    let changing: Vec<&Change> = changes.iter().filter(|it| it.text.is_some()).collect();
    if changing.is_empty() {
        return Outcome::Same;
    }
    let names: Vec<&str> = changing.iter().map(|it| it.name).collect();
    let files = names.join(" and ");
    let it = if changing.len() == 1 { "it" } else { "them" };
    match policy {
        Policy::Refuse(refusal) => {
            let missing: Vec<&str> = changing.iter().filter(|it| !it.existed).map(|it| it.name).collect();
            let head = match missing.as_slice() {
                [] => format!("{files} would change"),
                [one] if changing.len() == 1 => format!("{one} is missing"),
                _ => format!("{files} would change ({} missing)", missing.join(" and ")),
            };
            let mut what = format!("{head}, and this build may not change {it}: {}", refusal.because(manifest));
            for change in &changing {
                match (change.existed, change.moves.is_empty()) {
                    (false, _) if change.name == "Cargo.lock" => {
                        let _ = write!(what, "\n  {} would be written: {} packages", change.name, summary.packages);
                    }
                    (_, true) => {
                        let _ = write!(what, "\n  {} would be rewritten with the same contents in blaze's format", change.name);
                    }
                    _ => {
                        let _ = write!(what, "\n  {}:", change.name);
                        for line in &change.moves {
                            let _ = write!(what, "\n    {line}");
                        }
                    }
                }
            }
            let _ = write!(
                what,
                "\nfix: run `blaze lock` where the workspace is and commit {files}; or pass --no-locked to let this \
                 build write {it}"
            );
            Outcome::Refused(what)
        }
        Policy::Write => {
            let verb = if changing.iter().all(|it| !it.existed) { "wrote" } else { "updated" };
            let mut parts: Vec<String> = Vec::new(); // alloc: the headline's parts, once per lock write
            for change in &changing {
                match (change.name, change.existed) {
                    ("Cargo.lock", false) => parts.push(format!("{} packages", summary.packages)),
                    (name, true) if change.moves.is_empty() => parts.push(format!("{name} in blaze's format, nothing moved")),
                    (name, true) => parts.push(format!("{} {} moved", change.moves.len(), plural(change.moves.len(), name))),
                    (_, false) => {
                        if let Some(blaze) = &summary.pins.blaze {
                            parts.push(format!("blaze {} on {}", blaze.release, blaze.toolchain));
                        }
                        if !summary.pins.native.is_empty() {
                            let n = summary.pins.native.len();
                            parts.push(format!("{n} native input{}", if n == 1 { "" } else { "s" }));
                        }
                    }
                }
            }
            let cargo_changes = changing.iter().any(|it| it.name == "Cargo.lock");
            if cargo_changes && let Some(newest) = &summary.pins.index {
                parts.push(format!("index as of {}", minute(newest)));
            }
            let mut said = vec![format!("{verb} {files}: {} (commit {it})", parts.join(", "))]; // alloc: once per lock write
            for change in changing.iter().filter(|it| it.existed) {
                said.extend(change.moves.iter().map(|line| format!("  {}: {line}", change.name)));
            }
            let files = changing
                .iter()
                .map(|it| LockFile { path: it.path.clone(), text: it.text.clone().unwrap_or_default() })
                .collect();
            Outcome::Write(LockWrite { files, said })
        }
    }
}

/// What, besides the resolve, decides a workspace's lock files.
#[derive(Clone, Copy, Debug, Default)]
pub struct Inputs<'a> {
    /// The command line and the environment ([`policy`]).
    pub ask: LockAsk,
    /// The release of the blaze that runs ([`is_version`]); none for a build of the tree.
    pub running: Option<&'a str>,
    /// `BLAZE_IGNORE_PIN_FOR`: the root manifest `--ignore-pin` was given for, where the blaze
    /// that runs is not the one the workspace pins.
    pub ignore_pin_for: Option<&'a str>,
    /// A workspace of blaze's own (the standard library's): no `blaze.lock`, and no
    /// `lock = "required"` read.
    pub foreign: bool,
}

/// The lock files of workspace `ws` after its resolve `whole` (`old` and `old_text`: the
/// `Cargo.lock` there, read and as text), as the policy lets this command leave them: `Ok(None)`
/// when neither changes, the files to write with what to say, or the refusal naming every move.
/// The one place a resolve's lock files are decided: a build's load (the daemon's) and
/// blaze-metadata's commands both ask it. `Cargo.lock` is cargo's, minimal as cargo's resolve
/// keeps it (every package the lock file pins that the manifests still allow stays); `blaze.lock`
/// pins the rest ([`BlazeLock`]). Reads only through `files`, and no clock.
#[allow(clippy::too_many_arguments)] // the resolve's parts the lock files are made of, each once
pub fn after_resolve<F: Files>(
    files: &F,
    ws: &Workspace<'_, F>,
    inputs: Inputs<'_>,
    whole: &mut Resolve,
    old: Option<&Resolve>,
    old_text: Option<&str>,
    lowest_rust_version: Option<&RustVersion>,
    path_deps: &PathDeps,
) -> Result<Option<LockWrite>, LoadError> {
    let root = ws.root();
    let manifest = ws.root_manifest().display().to_string();
    let metadata = ws.custom_metadata();
    let required = match inputs.foreign {
        true => false,
        false => required(metadata, &manifest).map_err(|what| LoadError { what, at: Some(manifest.clone()) })?,
    };
    let policy = policy(inputs.ask, required).map_err(|what| LoadError { what, at: None })?;
    // Refused, a lock file that reads back to the same resolve is the same (cargo's `--locked`);
    // written, one whose lines differ is written again.
    let refusing = matches!(policy, Policy::Refuse(_));
    let cargo_path = root.join(lockfile::LOCKFILE_NAME);
    let text = lockfile::lockfile_change(whole, old_text, lowest_rust_version, refusing.then_some(path_deps));
    let moved = match text.is_some() {
        true => moves(old, whole),
        false => Vec::new(),
    };
    let cargo = Change {
        name: lockfile::LOCKFILE_NAME,
        path: cargo_path.display().to_string(),
        existed: old_text.is_some(),
        text,
        moves: moved,
    };
    let mut changes = vec![cargo]; // alloc: the two lock files, once per resolve
    let mut pins = BlazeLock::default();
    if !inputs.foreign {
        let path = root.join(BLAZE_LOCK);
        let shown = path.display().to_string();
        let old_text = match files.read(&path) {
            Ok(bytes) => Some(String::from_utf8(bytes.into_owned()).map_err(|error| LoadError {
                what: format!("{shown} is not UTF-8 ({error}); `blaze lock` writes it again"),
                at: Some(shown.clone()),
            })?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(LoadError { what: format!("failed to read `{shown}`: {error}"), at: Some(shown) }),
        };
        let old_pins = match &old_text {
            Some(text) => Some(BlazeLock::parse(text, &shown).map_err(|what| LoadError { what, at: Some(shown.clone()) })?),
            None => None,
        };
        let pin = metadata.and_then(|it| it.get("blaze")).and_then(|it| it.get("version")).and_then(toml::Value::as_str);
        let old_blaze = old_pins.as_ref().and_then(|it| it.blaze.as_ref());
        pins = BlazeLock {
            blaze: identity(pin, inputs, &manifest, old_blaze, &shown)
                .map_err(|what| LoadError { what, at: Some(shown.clone()) })?,
            index: newest(whole, old_pins.as_ref()),
            native: crate::load::native::locked_pins(files, metadata, ws.root_manifest(), root)?,
        };
        let same = match &old_pins {
            Some(old) => *old == pins,
            None => pins.is_empty(),
        };
        changes.push(Change {
            name: BLAZE_LOCK,
            path: shown,
            existed: old_text.is_some(),
            text: (!same).then(|| pins.render()),
            moves: match same {
                true => Vec::new(),
                false => pin_moves(old_pins.as_ref(), &pins),
            },
        });
    }
    let summary = Summary { packages: whole.len(), pins: &pins };
    match decide(policy, &manifest, &changes, &summary) {
        Outcome::Same => Ok(None),
        Outcome::Write(write) => Ok(Some(write)),
        Outcome::Refused(what) => Err(LoadError { what, at: Some(cargo_path.display().to_string()) }),
    }
}

/// Which blaze `blaze.lock` records (`[blaze]`), for a workspace whose root manifest `manifest`
/// pins `pin` (`[workspace.metadata.blaze] version`) and whose lock recorded `old`:
///
/// - No pin, or a channel: nothing. The workspace runs whichever blaze it gets, so a lock that
///   named one would change with every upgrade.
/// - A version, run by that release (no `--ignore-pin` for this workspace): the version and the
///   nightly it ships.
/// - A version, run by another blaze (a build of the tree, or `--ignore-pin`): what the lock
///   recorded when it records that version, or nothing when it records none. One that records
///   another version disagrees with the pin, and this blaze cannot say what the pinned release
///   ships: an error naming both and the fix, refused under any policy.
pub fn identity(
    pin: Option<&str>,
    inputs: Inputs<'_>,
    manifest: &str,
    old: Option<&Identity>,
    lock_path: &str,
) -> Result<Option<Identity>, String> {
    let Some(pin) = pin.filter(|it| is_version(it)) else { return Ok(None) };
    let runs_pin = inputs.running == Some(pin) && inputs.ignore_pin_for != Some(manifest);
    if runs_pin {
        return Ok(Some(Identity { release: pin.to_owned(), toolchain: toolchain().to_owned() }));
    }
    match old {
        None => Ok(None),
        Some(old) if old.release == pin => Ok(Some(old.clone())),
        Some(old) => {
            let this = inputs.running.unwrap_or("a build of blaze's tree");
            Err(format!(
                "{lock_path} records blaze {} but {manifest} pins blaze {pin}, and the blaze that runs ({this}) is not \
                 {pin}, so it cannot record what {pin} builds with\nfix: run `blaze lock` with blaze {pin} (the launcher \
                 runs it where the pin is, without --ignore-pin) and commit blaze.lock",
                old.release
            ))
        }
    }
}

/// The newest publish time of any package `whole` took from a registry's index, or the lock's
/// when that is newer: a package the lock file pinned was read from its manifest, which has
/// none, so a resolve that took nothing new keeps what the lock says.
fn newest(whole: &Resolve, old: Option<&BlazeLock>) -> Option<String> {
    let picked = whole.summaries().values().filter_map(|it| it.pubtime()).max();
    let kept = old.and_then(|it| it.index.as_deref());
    picked.into_iter().chain(kept).max().map(str::to_owned)
}

/// "package" or "pin" by how many and of which file.
fn plural(n: usize, file: &str) -> &'static str {
    match (file, n) {
        ("Cargo.lock", 1) => "package",
        ("Cargo.lock", _) => "packages",
        (_, 1) => "pin",
        _ => "pins",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every row of [`policy`]'s table, with `CI` as GitHub, Azure and people set it.
    #[test]
    fn the_decision_table() {
        let ask = |flag, no_locked, ci| LockAsk { flag, no_locked, ci, env_writes: false };
        let env = |flag, ci| LockAsk { flag, no_locked: false, ci, env_writes: true };
        let rows: [(LockAsk, bool, Result<Policy, ()>); 14] = [
            (env(None, true), true, Ok(Policy::Write)),
            (env(None, false), false, Ok(Policy::Write)),
            (env(Some("--locked"), true), false, Ok(Policy::Refuse(Refusal::Flag("--locked")))),
            (LockAsk { env_writes: true, ..ask(Some("--frozen"), true, false) }, false, Err(())),
            (ask(Some("--locked"), true, false), false, Err(())),
            (ask(Some("--frozen"), true, true), true, Err(())),
            (ask(Some("--locked"), false, false), false, Ok(Policy::Refuse(Refusal::Flag("--locked")))),
            (ask(Some("--frozen"), false, true), true, Ok(Policy::Refuse(Refusal::Flag("--frozen")))),
            (ask(None, true, true), true, Ok(Policy::Write)),
            (ask(None, true, false), false, Ok(Policy::Write)),
            (ask(None, false, true), false, Ok(Policy::Refuse(Refusal::Ci))),
            (ask(None, false, true), true, Ok(Policy::Refuse(Refusal::Ci))),
            (ask(None, false, false), true, Ok(Policy::Refuse(Refusal::Required))),
            (ask(None, false, false), false, Ok(Policy::Write)),
        ];
        for (ask, required, want) in rows {
            assert_eq!(policy(ask, required).map_err(|_| ()), want, "{ask:?}, required {required}");
        }
        for (ci, tf, says) in [
            (Some("true"), None, true),
            (Some("1"), None, true),
            (Some("FALSE"), None, false),
            (Some("0"), None, false),
            (Some(""), None, false),
            (None, Some("True"), true),
            (None, None, false),
        ] {
            assert_eq!(ci_says(ci, tf), says, "CI={ci:?} TF_BUILD={tf:?}");
        }
        assert_eq!(env_writes(None), Ok(false));
        assert_eq!(env_writes(Some("")), Ok(false));
        assert_eq!(env_writes(Some("write")), Ok(true));
        assert!(env_writes(Some("1")).unwrap_err().contains("BLAZE_LOCK"));
        let metadata = |text: &str| toml::Value::Table(toml::from_str::<toml::Table>(text).unwrap());
        assert_eq!(required(Some(&metadata("blaze.lock = \"required\"")), "/w/Cargo.toml"), Ok(true));
        assert_eq!(required(Some(&metadata("blaze.lock = \"write\"")), "/w/Cargo.toml"), Ok(false));
        assert_eq!(required(Some(&metadata("blaze.version = \"stable\"")), "/w/Cargo.toml"), Ok(false));
        assert!(required(Some(&metadata("blaze.lock = true")), "/w/Cargo.toml").unwrap_err().contains("/w/Cargo.toml"));

        // Which blaze `blaze.lock` records: only a version pin, and only its own release says
        // what it ships.
        let (p, q) = ("0.20261104.0", "0.20261030.1");
        let m = "/w/Cargo.toml";
        let runs = |running: Option<&'static str>| Inputs { running, ..Inputs::default() };
        let id = |release: &str| Identity { release: release.to_owned(), toolchain: toolchain().to_owned() };
        let old_q = Identity { release: q.to_owned(), toolchain: "nightly-2026-10-01".to_owned() };
        let at = "/w/blaze.lock";
        for pin in [None, Some("stable"), Some("canary")] {
            assert_eq!(identity(pin, runs(Some(p)), m, Some(&old_q), at), Ok(None), "unpinned: {pin:?}");
        }
        assert_eq!(identity(Some(p), runs(Some(p)), m, None, at), Ok(Some(id(p))));
        assert_eq!(identity(Some(p), runs(Some(p)), m, Some(&old_q), at), Ok(Some(id(p))), "a pin bump is a change");
        let ignored = Inputs { running: Some(p), ignore_pin_for: Some(m), ..Inputs::default() };
        assert_eq!(identity(Some(p), ignored, m, None, at), Ok(None), "--ignore-pin records nothing");
        assert_eq!(identity(Some(q), runs(None), m, Some(&old_q), at), Ok(Some(old_q.clone())), "the tree keeps it");
        assert_eq!(identity(Some(q), runs(None), m, None, at), Ok(None));
        let disagrees = identity(Some(p), runs(None), m, Some(&old_q), at).unwrap_err();
        assert!(disagrees.contains(q) && disagrees.contains(p) && disagrees.contains("blaze lock"), "{disagrees}");
    }

    /// `blaze.lock` is persisted state: what blaze writes reads back to the same value and
    /// renders to the same bytes, whatever order its parts were made in.
    #[test]
    fn blaze_lock_round_trips() {
        let lock = BlazeLock {
            blaze: Some(Identity { release: "0.20261104.0".to_owned(), toolchain: "nightly-2026-10-27".to_owned() }),
            index: Some("2026-11-04T10:12:00Z".to_owned()),
            native: vec![
                NativePin {
                    name: "zlib".to_owned(),
                    pin: "native/zlib.json".to_owned(),
                    targets: vec![("x86_64-unknown-linux-gnu".to_owned(), "b".repeat(64))],
                },
                NativePin {
                    name: "llvm".to_owned(),
                    pin: "crates/hir-ty-codegen/llvm.json".to_owned(),
                    targets: vec![
                        ("x86_64-unknown-linux-gnu".to_owned(), "c".repeat(64)),
                        ("aarch64-apple-darwin".to_owned(), "a".repeat(64)),
                    ],
                },
            ],
        };
        let text = lock.render();
        let read = BlazeLock::parse(&text, "/w/blaze.lock").unwrap();
        assert_eq!(read.render(), text, "parse then render gives the same bytes");
        assert_eq!(BlazeLock::parse(&read.render(), "/w/blaze.lock").unwrap(), read);
        assert!(text.find("[native.llvm]").unwrap() < text.find("[native.zlib]").unwrap(), "{text}");
        assert!(text.find("aarch64-apple-darwin").unwrap() < text.find("x86_64-unknown-linux-gnu").unwrap(), "{text}");
        let empty = BlazeLock::default();
        assert!(empty.is_empty());
        assert_eq!(BlazeLock::parse(&empty.render(), "/w/blaze.lock").unwrap(), empty);
        let newer = text.replace("version = 1", "version = 2");
        assert!(BlazeLock::parse(&newer, "/w/blaze.lock").unwrap_err().contains("format version 2"));
    }
}
