//! Named roots (docs/v2/plan/design/store.md, "Portable keys"): what makes a run's key, its
//! shapes and its kept value independent of where a checkout, its target directory or blaze's
//! tools are on disk, so two checkouts, or two repos with one dependency, find each other's
//! build-script runs and proc-macro expansions.
//!
//! A run names the directories its paths are under by what they are ([`Role`]): its package,
//! its `OUT_DIR`, the build's target directory, the workspace, blaze's tools and its launcher
//! ([`RootMap`]). Everything that goes into its key is written against them ([`Roots`]):
//!
//! - A path under a root is the root's placeholder (`\0` and the role's tag) and the rest of
//!   the path: `/home/a/ws/p/build.rs` is `\0p/build.rs` in this checkout and in every other
//!   ([`Roots::path`]). Under two roots, the nearer one. A path under no root stays absolute,
//!   and means the same file in another checkout only when it is in a machine-wide place
//!   ([`MACHINE`], blaze's fetched packages): a run that read any other is kept against where every one
//!   of its roots is ([`Roots::kept`]), as its key no longer says which checkout it ran in.
//! - A variable's value, or what a run printed, has each root in it that starts a path (after
//!   the string's start or a byte no path has) and ends a name replaced by its placeholder,
//!   and every `\0` already in it doubled ([`Roots::port`]): in one checkout a ported string
//!   is the port of exactly one string, and [`Roots::unport`] gives it back. A root with a path
//!   byte before it (`vendor/src/lib` with a root `/src`) stays as it is, and ties what holds
//!   it to this checkout (below).
//! - The key is given the roots' layout ([`Roots::layout`]): each role the run has, and which
//!   other root it is under and where, so two checkouts share a key only when their roots sit
//!   the same way to each other, and a placeholder in a shape kept under it always names a root
//!   the run has.
//!
//! What a path stood for is still covered: every file, listing and variable a run reads below a
//! root is in its key by its value, so writing the root by its role drops nothing the run read.
//! What is left is the root's string itself, which a run can see (`env!("OUT_DIR")`, its own
//! `argv[0]`). A run that uses it as a place gives the same result elsewhere up to that string,
//! so what it printed is kept ported and written back with this checkout's roots when it is
//! reused ([`Roots::stored`], [`Roots::restored`]). Whether it used it otherwise is not
//! guessed: a run is kept with a mark for every root, whose hash is the root's path
//! ([`Roots::mark`]), so it is found only where its roots are where they were, and is given to
//! another checkout only once a second checkout ran it and got the same ([`verify`]). A mark
//! is a trace entry, so it covers what the key holds as well as what the run printed.
//!
//! A run gets no copy for another checkout to verify, so is kept here alone, when something
//! ties it to its roots' strings already ([`Roots::kept`]): a root still in what it printed,
//! in a variable it read or in a string its key was given ([`RootMap::bind`]), once ported,
//! in any spelling (its links resolved, `/private` on macOS, separators doubled or turned,
//! case, `%`-escaped, UTF-16), the path from one root to another that the layout doesn't fix
//! (of two tops, [`Roots::embedded`]); a read outside its roots and the machine-wide places;
//! or a root whose links resolve elsewhere than when its map was made. A kept value never
//! embeds another checkout's path.
//!
//! On Windows a run has no roots (a path's bytes there are UTF-16 units, which the ports here
//! don't read): its keys are absolute, so nothing is shared between checkouts there.
//!
//! [`Rooted`] is what a kept run's shape is checked through: its placeholders resolved against
//! this run's roots, its marks answered with this run's root paths, and each variable's value
//! ported as the kept one was.

use std::{
    borrow::Cow,
    ffi::{OsStr, OsString},
    fmt,
    path::{Path, PathBuf},
};

use aho_corasick::AhoCorasick;

use blaze_core::Traced;
use blaze_ids::ContentId;

use crate::{
    inputs::Unreadable,
    run::Value,
    trace::{self, Entry, Inputs, Names},
};

/// What a root is: the directory it names in this run.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Role {
    /// The run's package, its manifest's directory: a build script's own, a proc macro's
    /// calling crate's (`CARGO_MANIFEST_DIR`).
    Package,
    /// The run's `OUT_DIR`.
    OutDir,
    /// The build's target directory, where every unit's outputs and the other `OUT_DIR`s are.
    Target,
    /// The workspace's root.
    Workspace,
    /// blaze's stand-ins for cargo's tools (`RUSTC`, `CARGO`, `RUSTDOC`).
    Tools,
    /// The launcher that makes the sandbox.
    Launcher,
}

impl Role {
    pub const ALL: [Role; 6] =
        [Role::Package, Role::OutDir, Role::Target, Role::Workspace, Role::Tools, Role::Launcher];

    /// The byte after `\0` in its placeholder.
    pub fn tag(self) -> u8 {
        match self {
            Role::Package => b'p',
            Role::OutDir => b'o',
            Role::Target => b't',
            Role::Workspace => b'w',
            Role::Tools => b'b',
            Role::Launcher => b'l',
        }
    }

    pub fn of(tag: u8) -> Option<Role> {
        Role::ALL.into_iter().find(|it| it.tag() == tag)
    }

    pub fn name(self) -> &'static str {
        match self {
            Role::Package => "package",
            Role::OutDir => "OUT_DIR",
            Role::Target => "target directory",
            Role::Workspace => "workspace",
            Role::Tools => "tools",
            Role::Launcher => "launcher",
        }
    }

    fn bit(self) -> u8 {
        1 << self as u8
    }
}

/// Roles: which roots a value embeds.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct RoleSet(u8);

impl RoleSet {
    pub fn insert(&mut self, role: Role) {
        self.0 |= role.bit();
    }

