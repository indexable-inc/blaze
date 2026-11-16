//! The program's entry, D59's INTERIM (docs/v2/plan/decisions.json): blaze appends the module
//! `__blaze_entry` to the root file's text BEFORE names resolve, so the appended text is part of
//! the text the names library is given (crates.rs `Texts::read`, which `resolve_crates` reads
//! and keys the root by: the root's key covers the appended module). The owner's file on disk is
//! never edited.
//!
//! Which of four constant texts ([`super::ENTRY_NONE`], [`super::ENTRY_CX`],
//! [`super::ENTRY_IO`], [`super::ENTRY_CX_IO`]) is chosen from the root file's OUTLINE and the
//! lowering of `fn main`'s signature alone: no parameter; one whose written type is a
//! reference (`&Cx`); one that is not (`Io`); two (`&Cx`, `Io`). Anything else is refused by
//! the rule `entry-takes-roots`. Nothing is appended when the root file already holds a free fn
//! exported as `bstd_hosted_start` or `main` (`#[no_mangle]`, `#[unsafe(no_mangle)]`,
//! `#[export_name = ".."]`, read from the lowered attribute rows as program.rs `symbols_of`
//! reads them), and nothing on a Rust edition (its `std` has no `guest`). The end state
//! (`entry::Main<Roots>` in bstd, one text) replaces this module whole.

use std::alloc::Global;
use std::boxed::Box;
use std::vec::Vec;

use blaze_file_items::outline;
use blaze_lower::machine::sig;
use blaze_lower::{CfgAnswer, form as lf, head};
use blaze_own::Alloc;
use blaze_surface::{Edition, ItemDetail, Range};

use crate::front::HOSTED_START;

/// The rule a `main` of another shape breaks.
pub const RULE: &str = "entry-takes-roots";

/// The roots `main` takes, by the interim's reading of its outline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Roots {
    None,
    Cx,
    Io,
    CxIo,
}

impl Roots {
    /// The module appended for these roots.
    pub fn text(self) -> &'static str {
        match self {
            Roots::None => super::ENTRY_NONE,
            Roots::Cx => super::ENTRY_CX,
            Roots::Io => super::ENTRY_IO,
            Roots::CxIo => super::ENTRY_CX_IO,
        }
    }
}

/// What was appended: the roots, where the user's text ends (every position at or past it is
/// in the appended module) and `main`'s parameter list in the root file, where a finding
/// inside the appended module is reported.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Appended {
    pub roots: Roots,
    pub user_len: u32,
    pub params: Range,
}

/// Why no entry module could be chosen: each names `main` in the root file by its range.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EntryError {
    /// `main` takes more than two parameters.
    TooManyRoots { params: usize, range: Range },
    /// `main` is not a plain fn: generic, `const`, `async`, `unsafe` or `extern`, or a method.
    NotPlain { why: &'static str, range: Range },
    /// Its signature's lowering asked a `cfg` the program's crate does not answer, or its
    /// lowered rows were not readable.
    Signature { why: Box<str>, range: Range },
}

impl EntryError {
    pub fn range(&self) -> Range {
        match self {
            EntryError::TooManyRoots { range, .. } | EntryError::NotPlain { range, .. } | EntryError::Signature { range, .. } => *range,
        }
    }
}

impl core::fmt::Display for EntryError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            EntryError::TooManyRoots { params, .. } => write!(f, "`main` takes {params} parameters; it takes at most two roots, `&Cx` then `Io` ({RULE})"),
            EntryError::NotPlain { why, .. } => write!(f, "`main` is {why}; the entry is a plain fn taking none, `&Cx`, `Io`, or `&Cx` then `Io` ({RULE})"),
            EntryError::Signature { why, .. } => write!(f, "`main`'s signature could not be read for its roots: {why} ({RULE})"),
        }
    }
}

/// The program crate's cfg as the lowering asks it.
struct Atoms<'c>(&'c [(&'c str, Option<&'c str>)]);

impl blaze_lower::Cfg for Atoms<'_> {
    fn atom(&mut self, name: &str, value: Option<&str>) -> CfgAnswer {
        if self.0.iter().any(|&(n, v)| n == name && v == value) { CfgAnswer::Yes } else { CfgAnswer::No }
    }
}

