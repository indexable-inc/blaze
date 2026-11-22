//! A token keeps its mark, in an item's head and its types (piece token-mark-F1; drive.md, "A
//! token keeps its mark"; the one resolver is blaze-lower `mark_site::site_of`, as
//! `BodyWorld::path` asks it for a body's paths).
//!
//! An item a macro call writes is a slice of that call's expansion text, and the expansion's
//! origin column (`blaze_scope::resolve::ExpansionText::origins`) holds the mark of each token
//! by where it starts. A path of the item's head starts at a token: its mark says where the
//! path resolves. The Reference, "Macros By Example", Hygiene: names a `macro_rules!` writes
//! are looked up "as if they appeared at the invocation site" (mixed-site: `$crate` at the
//! definition), and a `macro` item is def-site: its names resolve in the module that defines
//! it. So `pub macro mk() { pub fn get() -> Hidden { .. } }` names the macro's module's private
//! `Hidden` in `get`'s head, and the same head written by a `macro_rules!` names `Hidden` at
//! the call.
//!
//! A `$crate` is a mark, never a spelled word (token-mark-2): a head path whose first token is
//! `$crate` resolves in the crate its mark names, and a `$crate` no expansion wrote is refused
//! (`SiteRefused::UnmarkedDollarCrate`).

use std::format;

use blaze_id::ItemKey as LibKey;
use blaze_lower::View;
use blaze_lower::mark_site::{PathSite, SiteRefused, TextMark, site_of};
use blaze_scope::crate_roots::{RootsLost, TextMarks, column_in};
use blaze_scope::expansion_origins::{OriginsError, offset_in};
use blaze_scope::resolve::At;
use blaze_surface::Range;
use blaze_typeck::PathIdx;

use super::source::NamesSource;
use super::types::Why;
use crate::resolve::{Ns, Paths, Target};

/// Why an item's head has no marks to resolve its paths by (rule: front2 item_site.rs; drive.md,
/// "A token keeps its mark").
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MarksLost {
    /// The item has a text and no place: where its tokens were written is unknown.
    NoPlace { key: LibKey },
    /// The item's place is in the expansion `call`, and no text of that call is kept.
    NoExpansion { call: LibKey },
    /// The expansion's origin column is there and does not read.
    Roots { call: LibKey, error: RootsLost },
    /// The item's text is not a slice of the expansion text its place names: its tokens' marks
    /// would be another text's.
    NotInText { key: LibKey, call: LibKey },
    /// No member of the trait or impl `owner` is the member a head was made from (by its name
    /// and kind, and by its text where several share them): which text wrote it is unknown.
    NotAMember { owner: LibKey },
    /// The records keep the opened marks of `most` expansions already (front2 source.rs
    /// `MOST_MARKED_TEXTS`, rule 18), and `call` would be one more.
    TooManyTexts { call: LibKey, most: usize },
}

impl core::fmt::Display for MarksLost {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let rule = "(rule: front2 item_site.rs; drive.md, \"A token keeps its mark\")";
        match self {
            MarksLost::NoPlace { key } => write!(f, "item {key:?} has a text and no place, so its head's paths have no marks {rule}"),
            MarksLost::NoExpansion { call } => write!(f, "the macro call {call:?} that writes an item keeps no expansion text {rule}"),
            MarksLost::Roots { call, error } => write!(f, "the expansion of macro call {call:?}: {error} {rule}"),
            MarksLost::NotInText { key, call } => write!(f, "item {key:?}'s text is not a slice of the expansion of macro call {call:?} {rule}"),
            MarksLost::NotAMember { owner } => write!(f, "no member of {owner:?} is the one its head was made from, so which text wrote it is unknown {rule}"),
            MarksLost::TooManyTexts { call, most } => write!(f, "the expansion of macro call {call:?} is one past the {most} whose marks one command keeps (front2 source.rs `MOST_MARKED_TEXTS`) {rule}"),
        }
    }
}

impl MarksLost {
    /// As a head refusal: the item cannot say where its paths resolve.
    pub fn why(self) -> Why {
        Why::Program(format!("{self}").into_boxed_str(), Range::new(0, 0))
    }
}

