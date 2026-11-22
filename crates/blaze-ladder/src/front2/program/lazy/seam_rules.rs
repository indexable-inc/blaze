//! The seam's per-fn rules that both programs answer by (rule 30: one home each):
//! [`crate::front2::program::Front2Program`], whose rows are made by a census, and
//! [`super::LazyProgram`], whose rows are made one body at a time. Each rule takes what a
//! program holds (its keys, its head store, its names source, the body being built) and a
//! question about a row as a closure, so the two cannot answer differently. Kept under `lazy`
//! (visible to all of `program`) so the lazy program's files hold it whole.
//!
//! - [`fn_abi`]: a tabled fn's ABI is its row's; any other fn's is its head's (head_abi.rs).
//! - [`track_caller`]: the fn's own `#[track_caller]`, else, for a method of a trait impl, its
//!   trait's declaration's (the Reference's `track_caller` attribute: on a trait's method
//!   declaration it applies to every implementation of the method).
//! - [`caller_site`]: an expression's place as a code record keeps it (rule 71): the item and
//!   the expression's first byte from the item's first, never a line.
//! - [`source_position`]: the line and column of such a place, made only when the program is
//!   placed, from the item's first byte TODAY in today's text of its file
//!   (`NamesSource::item_start`, `NamesSource::file_text`), so an edit above the item moves
//!   the answer and no record.
//!
//! An item is found by `Keys::library_in` (never `Keys::library`): over records the keys map
//! files no item (keys.rs), the names source finds it; over solved names both answer alike.

use super::*;

use crate::front2::keys::Keys;
use crate::front2::program::head_abi::BuildAbi;
use crate::front2::source::NamesSource;
use crate::seam::PositionGap;
use blaze_build::SiteGap;

mod local_position;
mod position_source;
pub use position_source::PositionSource;

pub(in crate::front2::program) struct LlvmRow {
    pub(in crate::front2::program) kind: FnKind,
    pub(in crate::front2::program) build: BuildAbi,
    pub(in crate::front2::program) track_caller: bool,
    pub(in crate::front2::program) symbol: &'static str,
    pub(in crate::front2::program) intrinsic: Intrinsic,
}

/// The block ABI selects an LLVM declaration. Its link name selects the instruction.
/// The builder declares the instruction with the C convention and emits it in place.
pub(in crate::front2::program) fn llvm_row(foreign: Option<&str>, link_name: Option<&str>, path: &str) -> Result<Option<LlvmRow>, Front2Refusal> {
    use crate::front2::llvm_intrinsic;

    if foreign != Some(llvm_intrinsic::ABI) {
        return Ok(None);
    }
    let symbol = llvm_intrinsic::symbol(link_name).map_err(|why| Front2Refusal::Attrs { function: path.into(), why: why.form().into() })?;
    let target = blaze_build::intrinsics::target::named(symbol)
        .map_err(|_| Front2Refusal::TargetIntrinsic { function: path.into(), symbol: symbol.into() })?;
    Ok(Some(LlvmRow {
        kind: FnKind::Free,
        build: BuildAbi { abi: Abi::C, nounwind: true },
        track_caller: false,
        symbol: "",
        intrinsic: Intrinsic::Target(target),
    }))
}

/// A panic entry, allocator shim or std-internal declaration supplies the call's ABI.
/// Its row has no outside symbol to link.
pub(in crate::front2::program) fn bound_decl_row<'n, S: NamesSource<'n>>(
    names: S, store: &Store<'n>, key: ItemKey, panic_entry: bool, alloc_shim: Option<crate::front2::alloc_shim::Shim>, path: &str,
) -> Result<Option<(FnKind, &'static str)>, Front2Refusal> {
    let internal = crate::front2::internal_symbol::declares(names, store, key, store.tys.alloc())
        .map_err(|why| Front2Refusal::Attrs { function: path.into(), why: why.detail() })?;
    Ok((panic_entry || alloc_shim.is_some() || internal.is_some()).then_some((FnKind::Free, "")))
}

/// Bind a declaration in the program that will run the call. A failed lookup has the
/// reach walk's refusal, including the declaration's displayed key and the full detail.
pub(in crate::front2::program) fn internal_binding<'n, S: NamesSource<'n>>(
    names: S, keys: &Keys, store: &Store<'n>, item: ItemKey, on_lookup: impl FnOnce() -> Result<(), Front2Refusal>,
) -> Result<Option<ItemKey>, Front2Refusal> {
    use crate::front2::internal_symbol::{self, InternalRefused};
    let alloc = store.tys.alloc();
    let unchecked = |class: &'static str, first| Front2Refusal::Unchecked {
        function: crate::front2::item_shown::ItemShown::of(names, keys, item).boxed(),
        class: class.into(),
        first,
    };
    let refused = |why: InternalRefused| unchecked(why.class(), why.detail());
    let Some(decl) = internal_symbol::declares(names, store, item, alloc).map_err(refused)? else { return Ok(None) };
    on_lookup()?;
    let panic = crate::panic_strategy::of_program(names).map_err(|why| unchecked(why.class(), why.to_string().into_boxed_str()))?;
    internal_symbol::bound(names, keys, decl, panic, alloc).map(Some).map_err(refused)
}

