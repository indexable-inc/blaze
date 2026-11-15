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
//!
//! # Crate-local contexts
//!
//! A [`SyntaxContext`] is a `u32` inside every [`crate::Span`], so it cannot hold a
//! [`blaze_memo::Gid`] (8 bytes). It is an index into one crate's table of contexts instead:
//! the contexts interned in that crate's arena, whose data names the arena (its `table`
//! field), so every context a chain links to (its parent, its opaque forms) is in the same
//! table. Every lookup takes the arena the context is relative to: the
//! arena of the crate whose expansion or file the span came with. A root context (no
//! expansion) is the same value in every arena and needs none. A context of one crate read in
//! another (a macro definition's tokens, expanded downstream) is carried over with
//! [`SyntaxContext::translate`]: the same chain interned in the reading crate's table, so
//! comparisons within one table answer as they would over one global table.

pub use self::imp::*;

/// A syntax context describes a hierarchy tracking order of macro definitions.
mod imp {
    use std::{alloc::Global, fmt, num::NonZeroU32};

    use blaze_memo::{ArenaId, Database, Gid, Id};
    use syntax::Edition;

    use crate::{MacroCallId, Transparency};

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, blaze_memo::Home)]
    struct SyntaxContextData {
        /// The crate's table the context is in, which is its home: every context below is an
        /// index there.
        table: ArenaId,
        /// Of the calling crate's arena or a dependency's, so within `table`'s closure.
        outer_expn: MacroCallId,
        #[home(skip)]
        outer_transparency: Transparency,
        #[home(skip)]
        edition: Edition,
        #[home(skip)]
        parent: SyntaxContext,
        #[home(skip)]
        opaque_if_not_self: Option<SyntaxContext>,
        #[home(skip)]
        opaque_and_semiopaque_if_not_self: Option<SyntaxContext>,
    }

    impl SyntaxContextData {
        #[inline]
        fn opaque(&self, ctx: SyntaxContext) -> SyntaxContext {
            self.opaque_if_not_self.unwrap_or(ctx)
        }

        #[inline]
        fn opaque_and_semiopaque(&self, ctx: SyntaxContext) -> SyntaxContext {
            self.opaque_and_semiopaque_if_not_self.unwrap_or(ctx)
        }
    }

    // `unsafe(no_lifetime)`: an interned value lives as long as its arena, and a context is
    // only looked up through a database holding that arena.
    #[blaze_memo::interned(unsafe(no_lifetime))]
    struct SyntaxContextImpl {
        data: SyntaxContextData,
    }

    /// A syntax context: an index into one crate's table of contexts (see the module docs), or
    /// a root.
    ///
    /// The `u32` ([`SyntaxContext::into_u32`]) is `slot << 2 | edition`: the edition in the
    /// low two bits, so [`SyntaxContext::edition`] needs no table; `slot` 0 is the root of that
    /// edition, the same value in every arena, and any other `slot` is one plus the context's
    /// index in its arena (the arena's local index, its spill bit moved down to
    /// [`SyntaxContext::SPILL_SLOT`]).
    #[derive(Clone, Copy, PartialEq, Eq, Hash)]
    pub struct SyntaxContext(NonZeroU32);

    const _: () = assert!(
        (Edition::LATEST as u32) < 1 << SyntaxContext::EDITION_BITS,
        "every edition fits a context's edition bits"
    );

    impl SyntaxContext {
        const EDITION_BITS: u32 = 2;
        const EDITION_MASK: u32 = (1 << Self::EDITION_BITS) - 1;
        /// Where an arena's spill bit ([`blaze_memo::SPILL`]) goes in a slot.
        const SPILL_SLOT: u32 = 1 << 28;
        /// The most contexts one arena's table (and its spill table) holds.
        pub const MAX_PER_ARENA: u32 = Self::SPILL_SLOT - 1;

        /// The context's word, crate-local: an index of its arena's table, or a root.
        #[inline]
        pub fn into_u32(self) -> u32 {
            self.0.get() - 1
        }

        /// # Contract
        ///
        /// `u32` is what [`SyntaxContext::into_u32`] gave for a context of the arena it will be
        /// looked up in. Memory safety doesn't rest on it (the memo checks the kind of every
        /// id it reads): another value gives a panic, or another context's hygiene data.
        /// Panics on `u32::MAX`, which no context has.
        #[inline]
        pub fn from_u32(u32: u32) -> Self {
            match u32.checked_add(1).and_then(NonZeroU32::new) {
                Some(it) => Self(it),
                None => panic!("invalid SyntaxContext({u32})"),
            }
        }

        #[inline]
        const fn slot(self) -> u32 {
            (self.0.get() - 1) >> Self::EDITION_BITS
        }

        /// The interned id of this context in `arena`; `None` for a root.
        #[inline]
        fn id(self, arena: ArenaId) -> Option<SyntaxContextImpl> {
            let slot = self.slot();
            if slot == 0 {
                return None;
            }
            let Some(local) = Self::local_of(slot) else {
                panic!(
                    "SyntaxContext({}) is no root and no index of {arena:?}'s contexts",
                    self.into_u32()
                )
            };
            // Whether it is a `SyntaxContextImpl` the memo checks on lookup (the entry's kind),
            // panicking if not.
            Some(SyntaxContextImpl::from_gid(Gid::in_arena(arena, local)))
        }

        /// The arena-local index a context's `slot` (not 0, a root's) names, its spill bit
        /// moved back up; `None` past every index an arena hands out.
        #[inline]
        const fn local_of(slot: u32) -> Option<u32> {
            let at = slot - 1;
            if at > Self::SPILL_SLOT | Self::MAX_PER_ARENA {
                return None;
            }
            Some(match at & Self::SPILL_SLOT {
                0 => at,
                _ => (at & !Self::SPILL_SLOT) | blaze_memo::SPILL,
            })
        }

        /// The context `raw` names in `arena`'s table: a word read from outside the process's
        /// own memory (a proc macro's reply on the wire), checked, never trusted, where
        /// [`SyntaxContext::from_u32`] trusts its caller. A root's word is that root, in every
        /// arena. Any other word is the context at its index of `arena`'s table, when that
        /// index holds one there of the edition the word says; `None` when it holds none (past
        /// the indices an arena hands out, past what the table holds, an entry of another
        /// kind, an arena this snapshot does not hold), when the context there is of another
        /// edition, and for `u32::MAX`, which is no word.
        ///
        /// A word of another arena's context is not told apart from one of `arena`'s: it names
        /// nothing here (`None`), or the context of `arena`'s table at its index (another
        /// context than the one it was in its own table). That is safe for the caller: what it
        /// gets is always a context of `arena`'s own table or nothing, so every lookup it makes
        /// with it in `arena` answers from that table and none panics or reads another; at
        /// worst the hygiene is that of another context of the same crate, which the reply's
        /// sender could have named anyway by giving back that context's span. Telling the two
        /// apart is the caller's protocol's (a reply names only spans its call was given).
        pub fn checked(db: &dyn Database, arena: ArenaId, raw: u32) -> Option<SyntaxContext> {
            let ctx = SyntaxContext(raw.checked_add(1).and_then(NonZeroU32::new)?);
            let slot = ctx.slot();
            if slot == 0 {
                return Some(ctx);
            }
            let local = Self::local_of(slot)?;
            let id = SyntaxContextImpl::try_from_gid(db, Gid::in_arena(arena, local))?;
            let data = id.data(db);
            (data.table == arena && data.edition == ctx.edition()).then_some(ctx)
        }

        /// The context of interned `id`, of `edition`.
        fn of(id: SyntaxContextImpl, edition: Edition) -> SyntaxContext {
            let local = id.gid().local();
            let index = local & !blaze_memo::SPILL;
            assert!(
                index < Self::MAX_PER_ARENA,
                "{:?} holds {} syntax contexts: past what a context's u32 indexes",
                id.gid().arena(),
                Self::MAX_PER_ARENA
            );
            let at = match local & blaze_memo::SPILL {
                0 => index,
                _ => index | Self::SPILL_SLOT,
            };
            let word = ((at + 1) << Self::EDITION_BITS) | edition as u32;
            SyntaxContext(NonZeroU32::new(word + 1).expect("a word below u32::MAX"))
        }

        #[inline]
        pub const fn is_root(self) -> bool {
            self.slot() == 0
        }

        #[inline]
        pub fn remove_root_edition(&mut self) {
            if self.is_root() {
                *self = Self::root(Edition::Edition2015);
            }
        }

        /// The root context, which is the parent of all other contexts. All `FileId`s have this
        /// context. The same value in every arena.
        #[inline]
        pub const fn root(edition: Edition) -> Self {
            // INVARIANT: Roots are valid `SyntaxContext`s
            SyntaxContext(NonZeroU32::new(edition as u32 + 1).unwrap())
        }

        /// The context made by expansion `outer_expn` over `parent`, in `arena`'s table: the
        /// arena of the crate expanding (`outer_expn`'s, [`MacroCallId::arena`]). `parent` and
        /// the opaque forms are contexts of that table ([`SyntaxContext::translate`] carries a
        /// dependency's into it).
        ///
        /// # Panics
        ///
        /// Where `outer_expn` is homed past `arena` (a call of a crate that depends on this
        /// one), or the table is full ([`SyntaxContext::MAX_PER_ARENA`]).
        #[expect(clippy::too_many_arguments, reason = "a context's fields, and its table")]
        pub fn new(
            db: &dyn Database,
            arena: ArenaId,
            outer_expn: MacroCallId,
            outer_transparency: Transparency,
            edition: Edition,
            parent: SyntaxContext,
            opaque_if_not_self: Option<SyntaxContext>,
            opaque_and_semiopaque_if_not_self: Option<SyntaxContext>,
        ) -> Self {
            let result = SyntaxContextImpl::new(
                db,
                SyntaxContextData {
                    table: arena,
                    outer_expn,
                    outer_transparency,
                    edition,
                    parent,
                    opaque_if_not_self,
                    opaque_and_semiopaque_if_not_self,
                },
            );
            assert!(
                result.gid().arena() == arena,
                "a syntax context of {arena:?}'s table by expansion {outer_expn:?}, homed in \
                 {:?}: an expansion of a crate past the table's",
                result.gid().arena()
            );
            SyntaxContext::of(result, edition)
        }

        /// This context of `from`'s table as a context of `to`'s: the same chain (its
        /// parents and opaque forms) interned in `to`, so equal contexts of `from` give equal
        /// ones of `to` and distinct ones distinct. A root is itself. `to` is the arena of a
        /// crate reading `from`'s contexts (`from` in its closure).
        pub fn translate(self, db: &dyn Database, from: ArenaId, to: ArenaId) -> SyntaxContext {
            if from == to {
                return self;
            }
            let Some(id) = self.id(from) else { return self };
            let data = *id.data(db);
            // Every link is to a context made before this one (its own form is `None`), so the
            // walk ends.
            let parent = data.parent.translate(db, from, to);
            let opaque = data.opaque_if_not_self.map(|it| it.translate(db, from, to));
            let semi = data.opaque_and_semiopaque_if_not_self.map(|it| it.translate(db, from, to));
            SyntaxContext::new(
                db,
                to,
                data.outer_expn,
                data.outer_transparency,
                data.edition,
                parent,
                opaque,
                semi,
            )
        }

        /// Invariant: Only the root [`SyntaxContext`] has a [`None`] outer expansion.
        #[inline]
        pub fn outer_expn(self, db: &dyn Database, arena: ArenaId) -> Option<MacroCallId> {
            self.id(arena).map(|id| id.data(db).outer_expn)
        }

        #[inline]
        pub fn outer_transparency(self, db: &dyn Database, arena: ArenaId) -> Transparency {
            match self.id(arena) {
                Some(id) => id.data(db).outer_transparency,
                None => Transparency::Opaque,
            }
        }

        /// The context's edition: in its word, so no table is read.
        #[inline]
        pub fn edition(self) -> Edition {
            Edition::from_u32(self.into_u32() & Self::EDITION_MASK)
        }

        #[inline]
        pub fn parent(self, db: &dyn Database, arena: ArenaId) -> SyntaxContext {
            match self.id(arena) {
                Some(id) => id.data(db).parent,
                None => self,
            }
        }

        /// This context, but with all transparent and semi-opaque expansions filtered away.
        #[inline]
        pub fn opaque(self, db: &dyn Database, arena: ArenaId) -> SyntaxContext {
            match self.id(arena) {
                Some(id) => id.data(db).opaque(self),
                None => self,
            }
        }

        /// This context, but with all transparent expansions filtered away.
        #[inline]
        pub fn opaque_and_semiopaque(self, db: &dyn Database, arena: ArenaId) -> SyntaxContext {
            match self.id(arena) {
                Some(id) => id.data(db).opaque_and_semiopaque(self),
                None => self,
            }
        }

        #[inline]
        pub fn opaque_and_opaque_and_semiopaque(
            self,
            db: &dyn Database,
            arena: ArenaId,
        ) -> (SyntaxContext, SyntaxContext) {
            match self.id(arena) {
                Some(id) => {
                    let data = id.data(db);
                    (data.opaque(self), data.opaque_and_semiopaque(self))
                }
                None => (self, self),
            }
        }

        #[inline]
        pub fn outer_mark(
            self,
            db: &dyn Database,
            arena: ArenaId,
        ) -> (Option<MacroCallId>, Transparency) {
            match self.id(arena) {
                Some(id) => {
                    let data = id.data(db);
                    (Some(data.outer_expn), data.outer_transparency)
                }
                None => (None, Transparency::Opaque),
            }
        }

        #[inline]
        pub fn normalize_to_macros_2_0(self, db: &dyn Database, arena: ArenaId) -> SyntaxContext {
            self.opaque(db, arena)
        }

        #[inline]
        pub fn normalize_to_macro_rules(self, db: &dyn Database, arena: ArenaId) -> SyntaxContext {
            self.opaque_and_semiopaque(db, arena)
        }

        #[inline]
        pub fn is_opaque(self, db: &dyn Database, arena: ArenaId) -> bool {
            !self.is_root() && self.outer_transparency(db, arena).is_opaque()
        }

        #[inline]
        pub fn remove_mark(
            &mut self,
            db: &dyn Database,
            arena: ArenaId,
        ) -> (Option<MacroCallId>, Transparency) {
            match self.id(arena) {
                Some(id) => {
                    let data = id.data(db);
                    *self = data.parent;
                    (Some(data.outer_expn), data.outer_transparency)
                }
                None => (None, Transparency::Opaque),
            }
        }

        /// The marks outermost last. Only for a macros 1.0 call in a macros 2.0 expansion
        /// (`apply_mark`), so it collects on the global heap.
        pub fn marks(
            self,
            db: &dyn Database,
            arena: ArenaId,
        ) -> impl Iterator<Item = (MacroCallId, Transparency)> {
            let mut marks = Vec::new_in(Global);
            marks.extend(self.marks_rev(db, arena));
            marks.reverse();
            marks.into_iter()
        }

        #[inline]
        pub fn marks_rev(
            self,
            db: &dyn Database,
            arena: ArenaId,
        ) -> impl Iterator<Item = (MacroCallId, Transparency)> {
            let data = move |ctx: SyntaxContext| ctx.id(arena).map(|it| it.data(db));
            std::iter::successors(data(self), move |ctx| data(ctx.parent))
                .map(|ctx| (ctx.outer_expn, ctx.outer_transparency))
        }
    }

    impl fmt::Display for SyntaxContext {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            if self.is_root() {
                write!(f, "ROOT{}", self.edition().number())
            } else {
                write!(f, "{}", self.0.get())
            }
        }
    }

    impl fmt::Debug for SyntaxContext {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            if f.alternate() {
                fmt::Display::fmt(self, f)
            } else {
                f.debug_tuple("SyntaxContext").field(&self.0).finish()
            }
        }
    }
}