    pub fn contains(self, role: Role) -> bool {
        self.0 & role.bit() != 0
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub fn union(self, other: RoleSet) -> RoleSet {
        RoleSet(self.0 | other.0)
    }

    pub fn iter(self) -> impl Iterator<Item = Role> {
        Role::ALL.into_iter().filter(move |it| self.contains(*it))
    }
}

/// The byte a placeholder starts with: no path or variable has it, and one printed is doubled.
const MARK: u8 = 0;

/// The last name of a mark's path ([`Roots::mark`]): no file has it, a name being a C string.
pub const ROOT: &str = "\0root";

/// What a mark's hash, a root's path, is hashed under.
const ROOT_DOMAIN: &str = "blaze v2 root path 1";

/// A run's roots, owned: what a run's spec keeps, and lends as [`Roots`].
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct RootMap {
    roots: Vec<(Role, String)>,
    /// Each root's other spellings ([`spellings`]): looked for in what a run wrote, never
    /// ported.
    aliases: Vec<(Role, String)>,
    /// Machine-wide directories besides [`MACHINE`] (blaze's fetched registry packages
    /// and git checkouts, [`RootMap::with_sources`]): a path under one means the same file in every checkout on the machine.
    machine: Vec<PathBuf>,
    /// The path each root's links resolved to when it was set: a root whose links resolve
    /// elsewhere when the run is kept ([`Roots::relinked`]) ties it to this checkout.
    resolved: Vec<(Role, Option<String>)>,
    /// The roots still in a string the run's key was given once ported ([`RootMap::bind`]).
    given: RoleSet,
}

impl RootMap {
    /// `role`'s root is `path`, in place of any it had, with its other spellings: the path its
    /// links resolve to (read once, here, at setup) and the forms of each that [`spellings`]
    /// names. A path that is not UTF-8, not absolute or the file system's root names no root:
    /// what is under it stays absolute in keys. On Windows nothing is a root.
    pub fn set(&mut self, role: Role, path: &Path) -> &mut RootMap {
        self.roots.retain(|it| it.0 != role);
        self.aliases.retain(|it| it.0 != role);
        self.resolved.retain(|it| it.0 != role);
        if let Some(path) = path.to_str().map(trim).filter(|it| usable(it)) {
            let resolved = resolve(path);
            for alias in spellings(path, resolved.as_deref()) {
                self.aliases.push((role, alias));
            }
            self.resolved.push((role, resolved));
            self.roots.push((role, path.to_owned())); // alloc: setup, once per root of a run
        }
        self
    }

    /// The run's key was given a string that still holds the roots of `roles` once ported
    /// ([`Roots::left`]): the run is kept against where every root is, and no other checkout
    /// is given it ([`Roots::kept`]).
    pub fn bind(&mut self, roles: RoleSet) -> &mut RootMap {
        self.given = self.given.union(roles);
        self
    }

    pub fn with(mut self, role: Role, path: &Path) -> RootMap {
        self.set(role, path);
        self
    }

    /// `dir` is machine-wide: a read under it is of the same file in every checkout.
    pub fn with_machine(mut self, dir: &Path) -> RootMap {
        if dir.is_absolute() && dir.parent().is_some() {
            self.machine.push(dir.to_owned()); // alloc: setup, once per machine-wide directory
        }
        self
    }

    /// Where blaze keeps the packages it fetches (`blaze_manifest::home::sources`): its
    /// registry packages and git checkouts, machine-wide ([`RootMap::with_machine`]), the
    /// place every registry and git package of a build is.
    pub fn with_sources(self, sources: &Path) -> RootMap {
        self.with_machine(&sources.join("registry")).with_machine(&sources.join("git"))
    }

    pub fn view(&self) -> Roots<'_> {
        Roots {
            aliases: &self.aliases,
            machine: &self.machine,
            resolved: &self.resolved,
            given: self.given,
            ..Roots::new(self.roots.iter().map(|(role, path)| (*role, path.as_str())))
        }
    }
}

/// The path `root`'s links resolve to, when it is there and UTF-8.
fn resolve(root: &str) -> Option<String> {
    let resolved = std::fs::canonicalize(root).ok()?;
    resolved.to_str().map(|it| trim(it).to_owned()) // alloc: setup, once per root of a run
}

/// The directories a path in which means the same file in every checkout on a machine: the
/// system's, which the loader, the C compiler and its headers read. A run that read anywhere
/// else outside its roots is kept against where its roots are ([`Roots::kept`]).
pub const MACHINE: &[&str] = &[
    "/usr",
    "/etc",
    "/private/etc",
    "/nix/store",
    "/System",
    "/Library",
    "/private/var/db/dyld",
    "/lib",
    "/lib32",
    "/lib64",
    "/bin",
    "/sbin",
    "/dev",
    "/sys",
    "/proc/cpuinfo",
    "/proc/meminfo",
    "/proc/sys",
    "/proc/self/cgroup",
    "/proc/self/mountinfo",
];

/// The other spellings of the root `root`, whose links resolve to `resolved`: that path, each
/// of the two with `/private` taken off or put on (macOS's `/var` and `/tmp` are
/// `/private/var` and `/private/tmp`), and each `%`-escaped as a `file://` URL has it. Doubled
/// or turned separators and case are not spellings: [`spelled_in`] matches across them.
fn spellings(root: &str, resolved: Option<&str>) -> Vec<String> {
    let add = |out: &mut Vec<String>, it: &str| {
        if it != root && !out.iter().any(|known| known == it) {
            out.push(it.to_owned()); // alloc: setup, a root's spelling, once per root of a run
        }
    };
    let mut plain = Vec::new(); // alloc: as above
    for base in std::iter::once(root).chain(resolved) {
        add(&mut plain, base);
        if let Some(rest) = base.strip_prefix("/private").filter(|it| it.starts_with('/')) {
            add(&mut plain, rest);
        } else if ["/var", "/tmp"].iter().any(|dir| {
            base.strip_prefix(dir).is_some_and(|it| it.is_empty() || it.starts_with('/'))
        }) {
            add(&mut plain, &format!("/private{base}"));
        }
    }
    let mut out = plain.clone(); // clone: as above, each spelling escaped too
    for it in std::iter::once(root).chain(plain.iter().map(String::as_str)) {
        add(&mut out, &escaped(it));
    }
    out
}