/// A trait declaration supplies the call signature until dispatch selects an implementation.
/// For other Rust rows the builder needs a body or an intrinsic. The lazy row can count a
/// body declared in the head before checking it. Foreign symbols and shims have their own rows.
pub(in crate::front2::program) fn rust_row_kind(of_trait: bool, body_or_intrinsic: bool, path: &str) -> Result<FnKind, Front2Refusal> {
    if of_trait {
        Ok(FnKind::Trait(0))
    } else if body_or_intrinsic {
        Ok(FnKind::Free)
    } else {
        Err(Front2Refusal::NoBody { function: path.into() })
    }
}

/// THE ABI RULE: `tabled` is the row's ABI when `key` has a row; else the head's (head_abi.rs
/// `head_abi`). `None`: no head, or an ABI the builder spreads no arguments of (the builder's
/// `NoAbi` names the fn).
pub(in crate::front2::program) fn fn_abi<'n, S: NamesSource<'n>>(tabled: Option<Abi>, names: S, store: &Store<'n>, key: ItemKey) -> Option<Abi> {
    if let Some(abi) = tabled {
        return Some(abi);
    }
    crate::front2::program::head_abi::head_abi(names, store, key, || Box::from("")).ok().map(|read| read.abi)
}

/// The Reference, “The `track_caller` attribute”: “When applied to a function in a trait
/// declaration, the attribute applies to all implementations.” This includes overrides of
/// provided methods. An attribute on the impl method itself suffices independently.
pub(in crate::front2::program) fn track_caller(keys: &Keys, store: &Store<'_>, key: ItemKey, own: impl Fn(ItemKey) -> Option<bool>) -> Option<bool> {
    if own(key)? {
        return Some(true);
    }
    let Some(crate::front2::keys::Origin::Member { container, kind: MemberKind::Fn, name }) = keys.origin(key) else {
        return Some(false);
    };
    let Some(imp) = store.impls.get(container) else {
        // A trait method's own attribute is decisive. A missing impl head would leave
        // inheritance unknown, so require positive evidence that the owner is a trait.
        if store.trait_defs.contains_key(container) {
            return Some(false);
        }
        let lib = match keys.origin(*container) {
            Some(crate::front2::keys::Origin::Item(lib)) => *lib,
            _ => store.fns.get(&key)?.lib,
        };
        return store.maps.traits.contains_key(&lib).then_some(false);
    };
    let Some(trait_) = imp.trait_ else {
        return Some(false);
    };
    let declared = keys.filed_member(trait_, MemberKind::Fn, name)?;
    own(declared)
}

/// A function row already holds its own attribute. Without that row, use the function's
/// signature text. In particular an impl head files its members' identities before making
/// their signatures, and a trait declaration can supply the attribute without a body.
pub(in crate::front2::program) fn track_caller_from<'n, S: NamesSource<'n>>(
    names: S, keys: &Keys, store: &Store<'n>, key: ItemKey, tabled: impl Fn(ItemKey) -> Option<bool>,
) -> Option<bool> {
    track_caller(keys, store, key, |k| {
        if let Some(tracked) = tabled(k) {
            return Some(tracked);
        }
        if store.ctors.contains_key(&k) {
            return Some(false);
        }
        let (lib, text) = if let Some(head) = store.fns.get(&k) {
            (head.lib, head.text)
        } else if let Some(crate::front2::keys::Origin::Member { container, kind: MemberKind::Fn, name }) = keys.origin(k) {
            let lib = keys.library_in(names, *container)?;
            let mut text = None;
            let listed = names.each_member(lib, &mut |member| {
                if member.kind == blaze_surface::ItemKind::Function && member.name == Some(name.as_ref()) {
                        names.read_member_signature(member.key, member.written_by);
                    text = Some(member.text);
                }
            });
            if !listed {
                return None;
            }
            (lib, text?)
        } else if let Some(local) = store.locals.head(k) {
            if local.kind != crate::front2::locals::LocalKind::Fn {
                return None;
            }
            (local.lib, local.text)
        } else {
            let lib = keys.library_in(names, k)?;
            if names.kind_of_item(lib)? != blaze_surface::ItemKind::Function {
                return None;
            }
            (lib, names.text_of(lib)?.text)
        };
        symbols_of(names, lib, crate::front2::links::values_key(names, store, lib), text, "").ok().map(|symbols| symbols.track_caller)
    })
}

/// Keep the stable identity of the declaration that supplies the expression's
/// text and its item-relative byte. The lowering maps an in-body expansion to
/// its outermost invocation. For a whole generated item placement follows the
/// item-producing invocation, so the byte within that invocation is zero.
/// Each step that finds no fact is its own [`SiteGap`], named for `owner` (rule 19).
pub(in crate::front2::program) fn caller_site<'n, S: PositionSource<'n>>(names: S, keys: &Keys, store: &Store<'n>, owner: ItemKey, body: &Body<'_>, expr: ExprId) -> Result<blaze_build::CallerSite<'n>, SiteGap> {
    let offset = expr_offset(body, expr).ok_or(SiteGap::NoRange { owner, expr })?;
    let point = body_anchor(names, keys, store, owner, offset)?;
    let (krate, file, first) = point.anchor.placement(names, store.tys.alloc()).map_err(|gap| SiteGap::Placement { item: owner, gap })?;
    let clash = |_| SiteGap::KeyClash { item: owner };
    let (item, offset) = match (first, point.local) {
        (Start::Invocation(_) | Start::MemberInvocation(..), _) => (keys.item(point.anchor.lib()).map_err(clash)?, 0),
        (_, Some((live, relative))) => {
            let declaration = point.offset.checked_sub(relative).ok_or(SiteGap::BeforeDeclaration { item: owner, offset: point.offset, first: relative })?;
            // The site counts from the declaration's first token (local_position.rs
            // `first_token`: past a doc comment), as `source_position` and the cold reading
            // read it.
            let (source, first) = local_position::key_at(names, krate, point.anchor, declaration, store.tys.alloc()).map_err(|gap| SiteGap::Placement { item: owner, gap })?;
            let local = keys.file_source_local(source, live).map_err(|_| SiteGap::LocalKey { item: owner, declaration })?;
            (local, point.offset.checked_sub(first).ok_or(SiteGap::BeforeDeclaration { item: owner, offset: point.offset, first })?)
        }
        (_, None) => (keys.item(point.anchor.lib()).map_err(clash)?, point.offset),
    };
    let file = names.file_shown(krate, file).ok_or(SiteGap::NoShownFile { item: owner, krate, file })?;
    Ok(blaze_build::CallerSite { file, item, offset })
}

