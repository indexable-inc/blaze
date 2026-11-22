//! [`Dict<C, K, A>`]: a dictionary column builder: each row's value is given a dense code `C` (a
//! typed 32-bit id) in order of first sight, the distinct values kept once in code order. What
//! a dictionary page and its keys are made from (blaze-parquet writes them; blaze-columns'
//! `Column::dict` takes the keys and values), and what an interner of names is.
//!
//! The lookup is the crate's own open-addressing table of `(hash, code)` slots, linear probing,
//! on blaze-hash's fixed hasher: blaze-hash's `IndexSet` has no fallible insert, and here every
//! insert has a `try_` form that leaves the dictionary as it was.
//!
//! # Floor
//!
//! A row is its code: 4 bytes. A distinct value is itself once plus one 8-byte slot at a load
//! of at most 3/4 (between 10.7 and 21.3 bytes of slots a value, by where the power of two
//! falls). A push hashes once and reads one slot line, plus the value compared on a hash match
//! (a second line); a probe past one slot is rare at that load. Growth rehashes from the kept
//! hashes, reading no value.
//!
//! # Measured
//!
//! `examples/codec.rs` compares a push against `blaze_hash::HashMap` (the same FixedState hash,
//! hashbrown's layout) and std's SipHash map, each building the codes and values too. Against
//! blaze-hash's map: not yet measured.

use core::{
    alloc::Allocator,
    any::type_name,
    hash::{BuildHasher, Hash},
    marker::PhantomData,
};

use blaze_hash::FixedState;
use blaze_int::Int;
use blaze_own::{AllocFailure, Refused, Vec};

use crate::{
    error::{fail, overflow},
    id::Id,
    plant::{self, Plant},
    table::MOST,
};

/// An empty slot.
const EMPTY: u64 = u64::MAX;

/// The fewest slots a table has once it has any.
const MIN_SLOTS: usize = 8;

/// A dictionary of `K` with codes `C`, in `A` (see the module).
pub struct Dict<C: Id, K: Hash + Eq, A: Allocator + Clone> {
    values: Vec<K, A>,
    codes: Vec<C, A>,
    /// `hash32 << 32 | code`, or [`EMPTY`]; a power of two long.
    slots: Vec<u64, A>,
    _code: PhantomData<fn(C) -> C>,
}

#[inline]
fn hash32<K: Hash + ?Sized>(value: &K) -> u32 {
    // The low half: the slot index is its low bits.
    FixedState.hash_one(value).truncate_to::<u32>()
}

impl<C: Id, K: Hash + Eq, A: Allocator + Clone> Dict<C, K, A> {
    /// An empty dictionary; allocates nothing until a row comes.
    pub fn new_in(alloc: A) -> Dict<C, K, A> {
        Dict {
            values: Vec::new_in(alloc.clone()),
            codes: Vec::new_in(alloc.clone()),
            slots: Vec::new_in(alloc),
            _code: PhantomData,
        }
    }

    /// Rows pushed.
    pub fn len(&self) -> usize {
        self.codes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.codes.is_empty()
    }

    /// Distinct values.
    pub fn distinct(&self) -> usize {
        self.values.len()
    }

    /// Each row's code.
    pub fn codes(&self) -> &[C] {
        &self.codes
    }

    /// The distinct values, in code order.
    pub fn values(&self) -> &[K] {
        &self.values
    }

    /// The value of `code`; a code of another dictionary past this one's end panics.
    #[track_caller]
    pub fn value(&self, code: C) -> &K {
        match self.values.get(code.index()) {
            Some(value) => value,
            None => panic!("blaze_kit: code {} is not one of `{}`, which has {}", code.to_u32(), type_name::<Self>(), self.values.len()),
        }
    }

    /// The code of `value`, when it was pushed.
    pub fn code_of(&self, value: &K) -> Option<C> {
        self.find(hash32(value), value).ok().map(|slot| C::from_u32((self.slots[slot] & 0xffff_ffff).truncate_to::<u32>()))
    }

    /// The slot holding `value` (`Ok`), or the empty slot it would go in (`Err`); with no
    /// table, `Err(0)`.
    #[inline]
    fn find<Q: Eq + ?Sized>(&self, hash: u32, value: &Q) -> Result<usize, usize>
    where
        K: core::borrow::Borrow<Q>,
    {
        if self.slots.is_empty() {
            return Err(0);
        }
        let mask = self.slots.len() - 1;
        let mut at = blaze_int::idx(hash) & mask;
        loop {
            let slot = self.slots[at];
            if slot == EMPTY {
                return Err(at);
            }
            if (slot >> 32).truncate_to::<u32>() == hash {
                let code = blaze_int::idx((slot & 0xffff_ffff).truncate_to::<u32>());
                if self.values[code].borrow() == value {
                    return Ok(at);
                }
            }
            at = (at + 1) & mask;
        }
    }

