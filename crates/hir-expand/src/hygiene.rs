//! Machinery for hygienic macros.
//!
//! Inspired by Matthew Flatt et al., “Macros That Work Together: Compile-Time Bindings, Partial
//! Expansion, and Definition Contexts,” *Journal of Functional Programming* 22, no. 2
//! (March 1, 2012): 181–216, <https://doi.org/10.1017/S0956796812000093>.
//!
//! Also see <https://rustc-dev-guide.rust-lang.org/macro-expansion.html#hygiene-and-hierarchies>
//!
//! # The Expansion Order Hierarchy
//!
//! `ExpnData` in rustc, rust-analyzer's version is `MacroCallLoc`. Traversing the hierarchy
//! upwards can be achieved by walking up `MacroCallLoc::kind`'s contained file id, as
//! `MacroFile`s are interned `MacroCallLoc`s.
//!
//! # The Macro Definition Hierarchy
//!
//! `SyntaxContextData` in rustc and rust-analyzer. Basically the same in both.
//!
//! # The Call-site Hierarchy
//!
//! `ExpnData::call_site` in rustc, `MacroCallLoc::call_site` in rust-analyzer.
// FIXME: Move this into the span crate? Not quite possible today as that depends on `MacroCallLoc`
// which contains a bunch of unrelated things

use std::cell::Cell;

use base_db::Db;
use blaze_memo::ArenaId;
use span::{Edition, MacroCallId, Span, SyntaxContext};

pub use span::Transparency;

/// The last few answers of a pure function of a small key, for one expansion: a `macro_rules`
/// expansion marks the span of every token it transcribes, and nearly all of them carry one or
/// two contexts, so each distinct context is marked once (a few interned lookups and up to three
/// interns) instead of once per token. Local to the expansion, so no shared write.
pub(crate) struct MarkCache<K, V> {
    entries: Cell<[Option<(K, V)>; MARK_WAYS]>,
    next: Cell<usize>,
}

const MARK_WAYS: usize = 4;

impl<K: Copy + Eq, V: Copy> MarkCache<K, V> {
    pub(crate) fn new() -> Self {
        Self { entries: Cell::new([None; MARK_WAYS]), next: Cell::new(0) }
    }

    /// `compute(key)`, from the cache when `key` was one of the last [`MARK_WAYS`] missed.
    #[inline]
    pub(crate) fn get_or_insert_with(&self, key: K, compute: impl FnOnce(K) -> V) -> V {
        let mut entries = self.entries.get();
        if let Some(&(_, value)) = entries.iter().flatten().find(|(it, _)| *it == key) {
            return value;
        }
        let value = compute(key);
        let at = self.next.get();
        entries[at] = Some((key, value));
        self.entries.set(entries);
        self.next.set((at + 1) % MARK_WAYS);
        value
    }
}

pub fn span_with_def_site_ctxt(
    db: &Db,
    span: Span,
    expn_id: MacroCallId,
    edition: Edition,
) -> Span {
    span_with_ctxt_from_mark(db, span, expn_id, Transparency::Opaque, edition)
}

pub fn span_with_call_site_ctxt(
    db: &Db,
    span: Span,
    expn_id: MacroCallId,
    edition: Edition,
) -> Span {
    span_with_ctxt_from_mark(db, span, expn_id, Transparency::Transparent, edition)
}

pub fn span_with_mixed_site_ctxt(
    db: &Db,
    span: Span,
    expn_id: MacroCallId,
    edition: Edition,
) -> Span {
    span_with_ctxt_from_mark(db, span, expn_id, Transparency::SemiOpaque, edition)
}

fn span_with_ctxt_from_mark(
    db: &Db,
    span: Span,
    expn_id: MacroCallId,
    transparency: Transparency,
    edition: Edition,
) -> Span {
    // A root needs no table: the call's own is the one the mark is made in.
    let root = SyntaxContext::root(edition);
    Span { ctx: apply_mark(db, expn_id.arena(), root, expn_id, transparency, edition), ..span }
}