/// A property of a macro expansion that determines how identifiers
/// produced by that expansion are resolved.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Hash, Debug)]
pub enum Transparency {
    /// Identifier produced by a transparent expansion is always resolved at call-site.
    /// Call-site spans in procedural macros, hygiene opt-out in `macro` should use this.
    Transparent,
    /// Identifier produced by a semi-opaque expansion may be resolved
    /// either at call-site or at definition-site.
    /// If it's a local variable, label or `$crate` then it's resolved at def-site.
    /// Otherwise it's resolved at call-site.
    /// `macro_rules` macros behave like this, built-in macros currently behave like this too,
    /// but that's an implementation detail.
    SemiOpaque,
    /// Identifier produced by an opaque expansion is always resolved at definition-site.
    /// Def-site spans in procedural macros, identifiers from `macro` by default use this.
    Opaque,
}

impl Transparency {
    /// Returns `true` if the transparency is [`Opaque`].
    ///
    /// [`Opaque`]: Transparency::Opaque
    pub fn is_opaque(&self) -> bool {
        matches!(self, Self::Opaque)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use blaze_mem::Tag;
    use blaze_memo::{Alone, ArenaId, Config, Gid, Memo, Plant, Reads};
    use syntax::Edition;

    use super::*;

    /// A database of span's one interned struct: span is below base-db's `Db`.
    struct TestDb(Memo);

    impl blaze_memo::Database for TestDb {
        fn memo(&self) -> &Memo {
            &self.0
        }
    }

    /// A session of two crate arenas, the second depending on the first: the database and
    /// the two arenas.
    fn two_crates() -> (TestDb, ArenaId, ArenaId) {
        let mut builder = Memo::builder(Config {
            arenas: 3,
            reads: Reads::Off,
            flight: Arc::new_in(Alone::new(), std::alloc::Global),
            cap_bytes: u64::MAX,
            plant: Plant::None,
        });
        builder.arena(0, Tag::Query);
        let lower = builder.arena(1, Tag::Query).id();
        let upper = builder.arena(2, Tag::Query);
        upper.borrows(lower);
        let upper = upper.id();
        (TestDb(builder.build()), lower, upper)
    }

    /// A call of `arena`'s, stored as the expansion of a context and never looked up.
    fn call(arena: ArenaId, n: u32) -> crate::MacroCallId {
        crate::MacroCallId(Gid::in_arena(arena, n))
    }

    #[test]
    fn test_root_edition_is_root() {
        for edition in Edition::iter() {
            let ctx = SyntaxContext::root(edition);
            assert!(ctx.is_root(), "{edition} root should be identified as root");
        }
    }

    #[test]
    fn test_root_edition_editions() {
        for edition in Edition::iter() {
            let ctx = SyntaxContext::root(edition);
            assert_eq!(edition, ctx.edition(), "{edition} root should have edition {edition}");
        }
    }

    /// The invariant: no interned context, of any edition and either half of an arena (its
    /// table or its spill table), has a root's word.
    #[test]
    fn test_roots_do_not_overlap_with_contexts() {
        let (db, lower, _) = two_crates();
        let root = SyntaxContext::root(Edition::Edition2015);
        for edition in Edition::iter() {
            let ctx = SyntaxContext::new(
                &db,
                lower,
                call(lower, 2),
                Transparency::Opaque,
                edition,
                root,
                None,
                None,
            );
            assert!(!ctx.is_root(), "an interned context of {edition} reads as a root");
            assert_eq!(ctx.edition(), edition);
            assert!(
                Edition::iter().all(|it| SyntaxContext::root(it) != ctx),
                "{ctx:?} is a root's word"
            );
        }
    }

    #[test]
    fn test_non_root_value_is_not_root() {
        for edition in Edition::iter() {
            let ctx = SyntaxContext::from_u32((1 << 2) | edition as u32);
            assert!(!ctx.is_root(), "{edition} root should be identified as root");
        }
    }

    #[test]
    fn test_interned_context_round_trips_through_u32() {
        let (db, lower, _) = two_crates();
        let root = SyntaxContext::root(Edition::Edition2015);
        let ctx = SyntaxContext::new(
            &db,
            lower,
            call(lower, 2),
            Transparency::Opaque,
            Edition::Edition2021,
            root,
            Some(root),
            Some(root),
        );

        let round_tripped = SyntaxContext::from_u32(ctx.into_u32());
        assert_eq!(round_tripped.edition(), Edition::Edition2021);
        assert_eq!(round_tripped.parent(&db, lower), root);
        assert_eq!(round_tripped.outer_expn(&db, lower), Some(call(lower, 2)));
    }

    /// The invariant: a context carried into a reading crate's table keeps its data and its
    /// chain, equal contexts carry to equal ones and distinct ones to distinct ones, and a root
    /// is itself. The control: read in the other arena without carrying, the word names
    /// another context there.
    #[test]
    fn translated_context_keeps_its_chain() {
        let (db, lower, upper) = two_crates();
        let root = SyntaxContext::root(Edition::Edition2021);
        // `upper`'s own first context, so the carried ones take other indices than in `lower`.
        let own = SyntaxContext::new(
            &db,
            upper,
            call(upper, 2),
            Transparency::Transparent,
            Edition::Edition2021,
            root,
            None,
            None,
        );
        let a = SyntaxContext::new(
            &db,
            lower,
            call(lower, 2),
            Transparency::SemiOpaque,
            Edition::Edition2021,
            root,
            None,
            None,
        );
        let b = SyntaxContext::new(
            &db,
            lower,
            call(lower, 3),
            Transparency::Opaque,
            Edition::Edition2021,
            a,
            Some(a),
            None,
        );
        let up_b = b.translate(&db, lower, upper);
        let up_a = a.translate(&db, lower, upper);
        assert_eq!(up_b.parent(&db, upper), up_a, "the parent carried with it");
        assert_eq!(up_b.opaque(&db, upper), up_a);
        assert_eq!(up_b.opaque_and_semiopaque(&db, upper), up_b, "its own form stays its own");
        assert_eq!(up_b.outer_mark(&db, upper), (Some(call(lower, 3)), Transparency::Opaque));
        assert_eq!(up_b.marks_rev(&db, upper).count(), 2);
        assert_eq!(b.translate(&db, lower, upper), up_b, "equal contexts carry to one");
        assert_ne!(up_a, up_b);
        assert_ne!(up_a, own);
        assert_eq!(root.translate(&db, lower, upper), root);
        // The control: `b`'s word read in `upper` untranslated is `upper`'s second context,
        // the carried `a`, not `b`.
        assert_eq!(b.outer_mark(&db, upper), up_a.outer_mark(&db, upper));
        assert_ne!(b.outer_mark(&db, upper), b.outer_mark(&db, lower));
    }

    /// A word past every index an arena hands out panics when looked up instead of reading
    /// another table.
    #[test]
    #[should_panic(expected = "no root and no index")]
    fn a_context_past_the_indices_panics() {
        let (db, lower, _) = two_crates();
        let ctx = SyntaxContext::from_u32(u32::MAX - 1);
        assert!(!ctx.is_root());
        ctx.parent(&db, lower);
    }

    /// The invariant: a word from outside, checked against an arena, is a context of that
    /// arena's own table or nothing. A root's word is that root in every arena; the word of a
    /// context made in the arena is that context; a word of that index with another edition's
    /// bits, a word past the table, one past every index and `u32::MAX` are nothing. A word of
    /// another arena's context names nothing here where this table holds nothing at its index,
    /// and this table's own context there where it holds one: never the other table's.
    #[test]
    fn a_checked_word_is_a_context_of_its_arena_or_nothing() {
        let (db, lower, upper) = two_crates();
        for edition in Edition::iter() {
            let root = SyntaxContext::root(edition);
            assert_eq!(SyntaxContext::checked(&db, lower, root.into_u32()), Some(root));
            assert_eq!(SyntaxContext::checked(&db, upper, root.into_u32()), Some(root));
        }
        let root = SyntaxContext::root(Edition::Edition2021);
        let context = |arena: ArenaId, n: u32, parent: SyntaxContext| {
            SyntaxContext::new(
                &db,
                arena,
                call(arena, n),
                Transparency::Opaque,
                Edition::Edition2021,
                parent,
                None,
                None,
            )
        };
        // `upper`'s table holds one context, `lower`'s two: `a` at the index `own` is at.
        let own = context(upper, 2, root);
        let a = context(lower, 2, root);
        let b = context(lower, 3, a);
        assert_eq!(SyntaxContext::checked(&db, lower, a.into_u32()), Some(a));
        assert_eq!(SyntaxContext::checked(&db, lower, b.into_u32()), Some(b));
        assert_eq!(SyntaxContext::checked(&db, upper, own.into_u32()), Some(own));
        let checked_b = SyntaxContext::checked(&db, lower, b.into_u32()).unwrap();
        assert_eq!(checked_b.parent(&db, lower), a, "a checked context reads as itself");
        // `b`'s index with 2015's bits: the context there is of 2021.
        let other_edition = (b.into_u32() & !0b11) | Edition::Edition2015 as u32;
        assert_eq!(SyntaxContext::checked(&db, lower, other_edition), None);
        // The next index, which no context holds yet; past every index; no word at all.
        assert_eq!(SyntaxContext::checked(&db, lower, b.into_u32() + (1 << 2)), None);
        assert_eq!(SyntaxContext::checked(&db, lower, u32::MAX - 1), None);
        assert_eq!(SyntaxContext::checked(&db, lower, u32::MAX), None);
        // `lower`'s words in `upper`: `b`'s index holds nothing there; `a`'s holds `own`,
        // `upper`'s own context, which every lookup in `upper` reads from `upper`'s table.
        assert_eq!(SyntaxContext::checked(&db, upper, b.into_u32()), None);
        let a_in_upper = SyntaxContext::checked(&db, upper, a.into_u32());
        assert_eq!(a_in_upper, Some(own));
        assert_eq!(a_in_upper.unwrap().outer_expn(&db, upper), Some(call(upper, 2)));
    }

    #[test]
    #[should_panic(expected = "invalid SyntaxContext")]
    fn the_last_u32_is_no_context() {
        let _ = SyntaxContext::from_u32(u32::MAX);
    }

    /// What name resolution asks of a context, warm: each part of its data, its marks walked
    /// outward, and interning a context that exists again, none of which allocates.
    #[test]
    fn warm_context_reads_allocate_nothing() {
        assert!(blaze_alloc_count::installed());
        let (db, lower, _) = two_crates();
        let root = SyntaxContext::root(Edition::Edition2021);
        let transparencies =
            [Transparency::Opaque, Transparency::SemiOpaque, Transparency::Transparent];
        let make = |n: u64, parent: SyntaxContext| {
            SyntaxContext::new(
                &db,
                lower,
                call(lower, n as u32 + 2),
                transparencies[n as usize % 3],
                Edition::Edition2021,
                parent,
                Some(root),
                Some(root),
            )
        };
        let mut chain = Vec::new_in(std::alloc::Global);
        chain.push(root);
        for n in 0..8 {
            let parent = *chain.last().unwrap();
            chain.push(make(n, parent));
        }
        let read = || {
            for &ctx in &chain {
                std::hint::black_box((
                    ctx.outer_mark(&db, lower),
                    ctx.edition(),
                    ctx.parent(&db, lower),
                    ctx.opaque_and_opaque_and_semiopaque(&db, lower),
                    ctx.is_opaque(&db, lower),
                    ctx.marks_rev(&db, lower).count(),
                ));
            }
        };
        read();
        blaze_alloc_count::assert_at_most(0, "warm context reads", read);
        let again = blaze_alloc_count::assert_at_most(0, "interning a context that exists", || {
            make(0, root)
        });
        assert_eq!(again, chain[1]);
        assert_eq!(chain[8].marks_rev(&db, lower).count(), 8);
    }
}
