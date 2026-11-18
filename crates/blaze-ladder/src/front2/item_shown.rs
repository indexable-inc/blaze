//! The one printer of an item in a refusal (rule 19: an error names the thing, by its PATH).
//!
//! A key carries no name (rules 21 and 25: a key holds no machine data and no text), and its
//! short `Display` is its CRATE's part (`blaze_id::ItemKey`: the first 12 hex digits are the
//! crate prefix), so a refusal that prints a key names the crate, never the item: on the hello
//! copy users saw "ItemKey(c41ed2ee1fe2)" for a library key and a 38-digit number for a
//! checker key. So a refusal names its item by the path the printing layer resolves from the
//! names it already holds (the names source and the key map), with the key's ITEM part as a
//! short suffix tools can grep: [`ItemShown`].
//!
//! The path is built by a fixed number of steps (an owner, then the item), never by recursion:
//! a member's (an associated fn, const or type) is its trait's or impl's path and its name; a
//! variant's is its enum's path and its name; an impl, which has no name, is shown by its
//! header as written (`<impl Default for stat>`); an item whose module has no path (a block's
//! module) says so instead of being cut to its bare name.

use core::fmt;
use std::boxed::Box;
use std::format;
use std::string::{String, ToString};

use blaze_id::ItemKey as LibKey;
use blaze_typeck::ItemKey;

use super::check::Records;
use super::heads::{HeadError, Want};
use super::keys::{Keys, Origin};
use super::source::NamesSource;

/// The digits of a key shown after a path: bytes 8..14, the item's part (bytes 0..8 are the
/// crate's prefix, which the path already names).
const TAIL: core::ops::Range<usize> = 8..14;

/// An item for a refusal: its whole path when the names source gives one, and its key.
/// `Display`: `a::b::Item (key 1a2b3c4d5e6f)`; with no path, every digit of the key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemShown {
    /// The item's path, whole; `None`: the names source holds none for the key.
    path: Option<Box<str>>,
    /// For an item with no path, WHERE it is written ([`written`]: its module, and its file and
    /// line or the macro call that wrote it), so a path-less item is still named by a place a
    /// person can open (rule 19). `None` when it has a path, or the source holds no place.
    written: Option<Box<str>>,
    /// The key's first 16 bytes (a checker key is all of them, a library key's first 16).
    key: [u8; 16],
}

impl ItemShown {
    /// A checker key `k` over the key map `keys` (which files members) and `names`.
    pub fn of<'n, S: NamesSource<'n>>(names: S, keys: &Keys, k: ItemKey) -> ItemShown {
        // The library key a place is read from: the item's own, or a member's container's.
        let mut placed = keys.library_in(names, k);
        let path = match keys.origin(k) {
            Some(Origin::Member { container, name, .. }) => {
                let c = keys.library_in(names, *container);
                placed = placed.or(c);
                c.and_then(|c| lib_path(names, c)).map(|o| format!("{o}::{name}"))
            }
            _ => placed.and_then(|lib| lib_path(names, lib)),
        };
        let written = match path {
            Some(_) => None,
            None => placed.and_then(|lib| written(names, lib)),
        };
        ItemShown { path: path.map(String::into_boxed_str), written, key: k.0.to_be_bytes() }
    }

    /// A library key `lib` over `names`.
    pub fn of_lib<'n, S: NamesSource<'n>>(names: S, lib: LibKey) -> ItemShown {
        let [b0, b1, b2, b3, b4, b5, b6, b7, b8, b9, b10, b11, b12, b13, b14, b15, ..] = lib.0.0;
        let key = [b0, b1, b2, b3, b4, b5, b6, b7, b8, b9, b10, b11, b12, b13, b14, b15];
        let path = lib_path(names, lib);
        let written = match path {
            Some(_) => None,
            None => written(names, lib),
        };
        ItemShown { path: path.map(String::into_boxed_str), written, key }
    }

