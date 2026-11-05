//! A lowering for `use`-paths (more generally, paths without angle-bracketed segments).

use std::{
    cmp::Ordering,
    fmt::{self, Display as _},
    iter,
};

use crate::{
    hygiene::Transparency,
    name::{AsName, Name},
    tt,
};
use base_db::{Crate, Db};
use intern::{InternedSlice, sym};
use smallvec::SmallVec;
use span::{Edition, SyntaxContext};
use syntax::view::{self, AstNode as _};

intern::impl_slice_internable!(PathContexts, (), SyntaxContext);
intern::impl_slice_internable!(PathSegments, Option<InternedSlice<PathContexts>>, Name);

/// A path's kind and its segments. 16 bytes, `Copy`: the segments are interned once per
/// distinct list (one hash of the names' pointers and a probe, no allocation when the list
/// exists), so equality and hashing are two words and a clone is a copy. `None` is no segments.
///
/// The interned list's header holds each segment's syntax context normalized to macros 2.0
/// ([`ModPath::contexts`]), the input def-site hygiene reads for the segments after the first
/// (rustc's `Ident` of each segment): `None` when every one is a root context, as in every
/// path a real file or a `macro_rules!` writes. Two paths of the same names written with
/// different contexts are two paths.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ModPath {
    pub kind: PathKind,
    segments: Option<InternedSlice<PathSegments>>,
}

// The kind (8 bytes) and a pointer: 16 bytes on a 64-bit target.
const _: () = assert!(size_of::<ModPath>() == 8 + size_of::<usize>());

/// Segments are gathered here before they are interned: paths longer than this spill to the heap.
type SegmentBuf = SmallVec<[Name; 8]>;

/// The segments' contexts, gathered as [`SegmentBuf`].
type ContextBuf = SmallVec<[SyntaxContext; 8]>;

fn intern_segments(segments: &[Name]) -> Option<InternedSlice<PathSegments>> {
    intern_segments_in(segments, &[])
}

/// `segments` with `contexts`, one per segment or none at all: a list of root contexts is
/// stored as none.
fn intern_segments_in(
    segments: &[Name],
    contexts: &[SyntaxContext],
) -> Option<InternedSlice<PathSegments>> {
    if segments.is_empty() {
        return None;
    }
    let contexts = match contexts.iter().all(|it| it.is_root()) {
        true => None,
        false => {
            assert_eq!(
                contexts.len(),
                segments.len(),
                "a path of {} segments given {} contexts",
                segments.len(),
                contexts.len()
            );
            // alloc: none: `to_owned` makes the interned reference an owned handle
            Some(InternedSlice::from_header_and_slice((), contexts).to_owned())
        }
    };
    // alloc: none: `to_owned` makes the interned reference an owned handle, a pointer copy
    Some(InternedSlice::from_header_and_slice(contexts, segments).to_owned())
}

impl PartialOrd for ModPath {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// By kind, then by the segments' text, as when the segments were stored inline, then by the
/// segments' contexts (paths that differ only there were written by different expansions).
impl Ord for ModPath {
    fn cmp(&self, other: &Self) -> Ordering {
        self.kind.cmp(&other.kind).then_with(|| {
            if self.segments == other.segments {
                Ordering::Equal
            } else {
                self.segments().cmp(other.segments()).then_with(|| {
                    match (self.contexts(), other.contexts()) {
                        (Some(a), Some(b)) => {
                            a.iter().map(|it| it.into_u32()).cmp(b.iter().map(|it| it.into_u32()))
                        }
                        (a, b) => a.is_some().cmp(&b.is_some()),
                    }
                })
            }
        })
    }
}

impl fmt::Debug for ModPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut out = f.debug_struct("ModPath");
        out.field("kind", &self.kind).field("segments", &self.segments());
        // Only a path some of whose segments a macro 2.0 wrote has contexts: every other prints
        // as it did before paths had them.
        if let Some(contexts) = self.contexts() {
            out.field("contexts", &contexts);
        }
        out.finish()
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PathKind {
    Plain,
    /// `self::` is `Super(0)`
    Super(u8),
    Crate,
    /// Absolute path (::foo)
    Abs,
    // FIXME: Can we remove this somehow?
    /// `$crate` from macro expansion
    DollarCrate(Crate),
}

impl PathKind {
    pub const SELF: PathKind = PathKind::Super(0);

