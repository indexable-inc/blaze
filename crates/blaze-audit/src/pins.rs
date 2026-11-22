//! What `blaze.lock` pins by tree digest: each git source at its commit, and the advisory
//! database. blaze-units' `BlazeLock` reads and writes the rest of the file and hands these
//! two tables here, so their format has one home.
//!
//! ```toml
//! [git."https://github.com/o/r"]
//! 575ed6b8ceb79ffaf247ecf7e34333876b0d5be4 = "blake3:9f2c…"
//!
//! [advisories]
//! url = "https://github.com/rustsec/advisory-db"
//! commit = "0c2b…"
//! tree = "blake3:41aa…"
//! ```
//!
//! `[git."<url>"]` maps each commit of that repository the lock file pins (the whole id: 40 or
//! 64 lower-case hex digits) to the digest of its tree ([`crate::tree`]). The url is the one
//! `Cargo.lock` names after `git+`, without its query and commit. `[advisories]` is the
//! database `blaze audit` reads: its tree, and where it was fetched from when `blaze audit
//! fetch` fetched it (both `url` and `commit`, or neither). A digest is `<algo>:<hex>` in a
//! strong algorithm (blake3, sha256, sha512); md5 and sha1 are refused by name. Written with
//! sorted keys, so a write of the same pins is the same bytes.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use alloc::{borrow::ToOwned, format, string::{String, ToString}, vec::Vec};
use alloc::{alloc::Allocator, alloc::Global};
use core::fmt::Write as _;

use blaze_data::Value;
use blaze_fetch::Digest;
use blaze_toml::Item;

/// One git source's pin.
#[derive(Clone, PartialEq, Eq, Debug, PartialOrd, Ord)]
pub struct GitPin {
    pub url: String,
    /// The whole commit id, lower case.
    pub commit: String,
    pub tree: Digest,
}

/// The advisory database's pin.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DatabasePin {
    /// Where `blaze audit fetch` fetched it from, and at which commit; none for a database a
    /// person placed and `blaze audit pin` recorded.
    pub from: Option<(String, String)>,
    pub tree: Digest,
}

/// The tree pins of a `blaze.lock`.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct SourcePins {
    /// Sorted by url, then commit; each (url, commit) once.
    pub git: Vec<GitPin>,
    pub advisories: Option<DatabasePin>,
}

/// Whether `commit` is a whole git object id as `blaze.lock` writes one: SHA-1's 40 or
/// SHA-256's 64 lower-case hex digits.
pub fn is_commit(commit: &str) -> bool {
    matches!(commit.len(), 40 | 64) && commit.bytes().all(|it| matches!(it, b'0'..=b'9' | b'a'..=b'f'))
}

/// A TOML basic string.
fn quoted(text: &str) -> String {
    let value: Value<Global> = Value::Text(blaze_own::String::from_str_in(text, Global));
    let mut out = blaze_own::String::new_in(Global);
    match blaze_toml::write_inline(&value, &mut out) {
        Ok(()) => out.as_str().to_owned(),
        Err(why) => unreachable!("a text has a TOML form: {why}"),
    }
}

/// ` (line L, column C)`: where `item` is in `blaze.lock`.
fn place<A: Allocator>(item: &Item<'_, '_, A>) -> String {
    let (line, col) = item.line_col();
    format!(" (line {line}, column {col})")
}

fn digest_of(text: &str, at: &str, bad: &impl Fn(String) -> String) -> Result<Digest, String> {
    Digest::parse(text).map_err(|why| bad(format!("has `{at}` = `{text}`, which is no strong digest: {why}")))
}

impl SourcePins {
    pub fn is_empty(&self) -> bool {
        self.git.is_empty() && self.advisories.is_none()
    }

    /// The pin of `url` at `commit`, if `blaze.lock` has one.
    pub fn git(&self, url: &str, commit: &str) -> Option<&GitPin> {
        let at = self.git.binary_search_by(|it| (it.url.as_str(), it.commit.as_str()).cmp(&(url, commit))).ok()?;
        self.git.get(at)
    }

    /// `pin` recorded, replacing the one of its url and commit.
    pub fn set_git(&mut self, pin: GitPin) {
        match self.git.binary_search_by(|it| (it.url.as_str(), it.commit.as_str()).cmp(&(&pin.url, &pin.commit))) {
            Ok(at) => self.git[at] = pin,
            Err(at) => self.git.insert(at, pin),
        }
    }

