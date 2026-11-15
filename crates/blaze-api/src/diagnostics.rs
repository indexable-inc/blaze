//! What [`Request::Diagnostics`] answers (`blaze diagnostics --since`, the agents' after-write
//! hook): a check's errors and warnings against the ones the same client was last told of the
//! same checkout and scope, as new, fixed and unchanged ([`Seen`]), each under an id ([`id`])
//! that an edit which only moves lines leaves alone.
//!
//! The core here is sans-IO: [`found`] of a diagnostic from the source line it points at (the
//! session reads it from the job's view), [`diff`] of two lists, and [`Bases`], the bounded
//! table of what each client was last told, which lives in the [`crate::session::Session`] and
//! nowhere else.
//!
//! The id today is the file's form ([`id`]): its level, its code, its file relative to the
//! checkout, its message with line references made `#` (`:12:5`, `line 12`, `lines 3-5`), the
//! text of its primary span's first line, trimmed, and that span's label made the same way. No
//! line number: one inserted line moves every diagnostic below it. The end state is the item's
//! form, the rule's code, the path of the item the span is in and the span relative to that
//! item, which a diagnostic does not carry yet; every diagnostic gets the file's form until it
//! does.
//!
//! [`Request::Diagnostics`]: crate::Request::Diagnostics

use std::{alloc::Global, collections::VecDeque};

use blaze_hash::HashMap;
use blaze_ids::{ContentId, Hasher};
use serde::{Deserialize, Serialize};

use crate::{ApiError, Diagnostic, Level, Span, Targets, ViewRef};

/// How many bases [`Bases`] holds at most: one for each (checkout, scope, client) answered
/// last. Past it the least recently answered is dropped first; a client that asks with its
/// token is then told [`Base::Unknown`] and everything is new.
pub const BASES: usize = 64;

/// How many diagnostics the held bases hold together at most. Past it the least recently
/// answered bases are dropped first, down to the one just kept, which is kept whatever its size.
pub const HELD: usize = 65_536;

/// The one hash domain of this module's ids; each id's first part says which it is.
const DOMAIN: &str = "blaze v2 diagnostics 1";

/// One error or warning of a check, as [`Seen`] says it. Its fields are declared in the order
/// CBOR writes a map's keys.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Found {
    /// [`id`]'s: equal across an edit that only moves it.
    pub id: ContentId,
    /// rustc's error code (`E0308`) or the lint's name.
    pub code: Option<String>,
    /// Its children's messages, as `help: ...` and `note: ...`.
    pub help: Vec<String>,
    /// Its primary span (the first span when none is marked primary), its `file` relative to
    /// the checkout's root when the view has it (else as rustc named it: the standard
    /// library's, a registry crate's); `None` for one of the whole request.
    pub span: Option<Span>,
    /// Every suggested replacement of it and its children: the span with its replacement.
    pub fixes: Vec<Span>,
    pub level: Level,
    pub message: String,
    /// The package it is of (its unit's package id); `None` for one of the whole request.
    pub package: Option<String>,
    /// Of a package the request asked for ([`crate::UnitOf::primary`]), not of a dependency
    /// checked only because one depends on it; true for one of the whole request.
    pub primary: bool,
    /// rustc's text for it.
    pub rendered: String,
}

/// What a [`Seen`] was diffed against.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum Base {
    /// No token was given: everything is new.
    First,
    /// The token is the one the daemon holds for this checkout, scope and client.
    Held,
    /// The daemon holds no base under the token for this checkout, scope and client: it was
    /// dropped (past [`BASES`] or [`HELD`]), the daemon restarted, or it is another's.
    /// Everything is new.
    Unknown,
}