    /// The segments the kind stands for in [`ModPath::len`]: one a `super`, one for `crate` or
    /// `$crate`, none for `self`, `::` or a plain path.
    pub fn segment_count(self) -> usize {
        match self {
            PathKind::Plain => 0,
            PathKind::Super(i) => i as usize,
            PathKind::Crate => 1,
            PathKind::Abs => 0,
            PathKind::DollarCrate(_) => 1,
        }
    }
}

impl ModPath {
    pub fn from_src_view(
        db: &Db,
        path: view::Path<'_>,
        span_for_range: &mut (impl FnMut(::tt::TextRange) -> SyntaxContext + ?Sized),
    ) -> Option<ModPath> {
        convert_path_view(db, path, span_for_range, false)
    }

    /// [`ModPath::from_src_view`] with each segment's context ([`ModPath::contexts`]), read
    /// with `span_for_range`: for a path of a macro expansion that is resolved (every segment
    /// of a real file has a root context, so its reader need not ask).
    pub fn from_src_view_with_contexts(
        db: &Db,
        path: view::Path<'_>,
        span_for_range: &mut (impl FnMut(::tt::TextRange) -> SyntaxContext + ?Sized),
    ) -> Option<ModPath> {
        convert_path_view(db, path, span_for_range, true)
    }

    pub fn from_tt(db: &Db, tt: tt::TokenTreesView<'_>) -> Option<ModPath> {
        convert_path_tt(db, tt)
    }

    pub fn from_segments(kind: PathKind, segments: impl IntoIterator<Item = Name>) -> ModPath {
        // alloc: none for up to 8 segments, gathered on the stack
        let segments: SegmentBuf = segments.into_iter().collect();
        ModPath { kind, segments: intern_segments(&segments) }
    }

    /// [`ModPath::from_segments`] with each segment's syntax context, normalized to macros 2.0
    /// ([`ModPath::contexts`]): one per segment.
    pub fn from_segments_in(
        kind: PathKind,
        segments: impl IntoIterator<Item = (Name, SyntaxContext)>,
    ) -> ModPath {
        // alloc: none for up to 8 segments, gathered on the stack
        let (names, contexts): (SegmentBuf, ContextBuf) = segments.into_iter().unzip();
        ModPath { kind, segments: intern_segments_in(&names, &contexts) }
    }

    /// Creates a `ModPath` from a `PathKind`, with no extra path segments.
    pub const fn from_kind(kind: PathKind) -> ModPath {
        ModPath { kind, segments: None }
    }

    #[inline]
    pub fn segments(&self) -> &[Name] {
        match &self.segments {
            Some(segments) => &segments.slice,
            None => &[],
        }
    }

    /// The syntax context each segment is written with, normalized to macros 2.0: what
    /// def-site hygiene keys the lookup of a segment after the first by. `None` when every one
    /// is a root context.
    #[inline]
    pub fn contexts(&self) -> Option<&[SyntaxContext]> {
        self.segments.as_ref()?.header.header.as_ref().map(|it| &it.slice[..])
    }

    /// The context of the `i`th segment, as [`ModPath::contexts`]: a root context when it has
    /// none.
    pub fn context_of(&self, i: usize, root: SyntaxContext) -> SyntaxContext {
        self.contexts().and_then(|it| it.get(i).copied()).unwrap_or(root)
    }

    pub fn push_segment(&mut self, segment: Name) {
        self.push_segment_in(segment, SyntaxContext::root(Edition::Edition2015));
    }

    /// [`ModPath::push_segment`] of a segment written with `ctxt` (normalized to macros 2.0).
    pub fn push_segment_in(&mut self, segment: Name, ctxt: SyntaxContext) {
        let mut segments = SegmentBuf::from_slice(self.segments());
        segments.push(segment);
        let contexts = self.contexts_or_roots(ctxt);
        self.segments = intern_segments_in(&segments, &contexts);
    }