/// THE DEFINITION-SITE RULE (blaze-build `Adts::def_site`): where fn `item` is defined, in
/// [`caller_site`]'s form. rustc's `def_span` of a fn is its `sig.span` (rustc_middle
/// hir/map.rs `hir_span`), which rustc_parse starts at `lo`, the first token after the outer
/// attributes (item.rs `parse_item_common_`: `let lo = this.token.span;` before the
/// visibility; function.rs `parse_fn`: `sig_lo.to(sig_hi)`): so the site is the first token of
/// the declaration that is not an outer attribute or a doc comment (a doc comment is its
/// `#[doc]`), its visibility included. Counted as [`caller_site`] counts: from the item's
/// text, or for a written local from its declaration's first token; a fn a macro call writes
/// whole is placed at the call (byte 0), as [`caller_site`] places its expressions.
pub(in crate::front2::program) fn def_site<'n, S: PositionSource<'n>>(names: S, keys: &Keys, store: &Store<'n>, item: ItemKey) -> Result<blaze_build::CallerSite<'n>, SiteGap> {
    let point = body_anchor(names, keys, store, item, 0)?;
    let (krate, file, first) = point.anchor.placement(names, store.tys.alloc()).map_err(|gap| SiteGap::Placement { item, gap })?;
    let file = names.file_shown(krate, file).ok_or(SiteGap::NoShownFile { item, krate, file })?;
    let clash = |_| SiteGap::KeyClash { item };
    if let Start::Invocation(_) | Start::MemberInvocation(..) = first {
        return Ok(blaze_build::CallerSite { file, item: keys.item(point.anchor.lib()).map_err(clash)?, offset: 0 });
    }
    let text = point.anchor.text(names).ok_or(SiteGap::NoText { item })?;
    let edition = names.edition_of(krate).ok_or(SiteGap::NoEdition { item, krate })?;
    let alloc = store.tys.alloc();
    let past = SiteGap::PastAttributes { item };
    match point.local {
        None => {
            let at = past_outer_attributes(text, point.offset, edition, alloc).ok_or(past)?;
            Ok(blaze_build::CallerSite { file, item: keys.item(point.anchor.lib()).map_err(clash)?, offset: at })
        }
        Some((live, relative)) => {
            let declaration = point.offset.checked_sub(relative).ok_or(SiteGap::BeforeDeclaration { item, offset: point.offset, first: relative })?;
            let (source, first) = local_position::key_at(names, krate, point.anchor, declaration, alloc).map_err(|gap| SiteGap::Placement { item, gap })?;
            let at = past_outer_attributes(text, first, edition, alloc).ok_or(past)?;
            let local = keys.file_source_local(source, live).map_err(|_| SiteGap::LocalKey { item, declaration })?;
            Ok(blaze_build::CallerSite { file, item: local, offset: at.checked_sub(first).ok_or(SiteGap::BeforeDeclaration { item, offset: at, first })? })
        }
    }
}

/// The first byte of the first token at or after byte `from` of `text` that is not in an
/// outer attribute (`#[..]`) or a doc comment (lexed as its `#[doc = ".."]`, blaze-expand
/// `lex`). `None`: the text does not lex from `from`, or no token follows the attributes.
fn past_outer_attributes(text: &str, from: u32, edition: blaze_surface::Edition, alloc: Alloc<'_>) -> Option<u32> {
    use blaze_expand::origin_column::{InputShape, input_places};
    use blaze_expand::{Delim, TokenKind};
    let rest = text.get(from as usize..)?;
    let tokens = blaze_expand::lex(rest, edition, alloc).ok()?;
    let places = input_places(rest, InputShape::Whole, edition, alloc).ok()?;
    let list = tokens.tokens();
    if places.len() != list.len() {
        return None;
    }
    let mut at = 0usize;
    // Each turn moves past one attribute's closing `]`: the tokens bound the turns.
    while matches!(list.get(at).map(|t| t.kind), Some(TokenKind::Punct { char: '#', .. }))
        && matches!(list.get(at + 1).map(|t| t.kind), Some(TokenKind::Open(Delim::Bracket)))
    {
        at = tokens.close_of(at + 1)? + 1;
    }
    from.checked_add(places.get(at)?.0)
}

/// THE TRAP-SITE RULE: [`caller_site`]'s offset alone, `expr`'s first byte from its item's
/// (`View::site`), with no file: what a trap row keeps (blaze-build `Adts::expr_site`). It
/// needs no shown path, so a toolchain crate's body (core's `AlignmentEnum` copy) keeps its
/// trap's site; the report places it by the item. `None`: the expression has no range.
pub(in crate::front2::program) fn expr_offset(body: &Body<'_>, expr: ExprId) -> Option<u32> {
    let view = body.lowered.view();
    Some(view.site(view.expr_range(expr).ok()?).ok()?.start)
}

use blaze_scope::resolve::At as TextAt;
use crate::front2::source::MemberOf;

/// The names row relative to which a byte is counted. Ordinary associated items
/// have a member row but no item row. Its outline index is its own, even when
/// adjacent members have identical bodies.
#[derive(Clone, Copy)]
enum Anchor<'n> {
    Item(LibKey),
    Member { container: LibKey, row: MemberOf<'n> },
}

impl<'n> Anchor<'n> {
    fn lib(self) -> LibKey {
        match self {
            Self::Item(lib) => lib,
            Self::Member { row, .. } => row.key,
        }
    }

    fn from_lib<S: NamesSource<'n>>(names: S, lib: LibKey) -> Option<Self> {
        if names.has_text(lib) {
            return Some(Self::Item(lib));
        }
        let (container, _) = names.member_owner(lib)?;
        let row = member_row(names, container, lib)?;
        Some(Self::Member { container, row })
    }

    /// A whole item written by a macro uses the invocation. A function-like invocation
    /// itself has an item row (or an unnamed member row). Derives and attributes
    /// describe their invoking outline entry in the expansion origins.
    fn placement<S: PositionSource<'n>>(self, names: S, alloc: Alloc<'_>) -> Result<(usize, u32, Start<'n>), PositionGap> {
        /// The first byte, in the calling text, of the call site rustc places a `Location` of
        /// an attribute or derive call's expansion at. rustc_const_eval
        /// `location_triple_for_span`: "`span.ctxt().outer_expn().expansion_cause()`", the
        /// outermost expansion's `call_site`, and its `lo()`; rustc_expand `Invocation::span`:
        /// an attribute macro's call site is `attr.span` (the whole attribute, from its `#`), a
        /// derive's is `path.span` (its path in the `derive(..)` list). `origin.entry` is the
        /// calling item's whole outline entry, doc comments and outer attributes first: the
        /// attribute or the path is found among them by its tokens against the macro's path as
        /// written (`CallView::macro_name`: `::` kept, spaces skipped). A function-like call's
        /// site is its `span` (`InvocationKind::Bang`: the call from its path, the attributes
        /// and doc comments on the call not in it): the first token after them. An implicit
        /// attribute's, which no text writes (D104), is the entry's start. An attribute or
        /// derive not found: refused (`NoItem`), never the entry's start in its place (rule 3).
        fn invoked_at(origin: &crate::front2::expansion_origin::CallOrigin<'_>, alloc: Alloc<'_>) -> Result<u32, PositionGap> {
            use blaze_expand::origin_column::{InputShape, input_places};
            use blaze_expand::{Delim, TokenKind};
            let (start, end) = origin.entry;
            if origin.view.implicit {
                return Ok(start);
            }
            let fn_like = origin.view.shape == InputShape::Inside;
            let entry = origin.text.get(start as usize..end as usize).ok_or(PositionGap::PastText)?;
            let tokens = blaze_expand::lex(entry, origin.edition, alloc).map_err(|_| PositionGap::NoItem)?;
            let places = input_places(entry, InputShape::Whole, origin.edition, alloc).map_err(|_| PositionGap::NoItem)?;
            let list = tokens.tokens();
            if places.len() != list.len() {
                return Err(PositionGap::NoItem);
            }
            let kind = |i: usize| list.get(i).map(|t| t.kind);
            let punct = |i: usize, c: char| matches!(kind(i), Some(TokenKind::Punct { char, .. }) if char == c);
            // The token after the path written from token `i`, when that path is the macro's.
            let path_end = |i: usize| -> Option<usize> {
                let mut rest = origin.view.macro_name;
                let mut j = i;
                loop {
                    let word = match kind(j) {
                        Some(TokenKind::Ident { .. }) => tokens.text_at(j),
                        Some(TokenKind::Punct { char: ':', .. }) => ":",
                        _ => break,
                    };
                    rest = rest.trim_start().strip_prefix(word)?;
                    j += 1;
                }
                (j > i && rest.trim_start().is_empty()).then_some(j)
            };
            let first_byte = |i: usize| -> Result<u32, PositionGap> {
                let at = places.get(i).ok_or(PositionGap::NoItem)?.0;
                start.checked_add(at).ok_or(PositionGap::PastText)
            };
            let mut at = 0usize;
            while punct(at, '#') && matches!(kind(at + 1), Some(TokenKind::Open(Delim::Bracket))) {
                let close = tokens.close_of(at + 1).ok_or(PositionGap::NoItem)?;
                if fn_like {
                    at = close + 1;
                    continue;
                }
                // `#[path]`, `#[path(..)]` or `#[path = ..]`: the attribute macro's own call.
                if let Some(after) = path_end(at + 2)
                    && (after == close || punct(after, '=') || matches!(kind(after), Some(TokenKind::Open(_))))
                {
                    return first_byte(at);
                }
                // A path of a `derive(..)` list anywhere in the attribute: a `cfg_attr` keeps
                // its tokens where they are written.
                let mut j = at + 2;
                while j < close {
                    let derive = matches!(kind(j), Some(TokenKind::Ident { .. }))
                        && matches!(tokens.text_at(j), "derive" | "derive_const")
                        && matches!(kind(j + 1), Some(TokenKind::Open(Delim::Paren)));
                    if !derive {
                        j += 1;
                        continue;
                    }
                    let group = tokens.close_of(j + 1).ok_or(PositionGap::NoItem)?;
                    let mut k = j + 2;
                    while k < group {
                        if let Some(after) = path_end(k)
                            && (after == group || punct(after, ','))
                        {
                            return first_byte(k);
                        }
                        // The list's next path: past this one's `,`, a group skipped whole.
                        while k < group && !punct(k, ',') {
                            k = match kind(k) {
                                Some(TokenKind::Open(_)) => tokens.close_of(k).ok_or(PositionGap::NoItem)? + 1,
                                _ => k + 1,
                            };
                        }
                        k += 1;
                    }
                    j = group + 1;
                }
                at = close + 1;
            }
            if fn_like { first_byte(at) } else { Err(PositionGap::NoItem) }
        }

        let krate = names.crate_of(self.lib()).ok_or(PositionGap::NoItem)?;
        let mut anchor = self;
        let mut expanded = false;
        // A call's site in `anchor`'s own text, when the outermost call met so far is written
        // in a block of `anchor`'s body (`local_position::block_call_site`).
        let mut invoked: Option<u32> = None;
        let mut copied = AVec::new_in(alloc);
        for _ in 0..=crate::front2::expansion_origin::MOST_LEVELS {
            let (at, start) = match anchor {
                Self::Item(lib) => {
                    let (at, first) = names.item_start(lib).ok_or(PositionGap::NoItem)?;
                    (at, Start::Byte(first))
                }
                Self::Member { container, row } => {
                    let at = match row.written_by {
                        Some(call) => TextAt::Expansion(call),
                        None => names.item_start(container).ok_or(PositionGap::NoItem)?.0,
                    };
                    (at, Start::Entry(row.item, row.text))
                }
            };
            let call = match at {
                TextAt::File(file) => {
                    let start = if copied.is_empty() { start } else {
                        let text = names.file_text(krate, file).ok_or(PositionGap::NoFile)?;
                        let mut first = start.byte(names, krate, text, alloc)?;
                        let mut body = anchor.text(names).ok_or(PositionGap::NoItem)?;
                        let edition = names.edition_of(krate).ok_or(PositionGap::NoFile)?;
                        // Work inward in the current file. A change inside an outer
                        // copied declaration can move an inner one independently.
                        while let Some(chain) = copied.pop() {
                            let range = local_position::written_range(body, chain, edition, alloc)?;
                            first = first.checked_add(range.start).ok_or(PositionGap::PastText)?;
                            body = body.get(range.start as usize..range.end as usize).ok_or(PositionGap::PastText)?;
                        }
                        Start::Copied(first, body)
                    };
                    if let Some(at) = invoked {
                        let text = names.file_text(krate, file).ok_or(PositionGap::NoFile)?;
                        let first = start.byte(names, krate, text, alloc)?;
                        return Ok((krate, file, Start::Invocation(first.checked_add(at).ok_or(PositionGap::PastText)?)));
                    }
                    return Ok((krate, file, if expanded { start.invocation() } else { start }));
                }
                TextAt::Expansion(call) => call,
            };
            if let Self::Member { container, .. } = anchor
                && let Some(row) = member_row(names, container, call)
            {
                anchor = Self::Member { container, row };
            } else if names.has_text(call) {
                anchor = Self::Item(call);
            } else if let Some((parent, chain)) = local_position::copied_declaration(names, krate, anchor, call, alloc)? {
                copied.push(chain);
                anchor = parent;
                continue;
            } else {
                let mut call = call;
                let mut in_body = None;
                for _ in 0..crate::front2::expansion_origin::MOST_LEVELS {
                    let origin = names.call_origin(krate, call).map_err(|_| PositionGap::NoFile)?;
                    match origin.view.text {
                        TextAt::File(file) => return Ok((krate, file, Start::Invocation(invoked_at(&origin, alloc)?))),
                        // A call written in a block of a body is in the module text of the
                        // body's blocks, which no call wrote (it has no origins): its site is
                        // placed in the body's own text, and the body is placed as any item.
                        TextAt::Expansion(parent) => match local_position::block_call_site(names, krate, parent, origin.text, || invoked_at(&origin, alloc), alloc)? {
                            Some(site) => {
                                in_body = Some(site);
                                break;
                            }
                            None => call = parent,
                        },
                    }
                }
                let (body, at) = in_body.ok_or(PositionGap::NoFile)?;
                anchor = body;
                invoked = Some(at);
                copied.clear();
                continue;
            }
            copied.clear();
            expanded = true;
            // The calling item is itself written by a call: the outermost call is its site.
            invoked = None;
        }
        Err(PositionGap::NoFile)
    }

    fn text<S: NamesSource<'n>>(self, names: S) -> Option<&'n str> {
        match self {
            Self::Item(lib) => Some(names.text_of(lib)?.text),
            Self::Member { row, .. } => { names.read_member_body(row.key, row.written_by); Some(row.text) },
        }
    }
}