/// `path` as a `file://` URL's path has it: each byte but a letter, digit, `-._~` and `/` as
/// `%XX`.
fn escaped(path: &str) -> String {
    let mut out = String::with_capacity(path.len()); // alloc: setup, a root's spelling
    for byte in path.bytes() {
        match byte {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(char::from(byte))
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// `path` without the separators it ends with: `/ws/p/` is the root `/ws/p`.
fn trim(path: &str) -> &str {
    path.trim_end_matches(|it| it == '/' || (cfg!(windows) && it == '\\'))
}

fn usable(path: &str) -> bool {
    !cfg!(windows)
        && !path.is_empty()
        && Path::new(path).is_absolute()
        && !path.as_bytes().contains(&MARK)
}

/// Whether `byte` is one of a path's: a name's or a separator.
fn path_byte(byte: u8) -> bool {
    !word_end(Some(byte)) || byte == b'/' || byte == b'\\'
}

/// Whether a path's root may end before `byte`: at its end or a separator.
fn separator(byte: Option<u8>) -> bool {
    match byte {
        None | Some(b'/') => true,
        Some(b'\\') => cfg!(windows),
        Some(_) => false,
    }
}

/// Whether a root in a string may end before `byte`: at the string's end or anything that is
/// not part of a name (a separator, a space, `:` of a list, a quote).
fn word_end(byte: Option<u8>) -> bool {
    match byte {
        None => true,
        Some(it) => {
            !(it.is_ascii_alphanumeric()
                || matches!(it, b'.' | b'_' | b'-' | b'+' | b'~' | b'@')
                || it >= 0x80)
        }
    }
}

/// A run's roots, lent: what keys, shapes and kept values are written against. At most one
/// root a role, longest first, so a path under two is written against the nearer; made with no
/// allocation, as a proc macro's key is.
#[derive(Clone, Copy, Debug)]
pub struct Roots<'a> {
    roots: [(Role, &'a str); Role::ALL.len()],
    len: usize,
    /// The roots' other spellings, from a [`RootMap`]: none for roots made with [`Roots::new`].
    aliases: &'a [(Role, String)],
    /// Machine-wide directories besides [`MACHINE`].
    machine: &'a [PathBuf],
    /// What each root's links resolved to when its map was made: none for [`Roots::new`].
    resolved: &'a [(Role, Option<String>)],
    /// The roots its key's strings still hold ([`RootMap::bind`]).
    given: RoleSet,
}

impl<'a> Roots<'a> {
    /// No roots: every path stays absolute.
    pub fn none() -> Roots<'a> {
        Roots {
            roots: [(Role::Package, ""); Role::ALL.len()],
            len: 0,
            aliases: &[],
            machine: &[],
            resolved: &[],
            given: RoleSet(0),
        }
    }

    /// These roots, owned, with their other spellings ([`RootMap::set`]) and `machine`, the
    /// machine-wide directories: what a run made with [`Roots::new`] is kept through.
    pub fn to_map(&self, machine: impl FnOnce(RootMap) -> RootMap) -> RootMap {
        let mut map = RootMap::default();
        for &(role, root) in &self.roots[..self.len] {
            map.set(role, Path::new(root));
        }
        machine(map)
    }

    /// Every role the run has a root of.
    pub fn all(&self) -> RoleSet {
        let mut all = RoleSet::default();
        for &(role, _) in &self.roots[..self.len] {
            all.insert(role);
        }
        all
    }

    /// The roots `roots` names, the last of a role kept; one that is not absolute or is the
    /// file system's root is none.
    pub fn new(roots: impl IntoIterator<Item = (Role, &'a str)>) -> Roots<'a> {
        let mut out = Roots::none();
        for (role, path) in roots {
            let path = trim(path);
            if !usable(path) {
                continue;
            }
            match out.roots[..out.len].iter_mut().find(|it| it.0 == role) {
                Some(it) => it.1 = path,
                None => {
                    out.roots[out.len] = (role, path);
                    out.len += 1;
                }
            }
        }
        // Longest first; a tie (a workspace that is its one package) by role, the same in every
        // checkout.
        out.roots[..out.len].sort_unstable_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(&b.0)));
        out
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// `role`'s root, if the run has one.
    pub fn get(&self, role: Role) -> Option<&'a str> {
        self.roots[..self.len].iter().find(|it| it.0 == role).map(|it| it.1)
    }

    /// What a key is given of the roots, piece by piece to `emit`: in role order, each role the
    /// run has, then `+`, the nearest other root its root is under by whole names and the rest
    /// of its path below that one (length first), or `-` for none. Two checkouts share a key
    /// only when their roots sit the same way to each other: a target directory inside the
    /// workspace in one and elsewhere in the other, or a package that is its workspace in one
    /// and not the other, are two keys. A root's own string is not in it.
    pub fn layout(&self, mut emit: impl FnMut(&[u8])) {
        for role in Role::ALL {
            let Some(root) = self.get(role) else { continue };
            emit(&[role.tag()]);
            match self.under(role, root) {
                Some((other, path)) => {
                    let rest = &root.as_bytes()[path.len()..];
                    emit(&[b'+', other.tag()]);
                    emit(&(rest.len() as u64).to_le_bytes());
                    emit(rest);
                }
                None => emit(b"-"),
            }
        }
    }

    /// The nearest other root that `role`'s root, `root`, is under by whole names: longest first,
    /// ties by role, the same in every checkout. Of two equal roots (a package that is its
    /// workspace) only the lower role is under the other, so neither is under itself through
    /// the other and [`Roots::top`] ends.
    fn under(&self, role: Role, root: &str) -> Option<(Role, &'a str)> {
        self.roots[..self.len]
            .iter()
            .find(|(other, path)| {
                *other != role
                    && root.starts_with(*path)
                    && separator(root.as_bytes().get(path.len()).copied())
                    && (path.len() < root.len() || role < *other)
            })
            .copied()
    }

    /// The outermost root `role`'s root is under, following [`Roots::under`] (itself when it
    /// is under none): two roots of one top sit to each other as the layout says, and two of
    /// two tops as nothing in the key says.
    fn top(&self, role: Role) -> Role {
        let mut at = role;
        // Bounded: each step is to a shorter root, or to an equal one of a higher role
        // ([`Roots::under`]), so no root is visited twice and `len` steps reach the top.
        for _ in 0..self.len {
            match self.get(at).and_then(|root| self.under(at, root)) {
                Some((outer, _)) => at = outer,
                None => break,
            }
        }
        at
    }

    /// The root `bytes` start with, ended where `ends` says one may: its role and length.
    fn at(&self, bytes: &[u8], ends: fn(Option<u8>) -> bool) -> Option<(Role, usize)> {
        self.roots[..self.len]
            .iter()
            .find(|(_, root)| {
                bytes.starts_with(root.as_bytes()) && ends(bytes.get(root.len()).copied())
            })
            .map(|(role, root)| (*role, root.len()))
    }

    /// The root of the tag `tag`, or the error that `what`, which names it, is not of this run.
    fn root_of(&self, tag: u8, what: &[u8]) -> Result<&'a str, NoRoot> {
        Role::of(tag).and_then(|role| self.get(role)).ok_or_else(|| NoRoot {
            what: String::from_utf8_lossy(what).into_owned(), // alloc: error path
            why: "it names a root this run has none of: the store's bytes or the run's key are wrong",
        })
    }

    /// `bytes` ported, given to `emit` piece by piece: each `\0` doubled and each root in it,
    /// wherever it starts and ended where no name goes on, its placeholder. What a key hashes in
    /// place ([`crate::Given::ported`]) and [`Roots::ported`] collects.
    pub fn port(&self, bytes: &[u8], mut emit: impl FnMut(&[u8])) {
        let (mut from, mut at) = (0, 0);
        while at < bytes.len() {
            let (piece, skip) = if bytes[at] == MARK {
                ([MARK, MARK], 1)
            } else if let Some((role, len)) = self.occurrence(bytes, at) {
                ([MARK, role.tag()], len)
            } else {
                at += 1;
                continue;
            };
            emit(&bytes[from..at]);
            emit(&piece);
            at += skip;
            from = at;
        }
        emit(&bytes[from..]);
    }

    /// The root that is a placeholder at `at` in `bytes`: one that starts there, ends a name,
    /// and starts a path, right after the string's start or a byte no path has (a space, `=`,
    /// `:`, a quote). One with any path byte before it is not (`/src` in `/usr/src/linux`, in
    /// `vendor/src/lib` or in `-I/src`): which of its bytes are the root's is not sure, so it
    /// stays as it is, and a run whose output, traced variables or given strings hold it
    /// ([`Roots::left`]) is kept here alone: its marks cover its key and its trace, and no
    /// copy is made for another checkout ([`Roots::kept`]).
    fn occurrence(&self, bytes: &[u8], at: usize) -> Option<(Role, usize)> {
        let found = self.at(&bytes[at..], word_end)?;
        match at.checked_sub(1).map(|it| bytes[it]) {
            Some(before) if path_byte(before) => None,
            _ => Some(found),
        }
    }

    /// Whether [`Roots::port`] changes `bytes`.
    pub fn touches(&self, bytes: &[u8]) -> bool {
        (0..bytes.len()).any(|at| bytes[at] == MARK || self.occurrence(bytes, at).is_some())
    }

    /// The length of `bytes` ported.
    pub fn ported_len(&self, bytes: &[u8]) -> usize {
        let mut len = 0;
        self.port(bytes, |piece| len += piece.len());
        len
    }

    /// `bytes` ported: lent when that changes nothing.
    pub fn ported<'b>(&self, bytes: &'b [u8]) -> Cow<'b, [u8]> {
        if !self.touches(bytes) {
            return Cow::Borrowed(bytes);
        }
        // alloc: a string with a root or a `\0` in it, once as it is keyed or kept
        let mut out = Vec::with_capacity(bytes.len());
        self.port(bytes, |piece| out.extend_from_slice(piece));
        Cow::Owned(out)
    }

    /// What `bytes`, ported in some checkout, are in this one: each placeholder this run's root,
    /// each doubled `\0` one. An error for a placeholder of a root this run has not, or one cut
    /// short.
    pub fn unport<'b>(&self, bytes: &'b [u8]) -> Result<Cow<'b, [u8]>, NoRoot> {
        if !bytes.contains(&MARK) {
            return Ok(Cow::Borrowed(bytes));
        }
        // alloc: a kept string with a placeholder in it, once as it is reused
        let mut out = Vec::with_capacity(bytes.len());
        let mut rest = bytes;
        while let Some(at) = rest.iter().position(|it| *it == MARK) {
            out.extend_from_slice(&rest[..at]);
            match rest.get(at + 1).copied() {
                Some(MARK) => out.push(MARK),
                Some(tag) => out.extend_from_slice(self.root_of(tag, bytes)?.as_bytes()),
                None => {
                    return Err(NoRoot {
                        what: String::from_utf8_lossy(bytes).into_owned(), // alloc: error path
                        why: "it ends inside a root's placeholder: it is not a ported string",
                    });
                }
            }
            rest = &rest[at + 2..];
        }
        out.extend_from_slice(rest);
        Ok(Cow::Owned(out))
    }

    /// `value` ported, when it is UTF-8: what a key and a kept trace hold of a variable. A value
    /// that is not UTF-8 is kept as it is, and compared as it is: another checkout's differs.
    pub fn os<'v>(&self, value: &'v OsStr) -> Cow<'v, OsStr> {
        let Some(text) = value.to_str() else { return Cow::Borrowed(value) };
        match self.ported(text.as_bytes()) {
            Cow::Borrowed(_) => Cow::Borrowed(value),
            // A ported string is cut only at `\0` and at roots, which start with an ASCII byte
            // and are whole strings, and its placeholders are ASCII: it is UTF-8.
            Cow::Owned(bytes) => Cow::Owned(OsString::from(
                String::from_utf8(bytes).expect("a UTF-8 string ported is UTF-8"),
            )),
        }
    }

    /// `path` written against the roots: the nearest root under which it is, by whole names, as
    /// its placeholder and the rest; lent when it is under none (or is not UTF-8).
    pub fn path<'p>(&self, path: &'p Path) -> Cow<'p, Path> {
        let Some(text) = path.to_str() else { return Cow::Borrowed(path) };
        match self.at(text.as_bytes(), separator) {
            Some((role, len)) => {
                // alloc: a path under a root, once as it is keyed or kept
                let mut ported = String::with_capacity(2 + text.len() - len);
                ported.push(char::from(MARK));
                ported.push(char::from(role.tag()));
                ported.push_str(&text[len..]);
                Cow::Owned(PathBuf::from(ported))
            }
            None => Cow::Borrowed(path),
        }
    }

    /// The path in this checkout that `path`, written against some checkout's roots, names:
    /// lent when it is absolute. An error for a root this run has not.
    pub fn real_path<'p>(&self, path: &'p Path) -> Result<Cow<'p, Path>, NoRoot> {
        let bytes = path.as_os_str().as_encoded_bytes();
        if bytes.first() != Some(&MARK) {
            return Ok(Cow::Borrowed(path));
        }
        let bad =
            |why: &'static str| NoRoot { what: String::from_utf8_lossy(bytes).into_owned(), why }; // alloc: error path
        let text = path
            .to_str()
            .ok_or_else(|| bad("a path written against a root is UTF-8, and this is not"))?;
        let root = self.root_of(bytes.get(1).copied().unwrap_or(MARK), bytes)?;
        let rest = text.get(2..).ok_or_else(|| bad("a root's placeholder is cut short"))?;
        if !separator(rest.as_bytes().first().copied()) {
            return Err(bad("a root's placeholder goes on with no separator after it"));
        }
        Ok(Cow::Owned(PathBuf::from(format!("{root}{rest}")))) // alloc: a kept path under a root, once as it is checked
    }

    /// `traced` with each path and each variable's value written against the roots, sorted
    /// again as [`trace::normalize`] leaves it: what a run is kept under and its shape. Two
    /// entries of one checkout never become one: a path's port and a value's are each the port
    /// of exactly one.
    pub fn traced(&self, traced: &Traced) -> Traced {
        let mut ported = Traced {
            // alloc: the trace a run is kept under, once per run kept
            files: traced
                .files
                .iter()
                .map(|(path, hash)| (self.path(path).into_owned(), *hash))
                .collect(),
            // alloc: as above
            dirs: traced
                .dirs
                .iter()
                .map(|(path, hash)| (self.path(path).into_owned(), *hash))
                .collect(),
            env: traced
                .env
                .iter()
                // clone: as above, the names the kept trace owns
                .map(|(name, value)| {
                    (name.clone(), value.as_deref().map(|it| self.os(it).into_owned()))
                })
                .collect(), // alloc: as above
            untraceable: traced.untraceable.clone(), // clone: as above
        };
        trace::normalize(&mut ported);
        ported
    }

    /// Adds to `traced` (a ported trace) a mark for each root of `roles` the run has: a file
    /// entry `\0<tag>/\0root` whose hash is that root's path, so what is kept under it is found
    /// only where those roots are where they were ([`Rooted`] answers a mark with this run's).
    pub fn mark(&self, traced: &mut Traced, roles: RoleSet) {
        if roles.is_empty() {
            return;
        }
        for role in roles.iter() {
            if let Some(root) = self.get(role) {
                traced.files.push((mark_path(role), Some(root_hash(root))));
            }
        }
        trace::normalize(traced);
    }

    /// The roots whose path is anywhere in `bytes`, whatever is around it and however it is
    /// spelled: as it is or as any of its other spellings ([`RootMap::set`]), with separators
    /// doubled, turned or escaped (`//`, `\\`, `\/`) and in any case ([`spelled_in`]), or in
    /// UTF-16. What has one can't be given to another checkout, and is kept against where its
    /// roots are. Wider than [`Roots::port`] matches, never narrower.
    pub fn embedded(&self, bytes: &[u8]) -> RoleSet {
        let mut found = RoleSet::default();
        for &(role, root) in &self.roots[..self.len] {
            let aliased = self.aliases.iter().filter(|it| it.0 == role).map(|it| it.1.as_str());
            if std::iter::once(root).chain(aliased).any(|it| spelled_in(bytes, it.as_bytes()))
                || utf16_in(bytes, root)
            {
                found.insert(role);
            }
        }
        // The path from one root to another of another top, either way (`../../cache/target`
        // from a package to a target directory outside its workspace): the key says nothing of
        // how they sit, so another checkout's is another string under the same key. Two equal
        // roots have one top, and an empty path is in everything, so it is no path.
        for &(from, from_root) in &self.roots[..self.len] {
            for &(to, to_root) in &self.roots[..self.len] {
                if from == to || self.top(from) == self.top(to) {
                    continue;
                }
                let path = relative(from_root, to_root);
                if !path.is_empty() && spelled_in(bytes, path.as_bytes()) {
                    found.insert(from);
                    found.insert(to);
                }
            }
        }
        found
    }

    /// Whether `path` (a ported trace's) is outside every root and every machine-wide place: a
    /// file there in another checkout is not this one's. The machine-wide places are
    /// [`MACHINE`] and blaze's fetched registry packages and git checkouts, where a package is the same
    /// one at the same path in every checkout, except one that has the run's package,
    /// `OUT_DIR`, target directory or workspace in it (a checkout under `/usr/src`, a git
    /// dependency's build script): a sibling of those is another checkout's or another
    /// revision's. blaze's tools and launcher being in one (`/nix/store`, `/usr/bin`) don't
    /// count: no run's sources are next to them.
    fn outside(&self, path: &Path) -> bool {
        let bytes = path.as_os_str().as_encoded_bytes();
        let checkout = [Role::Package, Role::OutDir, Role::Target, Role::Workspace];
        let holds_root = |dir: &Path| {
            checkout
                .iter()
                .filter_map(|role| self.get(*role))
                .any(|root| Path::new(root).starts_with(dir))
        };
        let wide = |dir: &Path| path.starts_with(dir) && !holds_root(dir);
        bytes.first() != Some(&MARK)
            && !MACHINE.iter().any(|dir| wide(Path::new(dir)))
            && !self.machine.iter().any(|dir| wide(dir.as_path()))
    }

    /// The roots still in `bytes` once ported, in any spelling ([`Roots::embedded`]): what a
    /// string a run's key was given, a variable it read, or what it printed ties it to.
    pub fn left(&self, bytes: &[u8]) -> RoleSet {
        self.embedded(&self.ported(bytes))
    }

    /// Whether a root's links resolve elsewhere now than when its map was made
    /// ([`RootMap::set`], read again here as a run is kept): the spellings its output was
    /// scanned for may not be the ones it saw.
    pub fn relinked(&self) -> bool {
        self.resolved
            .iter()
            .any(|(role, then)| self.get(*role).is_some_and(|root| resolve(root) != *then))
    }

    /// `traced`, what a run read, and `value`, what it gave, as they are kept: the trace
    /// ported with a mark for every root ([`Marked::local`]) and, when nothing ties the run to
    /// its roots' strings (below), without them, for another checkout to verify
    /// ([`Marked::shared`], [`verify`]); and the value's output ported ([`Roots::stored`]).
    /// An error when the scan of the files it wrote could not be made ([`Roots::stored`]).
    pub fn kept(
        &self,
        traced: &Traced,
        value: &Value,
    ) -> Result<(Marked, Value), aho_corasick::BuildError> {
        let (stored, left) = self.stored(value)?;
        Ok((self.kept_trace(traced, left), stored))
    }

    /// [`Roots::kept`] of a run whose value is the bytes `written`, kept as they are (a proc
    /// macro's output), whose key was given strings that hold the roots `given` once ported
    /// ([`Roots::left`]).
    pub fn kept_bytes(&self, traced: &Traced, written: Option<&[u8]>, given: RoleSet) -> Marked {
        let left = written.map(|it| self.embedded(it)).unwrap_or_default();
        self.kept_trace(traced, left.union(given))
    }

    /// `traced` kept ([`Roots::kept`]), what the run gave still holding the roots `left`: tied
    /// to its roots' strings, so with no copy for another checkout, when any root is left in
    /// it, in a string its key was given ([`RootMap::bind`]) or in a variable it read, when it
    /// read outside its roots and the machine-wide places (whose path is this checkout's), or
    /// when a root's links moved ([`Roots::relinked`]).
    fn kept_trace(&self, traced: &Traced, left: RoleSet) -> Marked {
        let ported = self.traced(traced);
        let mut bound = left.union(self.given);
        for value in ported.env.iter().filter_map(|it| it.1.as_deref()) {
            // A ported value: a root left in it is one no port took (`-I/ws/p/include`).
            bound = bound.union(self.embedded(value.as_encoded_bytes()));
        }
        let read = ported.files.iter().map(|it| &it.0).chain(ported.dirs.iter().map(|it| &it.0));
        if read.into_iter().any(|path| self.outside(path)) {
            bound = bound.union(self.all());
        }
        let relinked = self.relinked();
        if relinked {
            bound = bound.union(self.all());
        }
        let shared = (bound.is_empty() && !self.is_empty()).then(|| ported.clone()); // clone: once per run kept, its copy's trace
        let mut local = ported;
        self.mark(&mut local, self.all());
        Marked { local, shared, bound, relinked }
    }

    /// `value` as it is kept: what it printed ported, and the roots still in that once ported
    /// (a root that a name goes on after, `{OUT_DIR}x`, or a path byte is before, is no
    /// placeholder), or anywhere in the files it wrote (their names and bytes, kept as they
    /// are), which tie it to this checkout. A root two checkouts share is the same string in
    /// both, so comparing them ([`verify`]) can't see it; this scan does, and such a value is
    /// kept here alone, with no copy. An error when the automaton that scans the files could
    /// not be built ([`Scan::of`]): nothing is kept on a scan that did not run.
    pub fn stored(&self, value: &Value) -> Result<(Value, RoleSet), aho_corasick::BuildError> {
        let stored = Value {
            code: value.code,
            signal: value.signal,
            stdout: self.ported(&value.stdout).into_owned(),
            stderr: self.ported(&value.stderr).into_owned(),
            files: value.files.clone(), // clone: the value kept, once per run kept
        };
        let mut left = self.embedded(&stored.stdout).union(self.embedded(&stored.stderr));
        // The files it wrote (an `OUT_DIR` can be hundreds of MB): one pass over each with
        // every spelling at once ([`Scan`]), and none once one root is found, since any root
        // left keeps the run here alone, marked with every root, whatever else is found.
        if left.is_empty() && !stored.files.is_empty() && !self.is_empty() {
            let scan = Scan::of(self)?;
            for file in &stored.files {
                let found = scan
                    .first(file.path.as_bytes())
                    .or_else(|| file.bytes.as_deref().and_then(|it| scan.first(it)));
                if let Some(roles) = found {
                    left = roles;
                    break;
                }
            }
        }
        Ok((stored, left))
    }

    /// A kept value as this checkout's run would have given it: what it printed with this run's
    /// roots. An error for a value that is not ported, or names a root the run has not.
    pub fn restored(&self, mut value: Value) -> Result<Value, NoRoot> {
        let stdout = match self.unport(&value.stdout)? {
            Cow::Borrowed(_) => None,
            Cow::Owned(it) => Some(it),
        };
        let stderr = match self.unport(&value.stderr)? {
            Cow::Borrowed(_) => None,
            Cow::Owned(it) => Some(it),
        };
        if let Some(it) = stdout {
            value.stdout = it;
        }
        if let Some(it) = stderr {
            value.stderr = it;
        }
        Ok(value)
    }
}