/// `ctxt` marked with the expansion `call_id`, in the context table `table` (contexts are
/// crate-local, see [`span::SyntaxContext`]): `ctxt` is a context of `table`, and so is the
/// result. `table` is the arena of the crate whose contexts are marked: the calling crate's
/// ([`span::MacroCallId::arena`]) for an expansion's own marks, or a crate that depends on
/// it when a context of that crate replays a dependency's marks (a glob's, in name
/// resolution). A context of another table is carried over with
/// [`SyntaxContext::translate`] before it is marked: a macro defined in a crate `D` and
/// expanded in `C` marks `D`'s definition-site contexts translated into `C`'s table.
///
/// The refinement: for `call_id` homed at `from` or below and `ctxt` of `from`,
/// `apply_mark(table, ctxt.translate(from, table), ..)` is
/// `apply_mark(from, ctxt, ..).translate(from, table)`: marking commutes with carrying a
/// context into a dependent's table.
///
/// # Panics
///
/// Where `call_id` is homed past `table` (a call of a crate that depends on `table`'s), as
/// [`SyntaxContext::new`] does.
pub fn apply_mark(
    db: &Db,
    table: ArenaId,
    ctxt: span::SyntaxContext,
    call_id: span::MacroCallId,
    transparency: Transparency,
    edition: Edition,
) -> SyntaxContext {
    if transparency == Transparency::Opaque {
        return apply_mark_in(db, table, ctxt, call_id, transparency, edition);
    }

    // The call site is in the calling crate's code, so its context is of the call's table,
    // carried into `table` (itself when they are one).
    let call_site_ctxt =
        crate::MacroCallId::from(call_id).loc(db).ctxt.translate(db, call_id.arena(), table);
    let mut call_site_ctxt = if transparency == Transparency::SemiOpaque {
        call_site_ctxt.normalize_to_macros_2_0(db, table)
    } else {
        call_site_ctxt.normalize_to_macro_rules(db, table)
    };

    if call_site_ctxt.is_root() {
        return apply_mark_in(db, table, ctxt, call_id, transparency, edition);
    }

    // Otherwise, `expn_id` is a macros 1.0 definition and the call site is in a
    // macros 2.0 expansion, i.e., a macros 1.0 invocation is in a macros 2.0 definition.
    //
    // In this case, the tokens from the macros 1.0 definition inherit the hygiene
    // at their invocation. That is, we pretend that the macros 1.0 definition
    // was defined at its invocation (i.e., inside the macros 2.0 definition)
    // so that the macros 2.0 definition remains hygienic.
    //
    // See the example at `test/ui/hygiene/legacy_interaction.rs`.
    for (mark, transparency) in ctxt.marks(db, table) {
        call_site_ctxt = apply_mark_in(db, table, call_site_ctxt, mark, transparency, edition);
    }
    apply_mark_in(db, table, call_site_ctxt, call_id, transparency, edition)
}

/// `ctxt` marked with `call_id`, both in `arena`'s table: `call_id` is homed at `arena` or
/// below it (a mark replayed from a context of that table).
fn apply_mark_in(
    db: &Db,
    arena: ArenaId,
    ctxt: SyntaxContext,
    call_id: MacroCallId,
    transparency: Transparency,
    edition: Edition,
) -> SyntaxContext {
    let (mut opaque, mut opaque_and_semiopaque) =
        ctxt.opaque_and_opaque_and_semiopaque(db, arena);

    if transparency >= Transparency::Opaque {
        let parent = opaque;
        opaque =
            SyntaxContext::new(db, arena, call_id, transparency, edition, parent, None, None);
    }

    if transparency >= Transparency::SemiOpaque {
        let parent = opaque_and_semiopaque;
        opaque_and_semiopaque = SyntaxContext::new(
            db,
            arena,
            call_id,
            transparency,
            edition,
            parent,
            Some(opaque),
            None,
        );
    }

    let parent = ctxt;
    SyntaxContext::new(
        db,
        arena,
        call_id,
        transparency,
        edition,
        parent,
        Some(opaque),
        Some(opaque_and_semiopaque),
    )
}

#[cfg(test)]
mod tests {
    use std::{alloc::Global, cell::RefCell};