    /// The contexts of the segments, then `last`, or none when all are root contexts.
    fn contexts_or_roots(&self, last: SyntaxContext) -> ContextBuf {
        match (self.contexts(), last.is_root()) {
            (None, true) => ContextBuf::new(),
            (Some(it), _) => it.iter().copied().chain(iter::once(last)).collect(),
            (None, false) => {
                iter::repeat_n(SyntaxContext::root(Edition::Edition2015), self.segments().len())
                    .chain(iter::once(last))
                    .collect()
            }
        }
    }

    /// `other`'s segments after this path's, with their contexts.
    pub fn append(&mut self, other: &ModPath) {
        let mut segments = SegmentBuf::from_slice(self.segments());
        segments.extend_from_slice(other.segments());
        let root = SyntaxContext::root(Edition::Edition2015);
        let contexts: ContextBuf = match (self.contexts(), other.contexts()) {
            (None, None) => ContextBuf::new(),
            _ => (0..self.segments().len())
                .map(|i| self.context_of(i, root))
                .chain((0..other.segments().len()).map(|i| other.context_of(i, root)))
                .collect(),
        };
        self.segments = intern_segments_in(&segments, &contexts);
    }

    pub fn pop_segment(&mut self) -> Option<Name> {
        let (&last, rest) = self.segments().split_last()?;
        let contexts = self.contexts().map_or(&[][..], |it| &it[..rest.len()]);
        self.segments = intern_segments_in(rest, contexts);
        Some(last)
    }

    /// Returns the number of segments in the path (counting special segments like `$crate` and
    /// `super`).
    pub fn len(&self) -> usize {
        self.segments().len() + self.kind.segment_count()
    }

    pub fn textual_len(&self) -> usize {
        let base = match self.kind {
            PathKind::Plain => 0,
            PathKind::SELF => "self".len(),
            PathKind::Super(i) => "super".len() * i as usize,
            PathKind::Crate => "crate".len(),
            PathKind::Abs => 0,
            PathKind::DollarCrate(_) => "$crate".len(),
        };
        self.segments().iter().map(|segment| segment.as_str().len()).fold(base, core::ops::Add::add)
    }

    pub fn is_ident(&self) -> bool {
        self.as_ident().is_some()
    }

    pub fn is_self(&self) -> bool {
        self.kind == PathKind::SELF && self.segments.is_none()
    }

    #[allow(non_snake_case)]
    pub fn is_Self(&self) -> bool {
        self.kind == PathKind::Plain && matches!(self.segments(), [name] if *name == sym::Self_)
    }

    /// If this path is a single identifier, like `foo`, return its name.
    pub fn as_ident(&self) -> Option<&Name> {
        if self.kind != PathKind::Plain {
            return None;
        }

        match self.segments() {
            [name] => Some(name),
            _ => None,
        }
    }
    pub fn display_verbatim<'a>(&'a self, db: &'a Db) -> impl fmt::Display + 'a {
        Display { db, path: self, edition: None }
    }

    pub fn display<'a>(&'a self, db: &'a Db, edition: Edition) -> impl fmt::Display + 'a {
        Display { db, path: self, edition: Some(edition) }
    }
}

impl Extend<Name> for ModPath {
    fn extend<T: IntoIterator<Item = Name>>(&mut self, iter: T) {
        let mut segments = SegmentBuf::from_slice(self.segments());
        segments.extend(iter);
        let root = SyntaxContext::root(Edition::Edition2015);
        let contexts: ContextBuf = match self.contexts() {
            Some(it) => it.iter().copied().chain(iter::repeat(root)).take(segments.len()).collect(),
            None => ContextBuf::new(),
        };
        self.segments = intern_segments_in(&segments, &contexts);
    }
}

struct Display<'a> {
    db: &'a Db,
    path: &'a ModPath,
    edition: Option<Edition>,
}

impl fmt::Display for Display<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        display_fmt_path(self.db, self.path, f, self.edition)
    }
}