/// [`Roots::embedded`] made once for many or large inputs: every spelling it looks for, as
/// patterns of one automaton each, so a written file is read once whatever the number of roots
/// and spellings.
///
/// [`spelled_in`] matches a needle where it is a substring of the hay once each run of
/// separators in both is made one `/` and ASCII case is ignored: so `paths` holds each
/// spelling and each relative path between roots of two tops in that form, matched without
/// case over the hay in that form ([`separators_folded`]). `wide` holds each root in UTF-16
/// (little- and big-endian), matched over the bytes as they are, as [`utf16_in`] does.
struct Scan {
    paths: Option<AhoCorasick>,
    /// The roles each of `paths`' patterns ties, by pattern.
    path_roles: Vec<RoleSet>,
    wide: Option<AhoCorasick>,
    wide_roles: Vec<RoleSet>,
}

impl Scan {
    fn of(roots: &Roots<'_>) -> Result<Scan, aho_corasick::BuildError> {
        let one = |role: Role| {
            let mut set = RoleSet::default();
            set.insert(role);
            set
        };
        let (mut paths, mut path_roles, mut wide, mut wide_roles) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new()); // alloc: once per run kept that wrote files, its patterns
        for &(role, root) in &roots.roots[..roots.len] {
            let aliased = roots.aliases.iter().filter(|it| it.0 == role).map(|it| it.1.as_str());
            for spelling in std::iter::once(root).chain(aliased) {
                let folded = separators_folded(spelling.as_bytes()).into_owned(); // alloc: as above
                if !folded.is_empty() {
                    paths.push(folded);
                    path_roles.push(one(role));
                }
            }
            let units: Vec<u16> = root.encode_utf16().collect(); // alloc: as above
            if !units.is_empty() {
                wide.push(units.iter().flat_map(|it| it.to_le_bytes()).collect::<Vec<u8>>()); // alloc: as above
                wide_roles.push(one(role));
                wide.push(units.iter().flat_map(|it| it.to_be_bytes()).collect::<Vec<u8>>()); // alloc: as above
                wide_roles.push(one(role));
            }
        }
        for &(from, from_root) in &roots.roots[..roots.len] {
            for &(to, to_root) in &roots.roots[..roots.len] {
                if from == to || roots.top(from) == roots.top(to) {
                    continue;
                }
                let folded =
                    separators_folded(relative(from_root, to_root).as_bytes()).into_owned(); // alloc: as above
                if !folded.is_empty() {
                    paths.push(folded);
                    path_roles.push(one(from).union(one(to)));
                }
            }
        }
        // A set of patterns no automaton can be built of (past its limits, which a run's few
        // dozen spellings never near) is an error the run's keep names (`KeepError::Scan`),
        // never a scan taken as done or a guess at what it would have found.
        let built = |patterns: &[Vec<u8>], fold: bool| {
            (!patterns.is_empty())
                .then(|| AhoCorasick::builder().ascii_case_insensitive(fold).build(patterns))
                .transpose()
        };
        let (paths, wide) = (built(&paths, true)?, built(&wide, false)?);
        Ok(Scan { paths, path_roles, wide, wide_roles })
    }

    /// The roles of the first spelling found in `hay`, if any is.
    fn first(&self, hay: &[u8]) -> Option<RoleSet> {
        let Some(paths) = &self.paths else {
            // No automaton for the paths: the run has no spelling to look for.
            return self.wide_first(hay);
        };
        let folded = separators_folded(hay);
        match paths.find(&*folded) {
            Some(found) => self.path_roles.get(found.pattern().as_usize()).copied(),
            None => self.wide_first(hay),
        }
    }

    fn wide_first(&self, hay: &[u8]) -> Option<RoleSet> {
        let found = self.wide.as_ref()?.find(hay)?;
        self.wide_roles.get(found.pattern().as_usize()).copied()
    }
}