    use base_db::{Crate, Db, all_crates};
    use blaze_ids::rng::SplitMix64;
    use blaze_memo::ArenaId;
    use span::{Edition, SyntaxContext};
    use syntax::{
        AstPtr, ast,
        view::{self, AstNode as _, HasName as _},
    };

    use super::{MarkCache, Transparency, apply_mark};
    use crate::{
        ExpandTo, HirFileId, InFile, MacroCallKind, MacroDefId, MacroDefKind,
        builtin::BuiltinFnLikeExpander,
        fixture::{self, Fixture},
    };

    /// Crate `d`, defining `stringify!`, and crate `c`, depending on it, each in its own source
    /// root, each with one call of it in an expression.
    const TWO_CRATES: &str = r#"
//- /d.rs crate:d
#[rustc_builtin_macro]
macro_rules! stringify { () => {} }
fn d() { let _ = stringify!(); }
//- /c.rs crate:c deps:d new_source_root:local
fn c() { let _ = stringify!(); }
"#;

    const EDITION: Edition = Edition::CURRENT;

    const TRANSPARENCIES: [Transparency; 3] =
        [Transparency::Transparent, Transparency::SemiOpaque, Transparency::Opaque];

    fn crate_named(db: &Db, name: &str) -> Crate {
        all_crates(db)
            .iter()
            .copied()
            .find(|krate| {
                krate.extra_data(db).display_name.as_ref().map(|it| it.canonical_name().as_str())
                    == Some(name)
            })
            .unwrap_or_else(|| panic!("no crate `{name}`"))
    }

    /// `d.rs`'s `macro_rules! stringify`, as the builtin of crate `d`.
    fn stringify(f: &Fixture, d: Crate) -> MacroDefId {
        let db = f.db();
        let file = HirFileId::from(f.file_at("/d.rs"));
        let rules = file
            .root(db)
            .descendants()
            .filter_map(view::MacroRules::cast)
            .find(|it| it.name().is_some_and(|it| it.text() == "stringify"))
            .expect("`d.rs` defines `stringify`");
        let ptr = AstPtr::<ast::Macro>::of(view::Macro::MacroRules(rules));
        let ast_id = InFile::new(file, file.ast_id_map(db).ast_id_for_ptr(ptr));
        MacroDefId {
            krate: d,
            edition: EDITION,
            kind: MacroDefKind::BuiltIn(ast_id, BuiltinFnLikeExpander::Stringify),
            local_inner: false,
            allow_internal_unsafe: false,
            edition_panic: false,
        }
    }

    /// A call of `def` at the `stringify!()` of `path`, by `krate`, its call site of context
    /// `ctxt` (of `krate`'s table).
    fn call(
        f: &Fixture,
        path: &str,
        def: MacroDefId,
        krate: Crate,
        ctxt: SyntaxContext,
    ) -> span::MacroCallId {
        let db = f.db();
        let file = HirFileId::from(f.file_at(path));
        let site = (file.root(db).descendants())
            .find_map(view::MacroCall::cast)
            .unwrap_or_else(|| panic!("`{path}` calls `stringify!`"));
        let ast_id = InFile::new(file, file.ast_id_map(db).ast_id_of_view::<ast::MacroCall>(site));
        let expand_to =
            ExpandTo::of_call_site_view(ast_id.to_view(db)).expect("a call in an expression");
        let kind = MacroCallKind::FnLike { ast_id, expand_to, eager: None };
        def.make_call(db, krate, kind, ctxt, 0).into()
    }

    fn marks(
        db: &Db,
        ctxt: SyntaxContext,
        table: ArenaId,
    ) -> Vec<(span::MacroCallId, Transparency), Global> {
        let mut marks = Vec::new_in(Global);
        marks.extend(ctxt.marks(db, table));
        marks
    }