    /// The path, when the names source gave one.
    pub fn path(&self) -> Option<&str> {
        self.path.as_deref()
    }

    /// The key's first 16 bytes, as [`Display`](fmt::Display) shows them.
    pub fn key(&self) -> [u8; 16] {
        self.key
    }

    /// Where a path-less item is written ([`written`]), when the source holds its place.
    pub fn written(&self) -> Option<&str> {
        self.written.as_deref()
    }

    /// The text a refusal's field holds (`Front2Refusal`'s `function`, `adt`, ...).
    pub fn boxed(&self) -> Box<str> {
        self.to_string().into_boxed_str()
    }
}

impl fmt::Display for ItemShown {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.path {
            Some(p) => {
                write!(f, "{p} (key ")?;
                self.key[TAIL].iter().try_for_each(|b| write!(f, "{b:02x}"))?;
                write!(f, ")")
            }
            None => {
                match &self.written {
                    Some(w) => write!(f, "an item with no path, {w} (key ")?,
                    None => write!(f, "an item the names source holds no path for (key ")?,
                }
                self.key.iter().try_for_each(|b| write!(f, "{b:02x}"))?;
                write!(f, ")")
            }
        }
    }
}

/// At most this many macro calls are followed out to a file (rule 18): an expansion's item is
/// placed at its call, which may itself be an expansion's.
const MOST_CALLS: usize = 16;

/// WHERE the path-less item `lib` is written, as words: `written in `core::fmt` at
/// library/core/src/fmt/mod.rs:420`, or `written by a macro call in `core::fmt`, the call at
/// library/core/src/fmt/mod.rs:420` (each call out to the outermost, by [`MOST_CALLS`] steps,
/// never recursion), with whichever part the source holds. Read only for a message: the line is
/// counted in the file's text the source already holds (rule 71: no `file:line` is kept).
/// `None`: the source holds neither the item's module path nor its place.
fn written<'n, S: NamesSource<'n>>(names: S, lib: LibKey) -> Option<Box<str>> {
    use blaze_scope::resolve::At;
    let module = names.home_of(lib).and_then(|m| names.module_path(m)).filter(|p| !p.is_empty());
    let mut k = lib;
    let mut by_macro = false;
    let mut at: Option<String> = None;
    for _ in 0..MOST_CALLS {
        let Some(place) = names.place_of(k) else { break };
        match place.at {
            At::File(file) => {
                let shown = names.file_shown(place.krate, file);
                let line = names.item_start(k).and_then(|(_, start)| {
                    let text = names.file_text(place.krate, file)?;
                    let before = text.as_bytes().get(..start as usize)?;
                    Some(before.iter().filter(|c| **c == b'\n').count() + 1)
                });
                at = match (shown, line) {
                    (Some(s), Some(l)) => Some(format!("{s}:{l}")),
                    (Some(s), None) => Some(format!("{s}")),
                    (None, _) => None,
                };
                break;
            }
            At::Expansion(call) => {
                by_macro = true;
                k = call;
            }
        }
    }
    let how = if by_macro { "written by a macro call" } else { "written" };
    let text = match (module, at) {
        (Some(m), Some(a)) if by_macro => format!("{how} in `{m}`, the call at {a}"),
        (Some(m), Some(a)) => format!("{how} in `{m}` at {a}"),
        (Some(m), None) => format!("{how} in `{m}`"),
        (None, Some(a)) if by_macro => format!("{how}, the call at {a}"),
        (None, Some(a)) => format!("{how} at {a}"),
        (None, None) => return None,
    };
    Some(text.into_boxed_str())
}

/// `k` as a refusal's field holds it ([`ItemShown::boxed`]), over a body's records: what
/// program.rs passes to `Front2Refusal` where it named the item by `reach::fn_shown` (its bare
/// name, which stays the name rows and symbols are made from).
pub(crate) fn refused<'n, S: NamesSource<'n>>(r: &Records<'_, 'n, S>, k: ItemKey) -> Box<str> {
    ItemShown::of(r.names, &*r.keys, k).boxed()
}