/// The entry module for the root file `text` (the program crate's cfg `cfg`), appended to it;
/// `None` (and `text` unchanged) when the file exports its own entry or has no `fn main` at
/// its top (the front's "no entry" refusal then speaks). `blaze` is whether the package is on
/// a blaze edition.
pub fn append(text: &mut std::string::String, syntax: Edition, blaze: bool, cfg: &[(&str, Option<&str>)]) -> Result<Option<Appended>, EntryError> {
    if !blaze {
        return Ok(None);
    }
    let Some(roots_at) = choose(text, syntax, cfg)? else { return Ok(None) };
    let (roots, params) = roots_at;
    let user_len = text.len() as u32;
    text.push_str(roots.text());
    Ok(Some(Appended { roots, user_len, params }))
}

/// The roots of the root file's `fn main` and its parameter list's range, or `None` when the
/// file exports an entry or has no top-level `fn main`.
fn choose(text: &str, syntax: Edition, cfg: &[(&str, Option<&str>)]) -> Result<Option<(Roots, Range)>, EntryError> {
    let o = outline(text, syntax, Alloc::global());
    let mut main = None;
    for item in o.items() {
        if !matches!(item.detail, ItemDetail::Fn { .. }) {
            continue;
        }
        let parent = item.parent.map(|p| o.item(p));
        // A free fn: at the file's top or in an inline module.
        if parent.is_some_and(|p| !matches!(p.detail, ItemDetail::Mod { .. })) {
            continue;
        }
        let whole = item.range;
        let it = &text[whole.start as usize..whole.end as usize];
        let at = item.name.map(|r| item.in_file(r)).unwrap_or(whole);
        let lowered = blaze_lower::lower_signature(it, syntax, &mut Atoms(cfg), Alloc::global())
            .map_err(|_| EntryError::Signature { why: "its signature needs a `cfg` the crate does not answer".into(), range: at })?;
        let v = lowered.view();
        let bad = |e: &dyn core::fmt::Debug| EntryError::Signature { why: std::format!("{e:?}").into_boxed_str(), range: at };
        for &n in v.rows(v.list(head::ATTRS)).map_err(|e| bad(&e))? {
            let a = v.attr(n).map_err(|e| bad(&e))?;
            let exported = match v.text_at(a.path).map_err(|e| bad(&e))?.trim() {
                "no_mangle" => item.name.map(|r| item.text(text, r)),
                "export_name" => Some(crate::front::unquote(v.text_at(a.args).map_err(|e| bad(&e))?)),
                _ => None,
            };
            if matches!(exported, Some(s) if s == HOSTED_START || s == "main") {
                return Ok(None);
            }
        }
        if parent.is_none() && item.name.map(|r| item.text(text, r)) == Some("main") {
            main = Some((whole.start, at, lowered));
        }
    }
    let Some((item_start, at, lowered)) = main else { return Ok(None) };
    let v = lowered.view();
    let bad = |e: &dyn core::fmt::Debug| EntryError::Signature { why: std::format!("{e:?}").into_boxed_str(), range: at };
    let flags = v.head[head::FLAGS];
    for (bit, why) in [(sig::CONST, "`const`"), (sig::ASYNC, "`async`"), (sig::UNSAFE, "`unsafe`"), (sig::EXTERN, "`extern`")] {
        if flags & bit != 0 {
            return Err(EntryError::NotPlain { why, range: at });
        }
    }
    if !v.rows(v.list(head::GENERICS)).map_err(|e| bad(&e))?.is_empty() {
        return Err(EntryError::NotPlain { why: "generic", range: at });
    }
    let rows = v.rows(v.list(head::PARAMS)).map_err(|e| bad(&e))?;
    let mut refs: Vec<bool, Global> = Vec::new_in(Global);
    let mut span: Option<Range> = None;
    for &n in rows {
        let p = lf::Param::decode(n).map_err(|e| bad(&e))?;
        if p.self_kind != lf::SelfKind::No {
            return Err(EntryError::NotPlain { why: "a method", range: at });
        }
        let Some(t) = p.ty else { return Err(EntryError::NotPlain { why: "a fn with an untyped parameter", range: at }) };
        refs.push(matches!(v.ty(t).map_err(|e| bad(&e))?, lf::TypeRef::Ref { .. }));
        if let Some(r) = v.item_relative(v.type_range(t).map_err(|e| bad(&e))?) {
            span = Some(match span {
                Some(s) => Range::new(s.start, r.end),
                None => r,
            });
        }
    }
    // The parameters' types' range, in the file.
    let params = span.map_or(at, |s| Range::new(item_start + s.start, item_start + s.end));
    let roots = match refs.as_slice() {
        [] => Roots::None,
        [true] => Roots::Cx,
        [false] => Roots::Io,
        [_, _] => Roots::CxIo,
        more => return Err(EntryError::TooManyRoots { params: more.len(), range: params }),
    };
    Ok(Some((roots, params)))
}