/// `bytes` with each run of separators (`/`, `\\`) made one `/`: the form [`spelled_in`]
/// compares in. Borrowed when it is already in that form.
fn separators_folded(bytes: &[u8]) -> Cow<'_, [u8]> {
    let sep = |it: u8| it == b'/' || it == b'\\';
    let folded =
        memchr::memchr(b'\\', bytes).is_none() && memchr::memmem::find(bytes, b"//").is_none();
    if folded {
        return Cow::Borrowed(bytes);
    }
    let mut out = Vec::with_capacity(bytes.len()); // alloc: once per input that has a doubled or turned separator
    let mut last_sep = false;
    for &byte in bytes {
        let is_sep = sep(byte);
        if is_sep && last_sep {
            continue;
        }
        out.push(if is_sep { b'/' } else { byte });
        last_sep = is_sep;
    }
    Cow::Owned(out)
}

/// Whether the path `needle` is anywhere in `hay` as a path spelled loosely: each run of
/// separators in either (`/`, `\\`, so `//`, a Windows path and JSON's `\/`) as one, and
/// ASCII letters in any case.
fn spelled_in(hay: &[u8], needle: &[u8]) -> bool {
    let sep = |it: u8| it == b'/' || it == b'\\';
    // An empty needle names no path.
    let Some(&first) = needle.first() else { return false };
    (0..hay.len())
        .filter(|&at| {
            sep(hay[at]) == sep(first) && (sep(first) || hay[at].eq_ignore_ascii_case(&first))
        })
        .any(|start| {
            let (mut i, mut j) = (0, start);
            while i < needle.len() {
                let Some(&got) = hay.get(j) else { return false };
                if sep(needle[i]) {
                    if !sep(got) {
                        return false;
                    }
                    while i < needle.len() && sep(needle[i]) {
                        i += 1;
                    }
                    while hay.get(j).copied().is_some_and(sep) {
                        j += 1;
                    }
                } else {
                    if !got.eq_ignore_ascii_case(&needle[i]) {
                        return false;
                    }
                    i += 1;
                    j += 1;
                }
            }
            true
        })
}

