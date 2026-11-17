//! A key's home: the arena its entry lives in.

use crate::{ArenaId, Gid, Memo, arena::Slot};

/// The arenas a value names, by the ids it holds: `#[derive(blaze_memo::Home)]` on the
/// front end's own types (a field that holds no id is `#[home(skip)]`), `#[interned]` and
/// `#[input]` structs implement it, and plain data names no arena.
pub trait Home {
    fn home(&self, homes: &mut Homes<'_>);
}

/// What [`Home::home`] is told of: the greatest arena seen, and (when the key's entry is
/// being written) each arena named, as a borrow of the entry's arena.
pub struct Homes<'a> {
    /// The greatest arena named; `None` while none is.
    max: Option<ArenaId>,
    marking: Option<(&'a Memo, &'a Slot)>,
    /// The first arena named outside the marking home's declared closure that is not open
    /// ([`crate::Inputs::open`]).
    outside: Option<ArenaId>,
    /// The snapshot a liveness walk asks ([`Memo::live`]), and the first arena named that it
    /// does not hold at that generation.
    checking: Option<&'a Memo>,
    dead: Option<ArenaId>,
}

impl Homes<'_> {
    pub fn arena(&mut self, arena: ArenaId) {
        if self.max.is_none_or(|max| arena > max) {
            self.max = Some(arena);
        }
        if let Some(memo) = self.checking
            && self.dead.is_none()
            && memo.try_slot(arena).is_none()
        {
            self.dead = Some(arena);
        }
        if let Some((memo, slot)) = self.marking {
            // An open arena is any home's to name: the borrow below is what keeps it sound.
            if self.outside.is_none()
                && !slot.reaches(arena)
                && !memo.try_slot(arena).is_some_and(Slot::is_open)
            {
                self.outside = Some(arena);
            }
            slot.borrow(arena, memo.epoch());
        }
    }
}

/// The greatest arena `key` names, of the generation its ids hold; `None` when it names none.
#[inline]
pub(crate) fn named_home<K: Home + ?Sized>(key: &K) -> Option<ArenaId> {
    let mut homes = Homes { max: None, marking: None, outside: None, checking: None, dead: None };
    key.home(&mut homes);
    homes.max
}

/// The first arena `value` names that `memo` does not hold at the generation its ids hold
/// (an id of a dropped arena, or of one made after `memo`); `None` when every id is live.
#[inline]
pub(crate) fn first_dead<K: Home + ?Sized>(memo: &Memo, value: &K) -> Option<ArenaId> {
    let mut homes = Homes { max: None, marking: None, outside: None, checking: Some(memo), dead: None };
    value.home(&mut homes);
    homes.dead
}

/// The home of `key` in `memo`: the greatest arena it names, and for a key that names none,
/// `memo`'s session arena at the generation this snapshot holds ([`Memo::session`]). There is no
/// constant of the session arena to home a key at (one would be generation 0's: once a commit
/// drops the session, a key of plain data homed there would read a dead arena); a key of
/// session-level data says so by naming nothing (`None` for an `Option<ArenaId>`). An id a key holds
/// keeps its own generation, so a key naming a dropped arena is refused by name as before.
#[inline]
pub(crate) fn home_of<K: Home + ?Sized>(memo: &Memo, key: &K) -> ArenaId {
    named_home(key).unwrap_or_else(|| memo.session())
}

/// Notes in `slot` (the key's home) a borrow of every other arena `key` names: an entry keyed
/// by an id of arena `B` lives no longer than `B`'s ids mean what they meant.
///
/// # Panics
///
/// Where `key` names an arena outside its home's declared closure ([`crate::Inputs::borrows`],
/// closed over the dependencies' own) that is not open ([`crate::Inputs::open`], a source
/// root's files: any key may name those): a key naming two unrelated crates is a key missing its
/// asking crate, and would drop its whole arena on an edit of the other. The message names the
/// key (`name` writes it) and both arenas; the fix is the asking `Crate` in the key.
pub(crate) fn mark_borrows<K: Home + ?Sized>(
    memo: &Memo,
    slot: &Slot,
    key: &K,
    name: &dyn Fn(&mut dyn std::fmt::Write),
) {
    let mut homes = Homes { max: None, marking: Some((memo, slot)), outside: None, checking: None, dead: None };
    key.home(&mut homes);
    if let Some(outside) = homes.outside
        && memo.plant() != crate::Plant::MarkOutsideClosure
    {
        let mut text = String::new();
        name(&mut text);
        // Which of two mistakes it is: a live id of an unrelated crate (the key misses its
        // asking crate), or an id of an arena this snapshot does not hold at that generation
        // (an id kept past `Memo::next`, whatever its key).
        let held = memo.held_at(outside.index());
        if held == Some(outside) {
            panic!(
                "{text} is homed at {:?} but names {outside:?}, which is outside its home's declared \
                 borrows (`Inputs::borrows`, closed over each dependency's): a key naming two \
                 unrelated crates is missing its asking crate; add the asking `Crate` to the key",
                slot.id
            );
        }
        panic!(
            "{text} is homed at {:?} but names {outside:?}, which this snapshot does not hold (it \
             holds {held:?} at that index): an id of a dropped arena, or of a later snapshot's, \
             was kept across `Memo::next` and reached this key",
            slot.id
        );
    }
}