fn member_row<'n, S: NamesSource<'n>>(names: S, container: LibKey, lib: LibKey) -> Option<MemberOf<'n>> {
    let mut row = None;
    names.each_member(container, &mut |m| {
        if m.key == lib {
            row = Some(m);
        }
    });
    row
}

#[derive(Clone, Copy)]
enum Start<'n> {
    Byte(u32),
    Copied(u32, &'n str),
    Entry(blaze_surface::ItemIndex, &'n str),
    Invocation(u32),
    MemberInvocation(blaze_surface::ItemIndex, &'n str),
}

impl<'n> Start<'n> {
    fn invocation(self) -> Self {
        match self {
            Self::Byte(at) | Self::Copied(at, _) => Self::Invocation(at),
            Self::Entry(i, text) => Self::MemberInvocation(i, text),
            other => other,
        }
    }
}

/// Body-local items have a range in the lowering of their enclosing item. An
/// expanded declaration has a copied text, and its entire range stands for one
/// invocation. A written declaration is a slice of its parent, so its inner
/// expression's offset is retained. Closures and async blocks keep the owner's
/// expression IDs and consequently use exactly the same rule.
struct BodySite<'n> {
    anchor: Anchor<'n>,
    offset: u32,
    /// The innermost declaration actually written in the top item's text,
    /// and the expression (or generating macro invocation) relative to it.
    local: Option<(ItemKey, u32)>,
}

/// Each step that finds no fact is its own [`SiteGap`] (rule 19). The enclosing heads are the
/// ones the make asks up front (the seam's `site_heads`, fill.rs `site_heads_asked`), so
/// [`SiteGap::NoEnclosingRow`] is a head that ask could not serve, never one this program
/// happened not to check before.
fn body_anchor<'n, S: NamesSource<'n>>(names: S, keys: &Keys, store: &Store<'n>, mut owner: ItemKey, mut offset: u32) -> Result<BodySite<'n>, SiteGap> {
    let asked = owner;
    let mut leaf = None;
    for _ in 0..=store.locals.kept() {
        if let Some(local) = store.locals.head(owner) {
            let enclosing = store.fns.get(&local.enclosing).ok_or(SiteGap::NoEnclosingRow { local: owner, enclosing: local.enclosing })?.text;
            let range = local.range;
            let outside = SiteGap::LocalOutsideEnclosing { local: owner };
            let written = enclosing.get(range.start as usize..range.end as usize).ok_or(outside)?;
            if written == local.text {
                leaf.get_or_insert((owner, offset));
                offset = range.start.checked_add(offset).ok_or(outside)?;
            } else {
                leaf = None;
                offset = range.start;
            }
            owner = local.enclosing;
            continue;
        }
        let anchor = match keys.origin(owner) {
            Some(crate::front2::keys::Origin::Member { .. }) => {
                let text = store.fns.get(&owner).ok_or(SiteGap::NoRow { item: owner })?.text;
                let (container, row) = keys.source_member(names, owner, text).ok_or(SiteGap::NoMemberRow { owner })?;
                Anchor::Member { container, row }
            }
            _ => {
                let lib = keys.library_in(names, owner).ok_or(SiteGap::NoLibrary { owner })?;
                Anchor::from_lib(names, lib).ok_or(SiteGap::NoAnchor { item: owner })?
            }
        };
        return Ok(BodySite { anchor, offset, local: leaf });
    }
    Err(SiteGap::TooDeep { owner: asked, kept: store.locals.kept() })
}

/// The file of a trap row's body owner. The trap always keeps a body-relative offset.
pub(in crate::front2::program) fn item_file<'n, S: PositionSource<'n>>(names: S, keys: &Keys, store: &Store<'n>, item: ItemKey) -> Option<&'n str> {
    let point = body_anchor(names, keys, store, item, 0).ok()?;
    let (krate, file, _) = point.anchor.placement(names, store.tys.alloc()).ok()?;
    names.file_shown(krate, file)
}

/// THE POSITION RULE: the line and column (each from 1; the column in chars) of byte `site` of
/// item `item`'s text: the item's first byte today (`NamesSource::item_start`) plus `site`, in
/// today's text of its file (`NamesSource::file_text`), counted there. What a `#[track_caller]`
/// `Location`'s line and column are when the program is placed: the code record holds only
/// `item` and `site` (rule 71).
pub(in crate::front2::program) fn source_position<'n, S: PositionSource<'n>>(names: S, keys: &Keys, store: &Store<'n>, item: ItemKey, site: u32) -> Result<(u32, u32), PositionGap> {
    if keys.origin(item).is_none() {
        return position_of_source_key(names, item, site, store.tys.alloc());
    }
    if let Some(live) = keys.source_local_body(item) {
        // A written local's source key (`caller_site`): its site counts from the declaration's
        // first token (local_position.rs `first_token`), which the live body's range (from its
        // first doc comment) need not start at.
        let point = body_anchor(names, keys, store, live, 0).map_err(|_| PositionGap::NoItem)?;
        let (_, relative) = point.local.ok_or(PositionGap::NoItem)?;
        let declaration = point.offset.checked_sub(relative).ok_or(PositionGap::NoItem)?;
        let text = point.anchor.text(names).ok_or(PositionGap::NoItem)?;
        let first = local_position::first_token(text, declaration).ok_or(PositionGap::PastText)?;
        return position_in_file(names, point.anchor, first.checked_add(site).ok_or(PositionGap::PastText)?, store.tys.alloc());
    }
    let point = body_anchor(names, keys, store, item, site).map_err(|_| PositionGap::NoItem)?;
    position_in_file(names, point.anchor, point.offset, store.tys.alloc())
}

impl<'n> crate::front2::source::Records<'n> {
    /// `NamesSource::call_origin` over records (source.rs `Records::call_origin`, which reads
    /// `parts`, the expansion's two stored origin parts: its call part and its column). The
    /// call identifies an entry in the calling text's outline. That index is valid against the
    /// text whose digest the record proves.
    pub(crate) fn recorded_call_origin(
        &'n self, parts: (&'n [u8], &'n [u8]), krate: usize, call: LibKey, alloc: Alloc<'_>,
    ) -> Result<crate::front2::expansion_origin::CallOrigin<'n>, crate::front2::expansion_origin::Unmapped> {
        use crate::front2::expansion_origin::{CallOrigin, Unmapped};
        let (header, column) = parts;
        let mut view = blaze_scope::expansion_origins::OriginsView::open_parts(header, column)
            .map_err(|error| Unmapped::Origins { call, error })?;
        let edition = self.edition_of(krate).ok_or(Unmapped::NoText { call })?;
        let shown = match view.text {
            TextAt::File(file) => self.file_shown(krate, file),
            TextAt::Expansion(_) => self.root_module(krate).and_then(|root| self.module_path(root)),
        }.ok_or(Unmapped::NoText { call })?;
        if let TextAt::File(file) = view.text {
            // The origins hold the entry number in the outline when the call
            // ran. If that entry has an item row (e.g. a derive's struct),
            // that row's chain finds it even after other entries have moved.
            let record = self.items_record(krate).ok_or(Unmapped::NoText { call })?;
            for i in 0..record.items() {
                let row = record.item_at(i).map_err(|_| Unmapped::NoText { call })?.ok_or(Unmapped::NoText { call })?;
                if row.outline == view.item && row.text.is_some_and(|span| span.at == view.text) {
                    let (at, first) = self.item_start(row.key).ok_or(Unmapped::NoText { call })?;
                    if at != view.text {
                        return Err(Unmapped::NoText { call });
                    }
                    let entry = self.text_of(row.key).ok_or(Unmapped::NoText { call })?;
                    let current = blaze_file_items::outline_item(entry.text, edition, alloc);
                    let item = current.items().first().ok_or(Unmapped::Input { call })?;
                    let pieces = record.pieces_at(i).map_err(|_| Unmapped::Input { call })?;
                    if item.hash != pieces.body {
                        return Err(Unmapped::Input { call });
                    }
                    // The item hash proves its tokens. Trivia can still change
                    // their bytes, so rebuild the input's byte range by the same
                    // rule as task/lower.rs fn_like_call and call_in_text.
                    let input = match view.shape {
                        blaze_expand::origin_column::InputShape::Whole => entry.text,
                        blaze_expand::origin_column::InputShape::Inside => {
                            let blaze_surface::ItemDetail::MacroCall { input: Some(range), .. } = item.detail else {
                                return Err(Unmapped::Input { call });
                            };
                            blaze_scope::task::inside(item.text(entry.text, range))
                        }
                    };
                    view.input_start = blaze_scope::expansion_origins::offset_in(entry.text, input).ok_or(Unmapped::Input { call })?;
                    view.input_len = u32::try_from(input.len()).map_err(|_| Unmapped::Input { call })?;
                    let len = u32::try_from(entry.text.len()).map_err(|_| Unmapped::NoText { call })?;
                    let end = first.checked_add(len).ok_or(Unmapped::NoText { call })?;
                    let text = self.file_text(krate, file).ok_or(Unmapped::NoText { call })?;
                    return Ok(CallOrigin { view, text, shown, entry: (first, end), edition });
                }
            }
        }
        let text = self.whole_text(krate, view.text).ok_or(Unmapped::NoText { call })?;
        let outline = blaze_file_items::outline(text, edition, alloc);
        let entry = outline.items().get(view.item.0 as usize).ok_or(Unmapped::NoText { call })?.range;
        Ok(CallOrigin { view, text, shown, entry: (entry.start, entry.end), edition })
    }

    /// Places a stored caller site before a code-store hit has constructed a lazy program.
    /// Members are indexed separately by their library key in the member owners section.
    pub fn source_position(&'n self, item: ItemKey, site: u32, alloc: Alloc<'_>) -> Result<(u32, u32), PositionGap> {
        position_of_source_key(self, item, site, alloc)
    }
}

/// The source key may have been produced by a different program. A member has
/// a named member row. A written local has a lexical key under a body row.
fn position_of_source_key<'n, S: PositionSource<'n>>(names: S, item: ItemKey, site: u32, alloc: Alloc<'_>) -> Result<(u32, u32), PositionGap> {
    let first = item.0.to_be_bytes();
    if let Some(lib) = names.key_beginning(first) {
        return position_in_file(names, Anchor::from_lib(names, lib).ok_or(PositionGap::NoItem)?, site, alloc);
    }
    let mut whole = [0; 32];
    whole[..16].copy_from_slice(&first);
    let krate = names.crate_of(LibKey(ContentId(whole))).ok_or(PositionGap::NoItem)?;
    if let Some(lib) = names.member_beginning(krate, item)? {
        return position_in_file(names, Anchor::from_lib(names, lib).ok_or(PositionGap::NoItem)?, site, alloc);
    }
    if first[8] & LibKey::INNER == 0 {
        return Err(PositionGap::NoItem);
    }
    local_position::in_source(names, krate, item, site, alloc)
}

impl<'n> Start<'n> {
    fn byte<S: NamesSource<'n>>(self, names: S, krate: usize, text: &str, alloc: Alloc<'_>) -> Result<u32, PositionGap> {
        match self {
            Start::Byte(at) | Start::Copied(at, _) | Start::Invocation(at) => Ok(at),
            Start::Entry(index, part) | Start::MemberInvocation(index, part) => {
                // The member row already has today's outline index. A loader may hand
                // back independent buffers for the file and the member's text.
                if let Some(at) = blaze_scope::expansion_origins::offset_in(text, part) {
                    return Ok(at);
                }
                let outline = blaze_file_items::outline(text, names.edition_of(krate).ok_or(PositionGap::NoFile)?, alloc);
                let range = outline.items().get(index.0 as usize).ok_or(PositionGap::NoItem)?.range;
                if text.get(range.start as usize..range.end as usize) != Some(part) {
                    return Err(PositionGap::NoItem);
                }
                Ok(range.start)
            }
        }
    }
}

/// Where the body of the library item `lib` stands in its file, for `line!()`/`column!()` in
/// eager input of its calls (front2 body_host.rs `Placed`): its crate, the file's whole text,
/// the byte its calls count from, and whether EVERY call in it is at that byte. By
/// [`Anchor::placement`], the rule a `#[track_caller]` `Location` is placed by (one answer in
/// both): a free fn or a member written in a file, at its first byte, each call at its own
/// byte after it; an item or a member a macro call wrote (a `macro_rules` call at module level,
/// a derive, an attribute macro), at the OUTERMOST call's site, every call in it (core `line!`:
/// "the first macro invocation leading up to the expansion of the `line!` macro"; rustc
/// `expansion_cause`). Before (tempo a2015, 8590 lines): such a body had no place, and its
/// `line!()` was refused (`HostError { ask: Place, why: Refused }`).
pub fn body_place<'n, S: PositionSource<'n>>(names: S, lib: LibKey, alloc: Alloc<'_>) -> Result<(usize, &'n str, u32, bool), PositionGap> {
    let (krate, file, start) = Anchor::from_lib(names, lib).ok_or(PositionGap::NoItem)?.placement(names, alloc)?;
    let text = names.file_text(krate, file).ok_or(PositionGap::NoFile)?;
    let at = start.byte(names, krate, text, alloc)?;
    Ok((krate, text, at, matches!(start, Start::Invocation(_) | Start::MemberInvocation(..))))
}

fn position_in_file<'n, S: PositionSource<'n>>(names: S, anchor: Anchor<'n>, site: u32, alloc: Alloc<'_>) -> Result<(u32, u32), PositionGap> {
    position_at(names, anchor.placement(names, alloc)?, site, alloc)
}

fn position_at<'n, S: NamesSource<'n>>(names: S, (krate, i, start): (usize, u32, Start<'n>), site: u32, alloc: Alloc<'_>) -> Result<(u32, u32), PositionGap> {
    let text = names.file_text(krate, i).ok_or(PositionGap::NoFile)?;
    let site = match start {
        Start::Invocation(_) | Start::MemberInvocation(..) => 0,
        _ => site,
    };
    line_and_column(text, start.byte(names, krate, text, alloc)?, site)
}

/// Byte `start + site` of `text` as a line and a column, each from 1, the column in chars.
/// [`PositionGap::PastText`]: past the end, or not on a char's first byte.
fn line_and_column(text: &str, start: u32, site: u32) -> Result<(u32, u32), PositionGap> {
    let byte = start.checked_add(site).ok_or(PositionGap::PastText)? as usize;
    let before = text.get(..byte).ok_or(PositionGap::PastText)?;
    let line_start = before.rfind('\n').map_or(0, |n| n + 1);
    let lines = before.bytes().filter(|&b| b == b'\n').count();
    let chars = before[line_start..].chars().count();
    let line = u32::try_from(lines + 1).map_err(|_| PositionGap::PastText)?;
    let col = u32::try_from(chars + 1).map_err(|_| PositionGap::PastText)?;
    Ok((line, col))
}

#[cfg(test)]
mod tests {
    use super::line_and_column;
    use crate::seam::PositionGap;

    /// Lines and columns from 1, the column in chars: a 'é' (two bytes) before the site counts
    /// once; a site inside a char, or past the text, is `PastText`.
    #[test]
    fn a_site_is_a_line_and_a_column_in_chars() {
        let text = "fn a() {}\nfn é() { f() }\n";
        // The item `fn é` starts at byte 10; `f()` at byte 10 + 10 (two bytes of 'é').
        assert_eq!(line_and_column(text, 10, 0), Ok((2, 1)));
        assert_eq!(line_and_column(text, 10, 10), Ok((2, 10)));
        assert_eq!(line_and_column(text, 10, 4), Err(PositionGap::PastText));
        assert_eq!(line_and_column(text, 10, 100), Err(PositionGap::PastText));
        std::println!("positions: line 2, column 10 for `f()` after a two-byte char");
    }
}