/// The path from the directory `from` to `to`, both absolute: a `..` for each of `from`'s
/// names below the two's common part, then `to`'s names below it.
fn relative(from: &str, to: &str) -> String {
    fn names(path: &str) -> Vec<&str> {
        path.split('/').filter(|name| !name.is_empty()).collect() // alloc: a scan as a run is kept
    }
    let (from, to) = (names(from), names(to));
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    let mut out = String::new(); // alloc: as above
    for _ in common..from.len() {
        out.push_str("../");
    }
    out.push_str(&to[common..].join("/"));
    out
}

/// Whether `root` is in `hay` as UTF-16 (a Windows object's or a wide string's): little- or
/// big-endian.
fn utf16_in(hay: &[u8], root: &str) -> bool {
    let units: Vec<u16> = root.encode_utf16().collect(); // alloc: a scan as a run is kept
    let le: Vec<u8> = units.iter().flat_map(|it| it.to_le_bytes()).collect(); // alloc: as above
    let be: Vec<u8> = units.iter().flat_map(|it| it.to_be_bytes()).collect(); // alloc: as above
    hay.windows(le.len().max(1)).any(|it| it == le.as_slice() || it == be.as_slice())
}

/// A run's trace as it is kept ([`Roots::kept`]).
#[derive(Debug, PartialEq, Eq)]
pub struct Marked {
    /// Ported, with a mark for every root the run has: found again only where each of its
    /// roots is where it was.
    pub local: Traced,
    /// Ported, with no mark: what another checkout's run is compared under before it is
    /// given the value ([`verify`]). None when the run is tied to its roots' strings, or has
    /// no roots.
    pub shared: Option<Traced>,
    /// The roots found tying it to its roots' strings ([`Roots::kept`]): every root for a read outside them or
    /// moved links.
    pub bound: RoleSet,
    /// Whether a root's links moved since its map was made ([`Roots::relinked`]).
    pub relinked: bool,
}