/// [`Request::Diagnostics`]'s answer. Its fields are declared in the order CBOR writes a map's
/// keys.
///
/// [`Request::Diagnostics`]: crate::Request::Diagnostics
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Seen {
    /// In the check, beyond what the base had: by file, line and column.
    pub new: Vec<Found>,
    pub base: Base,
    /// The root that was checked.
    pub root: ContentId,
    /// The build's number ([`crate::Worked::build_id`]) when the check said it.
    pub build: Option<u64>,
    /// The check ended. False: it was still running at the request's `wait_ms`, so nothing is
    /// new, fixed or unchanged, the base is kept, and the check goes on in the daemon.
    pub ended: bool,
    /// In the base, beyond what the check had.
    pub fixed: Vec<Found>,
    /// What to send as `since` next: the base now held. `None` when none is (a first check
    /// that did not end or failed).
    pub token: Option<ContentId>,
    /// The check could not run, or blaze failed in it (not the code): what and where. The base
    /// is kept.
    pub failed: Option<ApiError>,
    /// In both.
    pub unchanged: Vec<Found>,
}

/// What a base is held under: the checkout, the scope the request checks, and the client.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct Key {
    /// The head the view names, or the root's hex for a view of a root.
    pub checkout: String,
    /// [`scope`]'s.
    pub scope: ContentId,
    pub client: String,
}

impl Key {
    pub fn of(view: &ViewRef, targets: &Targets, client: String) -> Key {
        Key { checkout: checkout(view), scope: scope(targets), client }
    }
}

/// The checkout a view is of: its head, the root's hex, or a what-if's base's.
fn checkout(view: &ViewRef) -> String {
    match view {
        ViewRef::Head(head) => head.clone(), // clone: the key's own copy, once a request
        ViewRef::Root(view) => format!("{:x}", view.root), // alloc: as above
        ViewRef::WhatIf { base, .. } => checkout(base),
    }
}

/// What a check of `targets` checks: its packages, features, profile and target, and the
/// command line's words and directory, which say the same. Not its environment, width or color,
/// which change no diagnostic's id and differ between two shells of one person.
pub fn scope(targets: &Targets) -> ContentId {
    let mut hasher = Hasher::new(DOMAIN);
    hasher.part(b"scope").part(targets.profile.as_bytes()).opt_part(targets.target.as_deref());
    hasher.update(&[u8::from(targets.all_features), u8::from(targets.no_default_features)]);
    hasher.update(&(targets.packages.len() as u64).to_le_bytes());
    for package in &targets.packages {
        hasher.part(package.as_bytes());
    }
    hasher.update(&(targets.features.len() as u64).to_le_bytes());
    for feature in &targets.features {
        hasher.part(feature.as_bytes());
    }
    match &targets.cargo {
        None => {
            hasher.update(&[0]);
        }
        Some(cargo) => {
            hasher.update(&[1]).part(cargo.cwd.as_bytes()).part(cargo.command.as_bytes());
            hasher.update(&(cargo.args.len() as u64).to_le_bytes());
            for arg in &cargo.args {
                hasher.part(arg.as_bytes());
            }
        }
    }
    hasher.finish()
}

/// The id of `diagnostic`, whose primary span's file is `file` relative to the checkout and
/// whose first line there is `line` (`None` when it is no file of the view).
pub fn id(diagnostic: &Diagnostic, file: &str, line: Option<&str>) -> ContentId {
    let mut scratch = String::new(); // alloc: a message's normalized text, once a diagnostic
    let mut hasher = Hasher::new(DOMAIN);
    hasher.part(b"id").part(diagnostic.level.name().as_bytes());
    hasher.opt_part(diagnostic.code.as_deref()).part(file.as_bytes());
    normalized_into(&diagnostic.message, &mut scratch);
    hasher.part(scratch.as_bytes());
    hasher.part(line.map_or("", str::trim).as_bytes());
    scratch.clear();
    let label = primary(&diagnostic.spans).and_then(|it| it.label.as_deref());
    if let Some(label) = label {
        normalized_into(label, &mut scratch);
    }
    hasher.opt_part(label.map(|_| scratch.as_bytes()));
    hasher.finish()
}

/// The span a diagnostic is at: the one marked primary, else the first.
pub fn primary(spans: &[Span]) -> Option<&Span> {
    spans.iter().find(|it| it.primary).or(spans.first())
}