/// `lib`'s path: its owner's (a member's trait or impl, a variant's enum, else itself) module
/// path and name, then its own name. `None`: the source names neither the item nor its owner.
fn lib_path<'n, S: NamesSource<'n>>(names: S, lib: LibKey) -> Option<String> {
    let (owner, last) = match names.member_owner(lib) {
        Some((owner, name)) => (owner, Some(name)),
        None => match names.variant_of(lib) {
            // A variant has no item row (no `name_of`): its name is its enum's list's.
            Some((enum_, index)) => (enum_, Some(names.variants(enum_)?.get(index as usize)?.name)),
            None => (lib, None),
        },
    };
    let mut out = String::new();
    match names.home_of(owner) {
        Some(m) => match names.module_path(m) {
            Some(p) if !p.is_empty() => {
                out.push_str(p);
                out.push_str("::");
            }
            Some(_) => {}
            None => out.push_str("<a module the names source holds no path for>::"),
        },
        None => {}
    }
    match names.name_of(owner) {
        Some(n) => out.push_str(n),
        // An impl has no name: its header as written, from its first token.
        None => out.push_str(&impl_shown(names.text_of(owner)?.impl_header?)),
    }
    if let Some(l) = last {
        out.push_str("::");
        out.push_str(l);
    }
    Some(out)
}

/// An impl as a path segment shows it (here and in row_path.rs, a row's path): `<` its header
/// `>`, from its first modifier (`const`, `unsafe`) or the keyword `impl`, on one line. The
/// header's span starts after the impl's outer attributes but not after a comment written
/// between them and the header (core's `// FIXME(fee1-dead): ..` line above
/// `const impl<T, A: ..> RawVec<T, A>`, which hello's stops printed as the path), so the
/// comments, doc comments and attributes before the first token are left out, and each run of
/// whitespace and comments inside it is one space: a path is one line whatever the header's
/// layout. A string literal (an attribute's, a const argument's) is read whole, so a `//` or
/// `]` in it is neither a comment nor an attribute's end. A loop over bytes that cuts only at
/// ASCII bytes, so every slice is on a char boundary.
pub(crate) fn impl_shown(header: &str) -> String {
    let bytes = header.as_bytes();
    let mut out = String::with_capacity(header.len() + 2);
    out.push('<');
    let mut at = 0usize;
    // Whitespace or a comment stands since the last token byte copied.
    let mut gap = false;
    // No token is copied yet: an attribute here is an outer one, left out.
    let mut lead = true;
    while at < bytes.len() {
        let c = bytes[at];
        if c.is_ascii_whitespace() {
            at += 1;
            gap = true;
            continue;
        }
        if c == b'/' && bytes.get(at + 1) == Some(&b'/') {
            while at < bytes.len() && bytes[at] != b'\n' {
                at += 1;
            }
            gap = true;
            continue;
        }
        if c == b'/' && bytes.get(at + 1) == Some(&b'*') {
            at = block_comment_end(bytes, at);
            gap = true;
            continue;
        }
        if lead && c == b'#' {
            if let Some(end) = attribute_end(bytes, at) {
                at = end;
                gap = true;
                continue;
            }
        }
        if gap && !lead {
            out.push(' ');
        }
        gap = false;
        lead = false;
        let start = at;
        if c == b'"' {
            at = string_end(bytes, at);
        } else {
            at += 1;
            while at < bytes.len() && !bytes[at].is_ascii_whitespace() && bytes[at] != b'/' && bytes[at] != b'"' {
                at += 1;
            }
        }
        out.push_str(&header[start..at]);
    }
    out.push('>');
    out
}

/// The byte after the string literal whose opening `"` is at `at` (its escapes skipped); the
/// text's end when it is not closed.
fn string_end(bytes: &[u8], at: usize) -> usize {
    let mut i = at + 1;
    while i < bytes.len() && bytes[i] != b'"' {
        i += if bytes[i] == b'\\' { 2 } else { 1 };
    }
    (i + 1).min(bytes.len())
}