/// The marks of the expansion `call` of crate `krate` wrote, read from `made` (one home for
/// both sources: [`NamesSource::text_marks`]). `Ok(None)`: the text keeps no origin column by
/// design (blaze-scope `OriginsError::Rewritten`: a call's statements read as items, a block's
/// module text; a kept item's text keeps an identity column with no marks, `kept`, so it reads
/// as before); `Unplaced`: a call the tables did not place). Such a
/// text's heads resolve at their module, as before this piece, which is right for every token
/// the text copies from the call as written; a token of such a text that a def-site macro wrote
/// is the open case (token-mark-2 carries the column into rewritten texts).
///
/// This OPENS the column (every mark and row read once: blaze-expand origin_column.rs
/// `OriginsRef::open`), so it is called once per expansion asked: the records' source keeps
/// what it answered (front2 source.rs `Records::text_marks`), the solved names keep the column
/// opened when the text was made (blaze-scope resolve.rs `ExpansionText::column`), and both
/// hand out the opened view ([`marks_of_column`]) afterwards.
pub fn marks_of<'t>(call: LibKey, text: &'t str, origins: &Result<&'t [u8], OriginsError>) -> Result<Option<TextMarks<'t>>, MarksLost> {
    marks_of_column(call, text, column_in(origins))
}

/// [`marks_of`] over a column already opened (`column_in`'s answer, kept): O(1).
pub fn marks_of_column<'t>(call: LibKey, text: &'t str, column: Result<blaze_expand::origin_column::OriginsRef<'t>, RootsLost>) -> Result<Option<TextMarks<'t>>, MarksLost> {
    match column {
        Ok(column) => Ok(Some(TextMarks { text, column })),
        Err(RootsLost::NoOrigins { error: OriginsError::Rewritten { .. } | OriginsError::Unplaced { .. } }) => Ok(None),
        Err(error) => Err(MarksLost::Roots { call, error }),
    }
}

/// The marks the tokens of `key`'s text `text` keep: `None` for an item of a file (every token
/// as written there, at its module) or of an expansion that keeps no column ([`marks_of`]).
pub fn item_marks<'n, S: NamesSource<'n>>(names: S, key: LibKey, text: &str) -> Result<Option<TextMarks<'n>>, MarksLost> {
    let place = names.place_of(key).ok_or(MarksLost::NoPlace { key })?;
    let call = match place.at {
        At::File(_) => return Ok(None),
        At::Expansion(call) => call,
    };
    let Some(marks) = names.text_marks(place.krate, call)? else { return Ok(None) };
    if offset_in(marks.text, text).is_none() {
        return Err(MarksLost::NotInText { key, call });
    }
    Ok(Some(marks))
}

/// The marks of the tokens of a member's text `text` of container `owner`: those of the
/// expansion `written_by` (a macro call in the container's body) wrote it, or else the
/// container's own ([`item_marks`]).
pub fn member_marks<'n, S: NamesSource<'n>>(names: S, owner: LibKey, written_by: Option<LibKey>, text: &str) -> Result<Option<TextMarks<'n>>, MarksLost> {
    let Some(call) = written_by else { return item_marks(names, owner, text) };
    let krate = names.crate_of(owner).ok_or(MarksLost::NoPlace { key: owner })?;
    let Some(marks) = names.text_marks(krate, call)? else { return Ok(None) };
    if offset_in(marks.text, text).is_none() {
        return Err(MarksLost::NotInText { key: owner, call });
    }
    Ok(Some(marks))
}

/// The marks of the member `name` (of `kind`) of `owner` whose head was made from `text`: the
/// SAME member the head read ([`member_marks`] with that member's own `written_by`), found
/// again by what front2's member key is made of (its container, kind and name: keys.rs
/// `member_key`), never by comparing keys. front2's member key is not the names library's
/// member key, so a body asked by the checker's key and matched against the library's never
/// found its member, and read its text with the CONTAINER's marks: a member a nested call
/// wrote (core's `min_max_impl!($t)` inside `ord_impl!`'s `const impl Ord for $t { .. }`,
/// num-traits' `impl_to_primitive_uint_to_uint!` inside `impl ToPrimitive for $T`) is a slice
/// of the inner call's expansion, not the outer's (`MarksLost::NotInText`). Several members of
/// one name and kind (each under its own `cfg`): the one whose text the head holds.
pub fn head_marks<'n, S: NamesSource<'n>>(names: S, owner: LibKey, name: &str, kind: blaze_surface::ItemKind, text: &str) -> Result<Option<TextMarks<'n>>, MarksLost> {
    // By content: over records a member's text may be served again into the frame, at another
    // address than the head's copy.
    let same = |t: &str| t == text;
    // The first member of the name and kind's `written_by`; whether there are several; the
    // one whose text the head holds.
    let mut first: Option<Option<LibKey>> = None;
    let mut several = false;
    let mut exact: Option<Option<LibKey>> = None;
    names.each_member(owner, &mut |m| {
        if m.kind != kind || m.name != Some(name) {
            return;
        }
        names.read_member_body(m.key, m.written_by);
        match first {
            Some(_) => several = true,
            None => first = Some(m.written_by),
        }
        if exact.is_none() && same(m.text) {
            exact = Some(m.written_by);
        }
    });
    let written_by = match (first, several, exact) {
        (Some(by), false, _) | (_, true, Some(by)) => by,
        _ => return Err(MarksLost::NotAMember { owner }),
    };
    member_marks(names, owner, written_by, text)
}