/// Whether a diagnostic of `level` is said by [`Request::Diagnostics`]: errors and warnings,
/// as an agent acts on them; notes, helps and rustc's closing lines are not diagnostics of the
/// code of their own (a child note is said under its parent, in [`Found::help`]).
///
/// [`Request::Diagnostics`]: crate::Request::Diagnostics
pub fn said(level: Level) -> bool {
    matches!(level, Level::Error | Level::Warning)
}

/// `diagnostic` as [`Seen`] says it, its primary span's file `file` relative to the checkout
/// and its first line there `line`.
pub fn found(diagnostic: Diagnostic, file: &str, line: Option<&str>) -> Found {
    let id = id(&diagnostic, file, line);
    let mut span = primary(&diagnostic.spans).cloned(); // clone: the answer's own span
    // The span says where it is as the client reads it: relative to the checkout, the place
    // the view has it at, not the name rustc gave relative to the directory it ran in.
    if let Some(span) = span.as_mut()
        && !file.is_empty()
    {
        file.clone_into(&mut span.file);
    }
    let mut fixes = Vec::new(); // alloc: the answer's own list, once a diagnostic
    let mut help = Vec::new(); // alloc: as above
    replacements(&diagnostic, &mut fixes);
    for child in &diagnostic.children {
        help.push(format!("{}: {}", child.level.name(), child.message)); // alloc: as above
    }
    let package = diagnostic.unit.as_ref().map(|it| it.package_id.clone()); // clone: as above
    Found {
        id,
        code: diagnostic.code,
        help,
        span,
        fixes,
        level: diagnostic.level,
        message: diagnostic.message,
        package,
        primary: diagnostic.unit.as_ref().is_none_or(|it| it.primary),
        rendered: diagnostic.rendered,
    }
}

fn replacements(diagnostic: &Diagnostic, out: &mut Vec<Span>) {
    for span in diagnostic.spans.iter().filter(|it| it.replacement.is_some()) {
        out.push(span.clone()); // clone: the answer's own span
    }
    for child in &diagnostic.children {
        replacements(child, out);
    }
}

/// Where a span's file is in the view, relative to its root, most likely first: the name as
/// it is when relative, under `head` when absolute; then under each directory from the unit's
/// manifest directory (`manifest_dir`, relative to the root) up, as `Sources::file` in
/// blaze-cargo's `cli/json.rs` resolves a name rustc gave relative to the directory it ran in.
pub fn places(file: &str, head: &str, manifest_dir: Option<&str>) -> Vec<String> {
    let mut places = Vec::new(); // alloc: a few candidates, once a diagnostic
    if let Some(rest) = file.strip_prefix(head).and_then(|it| it.strip_prefix('/')) {
        places.push(rest.to_owned()); // alloc: as above
        return places;
    }
    if file.starts_with('/') {
        return places;
    }
    let mut dir = manifest_dir.unwrap_or("");
    loop {
        let place = match dir {
            "" => file.to_owned(), // alloc: as above
            dir => format!("{dir}/{file}"), // alloc: as above
        };
        if !places.contains(&place) {
            places.push(place);
        }
        match dir.rfind('/') {
            Some(at) => dir = &dir[..at],
            None if dir.is_empty() => break,
            None => dir = "",
        }
    }
    places
}

/// The first line of `bytes` at 1-based `line`, as UTF-8 (lossily: a line that is not reads as
/// what it says); `None` past its end.
pub fn line_of(bytes: &[u8], line: u32) -> Option<String> {
    let index = usize::try_from(line.checked_sub(1)?).ok()?;
    let text = bytes.split(|&it| it == b'\n').nth(index)?;
    Some(String::from_utf8_lossy(text).into_owned()) // alloc: the line's text, once a diagnostic
}

/// `found` put in the answer's order (by file, line, column, level and id, so the answer and
/// its token depend on nothing but the diagnostics), and each said twice at one place (the
/// library and its unit tests both check one function) made one.
pub fn settle(found: &mut Vec<Found>) {
    fn at(it: &Found) -> (&str, u32, u32) {
        it.span.as_ref().map_or(("", 0, 0), |span| (span.file.as_str(), span.line, span.column))
    }
    found.sort_by(|a, b| {
        (at(a), level_rank(a.level), a.id, &a.message).cmp(&(
            at(b),
            level_rank(b.level),
            b.id,
            &b.message,
        ))
    });
    found.dedup_by(|a, b| at(a) == at(b) && a.level == b.level && a.id == b.id);
}

