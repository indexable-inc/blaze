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
    Span {
        ctx: apply_mark(db, SyntaxContext::root(edition), expn_id, transparency, edition),
        ..span
    }
}

pub(super) fn apply_mark(
    db: &Db,
    ctxt: span::SyntaxContext,
    call_id: span::MacroCallId,
    transparency: Transparency,
    edition: Edition,
) -> SyntaxContext {
    if transparency == Transparency::Opaque {
        return apply_mark_internal(db, ctxt, call_id, transparency, edition);
    }

    let call_site_ctxt = crate::MacroCallId::from(call_id).loc(db).ctxt;
    let mut call_site_ctxt = if transparency == Transparency::SemiOpaque {
        call_site_ctxt.normalize_to_macros_2_0(db)
    } else {
        call_site_ctxt.normalize_to_macro_rules(db)
    };

    if call_site_ctxt.is_root() {
        return apply_mark_internal(db, ctxt, call_id, transparency, edition);
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
    for (call_id, transparency) in ctxt.marks(db) {
        call_site_ctxt = apply_mark_internal(db, call_site_ctxt, call_id, transparency, edition);
    }
    apply_mark_internal(db, call_site_ctxt, call_id, transparency, edition)
}

fn apply_mark_internal(
    db: &Db,
    ctxt: SyntaxContext,
    call_id: MacroCallId,
    transparency: Transparency,
    edition: Edition,
) -> SyntaxContext {
    let (mut opaque, mut opaque_and_semiopaque) = ctxt.opaque_and_opaque_and_semiopaque(db);

    if transparency >= Transparency::Opaque {
        let parent = opaque;
        opaque = SyntaxContext::new(db, call_id, transparency, edition, parent, None, None);
    }

    if transparency >= Transparency::SemiOpaque {
        let parent = opaque_and_semiopaque;
        opaque_and_semiopaque =
            SyntaxContext::new(db, call_id, transparency, edition, parent, Some(opaque), None);
    }

    let parent = ctxt;
    SyntaxContext::new(
        db,
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
    use std::cell::RefCell;

    use blaze_ids::rng::SplitMix64;

    use super::MarkCache;

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