/// The path of `role`'s mark.
fn mark_path(role: Role) -> PathBuf {
    PathBuf::from(format!("{}{}/{ROOT}", char::from(MARK), char::from(role.tag()))) // alloc: a mark, once per root of a run kept
}

/// What a mark of the root at `root` hashes as.
fn root_hash(root: &str) -> [u8; 32] {
    ContentId::of(ROOT_DOMAIN, root.as_bytes()).0
}

/// The tag of the root `path` is the mark of, when it is one.
fn marked(path: &Path) -> Option<u8> {
    let bytes = path.as_os_str().as_encoded_bytes();
    match bytes.strip_suffix(ROOT.as_bytes())? {
        [MARK, tag, b'/'] => Some(*tag),
        _ => None,
    }
}

/// A path or string written against roots this run has not, or that is not one written
/// against roots: the key's roots and the store's bytes disagree.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct NoRoot {
    /// The path or string, as text.
    pub what: String,
    pub why: &'static str,
}

impl fmt::Display for NoRoot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the kept {:?} can't be read against this run's roots: {}", self.what, self.why)
    }
}

impl std::error::Error for NoRoot {}

impl From<NoRoot> for Unreadable {
    fn from(it: NoRoot) -> Unreadable {
        Unreadable { path: PathBuf::from(it.what), error: it.why.to_owned() } // alloc: error path
    }
}