/// The byte after the block comment opened at `at` (`/*`), nested ones counted as Rust nests
/// them; the text's end when it is not closed.
fn block_comment_end(bytes: &[u8], at: usize) -> usize {
    let mut i = at + 2;
    let mut depth = 1usize;
    while i < bytes.len() && depth > 0 {
        match (bytes[i], bytes.get(i + 1)) {
            (b'/', Some(b'*')) => {
                depth += 1;
                i += 2;
            }
            (b'*', Some(b'/')) => {
                depth -= 1;
                i += 2;
            }
            _ => i += 1,
        }
    }
    i
}

/// The byte after the attribute whose `#` is at `at` (`#[..]` or `#![..]`, spaced as an
/// expansion prints it: `# [doc = ".."]`), its brackets balanced outside string literals;
/// `None`: no attribute starts there.
fn attribute_end(bytes: &[u8], at: usize) -> Option<usize> {
    let mut i = at + 1;
    let skip_space = |mut i: usize| {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        i
    };
    i = skip_space(i);
    if bytes.get(i) == Some(&b'!') {
        i = skip_space(i + 1);
    }
    if bytes.get(i) != Some(&b'[') {
        return None;
    }
    let mut depth = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                i = string_end(bytes, i);
                continue;
            }
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// What a want asks for, by name: `the head of a::B (key ..)`.
fn want_shown<'n, S: NamesSource<'n>>(names: S, w: Want) -> String {
    let (what, k) = match w {
        Want::Head(k) => ("the head of", k),
        Want::Adt(k) => ("the ADT head of", k),
        Want::Impl(k) => ("the header of", k),
        Want::Fn(k) => ("the signature of", k),
        Want::Members(k) => ("the members' predicates and signatures of", k),
        Want::MemberFn(k, f) => return format!("the signature of member fn (key {:032x}) of {}", f.0, ItemShown::of_lib(names, k)),
    };
    format!("{what} {}", ItemShown::of_lib(names, k))
}

/// What stopped a body's check ([`CheckFail`](super::check::CheckFail)), as a phrase a refusal
/// note groups items by (rule 19: `blaze check` said "check stopped" and named no reason,
/// port27/mini/clap-root 2026-11-17): the variant and its error's class, each item it is about
/// by its PATH (never a key or a range, so one cause of many bodies is one line); an item the
/// names source holds no path for says so. The whole refusal, keys and all, stays in the
/// finding's detail.
pub fn check_fail_class<'n, S: NamesSource<'n>>(names: S, keys: &Keys, e: &super::check::CheckFail) -> String {
    use super::check::CheckFail;
    let path = |k: ItemKey| -> String {
        match ItemShown::of(names, keys, k).path() {
            Some(p) => format!("`{p}`"),
            None => String::from("an item the names source holds no path for"),
        }
    };
    let lib = |k: LibKey| -> String {
        match ItemShown::of_lib(names, k).path() {
            Some(p) => format!("`{p}`"),
            None => String::from("an item the names source holds no path for"),
        }
    };
    match e {
        CheckFail::Gap { gaps, .. } => match gaps.first() {
            Some(first) => format!("a question the world cannot answer yet ({})", debug_class(&format!("{first:?}"), 2)),
            None => String::from("a question the world cannot answer yet"),
        },
        CheckFail::NothingToFill { questions, .. } => match questions.first() {
            Some(q) => format!("the checker needs a record the world noted nothing to make for ({})", debug_class(&format!("{q:?}"), 1)),
            None => String::from("the checker needs a record the world noted nothing to make for"),
        },
        CheckFail::Head { error, .. } => format!("a head the store could not make: {}", head_error_shown(names, error)),
        CheckFail::Buckets { error, .. } => match error {
            super::buckets::Unfilled::Impl(k) => format!("the candidate impl {} has no header in the store when its question is answered", lib(*k)),
            super::buckets::Unfilled::Fn(k) => format!("the inherent impl fn {} has no `self` flag in its impl's head", path(*k)),
            super::buckets::Unfilled::Sight(why) => format!("{why}"),
        },
        CheckFail::Tables { error, .. } => format!("the impl tables could not file a question's impls: {error}"),
        CheckFail::Check { error, .. } => format!("the checker stopped: {}", check_error_class(&path, error)),
        CheckFail::Diags { .. } => String::from("the check found errors"),
        CheckFail::Rounds { last, .. } => match last.first() {
            Some(q) => format!("still asking after {} rounds ({})", super::check::MOST_ROUNDS, debug_class(&format!("{q:?}"), 1)),
            None => format!("still asking after {} rounds", super::check::MOST_ROUNDS),
        },
        CheckFail::Const { shown, error, .. } => format!("the constant `{shown}` an array length names has no value ({})", debug_class(&format!("{error:?}"), 2)),
        CheckFail::Resolve { item, error, .. } => {
            let why = match error {
                blaze_typeck::ResolveError::Missing(q) => format!("the world has no answer to {}", debug_class(&format!("{q:?}"), 1)),
                blaze_typeck::ResolveError::NoImpl { trait_, .. } => format!("no impl of {} holds", path(*trait_)),
                blaze_typeck::ResolveError::Overlap { trait_, .. } => format!("two impls of {} hold", path(*trait_)),
                blaze_typeck::ResolveError::NotGround { .. } => String::from("an argument is not ground"),
                blaze_typeck::ResolveError::Check(c) => check_error_class(&path, c),
            };
            format!("instance resolution of the call of {} failed: {why}", path(*item))
        }
        CheckFail::ReadsDiffer { .. } => String::from("checked again, the body read other answers than its first check (rule 21)"),
    }
}

