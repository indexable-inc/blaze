//! Per-thread buffers for the hit paths that run outside any query run (`Semantics`, display),
//! so with no frame: each kind is a `blaze_mem::scratch` shelf of this crate's, lent empty and
//! kept at its high-water mark, so a warm call allocates nothing. A call inside a call gets
//! another buffer of the kind, up to `scratch::DEPTH` deep.

use std::{
    cell::RefCell,
    ops::{Deref, DerefMut},
};

use blaze_mem::scratch::{self, DEPTH, Shelved};
use hir_expand::{HirFileId, MacroCallId};
use span::SyntaxContext;
use syntax::{SmolStr, SyntaxToken};

use crate::{EnumVariant, Field, InFile, semantics::LintAttr};

macro_rules! shelves {
    ($($(#[$doc:meta])* $name:ident, $shelf:ident: $elem:ty;)*) => {$(
        $(#[$doc])*
        #[derive(Default)]
        pub(crate) struct $name(Vec<$elem>);

        thread_local! {
            // alloc: none until a buffer is shelved; then one slot per buffer lent at once.
            static $shelf: RefCell<Vec<$name>> = const { RefCell::new(Vec::new()) };
        }

        impl Deref for $name {
            type Target = Vec<$elem>;
            fn deref(&self) -> &Vec<$elem> {
                &self.0
            }
        }

        impl DerefMut for $name {
            fn deref_mut(&mut self) -> &mut Vec<$elem> {
                &mut self.0
            }
        }

        impl Shelved for $name {
            fn empty(&mut self) {
                self.0.clear();
            }

            fn held(&self) -> usize {
                self.0.capacity()
            }

            fn unshelve() -> Option<Self> {
                $shelf.try_with(|shelf| shelf.borrow_mut().pop()).ok().flatten()
            }

            fn shelve(self) {
                // A thread that is ending, or a shelf already `DEPTH` deep: the buffer is freed.
                let _ = $shelf.try_with(|shelf| {
                    let mut shelf = shelf.borrow_mut();
                    if shelf.len() < DEPTH {
                        // alloc: the shelf's own `DEPTH` slots, once per thread
                        shelf.push(self);
                    }
                });
            }
        }

        impl $name {
            /// Lends the thread's buffer of this kind, empty, for `f`.
            #[allow(dead_code, reason = "not every kind is lent through `with`")]
            pub(crate) fn with<R>(f: impl FnOnce(&mut Vec<$elem>) -> R) -> R {
                scratch::with::<$name, R>(|buffer| f(&mut buffer.0))
            }
        }
    )*};
}

shelves! {
    /// `descend_into_macros_impl`'s groups still to walk: a file and how many of the pending
    /// tokens at the top are its.
    DescendGroups, DESCEND_GROUPS: (HirFileId, u32);
    /// `descend_into_macros_impl`'s tokens: those pending, and the group being walked.
    DescendTokens, DESCEND_TOKENS: (SyntaxToken, SyntaxContext);
    /// The derive and derive-attribute calls of an ADT, as `descend_into_macros_impl` walks them.
    DescendCalls, DESCEND_CALLS: MacroCallId;
    /// The first token's descendants in `descend_node_into_attributes`.
    DescendFirsts, DESCEND_FIRSTS: InFile<SyntaxToken>;
    /// The fields `display` shows.
    Fields, FIELDS: Field;
    /// The variants `display` shows.
    Variants, VARIANTS: EnumVariant;
    /// `Semantics::lint_attrs`' attributes, before they are handed out.
    LintAttrs, LINT_ATTRS: (LintAttr, SmolStr);
    /// Clauses `display` writes, kept as `'static` and lent as `'db` through [`recycle`].
    Clauses, CLAUSES: hir_ty::next_solver::Clause<'static>;
    /// `query_external_importables`' items, first found first.
    Importables, IMPORTABLES: (hir_def::item_scope::ItemInNs, hir_def::Complete);
}

type ImportablesSeen = blaze_hash::HashSet<(hir_def::item_scope::ItemInNs, hir_def::Complete)>;

thread_local! {
    /// The items `query_external_importables` has handed out, to skip one found again.
    static IMPORTABLES_SEEN: RefCell<ImportablesSeen> = RefCell::new(ImportablesSeen::default());
}

/// Takes the thread's seen-set, empty: taken rather than borrowed, so a search inside a search
/// gets one of its own.
pub(crate) fn take_importables_seen() -> ImportablesSeen {
    IMPORTABLES_SEEN.try_with(|it| std::mem::take(&mut *it.borrow_mut())).unwrap_or_default()
}

/// Gives the set back, emptied, keeping its capacity.
pub(crate) fn give_importables_seen(mut seen: ImportablesSeen) {
    seen.clear();
    let _ = IMPORTABLES_SEEN.try_with(|it| {
        let mut slot = it.borrow_mut();
        if slot.capacity() < seen.capacity() {
            *slot = seen;
        }
    });
}

/// `buffer`, emptied, as a vector of `U`: the same allocation when `T` and `U` have one size and
/// alignment (they differ only in a lifetime here), by std's in-place `collect`.
pub(crate) fn recycle<T, U>(mut buffer: Vec<T>) -> Vec<U> {
    const { assert!(size_of::<T>() == size_of::<U>() && align_of::<T>() == align_of::<U>()) };
    buffer.clear();
    // alloc: none, the `collect` reuses `buffer`'s allocation.
    buffer.into_iter().map(|_| -> U { unreachable!("the buffer was emptied") }).collect()
}

/// Lends the thread's clause buffer, empty, as `Clause<'db>`s for `f`.
pub(crate) fn with_clauses<'db, R>(
    f: impl FnOnce(&mut Vec<hir_ty::next_solver::Clause<'db>>) -> R,
) -> R {
    Clauses::with(|kept| {
        let mut lent = recycle(std::mem::take(kept));
        let result = f(&mut lent);
        *kept = recycle(lent);
        result
    })
}

/// The items of a lent buffer, first to last or from the back, the buffer going back to its
/// shelf when this is dropped.
pub(crate) struct LentDrain<B: Shelved, T> {
    buffer: scratch::Lent<B>,
    /// The next item from the front; those before it are taken.
    front: usize,
    _item: std::marker::PhantomData<fn() -> T>,
}

impl<B: Shelved + DerefMut<Target = Vec<T>>, T> LentDrain<B, T> {
    pub(crate) fn new(buffer: scratch::Lent<B>) -> Self {
        LentDrain { buffer, front: 0, _item: std::marker::PhantomData }
    }
}

impl<B: Shelved + DerefMut<Target = Vec<T>>, T: Default> Iterator for LentDrain<B, T> {
    type Item = T;

    fn next(&mut self) -> Option<T> {
        let item = self.buffer.get_mut(self.front)?;
        self.front += 1;
        Some(std::mem::take(item))
    }
}

impl<B: Shelved + DerefMut<Target = Vec<T>>, T: Default> DoubleEndedIterator for LentDrain<B, T> {
    fn next_back(&mut self) -> Option<T> {
        if self.buffer.len() == self.front {
            return None;
        }
        self.buffer.pop()
    }
}

/// The items of a lent buffer of `Copy` items, first to last, the buffer going back to its
/// shelf when this is dropped.
pub(crate) struct LentCopied<B: Shelved, T> {
    buffer: scratch::Lent<B>,
    front: usize,
    _item: std::marker::PhantomData<fn() -> T>,
}

impl<B: Shelved + Deref<Target = Vec<T>>, T: Copy> LentCopied<B, T> {
    pub(crate) fn new(buffer: scratch::Lent<B>) -> Self {
        LentCopied { buffer, front: 0, _item: std::marker::PhantomData }
    }
}

impl<B: Shelved + Deref<Target = Vec<T>>, T: Copy> Iterator for LentCopied<B, T> {
    type Item = T;

    fn next(&mut self) -> Option<T> {
        let item = *self.buffer.get(self.front)?;
        self.front += 1;
        Some(item)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let left = self.buffer.len() - self.front;
        (left, Some(left))
    }
}