impl From<Name> for ModPath {
    fn from(name: Name) -> ModPath {
        ModPath::from_segments(PathKind::Plain, iter::once(name))
    }
}

fn display_fmt_path(
    db: &Db,
    path: &ModPath,
    f: &mut fmt::Formatter<'_>,
    edition: Option<Edition>,
) -> fmt::Result {
    let mut first_segment = true;
    let mut add_segment = |s| -> fmt::Result {
        if !first_segment {
            f.write_str("::")?;
        }
        first_segment = false;
        f.write_str(s)?;
        Ok(())
    };
    match path.kind {
        PathKind::Plain => {}
        PathKind::SELF => add_segment("self")?,
        PathKind::Super(n) => {
            for _ in 0..n {
                add_segment("super")?;
            }
        }
        PathKind::Crate => add_segment("crate")?,
        PathKind::Abs => add_segment("")?,
        PathKind::DollarCrate(_) => add_segment("$crate")?,
    }
    for segment in path.segments() {
        if !first_segment {
            f.write_str("::")?;
        }
        first_segment = false;
        match edition {
            Some(edition) => segment.display(db, edition).fmt(f)?,
            None => fmt::Display::fmt(segment.as_str(), f)?,
        };
    }
    Ok(())
}

fn convert_path_view(
    db: &Db,
    path: view::Path<'_>,
    span_for_range: &mut (impl FnMut(::tt::TextRange) -> SyntaxContext + ?Sized),
    with_contexts: bool,
) -> Option<ModPath> {
    let root = SyntaxContext::root(Edition::Edition2015);
    let mut segments = path.segments();

    let segment = segments.next()?;
    let mut names = SegmentBuf::new_const();
    // Each name's context normalized to macros 2.0, pushed with it (`ModPath::contexts`).
    let mut contexts = ContextBuf::new_const();
    let handle_super_kw = &mut |init_deg,
                                names: &mut SegmentBuf,
                                contexts: &mut ContextBuf,
                                span_for_range: &mut (
                                         dyn FnMut(::tt::TextRange) -> SyntaxContext + '_
                                     )| {
        let mut deg = init_deg;
        for segment in segments.by_ref() {
            match segment.kind()? {
                view::PathSegmentKind::SuperKw => deg += 1,
                view::PathSegmentKind::Name(name) => {
                    names.push(name.as_name());
                    contexts.push(match with_contexts {
                        true => {
                            span_for_range(name.syntax().text_range()).normalize_to_macros_2_0(db)
                        }
                        false => root,
                    });
                    break;
                }
                view::PathSegmentKind::Type { .. }
                | view::PathSegmentKind::SelfTypeKw
                | view::PathSegmentKind::SelfKw
                | view::PathSegmentKind::CrateKw => return None,
            }
        }
        Some(PathKind::Super(deg))
    };

    let mut kind = match segment.kind()? {
        view::PathSegmentKind::Name(name_ref) => {
            if name_ref.text() == "$crate" {
                resolve_crate_root(db, span_for_range(name_ref.syntax().text_range()))
                    .map(PathKind::DollarCrate)
                    .unwrap_or(PathKind::Crate)
            } else {
                names.push(name_ref.as_name());
                contexts.push(match with_contexts {
                    true => {
                        span_for_range(name_ref.syntax().text_range()).normalize_to_macros_2_0(db)
                    }
                    false => root,
                });
                segment.coloncolon_token().map_or(PathKind::Plain, |_| PathKind::Abs)
            }
        }
        view::PathSegmentKind::SelfTypeKw => {
            names.push(Name::new_symbol_root(sym::Self_));
            contexts.push(SyntaxContext::root(Edition::Edition2015));
            PathKind::Plain
        }
        view::PathSegmentKind::CrateKw => PathKind::Crate,
        view::PathSegmentKind::SelfKw => {
            handle_super_kw(0, &mut names, &mut contexts, &mut |range| span_for_range(range))?
        }
        view::PathSegmentKind::SuperKw => {
            handle_super_kw(1, &mut names, &mut contexts, &mut |range| span_for_range(range))?
        }
        view::PathSegmentKind::Type { .. } => {
            // not allowed in imports
            return None;
        }
    };

    for segment in segments {
        let (name, ctxt) = match segment.kind()? {
            view::PathSegmentKind::Name(name) => {
                let ctxt = match with_contexts {
                    true => span_for_range(name.syntax().text_range()).normalize_to_macros_2_0(db),
                    false => root,
                };
                (name.as_name(), ctxt)
            }
            view::PathSegmentKind::SelfKw => continue,
            view::PathSegmentKind::SelfTypeKw => {
                (Name::new_symbol_root(sym::Self_), SyntaxContext::root(Edition::Edition2015))
            }
            _ => return None,
        };
        names.push(name);
        contexts.push(ctxt);
    }

    // handle local_inner_macros :
    // Basically, even in rustc it is quite hacky:
    // https://github.com/rust-lang/rust/blob/614f273e9388ddd7804d5cbc80b8865068a3744e/src/librustc_resolve/macros.rs#L456
    // We follow what it did anyway :)
    if names.len() == 1
        && kind == PathKind::Plain
        && let Some(_macro_call) = path.syntax().parent().and_then(view::MacroCall::cast)
    {
        let syn_ctx = span_for_range(segment.syntax().text_range());
        if let Some(macro_call_id) = syn_ctx.outer_expn(db)
            && crate::MacroCallId::from(macro_call_id).loc(db).def.local_inner
        {
            kind = match resolve_crate_root(db, syn_ctx) {
                Some(crate_root) => PathKind::DollarCrate(crate_root),
                None => PathKind::Crate,
            }
        }
    }

    Some(ModPath { kind, segments: intern_segments_in(&names, &contexts) })
}