    /// The refinement: for a call homed at `d` and a context of `d`'s table, marking the
    /// context carried into `c`'s table ([`SyntaxContext::translate`]) in `c`'s table is
    /// marking it in `d`'s and carrying the result: `apply_mark` commutes with `translate`.
    /// Held for every transparency, over the root and a marked context, for a call whose call
    /// site is the root and one whose call site is marked (so the transparent and semi-opaque
    /// marks replay the context's marks over the call site's, carried from the call's table).
    #[test]
    fn marking_commutes_with_carrying_into_a_dependents_table() {
        let f = fixture::db(TWO_CRATES);
        let db = f.db();
        let (d, c) = (crate_named(db, "d"), crate_named(db, "c"));
        let def = stringify(&f, d);
        let root = SyntaxContext::root(EDITION);
        let d_call = call(&f, "/d.rs", def, d, root);
        let (from, to) = (d_call.arena(), call(&f, "/c.rs", def, c, root).arena());
        assert_ne!(from, to, "`d` and `c` have a table each, or the test carries nothing");

        let marked = apply_mark(db, from, root, d_call, Transparency::Opaque, EDITION);
        let nested = call(&f, "/d.rs", def, d, marked);
        assert_eq!(nested.arena(), from, "a call of `d` with a call site of `d` is `d`'s");
        assert!(!marked.translate(db, from, to).is_root());

        for transparency in TRANSPARENCIES {
            for ctxt in [root, marked] {
                for call_id in [d_call, nested] {
                    let in_d = apply_mark(db, from, ctxt, call_id, transparency, EDITION);
                    let carried_then_marked = apply_mark(
                        db,
                        to,
                        ctxt.translate(db, from, to),
                        call_id,
                        transparency,
                        EDITION,
                    );
                    let marked_then_carried = in_d.translate(db, from, to);
                    let at = format!("{transparency:?} by {call_id:?} over {ctxt:?}");
                    assert_eq!(carried_then_marked, marked_then_carried, "{at}");
                    assert_eq!(marks(db, carried_then_marked, to), marks(db, in_d, from), "{at}");
                }
            }
        }
    }

    /// The invariant: `c`'s mark over a context of `d` carried into `c`'s table is a context of
    /// `c`'s table whose marks are `d`'s, then `c`'s call's.
    #[test]
    fn a_dependents_mark_over_a_carried_context_keeps_the_dependencys_marks() {
        let f = fixture::db(TWO_CRATES);
        let db = f.db();
        let (d, c) = (crate_named(db, "d"), crate_named(db, "c"));
        let def = stringify(&f, d);
        let root = SyntaxContext::root(EDITION);
        let d_call = call(&f, "/d.rs", def, d, root);
        let c_call = call(&f, "/c.rs", def, c, root);
        let (from, to) = (d_call.arena(), c_call.arena());
        assert_ne!(from, to, "`d` and `c` have a table each, or the test carries nothing");

        let in_d = apply_mark(db, from, root, d_call, Transparency::SemiOpaque, EDITION);
        let carried = in_d.translate(db, from, to);
        for transparency in TRANSPARENCIES {
            let in_c = apply_mark(db, to, carried, c_call, transparency, EDITION);
            assert_eq!(
                marks(db, in_c, to),
                [(d_call, Transparency::SemiOpaque), (c_call, transparency)],
                "{transparency:?}"
            );
        }
    }

    /// Over seeded key sequences, the cache gives what the function gives, and computes a key
    /// again only after four others were missed since.
    #[test]
    fn cache_gives_the_function() {
        let f = |key: u32| key.wrapping_mul(2_654_435_761) ^ 7;
        let mut rng = SplitMix64::new(0x9e37_79b9);
        for distinct in [1u32, 2, 3, 4, 5, 9] {
            let calls = RefCell::new(Vec::new());
            let cache = MarkCache::new();
            let mut misses_model: Vec<u32> = Vec::new();
            for _ in 0..500 {
                let key = rng.next_u32() % distinct;
                let got = cache.get_or_insert_with(key, |it| {
                    calls.borrow_mut().push(it);
                    f(it)
                });
                assert_eq!(got, f(key));
                let window = misses_model.len().saturating_sub(4);
                if !misses_model[window..].contains(&key) {
                    misses_model.push(key);
                }
            }
            assert_eq!(*calls.borrow(), misses_model, "{distinct} keys");
            if distinct <= 4 {
                assert_eq!(calls.borrow().len(), distinct as usize);
            }
        }
    }
}