/// [`blaze_typeck::CheckError`]'s class: its variant and the inner variant, items by `path`.
fn check_error_class(path: &dyn Fn(ItemKey) -> String, e: &blaze_typeck::CheckError) -> String {
    use blaze_typeck::CheckError as E;
    match e {
        E::Limit(l) => format!("a bound of the checker passed ({l:?})"),
        E::Malformed(m) => format!("malformed input ({})", debug_class(&format!("{m:?}"), 1)),
        E::Solve(s) => format!("the trait solver ({})", debug_class(&format!("{s:?}"), 2)),
        E::Unported(what) => format!("{what} is not checked yet"),
        E::DynUndecided { trait_, .. } => format!("the dyn compatibility of {} is not decided", path(*trait_)),
        E::UnknownInCleanBody { .. } => String::from("a body with no diagnostic recorded an unknown type (the checker's own invariant)"),
        E::DiagWithoutPlace { code, .. } => format!("diagnostic {} has an empty range (the checker's own invariant)", code.code()),
        E::ImplTraitArity { trait_, .. } => format!("an impl header of {} has the wrong number of trait arguments", path(*trait_)),
        E::OpaqueHiddenUnknown { trait_, .. } => format!("a goal of {} on an opaque type with no hidden type", path(*trait_)),
        E::TraitDefaultsNotKnown { trait_ } => format!("the defaults of {} were not read", path(*trait_)),
    }
}

/// The leading variant names of a `Debug` text, `depth` levels into tuple variants:
/// `Malformed(Expr(ExprId(3)))` at depth 2 is `Malformed Expr`; a struct variant stops at its
/// name. No key, number or range survives, so one class of many bodies is one phrase.
fn debug_class(d: &str, depth: usize) -> String {
    let mut out = String::new();
    let mut rest = d;
    for _ in 0..depth {
        let end = rest.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).unwrap_or(rest.len());
        let name = &rest[..end];
        if name.is_empty() || name.starts_with(|c: char| c.is_ascii_digit()) {
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(name);
        match rest[end..].strip_prefix('(') {
            Some(inner) => rest = inner,
            None => break,
        }
    }
    out
}