fn convert_path_tt(db: &Db, tt: tt::TokenTreesView<'_>) -> Option<ModPath> {
    let mut leaves = tt.iter().filter_map(|tt| match tt {
        tt::TtElement::Leaf(leaf) => Some(leaf),
        tt::TtElement::Subtree(..) => None,
    });
    let mut segments = SegmentBuf::new_const();
    let mut contexts = ContextBuf::new_const();
    let kind = match leaves.next()? {
        tt::Leaf::Punct(tt::Punct { char: ':', .. }) => match leaves.next()? {
            tt::Leaf::Punct(tt::Punct { char: ':', .. }) => PathKind::Abs,
            _ => return None,
        },
        tt::Leaf::Ident(tt::Ident { sym: text, span, .. }) if text == sym::dollar_crate => {
            resolve_crate_root(db, span.ctx).map(PathKind::DollarCrate).unwrap_or(PathKind::Crate)
        }
        tt::Leaf::Ident(tt::Ident { sym: text, .. }) if text == sym::self_ => PathKind::SELF,
        tt::Leaf::Ident(tt::Ident { sym: text, .. }) if text == sym::super_ => {
            let mut deg = 1;
            while let Some(tt::Leaf::Ident(tt::Ident { sym: text, span, is_raw: _ })) =
                leaves.next()
            {
                if text != sym::super_ {
                    segments.push(Name::new_symbol(text, span.ctx));
                    contexts.push(span.ctx.normalize_to_macros_2_0(db));
                    break;
                }
                deg += 1;
            }
            PathKind::Super(deg)
        }
        tt::Leaf::Ident(tt::Ident { sym: text, .. }) if text == sym::crate_ => PathKind::Crate,
        tt::Leaf::Ident(ident) => {
            segments.push(Name::new_symbol(ident.sym, ident.span.ctx));
            contexts.push(ident.span.ctx.normalize_to_macros_2_0(db));
            PathKind::Plain
        }
        _ => return None,
    };
    for leaf in leaves {
        if let ::tt::Leaf::Ident(ident) = leaf {
            segments.push(Name::new_symbol(ident.sym, ident.span.ctx));
            contexts.push(ident.span.ctx.normalize_to_macros_2_0(db));
        }
    }
    Some(ModPath { kind, segments: intern_segments_in(&segments, &contexts) })
}

