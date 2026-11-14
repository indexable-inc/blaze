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
    max: ArenaId,
    marking: Option<(&'a Slot, u64)>,
}

impl Homes<'_> {
    pub fn arena(&mut self, arena: ArenaId) {
        if arena > self.max {
            self.max = arena;
        }
        if let Some((slot, epoch)) = self.marking {
            slot.borrow(arena, epoch);
        }
    }
}

/// The home of `key`.
#[inline]
pub(crate) fn home_of<K: Home + ?Sized>(key: &K) -> ArenaId {
    let mut homes = Homes { max: ArenaId::SESSION, marking: None };
    key.home(&mut homes);
    homes.max
}

/// Notes in `slot` (the key's home) a borrow of every other arena `key` names: an entry keyed
/// by an id of arena `B` lives no longer than `B`'s ids mean what they meant.
pub(crate) fn mark_borrows<K: Home + ?Sized>(memo: &Memo, slot: &Slot, key: &K) {
    let mut homes = Homes { max: ArenaId::SESSION, marking: Some((slot, memo.epoch())) };
    key.home(&mut homes);
}

impl Home for Gid {
    #[inline]
    fn home(&self, homes: &mut Homes<'_>) {
        homes.arena(self.arena());
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
);

impl<T: Home + ?Sized> Home for &T {
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