impl From<NoRoot> for String {
    fn from(it: NoRoot) -> String {
        it.to_string() // alloc: error path
    }
}

/// `inner`, the inputs as they are now, asked through a kept shape written against roots: each
/// path's placeholder resolved against this run's roots, each mark answered with this run's
/// root's path, and each variable's value ported as the kept one was. What every check of a
/// run kept with [`Roots::traced`] goes through.
pub struct Rooted<'r, 'i, I> {
    roots: Roots<'r>,
    inner: &'i mut I,
    /// The last variable's value ported, lent to the check.
    value: OsString,
    /// The whole environment ported, when a value in it has a root.
    all: Option<Vec<(OsString, OsString)>>,
}

impl<'r, 'i, I> Rooted<'r, 'i, I> {
    pub fn new(roots: Roots<'r>, inner: &'i mut I) -> Rooted<'r, 'i, I> {
        Rooted { roots, inner, value: OsString::new(), all: None }
    }
}

impl<I: Inputs> Inputs for Rooted<'_, '_, I>
where
    I::Error: From<NoRoot>,
{
    type Error = I::Error;

    fn file(&mut self, path: &Path) -> Result<Option<[u8; 32]>, I::Error> {
        if let Some(tag) = marked(path) {
            let root = self.roots.root_of(tag, path.as_os_str().as_encoded_bytes())?;
            return Ok(Some(root_hash(root)));
        }
        let real = self.roots.real_path(path)?;
        self.inner.file(&real)
    }

    fn dir(&mut self, path: &Path) -> Result<Option<[u8; 32]>, I::Error> {
        let real = self.roots.real_path(path)?;
        self.inner.dir(&real)
    }

    fn env(&mut self, name: &OsStr) -> Option<&OsStr> {
        let Rooted { roots, inner, value, .. } = self;
        let found = inner.env(name)?;
        match roots.os(found) {
            Cow::Borrowed(_) => Some(found),
            Cow::Owned(ported) => {
                *value = ported;
                Some(value.as_os_str())
            }
        }
    }

    fn env_all(&mut self) -> &[(OsString, OsString)] {
        let Rooted { roots, inner, all, .. } = self;
        let touched = inner
            .env_all()
            .iter()
            .any(|(_, value)| value.to_str().is_some_and(|it| roots.touches(it.as_bytes())));
        if !touched {
            return inner.env_all();
        }
        // The names are as they were, so the list is still sorted by them.
        let ported = inner
            .env_all()
            .iter()
            .map(|(name, value)| (name.clone(), roots.os(value).into_owned())) // clone: a whole environment with a root in it, once per check that reads it
            .collect(); // alloc: as above
        all.insert(ported).as_slice()
    }

    fn names(&mut self, path: &Path, out: &mut Names<'_>) -> Result<bool, I::Error> {
        let real = self.roots.real_path(path)?;
        self.inner.names(&real, out)
    }

    fn entry(&mut self, path: &Path) -> Result<Entry, I::Error> {
        let real = self.roots.real_path(path)?;
        self.inner.entry(&real)
    }

    fn tree(&mut self, dir: &Path, package: bool) -> Result<Option<[u8; 32]>, I::Error> {
        let real = self.roots.real_path(dir)?;
        self.inner.tree(&real, package)
    }
}

pub mod verify;

// Windows has no roots: every key there is absolute.
#[cfg(all(test, not(windows)))]
mod tests;