pub fn resolve_crate_root(db: &Db, mut ctxt: SyntaxContext) -> Option<Crate> {
    // When resolving `$crate` from a `macro_rules!` invoked in a `macro`,
    // we don't want to pretend that the `macro_rules!` definition is in the `macro`
    // as described in `SyntaxContextId::apply_mark`, so we ignore prepended opaque marks.
    // FIXME: This is only a guess and it doesn't work correctly for `macro_rules!`
    // definitions actually produced by `macro` and `macro` definitions produced by
    // `macro_rules!`, but at least such configurations are not stable yet.
    ctxt = ctxt.normalize_to_macro_rules(db);
    let mut iter = ctxt.marks_rev(db).peekable();
    let mut result_mark = None;
    // Find the last opaque mark from the end if it exists.
    while let Some(&(mark, Transparency::Opaque)) = iter.peek() {
        result_mark = Some(mark);
        iter.next();
    }
    // Then find the last semi-opaque mark from the end if it exists.
    while let Some((mark, Transparency::SemiOpaque)) = iter.next() {
        result_mark = Some(mark);
    }

    result_mark.map(|call| crate::MacroCallId::from(call).loc(db).def.krate)
}

pub use crate::name as __name;

#[macro_export]
macro_rules! __known_path {
    (core::iter::IntoIterator) => {};
    (core::iter::Iterator) => {};
    (core::result::Result) => {};
    (core::option::Option) => {};
    (core::ops::Range) => {};
    (core::ops::RangeFrom) => {};
    (core::ops::RangeFull) => {};
    (core::ops::RangeTo) => {};
    (core::ops::RangeToInclusive) => {};
    (core::ops::RangeInclusive) => {};
    (core::range::Range) => {};
    (core::range::RangeFrom) => {};
    (core::range::RangeInclusive) => {};
    (core::range::RangeToInclusive) => {};
    (core::async_iter::AsyncIterator) => {};
    (core::future::Future) => {};
    (core::future::IntoFuture) => {};
    (core::fmt::Debug) => {};
    (std::fmt::format) => {};
    (core::ops::Try) => {};
    (core::convert::From) => {};
    (core::convert::TryFrom) => {};
    (core::str::FromStr) => {};
    ($path:path) => {
        compile_error!("Please register your known path in the path module")
    };
}

#[macro_export]
macro_rules! __path {
    ($start:ident $(:: $seg:ident)*) => ({
        $crate::__known_path!($start $(:: $seg)*);
        $crate::mod_path::ModPath::from_segments($crate::mod_path::PathKind::Abs, [
            $crate::name::Name::new_symbol_root($crate::intern::sym::$start), $($crate::name::Name::new_symbol_root($crate::intern::sym::$seg),)*
        ])
    });
}

pub use crate::__path as path;

#[macro_export]
macro_rules! __tool_path {
    ($start:ident $(:: $seg:ident)*) => ({
        $crate::mod_path::ModPath::from_segments($crate::mod_path::PathKind::Plain, [
            $crate::name::Name::new_symbol_root($crate::intern::sym::rust_analyzer), $crate::name::Name::new_symbol_root($crate::intern::sym::$start), $($crate::name::Name::new_symbol_root($crate::intern::sym::$seg),)*
        ])
    });
}

#[cfg(test)]
mod tests {
    use std::hash::BuildHasher;

    use blaze_ids::rng::SplitMix64;

    use super::*;

    fn names(texts: &[&str]) -> Vec<Name> {
        texts.iter().map(|it| Name::new_root(it)).collect()
    }

    /// The old layout's answers: a kind and its own list of names.
    #[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
    struct Model {
        kind: PathKind,
        segments: Vec<Name>,
    }

    fn model(path: &ModPath) -> Model {
        Model { kind: path.kind, segments: path.segments().to_vec() }
    }