impl Home for Gid {
    #[inline]
    fn home(&self, homes: &mut Homes<'_>) {
        homes.arena(self.arena());
        // A spill id is of the spill table of the one snapshot that made it, which a value kept
        // across `Memo::next` cannot say: a liveness walk refuses it whatever its arena.
        if self.is_spill()
            && homes.checking.is_some()
            && homes.dead.is_none()
        {
            homes.dead = Some(self.arena());
        }
    }
}

impl Home for ArenaId {
    #[inline]
    fn home(&self, homes: &mut Homes<'_>) {
        homes.arena(*self);
    }
}

macro_rules! names_none {
    ($($ty:ty),* $(,)?) => {
        $(
            impl Home for $ty {
                #[inline]
                fn home(&self, _: &mut Homes<'_>) {}
            }
        )*
    };
}

names_none!(
    (),
    bool,
    char,
    u8,
    u16,
    u32,
    u64,
    u128,
    usize,
    i8,
    i16,
    i32,
    i64,
    i128,
    isize,
    str,
    std::num::NonZeroU32,
    std::num::NonZeroU64,
    std::num::NonZeroUsize,
    crate::Fact,
    blaze_ids::ContentId,
);

// Types of crates below the front end that name no arena, so every lane does not write its
// own: an index into a body's arena (`ExprId`, `PatId`, `LocalFieldId`, ...: an
// `la_arena::Idx`) and an interned symbol (a process-wide name, never an arena's id). span's
// `ErasedFileAstId`, `FileAstId<N>`, `AstId<N>` and its ids implement `Home` in span itself
// (span depends on blaze-memo, so blaze-memo cannot name them).
impl<T> Home for la_arena::Idx<T> {
    #[inline]
    fn home(&self, _: &mut Homes<'_>) {}
}

impl<T> Home for la_arena::IdxRange<T> {
    #[inline]
    fn home(&self, _: &mut Homes<'_>) {}
}

impl Home for intern::Symbol {
    #[inline]
    fn home(&self, _: &mut Homes<'_>) {}
}

impl<T: Home + ?Sized> Home for &T {
    #[inline]
    fn home(&self, homes: &mut Homes<'_>) {
        (**self).home(homes);
    }
}

/// A boxed value names what it holds: an `owns` value's or an input's field (a key holds none,
/// as a key is `Copy`).
impl<T: Home + ?Sized, A: std::alloc::Allocator> Home for Box<T, A> {
    #[inline]
    fn home(&self, homes: &mut Homes<'_>) {
        (**self).home(homes);
    }
}

impl<T: Home> Home for [T] {
    fn home(&self, homes: &mut Homes<'_>) {
        for it in self {
            it.home(homes);
        }
    }
}

impl<T: Home, const N: usize> Home for [T; N] {
    fn home(&self, homes: &mut Homes<'_>) {
        for it in self {
            it.home(homes);
        }
    }
}

impl<T: Home> Home for Option<T> {
    #[inline]
    fn home(&self, homes: &mut Homes<'_>) {
        if let Some(it) = self {
            it.home(homes);
        }
    }
}

impl<T: Home, E: Home> Home for Result<T, E> {
    #[inline]
    fn home(&self, homes: &mut Homes<'_>) {
        match self {
            Ok(it) => it.home(homes),
            Err(it) => it.home(homes),
        }
    }
}

impl<T: ?Sized> Home for std::marker::PhantomData<T> {
    #[inline]
    fn home(&self, _: &mut Homes<'_>) {}
}

macro_rules! tuple {
    ($($name:ident),+) => {
        impl<$($name: Home),+> Home for ($($name,)+) {
            #[inline]
            #[allow(non_snake_case)]
            fn home(&self, homes: &mut Homes<'_>) {
                let ($($name,)+) = self;
                $($name.home(homes);)+
            }
        }
    };
}

tuple!(A);
tuple!(A, B);
tuple!(A, B, C);
tuple!(A, B, C, D);
tuple!(A, B, C, D, E);
tuple!(A, B, C, D, E, F);
tuple!(A, B, C, D, E, F, G);
tuple!(A, B, C, D, E, F, G, H);

#[cfg(test)]
mod tests {
    use std::alloc::Global;

    use super::named_home;
    use crate::{ArenaId, Gid};

    /// The invariant: a box is homed by what it holds (the control: a box of plain data names
    /// no arena, so it is homed in the session arena the snapshot holds, `home_of`).
    #[test]
    fn a_box_is_homed_by_what_it_holds() {
        let id = Gid::in_arena(ArenaId::new(3, 2), 9);
        assert_eq!(named_home(&Box::new_in(id, Global)), Some(id.arena()));
        assert_eq!(named_home(&Box::new_in(Some(id), Global)), Some(id.arena()));
        assert_eq!(named_home(&Box::new_in(7_u32, Global)), None);
    }
}