    /// A table of `slots` slots holding every code now in, from the kept hashes.
    fn try_rehash(&self, slots: usize) -> Result<Vec<u64, A>, AllocFailure> {
        let mut table = Vec::try_with_capacity_in(slots, self.slots.allocator().clone())?;
        table.resize(slots, EMPTY);
        let mask = slots - 1;
        for &slot in self.slots.iter().filter(|&&slot| slot != EMPTY) {
            let mut at = blaze_int::idx((slot >> 32).truncate_to::<u32>()) & mask;
            while table[at] != EMPTY {
                at = (at + 1) & mask;
            }
            table[at] = slot;
        }
        Ok(table)
    }

    /// Pushes a row of `value`; its code, new when the value is. Refused with the value back
    /// and nothing changed.
    pub fn try_push(&mut self, value: K) -> Result<C, Refused<K>> {
        let hash = hash32(&value);
        if let Err(why) = self.codes.try_reserve(1) {
            return Err(Refused { value, why });
        }
        if let Ok(slot) = self.find(hash, &value) {
            let code = C::from_u32((self.slots[slot] & 0xffff_ffff).truncate_to::<u32>());
            self.codes.push(code);
            return Ok(code);
        }
        let len = self.values.len();
        let raw = match len.narrow::<u32>() {
            Ok(raw) if raw < MOST => raw,
            _ => return Err(Refused { value, why: overflow(type_name::<Self>(), len, 1) }),
        };
        if let Err(why) = self.values.try_reserve(1) {
            return Err(Refused { value, why });
        }
        // Grow past a load of 3/4.
        if (len + 1) * 4 > self.slots.len() * 3 {
            let slots = (self.slots.len() * 2).max(MIN_SLOTS);
            match self.try_rehash(slots) {
                Ok(table) => self.slots = table,
                Err(why) => return Err(Refused { value, why }),
            }
        }
        let at = match self.find(hash, &value) {
            Err(at) => at,
            Ok(_) => unreachable!("the value was not in the table"),
        };
        let raw = if plant::on(Plant::DictReusesCode) && raw > 0 { raw - 1 } else { raw };
        self.slots[at] = (u64::from(hash) << 32) | u64::from(raw);
        if blaze_int::idx(raw) == len {
            self.values.push(value);
        }
        let code = C::from_u32(raw);
        self.codes.push(code);
        Ok(code)
    }

    /// Pushes a row of the value `key` names, made by `own` only when it is new: a text
    /// column's `&str` against `String` values, with nothing made for a row already seen. `K`'s
    /// hash and equality are `Q`'s (what `Borrow` promises). Refused with nothing changed.
    pub fn try_push_by<Q: Hash + Eq + ?Sized>(
        &mut self,
        key: &Q,
        own: impl FnOnce(&Q) -> Result<K, AllocFailure>,
    ) -> Result<C, AllocFailure>
    where
        K: core::borrow::Borrow<Q>,
    {
        let hash = hash32(key);
        self.codes.try_reserve(1)?;
        if let Ok(slot) = self.find(hash, key) {
            let code = C::from_u32((self.slots[slot] & 0xffff_ffff).truncate_to::<u32>());
            self.codes.push(code);
            return Ok(code);
        }
        let len = self.values.len();
        let raw = match len.narrow::<u32>() {
            Ok(raw) if raw < MOST => raw,
            _ => return Err(overflow(type_name::<Self>(), len, 1)),
        };
        self.values.try_reserve(1)?;
        if (len + 1) * 4 > self.slots.len() * 3 {
            let slots = (self.slots.len() * 2).max(MIN_SLOTS);
            self.slots = self.try_rehash(slots)?;
        }
        let value = own(key)?;
        let at = match self.find(hash, key) {
            Err(at) => at,
            Ok(_) => unreachable!("the value was not in the table"),
        };
        self.slots[at] = (u64::from(hash) << 32) | u64::from(raw);
        self.values.push(value);
        let code = C::from_u32(raw);
        self.codes.push(code);
        Ok(code)
    }

    /// [`Dict::try_push`], panicking with the refusal.
    pub fn push(&mut self, value: K) -> C {
        match self.try_push(value) {
            Ok(code) => code,
            Err(refused) => fail(refused.why),
        }
    }

    /// Empties the dictionary, keeping its room.
    pub fn clear(&mut self) {
        self.values.clear();
        self.codes.clear();
        self.slots.iter_mut().for_each(|slot| *slot = EMPTY);
    }
}