fn level_rank(level: Level) -> u8 {
    match level {
        Level::Error => 0,
        Level::Warning => 1,
        Level::Note => 2,
        Level::Help => 3,
        Level::FailureNote => 4,
    }
}

/// `now` against `base`, counted as multisets of ids (the same warning on two lines is two):
/// (new, unchanged, fixed). `now` is in its settled order and stays so.
pub fn diff(base: &[Found], now: Vec<Found>) -> (Vec<Found>, Vec<Found>, Vec<Found>) {
    let mut left: HashMap<ContentId, u32, Global> = HashMap::new_in(Global);
    for it in base {
        *left.entry(it.id).or_insert(0) += 1;
    }
    let (mut new, mut unchanged) = (Vec::new(), Vec::new()); // alloc: the answer's lists
    for it in now {
        match left.get_mut(&it.id) {
            Some(count) if *count > 0 => {
                *count -= 1;
                unchanged.push(it);
            }
            _ => new.push(it),
        }
    }
    let mut fixed = Vec::new(); // alloc: as above
    for it in base {
        if let Some(count) = left.get_mut(&it.id).filter(|it| **it > 0) {
            *count -= 1;
            fixed.push(it.clone()); // clone: the answer's copy; the base is dropped after
        }
    }
    (new, unchanged, fixed)
}

/// What each (checkout, scope, client) was last told, bounded by [`BASES`] and [`HELD`], the
/// least recently answered dropped first.
#[derive(Default)]
pub struct Bases {
    /// Oldest answered first.
    held: VecDeque<Held>,
    /// The diagnostics of every base held.
    count: usize,
}

struct Held {
    key: Key,
    token: ContentId,
    found: Vec<Found>,
}

impl Bases {
    /// What `since` is for `key`.
    pub fn base(&self, key: &Key, since: Option<ContentId>) -> Base {
        match since {
            None => Base::First,
            Some(since) if self.find(key).is_some_and(|at| self.held[at].token == since) => {
                Base::Held
            }
            Some(_) => Base::Unknown,
        }
    }

    /// How many bases are held, and their diagnostics together.
    pub fn held(&self) -> (usize, usize) {
        (self.held.len(), self.count)
    }

    fn find(&self, key: &Key) -> Option<usize> {
        self.held.iter().position(|it| it.key == *key)
    }

    /// The check of `key` ended with `now` (settled): the answer against the base `since`
    /// names, and `now` held as the next base in its place.
    pub fn ended(
        &mut self,
        key: Key,
        since: Option<ContentId>,
        mut now: Vec<Found>,
        root: ContentId,
        build: Option<u64>,
    ) -> Seen {
        settle(&mut now);
        let base = self.base(&key, since);
        let token = token(&key, &now);
        let kept = now.clone(); // clone: the base held for the next request, once an answer
        let (new, unchanged, fixed) = match (base, self.find(&key)) {
            (Base::Held, Some(at)) => diff(&self.held[at].found, now),
            _ => (now, Vec::new(), Vec::new()), // alloc: none, empty
        };
        self.keep(key, token, kept);
        Seen {
            new,
            base,
            root,
            build,
            ended: true,
            fixed,
            token: Some(token),
            failed: None,
            unchanged,
        }
    }

    /// The check of `key` did not end by the request's wait, or failed (`failed`): nothing
    /// said of it, the base kept.
    pub fn kept(
        &self,
        key: &Key,
        since: Option<ContentId>,
        root: ContentId,
        failed: Option<ApiError>,
    ) -> Seen {
        let base = self.base(key, since);
        Seen {
            new: Vec::new(), // alloc: none, empty
            base,
            root,
            build: None,
            ended: failed.is_some(),
            fixed: Vec::new(), // alloc: none, empty
            token: since.filter(|_| base == Base::Held),
            failed,
            unchanged: Vec::new(), // alloc: none, empty
        }
    }