/// The path `p` of `v` (segments `texts`, from `::` when `abs`) written in `module`, looked up in
/// `ns`, where its first token's mark says ([`site_of`]): `v.mark_of_path` for a path of an
/// expansion lowered in place; else the mark `marks` holds for the token the path starts at;
/// with no marks, at `module`. A `$crate` first segment names the crate of its mark.
#[expect(clippy::too_many_arguments, reason = "one resolve of one path: the path, its view and its marks")]
pub fn resolve_written<'n, S: NamesSource<'n>>(
    resolver: &Paths<S>,
    marks: Option<TextMarks<'_>>,
    module: LibKey,
    v: &View<'_>,
    p: PathIdx,
    abs: bool,
    texts: &[&str],
    ns: Ns,
    r: Range,
) -> Result<Target, Why> {
    let first = texts.first().copied().unwrap_or("");
    let site = written_site(resolver, marks, module, v, p, abs, first, r)?;
    resolver.path_with(site.module, site.dollar_crate, abs, texts, ns).map_err(|e| Why::Resolve(e, r))
}

/// Where the path `p` of `v` (first segment `first`, from `::` when `abs`), written in
/// `module`, is read: the module and `$crate`'s crate its first token's mark says
/// ([`site_in`]), the mark found as [`resolve_written`] finds it. The one reading of a written
/// path's site for [`resolve_written`] and a written type read through a block's `use`
/// (super::block_type_uses).
#[expect(clippy::too_many_arguments, reason = "one site of one path: the path, its view and its marks")]
pub fn written_site<'n, S: NamesSource<'n>>(
    resolver: &Paths<S>,
    marks: Option<TextMarks<'_>>,
    module: LibKey,
    v: &View<'_>,
    p: PathIdx,
    abs: bool,
    first: &str,
    r: Range,
) -> Result<PathSite, Why> {
    let range = v.path_range(p)?;
    let mark = match (v.mark_of_path(p)?, marks) {
        (Some(m), _) => Some(m),
        (None, Some(marks)) if !range.in_expansion() => {
            let start = v.text_at(range)?;
            marks.mark_of(start).map_err(|e| Why::Program(format!("{e}").into_boxed_str(), r))?
        }
        (None, _) => None,
    };
    marked_site(resolver.names, mark, first, abs, module, r)
}

/// Resolves a first token read from a path row or copied out of a constant argument.
pub(super) fn marked_site<'n, S: NamesSource<'n>>(names: S, mark: Option<TextMark>, first: &str, abs: bool, module: LibKey, r: Range) -> Result<PathSite, Why> {
    site_in(names, mark, first, abs, module).map_err(|refused| match refused {
        SiteRefused::UnmarkedDollarCrate => Why::Unsupported("`$crate` on a token no expansion wrote (drive.md, \"A token keeps its mark\")", r),
        SiteRefused::OpaqueWithoutModule { .. } => Why::Unsupported("a path under a def-site mark of a crate this build does not hold (drive.md, \"A token keeps its mark\")", r),
    })
}

/// [`site_of`], with a def-site mark whose definition names no module read at its crate's
/// ROOT, as `DefSite` says (blaze-surface macro_def.rs: "`None` for `macro_rules!`, whose
/// def-site names are its crate's"; blaze-scope crate_roots.rs `root_mark` makes such a mark
/// for a `crate` written for another crate's `$crate`). A builtin derive writes its own tokens
/// opaque under its crate with no module (blaze-expand derive.rs `MADE`): `#[derive(PartialEq)]`'s
/// `fn eq(&self, other: &Self) -> bool` names `bool` so, and its head was refused (tempo census
/// r16, lane TL row (c)). rustc writes these at the derive's `def_site`; the names a derive
/// writes relatively are primitives (`bool`) and its own `let` bindings, which read the same at
/// the crate root as at the module of `macro PartialEq` (core::cmp). `OpaqueWithoutModule`
/// stays the refusal for a crate the build does not hold.
pub fn site_in<'n, S: NamesSource<'n>>(names: S, mark: Option<TextMark>, first: &str, abs: bool, module: LibKey) -> Result<PathSite, SiteRefused> {
    match site_of(mark, first, abs, module) {
        Err(SiteRefused::OpaqueWithoutModule { krate }) => {
            let root = (0..names.crate_count()).find(|&i| names.crate_key(i) == Some(krate)).and_then(|i| names.root_module(i));
            root.map(|module| PathSite { module, dollar_crate: None }).ok_or(SiteRefused::OpaqueWithoutModule { krate })
        }
        other => other,
    }
}