    /// Seeded lists of names drawn from a few words, so many repeat and many share prefixes.
    fn corpus() -> Vec<(PathKind, Vec<&'static str>)> {
        const WORDS: &[&str] = &["std", "core", "fmt", "Debug", "a", "b", "Self", "Zeta"];
        const KINDS: &[PathKind] = &[
            PathKind::Plain,
            PathKind::Abs,
            PathKind::Crate,
            PathKind::Super(0),
            PathKind::Super(2),
        ];
        let mut rng = SplitMix64::new(0x2545_f491_4f6c_dd1d);
        (0..400)
            .map(|_| {
                let kind = KINDS[rng.index(KINDS.len())];
                let len = rng.index(12);
                let words = (0..len).map(|_| WORDS[rng.index(WORDS.len())]).collect();
                (kind, words)
            })
            .collect()
    }

    #[test]
    fn same_answers_as_a_list() {
        let corpus = corpus();
        let paths: Vec<ModPath> = corpus
            .iter()
            .map(|(kind, words)| ModPath::from_segments(*kind, names(words)))
            .collect();
        for (path, (kind, words)) in paths.iter().zip(&corpus) {
            assert_eq!(path.kind, *kind);
            assert_eq!(path.segments(), &names(words)[..]);
            let special = match kind {
                PathKind::Super(n) => *n as usize,
                PathKind::Crate | PathKind::DollarCrate(_) => 1,
                PathKind::Plain | PathKind::Abs => 0,
            };
            assert_eq!(path.len(), words.len() + special);
            assert_eq!(path.is_ident(), *kind == PathKind::Plain && words.len() == 1);
            assert_eq!(path.is_self(), *kind == PathKind::SELF && words.is_empty());
        }
        for a in &paths {
            for b in &paths {
                assert_eq!(a == b, model(a) == model(b));
                assert_eq!(a.cmp(b), model(a).cmp(&model(b)), "{a:?} {b:?}");
                if a == b {
                    let hash = |it: &ModPath| blaze_hash::FixedState.hash_one(it);
                    assert_eq!(hash(a), hash(b));
                }
            }
        }
    }

    #[test]
    fn edits_give_the_built_path() {
        for (kind, words) in corpus() {
            let mut pushed = ModPath::from_kind(kind);
            for name in names(&words) {
                pushed.push_segment(name);
            }
            let built = ModPath::from_segments(kind, names(&words));
            assert_eq!(pushed, built);

            let mut extended = ModPath::from_kind(kind);
            extended.extend(names(&words));
            assert_eq!(extended, built);

            let mut popped = built;
            let mut expect = names(&words);
            while let Some(last) = popped.pop_segment() {
                assert_eq!(Some(last), expect.pop());
                assert_eq!(popped, ModPath::from_segments(kind, expect.iter().copied()));
            }
            assert!(expect.is_empty());
            assert_eq!(popped, ModPath::from_kind(kind));
        }
    }

    #[test]
    fn debug_is_as_before() {
        let path = ModPath::from_segments(PathKind::Abs, names(&["core", "fmt"]));
        let segments: smallvec::SmallVec<[Name; 1]> = path.segments().iter().copied().collect();
        assert_eq!(format!("{path:?}"), format!("ModPath {{ kind: Abs, segments: {segments:?} }}"));
        assert_eq!(
            format!("{:?}", ModPath::from_kind(PathKind::Crate)),
            "ModPath { kind: Crate, segments: [] }"
        );
    }

    #[test]
    fn a_kinds_segment_count_is_the_old_len_match() {
        let old = |kind: PathKind| match kind {
            PathKind::Plain => 0,
            PathKind::Super(i) => i as usize,
            PathKind::Crate => 1,
            PathKind::Abs => 0,
            PathKind::DollarCrate(_) => 1,
        };
        let kinds = [PathKind::Plain, PathKind::Crate, PathKind::Abs]
            .into_iter()
            .chain((0..=u8::MAX).map(PathKind::Super));
        for kind in kinds {
            assert_eq!(kind.segment_count(), old(kind), "{kind:?}");
        }
    }

    #[test]
    fn known_paths() {
        let path = path![core::fmt::Debug];
        assert_eq!(path.kind, PathKind::Abs);
        assert_eq!(path.segments(), &names(&["core", "fmt", "Debug"])[..]);
        assert_eq!(path, path![core::fmt::Debug]);
    }
}

pub use crate::__tool_path as tool_path;