    fn keep(&mut self, key: Key, token: ContentId, found: Vec<Found>) {
        if let Some(at) = self.find(&key) {
            let old = self.held.remove(at).expect("a held base");
            self.count -= old.found.len();
        }
        self.count += found.len();
        self.held.push_back(Held { key, token, found });
        while self.held.len() > BASES || (self.count > HELD && self.held.len() > 1) {
            let oldest = self.held.pop_front().expect("a base past the bound");
            self.count -= oldest.found.len();
        }
    }
}

/// The token of the base `found` (settled) of `key`: its hash, so a token names one base of one
/// key and a restarted daemon, which holds none, knows none.
fn token(key: &Key, found: &[Found]) -> ContentId {
    let mut hasher = Hasher::new(DOMAIN);
    hasher.part(b"token").part(key.checkout.as_bytes()).id(&key.scope);
    hasher.part(key.client.as_bytes());
    for it in found {
        hasher.id(&it.id);
    }
    hasher.finish()
}

/// `text` with every line reference made `#`: `:12:5` (a place, as `file:12:5` names it) and
/// `line 12`, `lines 3-5`, `lines 3 to 5` on word boundaries, as the agents' Python core did
/// (`(?::\d+:\d+\b)|(?:\blines? \d+(?:\s*(?:-|to)\s*\d+)?\b)`, word characters ASCII here).
pub fn normalized_into(text: &str, out: &mut String) {
    let bytes = text.as_bytes();
    let mut at = 0;
    while at < bytes.len() {
        if let Some(end) = place_ref(bytes, at).or_else(|| lines_ref(bytes, at)) {
            out.push('#');
            at = end;
            continue;
        }
        let ch = text[at..].chars().next().expect("at is on a char boundary");
        out.push(ch);
        at += ch.len_utf8();
    }
}

fn word(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// The end of a run of digits from `at`, at least one.
fn digits(bytes: &[u8], at: usize) -> Option<usize> {
    let end = at + bytes[at..].iter().take_while(|it| it.is_ascii_digit()).count();
    (end > at).then_some(end)
}

/// A word boundary after a digit at `end`: the end of the text or a byte that is no word's.
fn boundary(bytes: &[u8], end: usize) -> bool {
    bytes.get(end).is_none_or(|&it| !word(it))
}

/// `:\d+:\d+\b` at `at`: its end.
fn place_ref(bytes: &[u8], at: usize) -> Option<usize> {
    (bytes[at] == b':').then_some(())?;
    let end = digits(bytes, at + 1)?;
    (bytes.get(end) == Some(&b':')).then_some(())?;
    let end = digits(bytes, end + 1)?;
    boundary(bytes, end).then_some(end)
}

/// `\blines? \d+(?:\s*(?:-|to)\s*\d+)?\b` at `at`: its end.
fn lines_ref(bytes: &[u8], at: usize) -> Option<usize> {
    if at > 0 && word(bytes[at - 1]) {
        return None;
    }
    let mut end = at + bytes[at..].starts_with(b"line").then_some(4)?;
    if bytes.get(end) == Some(&b's') {
        end += 1;
    }
    (bytes.get(end) == Some(&b' ')).then_some(())?;
    let first = digits(bytes, end + 1)?;
    let range = (|| {
        let mut at = first;
        while bytes.get(at).is_some_and(u8::is_ascii_whitespace) {
            at += 1;
        }
        at += match () {
            _ if bytes.get(at) == Some(&b'-') => 1,
            _ if bytes[at.min(bytes.len())..].starts_with(b"to") => 2,
            _ => return None,
        };
        while bytes.get(at).is_some_and(u8::is_ascii_whitespace) {
            at += 1;
        }
        let end = digits(bytes, at.min(bytes.len()))?;
        boundary(bytes, end).then_some(end)
    })();
    match range {
        Some(end) => Some(end),
        None => boundary(bytes, first).then_some(first),
    }
}

#[cfg(test)]
mod tests;