    /// The `[git]` table of `blaze.lock` at `path` (for errors): every value a strong digest
    /// under a whole commit. `bad` makes an error of what is wrong, naming the file and fix.
    pub fn parse_git<A: Allocator>(&mut self, value: Item<'_, '_, A>, bad: &impl Fn(String) -> String) -> Result<(), String> {
        if !value.is_table() {
            return Err(bad(format!("has `git`, which is not a table{}", place(&value))));
        }
        for commits in value.children() {
            let url = commits.key();
            if url.is_empty() || url.contains(char::is_whitespace) {
                return Err(bad(format!("has `git.{}`, which is no url{}", quoted(url), place(&commits))));
            }
            if !commits.is_table() {
                return Err(bad(format!("has `git.{}`, which is not a table of commits{}", quoted(url), place(&commits))));
            }
            for tree in commits.children() {
                let commit = tree.key();
                let at = format!("git.{}.{commit}", quoted(url));
                if !is_commit(commit) {
                    return Err(bad(format!(
                        "has `{at}`, whose key is no whole commit id (40 or 64 lower-case hex digits){}",
                        place(&tree)
                    )));
                }
                let text = tree.as_str().ok_or_else(|| bad(format!("has `{at}`, which is not a string{}", place(&tree))))?;
                let tree = digest_of(text, &at, bad)?;
                self.set_git(GitPin { url: url.to_owned(), commit: commit.to_owned(), tree });
            }
        }
        Ok(())
    }

    /// The `[advisories]` table of `blaze.lock`.
    pub fn parse_advisories<A: Allocator>(&mut self, value: Item<'_, '_, A>, bad: &impl Fn(String) -> String) -> Result<(), String> {
        if !value.is_table() {
            return Err(bad(format!("has `advisories`, which is not a table{}", place(&value))));
        }
        let (mut url, mut commit, mut tree) = (None, None, None);
        for value in value.children() {
            let key = value.key();
            let at = format!("advisories.{key}");
            let text = value.as_str().ok_or_else(|| bad(format!("has `{at}`, which is not a string{}", place(&value))))?;
            match key {
                "url" => url = Some(text.to_owned()),
                "commit" if is_commit(text) => commit = Some(text.to_owned()),
                "commit" => return Err(bad(format!("has `{at}` = `{text}`, which is no whole commit id{}", place(&value)))),
                "tree" => tree = Some(digest_of(text, &at, bad)?),
                other => {
                    return Err(bad(format!(
                        "has `advisories.{other}`, which is not known (`url`, `commit` and `tree` are){}",
                        place(&value)
                    )));
                }
            }
        }
        let Some(tree) = tree else {
            return Err(bad("has an `[advisories]` without its `tree`".to_owned()));
        };
        let from = match (url, commit) {
            (Some(url), Some(commit)) => Some((url, commit)),
            (None, None) => None,
            _ => return Err(bad("has an `[advisories]` with one of `url` and `commit`: both or neither".to_owned())),
        };
        self.advisories = Some(DatabasePin { from, tree });
        Ok(())
    }

    /// The two tables as `blaze.lock` holds them, each only when it has a pin, keys sorted.
    pub fn render(&self, out: &mut String) {
        let mut url: Option<&str> = None;
        for pin in &self.git {
            if url != Some(pin.url.as_str()) {
                _ = write!(out, "\n[git.{}]\n", quoted(&pin.url));
                url = Some(&pin.url);
            }
            _ = writeln!(out, "{} = {}", pin.commit, quoted(&pin.tree.to_string()));
        }
        if let Some(db) = &self.advisories {
            out.push_str("\n[advisories]\n");
            if let Some((url, commit)) = &db.from {
                _ = writeln!(out, "commit = {}", quoted(commit));
                _ = writeln!(out, "tree = {}", quoted(&db.tree.to_string()));
                _ = writeln!(out, "url = {}", quoted(url));
            } else {
                _ = writeln!(out, "tree = {}", quoted(&db.tree.to_string()));
            }
        }
    }

    /// What moved from `old` to `self`, one line each, as `blaze lock` says a pin's move.
    pub fn moves(&self, old: &SourcePins) -> Vec<String, Global> {
        let mut said = Vec::new_in(Global);
        for pin in &self.git {
            match old.git(&pin.url, &pin.commit) {
                None => said.push(format!("+ git {}#{} {}", pin.url, &pin.commit[..12], pin.tree)),
                Some(was) if was.tree != pin.tree => {
                    said.push(format!("~ git {}#{} {} → {}", pin.url, &pin.commit[..12], was.tree, pin.tree));
                }
                Some(_) => {}
            }
        }
        for pin in &old.git {
            if self.git(&pin.url, &pin.commit).is_none() {
                said.push(format!("- git {}#{}", pin.url, &pin.commit[..12]));
            }
        }
        match (&old.advisories, &self.advisories) {
            (None, Some(new)) => said.push(format!("+ advisories {}", new.tree)),
            (Some(was), Some(new)) if was != new => said.push(format!("~ advisories {} → {}", was.tree, new.tree)),
            (Some(_), None) => said.push("- advisories".to_owned()),
            _ => {}
        }
        said
    }
}