/// [`HeadError`]'s text with each key shown by its path ([`ItemShown`]): what its `Display`
/// says, which holds no names and so prints keys. An [`HeadError::Item`] reads
/// `item a::B (key ..): <why>`, so a census that cuts at the first `"): "` keeps one class per
/// reason.
pub fn head_error_shown<'n, S: NamesSource<'n>>(names: S, e: &HeadError) -> String {
    let item = |k: LibKey| ItemShown::of_lib(names, k);
    match e {
        HeadError::NoText(k) => format!("the names library holds no text for item {}", item(*k)),
        HeadError::Cfg(k) => format!("item {} asks for cfg atoms its crate's set does not answer", item(*k)),
        HeadError::NotAHead(k, what) => format!("item {} is {what}, not a struct, enum, union, trait, impl or fn", item(*k)),
        HeadError::Cycle(ks) => {
            let mut out = String::from("these items need each other's heads in a cycle: [");
            for (i, k) in ks.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(&item(*k).to_string());
            }
            out.push(']');
            out
        }
        HeadError::NeedMade { need, by } => format!(
            "{} needs {}, which is made, yet asks for it again (its lookup reads what that want does not make)",
            want_shown(names, *by),
            want_shown(names, *need)
        ),
        HeadError::Key(c) => format!("{c}"),
        HeadError::Item(k, why) => format!("item {}: {why:?}", item(*k)),
        HeadError::Seed { need, by } => format!(
            "{} needs {}, whose frozen head could not be filed (the seed holds why)",
            want_shown(names, *by),
            want_shown(names, *need)
        ),
        HeadError::Filed { want, at, what } => format!("the frozen head of {}, head {at:?}: {what}", want_shown(names, *want)),
    }
}

#[cfg(test)]
mod tests {
    use super::{debug_class, impl_shown};

    /// A check refusal's class keeps variant names only: no key, number or struct field, so
    /// two bodies stopped by one class group under one note.
    #[test]
    fn debug_class_keeps_variants_only() {
        assert_eq!(debug_class("Malformed(Expr(ExprId(3)))", 2), "Malformed Expr");
        assert_eq!(debug_class("NoRecord(FnSig(ItemKey(12)))", 1), "NoRecord");
        assert_eq!(debug_class("Assoc { trait_: ItemKey(1) }", 2), "Assoc");
        assert_eq!(debug_class("Solve(Unported(\"x\"))", 3), "Solve Unported");
    }

    /// core's RawVec impl as its header span holds it (a comment line before `const impl`), an
    /// impl with a doc comment, an attribute whose string holds `]`, nested block comments and
    /// a where clause over three lines, and an expansion's spaced `# [doc = "//"]`: each is
    /// shown from its first modifier or `impl`, on one line, an attribute inside the generics
    /// kept. Prints nothing; before the fix the first was shown as `<// FIXME(fee1-dead): ..`.
    #[test]
    fn an_impl_is_shown_from_its_first_token_on_one_line() {
        let raw_vec = "// FIXME(fee1-dead): temporary measure before rustfmt is bumped\nconst impl<T, A: [const] Allocator + [const] Destruct> RawVec<T, A>";
        assert_eq!(impl_shown(raw_vec), "<const impl<T, A: [const] Allocator + [const] Destruct> RawVec<T, A>>");
        let boxed = "/// docs\n#[rustc_const_unstable(feature = \"a]b\")] /* x /* y */ */ unsafe impl<#[may_dangle] T> Drop for Box<T>\nwhere\n    T: Sized, // why\n";
        assert_eq!(impl_shown(boxed), "<unsafe impl<#[may_dangle] T> Drop for Box<T> where T: Sized,>");
        assert_eq!(impl_shown("# [doc = \"//\"] impl Default for Stat"), "<impl Default for Stat>");
        assert_eq!(impl_shown("impl Default for stat"), "<impl Default for stat>");
    }
}
