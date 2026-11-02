//! The discriminants of an enum's variants, as rustc's `check_enum_variant_types`,
//! `AdtDef::discriminant_for_variant` and `Discr::checked_add` count them (MIT or Apache-2.0):
//! a variant's is the nearest explicit one at or before it (else 0) plus how many variants later
//! it is, in the enum's `repr` integer type.
//!
//! [`enum_discriminants`] holds every variant's, made in one pass, with the lookup from a value
//! to its variant. One variant's, [`crate::db::HirDatabase::const_eval_discriminant`], reads only
//! its run's explicit value ([`explicit_discriminant`]) and not the table, so an explicit
//! discriminant may name an earlier variant (`B = A as isize + 4`) as rustc's THIR lowering
//! allows, without a cycle through the variants after it.
//!
//! The table is runs, not a value per variant: a run starts at the first variant, at each
//! explicit discriminant and where a count passes the type's greatest value, and the variants
//! in it count up by one. Almost every enum is one run, held inline; a variant's value is a
//! search of the run starts, and a value's variant a search of the runs by value.

use base_db::Db;
use hir_def::{AdtId, EnumId, EnumVariantId, HasModule, attrs::AttrFlags};
use hir_expand::Lookup;
use rustc_abi::{Integer, IntegerType};
use smallvec::SmallVec;

use crate::{
    consteval::{ConstEvalError, allocation_as_isize, allocation_as_usize, hir_eval},
    db::HirDatabase,
    layout::LayoutError,
    mir::MirLowerError,
    next_solver::GenericArgs,
};

/// An enum's `repr` integer type, its discriminants' type: `isize` without one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DiscrInt {
    pub signed: bool,
    /// Its width in bits, 8 to 128.
    pub bits: u32,
}

impl DiscrInt {
    pub fn of(db: &Db, e: EnumId) -> Result<DiscrInt, ConstEvalError<'_>> {
        let int = AttrFlags::repr(db, e.into()).unwrap_or_default().discr_type();
        let integer = match int {
            IntegerType::Fixed(integer, _) => integer,
            IntegerType::Pointer(_) => match db.target_data_layout(AdtId::from(e).krate(db)) {
                Ok(dl) => dl.ptr_sized_integer(),
                Err(_) => {
                    return Err(ConstEvalError::MirLowerError(MirLowerError::LayoutError(
                        LayoutError::TargetLayoutNotAvailable,
                    )));
                }
            },
        };
        Ok(DiscrInt::new(int.is_signed(), integer))
    }

    pub fn new(signed: bool, integer: Integer) -> DiscrInt {
        DiscrInt { signed, bits: integer.size().bits() as u32 }
    }

    /// `value` as a value of the type: its low `bits`, sign-extended when the type is signed
    /// and zero-extended when not. Every value a discriminant query gives is in this form.
    pub fn normalize(self, value: i128) -> i128 {
        let shift = 128 - self.bits;
        if self.signed {
            (value << shift) >> shift
        } else {
            ((value as u128) << shift >> shift) as i128
        }
    }

    /// The type's least value.
    pub fn least(self) -> i128 {
        if self.signed { i128::MIN >> (128 - self.bits) } else { 0 }
    }

    /// The type's greatest value.
    pub fn greatest(self) -> i128 {
        if self.signed {
            i128::MAX >> (128 - self.bits)
        } else {
            (u128::MAX >> (128 - self.bits)) as i128
        }
    }

    /// Where a normalized `value` is in the type's order, as an unsigned number.
    fn key(self, value: i128) -> u128 {
        if self.signed { (value as u128) ^ (1 << 127) } else { value as u128 }
    }

    /// How many values after a normalized `value` the type has.
    fn room(self, value: i128) -> u128 {
        self.key(self.greatest()) - self.key(value)
    }

    /// A normalized `value` plus `n`, or `None` past the type's greatest value: rustc's
    /// `Discr::checked_add`, whose overflow is E0370.
    pub fn checked_add(self, value: i128, n: u128) -> Option<i128> {
        (n <= self.room(value)).then(|| value.wrapping_add(n as i128))
    }

    /// The value after a normalized `value`, the least past the greatest: rustc's `wrap_incr`.
    pub fn wrapping_next(self, value: i128) -> i128 {
        self.checked_add(value, 1).unwrap_or_else(|| self.least())
    }

    /// `value` as rustc writes a `Discr`: negative only in a signed type.
    pub fn display(self, value: i128) -> impl std::fmt::Display {
        enum Shown {
            Signed(i128),
            Unsigned(u128),
        }
        impl std::fmt::Display for Shown {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                match self {
                    Shown::Signed(it) => it.fmt(f),
                    Shown::Unsigned(it) => it.fmt(f),
                }
            }
        }
        let value = self.normalize(value);
        if self.signed { Shown::Signed(value) } else { Shown::Unsigned(self.key(value)) }
    }
}

/// Every variant's discriminant of one enum, by [`enum_discriminants`].
///
/// Run `r` is variants `starts[r]..starts[r + 1]` (the last one to the end), counting up by one
/// from `firsts[r]`. A run whose variants have no discriminant (an explicit one failed to
/// evaluate, or a count passed the type's greatest value) is in `failed` with why; its values
/// are still rustc's, counted on as its recovery does, for the errors' messages.
#[derive(Debug, PartialEq, Eq, blaze_query::Value)]
pub struct EnumDiscriminants<'db> {
    /// The type the values are of; `None` when it is not known (no target layout), and every
    /// variant failed.
    int: Option<DiscrInt>,
    count: u32,
    starts: SmallVec<[u32; 1]>,
    firsts: SmallVec<[i128; 1]>,
    /// Failed runs, ascending.
    failed: Box<[(u32, ConstEvalError<'db>)]>,
    /// The runs not failed, by their first value in the type's order, then their start.
    by_value: SmallVec<[u32; 1]>,
    /// The variants a count passed the type's greatest value at (E0370), ascending.
    overflowed: Box<[u32]>,
    /// Whether two variants have the same value (E0081), failed runs included.
    duplicates: bool,
}

const _: () = assert!(size_of::<DiscrInt>() == 8);

impl<'db> EnumDiscriminants<'db> {
    /// A table of `count` variants none of which has a discriminant, for `error`.
    fn all_failed(count: usize, error: ConstEvalError<'db>) -> Self {
        let (starts, firsts, failed) = match count {
            0 => (SmallVec::new(), SmallVec::new(), Box::default()),
            // alloc: error path, the one failed run
            _ => (SmallVec::from_elem(0, 1), SmallVec::from_elem(0, 1), Box::from([(0, error)])),
        };
        EnumDiscriminants {
            int: None,
            count: count as u32,
            starts,
            firsts,
            failed,
            by_value: SmallVec::new(),
            overflowed: Box::default(),
            duplicates: false,
        }
    }

    pub fn len(&self) -> usize {
        self.count as usize
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// The type of the values, when it is known.
    pub fn int(&self) -> Option<DiscrInt> {
        self.int
    }

    fn run_of(&self, index: usize) -> usize {
        assert!(index < self.len(), "variant {index} of an enum of {}", self.count);
        self.starts.partition_point(|&start| start as usize <= index) - 1
    }

    fn run_end(&self, run: usize) -> u32 {
        self.starts.get(run + 1).copied().unwrap_or(self.count)
    }

    fn run_error(&self, run: usize) -> Option<&ConstEvalError<'db>> {
        let at = self.failed.binary_search_by_key(&(run as u32), |it| it.0).ok()?;
        Some(&self.failed[at].1)
    }

    /// The discriminant of variant `index`, or why it has none: what
    /// [`crate::db::HirDatabase::const_eval_discriminant`] gives for it.
    pub fn discriminant(&self, index: usize) -> Result<i128, &ConstEvalError<'db>> {
        let run = self.run_of(index);
        match self.run_error(run) {
            Some(error) => Err(error),
            None => Ok(self.firsts[run].wrapping_add((index - self.starts[run] as usize) as i128)),
        }
    }

    /// Variant `index`'s value as rustc counts it, past errors too: what E0370 and E0081 show.
    pub fn value(&self, index: usize) -> i128 {
        let run = self.run_of(index);
        self.firsts[run].wrapping_add((index - self.starts[run] as usize) as i128)
    }

    /// Each variant's [`EnumDiscriminants::discriminant`], in order, in one pass.
    pub fn iter(&self) -> impl Iterator<Item = Result<i128, &ConstEvalError<'db>>> + '_ {
        (0..self.starts.len()).flat_map(move |run| {
            let (start, first, error) = (self.starts[run], self.firsts[run], self.run_error(run));
            (0..self.run_end(run) - start).map(move |offset| match error {
                Some(error) => Err(error),
                None => Ok(first.wrapping_add(offset as i128)),
            })
        })
    }

    /// The first variant whose discriminant is `value`, of those that have one.
    pub fn variant_of(&self, value: i128) -> Option<usize> {
        let int = self.int?;
        if int.normalize(value) != value {
            return None;
        }
        let key = int.key(value);
        let below = self.by_value.partition_point(|&run| int.key(self.firsts[run as usize]) <= key);
        let in_run = |run: u32| {
            let run = run as usize;
            let offset = key - int.key(self.firsts[run]);
            let len = self.run_end(run) - self.starts[run];
            (offset < len as u128).then(|| self.starts[run] as usize + offset as usize)
        };
        if !self.duplicates {
            return in_run(*self.by_value[..below].last()?);
        }
        // Runs overlap only in an enum with E0081: the first variant of those with the value.
        self.by_value[..below].iter().filter_map(|&run| in_run(run)).min()
    }

    /// The variants a count passed the type's greatest value at: rustc's E0370.
    pub fn overflowed(&self) -> &[u32] {
        &self.overflowed
    }

    pub fn has_duplicates(&self) -> bool {
        self.duplicates
    }

    /// The variants of each value two or more have (rustc's E0081), by [`Self::value`]: each
    /// group in the order of its variants, the groups in the order of their first.
    pub fn duplicates(&self) -> Vec<SmallVec<[u32; 2]>> {
        // alloc: empty, allocates nothing
        let Some(int) = self.int.filter(|_| self.duplicates) else { return Vec::new() };
        // alloc: error path, an enum with E0081
        let mut keyed = Vec::with_capacity(self.len());
        for run in 0..self.starts.len() {
            let (start, first) = (self.starts[run], int.key(self.firsts[run]));
            keyed.extend((start..self.run_end(run)).map(|index| (first + (index - start) as u128, index)));
        }
        keyed.sort_unstable();
        let mut groups: Vec<SmallVec<[u32; 2]>> = keyed
            .chunk_by(|a, b| a.0 == b.0)
            .filter(|group| group.len() > 1)
            // alloc: error path, inline up to two variants
            .map(|group| group.iter().map(|it| it.1).collect())
            // alloc: error path, a group per value E0081 names
            .collect();
        groups.sort_unstable_by_key(|group| group[0]);
        groups
    }
}

/// Makes an [`EnumDiscriminants`] from the variants in order, one call each.
pub struct Builder<'db> {
    int: DiscrInt,
    count: u32,
    starts: SmallVec<[u32; 1]>,
    firsts: SmallVec<[i128; 1]>,
    failed: Vec<(u32, ConstEvalError<'db>)>,
    overflowed: Vec<u32>,
}

impl<'db> Builder<'db> {
    pub fn new(int: DiscrInt) -> Self {
        Builder {
            int,
            count: 0,
            starts: SmallVec::new(),
            firsts: SmallVec::new(),
            // alloc: empty, allocates nothing unless an error is found
            failed: Vec::new(),
            // alloc: empty, allocates nothing unless an error is found
            overflowed: Vec::new(),
        }
    }

    /// The last variant's value, and how many values after it the type has.
    fn last(&self) -> Option<(i128, u128)> {
        let (&start, &first) = (self.starts.last()?, self.firsts.last()?);
        let offset = (self.count - 1 - start) as u128;
        Some((first.wrapping_add(offset as i128), self.int.room(first) - offset))
    }

    fn start_run(&mut self, first: i128, error: Option<ConstEvalError<'db>>) {
        if let Some(error) = error {
            // alloc: error path
            self.failed.push((self.starts.len() as u32, error));
        }
        // alloc: inline for the first run
        self.starts.push(self.count);
        self.firsts.push(first);
    }

    /// A variant without an explicit discriminant: one after the last (rustc's `disr_incr`), 0
    /// for the first. Past the type's greatest value it is E0370, and counts on from the least.
    pub fn implicit(&mut self) {
        match self.last() {
            None => self.start_run(0, None),
            Some((_, room)) if room > 0 => {}
            Some(_) => {
                // alloc: error path
                self.overflowed.push(self.count);
                let error = match self.failed.last() {
                    Some((run, error)) if *run as usize == self.starts.len() - 1 => error.clone(), // clone: error path
                    _ => ConstEvalError::MirLowerError(MirLowerError::HasErrors),
                };
                self.start_run(self.int.least(), Some(error));
            }
        }
        self.count += 1;
    }

    /// A variant with an explicit discriminant, its expression's value. One that failed to
    /// evaluate counts on from the last, as rustc's recovery does.
    pub fn explicit(&mut self, value: Result<i128, ConstEvalError<'db>>) {
        match value {
            Ok(value) => self.start_run(self.int.normalize(value), None),
            Err(error) => {
                let wrapped = self.last().map_or(0, |(last, _)| self.int.wrapping_next(last));
                self.start_run(wrapped, Some(error));
            }
        }
        self.count += 1;
    }

    pub fn finish(self) -> EnumDiscriminants<'db> {
        let Builder { int, count, starts, firsts, failed, overflowed } = self;
        let runs = starts.len();
        let len = |run: usize| (starts.get(run + 1).copied().unwrap_or(count) - starts[run]) as u128;
        // alloc: inline for one run
        let mut by_value: SmallVec<[u32; 1]> = (0..runs as u32).collect();
        by_value.sort_unstable_by_key(|&run| (int.key(firsts[run as usize]), starts[run as usize]));
        let mut duplicates = false;
        let mut covered: Option<u128> = None;
        for &run in &by_value {
            let low = int.key(firsts[run as usize]);
            duplicates |= covered.is_some_and(|covered| low <= covered);
            let high = low + (len(run as usize) - 1);
            covered = Some(covered.map_or(high, |covered| covered.max(high)));
        }
        if !failed.is_empty() {
            by_value.retain(|run| failed.binary_search_by_key(&*run, |it| it.0).is_err());
        }
        EnumDiscriminants {
            int: Some(int),
            count,
            starts,
            firsts,
            failed: failed.into_boxed_slice(),
            by_value,
            overflowed: overflowed.into_boxed_slice(),
            duplicates,
        }
    }
}

/// The indices of the variants of `e` written with an explicit discriminant, ascending: where
/// its runs start, read off the syntax, so a variant finds its run's start without evaluating.
#[blaze_query::tracked(returns(ref), mem_tag = Infer)]
pub fn explicit_discriminant_indices(db: &Db, e: EnumId) -> Box<[u32]> {
    let variants = e.enum_variants(db).with_explicit_discriminant();
    // alloc: the memo, empty (no allocation) for an enum without explicit discriminants
    variants.enumerate().filter(|(_, (_, _, explicit))| *explicit).map(|(at, _)| at as u32).collect()
}

/// The value of `variant`'s explicit discriminant expression, in the enum's `repr` type.
#[blaze_query::tracked(cycle_result = explicit_discriminant_cycle_result, returns(clone), mem_tag = Infer)]
pub fn explicit_discriminant<'db>(
    db: &'db Db,
    variant: EnumVariantId,
) -> Result<i128, ConstEvalError<'db>> {
    let int = DiscrInt::of(db, variant.lookup(db).parent)?;
    let value = hir_eval::value(db, variant.into(), GenericArgs::empty(), None)?;
    let value = if int.signed { allocation_as_isize(value) } else { allocation_as_usize(value) as i128 };
    Ok(int.normalize(value))
}

fn explicit_discriminant_cycle_result<'db>(
    _: &'db Db,
    _: blaze_query::db::Id,
    _: EnumVariantId,
) -> Result<i128, ConstEvalError<'db>> {
    Err(ConstEvalError::MirLowerError(MirLowerError::Loop))
}

/// Every variant's discriminant of `e`, in one pass over its variants: each explicit one by its
/// own [`explicit_discriminant`], so an edit of one evaluates only it, each other one after the
/// last.
#[blaze_query::tracked(returns(ref), cycle_result = enum_discriminants_cycle_result, mem_tag = Infer)]
pub fn enum_discriminants<'db>(db: &'db Db, e: EnumId) -> EnumDiscriminants<'db> {
    let variants = e.enum_variants(db);
    let int = match DiscrInt::of(db, e) {
        Ok(int) => int,
        Err(error) => return EnumDiscriminants::all_failed(variants.variants.len(), error),
    };
    let mut builder = Builder::new(int);
    for (variant, _, explicit) in variants.with_explicit_discriminant() {
        match explicit {
            true => builder.explicit(explicit_discriminant(db, variant)),
            false => builder.implicit(),
        }
    }
    builder.finish()
}

fn enum_discriminants_cycle_result<'db>(
    db: &'db Db,
    _: blaze_query::db::Id,
    e: EnumId,
) -> EnumDiscriminants<'db> {
    let count = e.enum_variants(db).variants.len();
    EnumDiscriminants::all_failed(count, ConstEvalError::MirLowerError(MirLowerError::Loop))
}

/// `variant`'s discriminant: its run's explicit value plus how far into the run it is, as
/// rustc's `discriminant_for_variant` reads it; what [`EnumDiscriminants::discriminant`] gives.
pub(crate) fn variant_discriminant<'db>(
    db: &'db Db,
    variant: EnumVariantId,
) -> Result<i128, ConstEvalError<'db>> {
    let loc = variant.lookup(db);
    let index = loc.index(db);
    let int = DiscrInt::of(db, loc.parent)?;
    let explicit = explicit_discriminant_indices(db, loc.parent);
    let (start, first) = match explicit.partition_point(|&at| at as usize <= index) {
        0 => (0, 0),
        after => {
            let start = explicit[after - 1] as usize;
            let variants = &loc.parent.enum_variants(db).variants;
            (start, explicit_discriminant(db, variants[start].0)?)
        }
    };
    int.checked_add(first, (index - start) as u128)
        .ok_or(ConstEvalError::MirLowerError(MirLowerError::HasErrors))
}

#[cfg(test)]
mod tests {
    use super::*;

    type Error = ConstEvalError<'static>;

    fn int(signed: bool, integer: Integer) -> DiscrInt {
        DiscrInt::new(signed, integer)
    }

    fn failed() -> Error {
        ConstEvalError::MirLowerError(MirLowerError::Loop)
    }

    fn overflow() -> Error {
        ConstEvalError::MirLowerError(MirLowerError::HasErrors)
    }

    /// A variant as the tests write one: `None` without an explicit discriminant.
    fn build(int: DiscrInt, variants: &[Option<Result<i128, Error>>]) -> EnumDiscriminants<'static> {
        let mut builder = Builder::new(int);
        for variant in variants {
            match variant {
                None => builder.implicit(),
                Some(value) => builder.explicit(value.clone()),
            }
        }
        builder.finish()
    }

    /// Each variant's discriminant by the table, checked against a pass that counts each one
    /// from its run's explicit value, as `variant_discriminant` does.
    fn values(table: &EnumDiscriminants<'static>, variants: &[Option<Result<i128, Error>>]) -> Vec<Result<i128, Error>> {
        let int = table.int().unwrap();
        let mut start = (0, Ok(0));
        let mut out = Vec::new();
        for (index, variant) in variants.iter().enumerate() {
            if let Some(value) = variant {
                start = (index, value.clone().map(|it| int.normalize(it)));
            }
            let by_run = start.1.clone().and_then(|first| {
                int.checked_add(first, (index - start.0) as u128).ok_or_else(overflow)
            });
            let by_table = table.discriminant(index).map_err(Clone::clone);
            assert_eq!(by_table, by_run, "variant {index}");
            out.push(by_table);
        }
        let iterated: Vec<_> = table.iter().map(|it| it.map_err(Clone::clone)).collect();
        assert_eq!(iterated, out);
        out
    }

    #[test]
    fn implicit_discriminants_count_from_zero_and_from_each_explicit_one() {
        let variants = [None, None, Some(Ok(10)), None, None, Some(Ok(-3)), None];
        let table = build(int(true, Integer::I64), &variants);
        assert_eq!(values(&table, &variants), [Ok(0), Ok(1), Ok(10), Ok(11), Ok(12), Ok(-3), Ok(-2)]);
        assert_eq!(table.starts.len(), 3);
        assert!(!table.has_duplicates());
        assert!(table.overflowed().is_empty());
        for index in 0..variants.len() {
            assert_eq!(table.variant_of(table.value(index)), Some(index));
        }
        assert_eq!(table.variant_of(2), None);
        assert_eq!(table.variant_of(-4), None);
        assert_eq!(table.variant_of(13), None);
    }

    #[test]
    fn an_implicit_discriminant_past_the_greatest_value_is_e0370() {
        // `#[repr(u8)] enum E { A = 255, B, C }`: B is E0370, rustc counts it as 0, C as 1.
        let variants = [Some(Ok(255)), None, None];
        let table = build(int(false, Integer::I8), &variants);
        assert_eq!(values(&table, &variants), [Ok(255), Err(overflow()), Err(overflow())]);
        assert_eq!(table.overflowed(), [1]);
        assert_eq!((table.value(1), table.value(2)), (0, 1));
        assert_eq!(table.variant_of(255), Some(0));
        assert_eq!(table.variant_of(0), None);
        assert_eq!(table.variant_of(256), None);
        // `#[repr(i8)] enum E { A = 127, B }`.
        let variants = [Some(Ok(127)), None];
        let table = build(int(true, Integer::I8), &variants);
        assert_eq!(values(&table, &variants), [Ok(127), Err(overflow())]);
        assert_eq!(table.value(1), -128);
        assert_eq!(int(true, Integer::I8).display(table.value(1)).to_string(), "-128");
    }

    #[test]
    fn an_explicit_discriminant_resets_the_count_after_an_overflow() {
        let variants = [Some(Ok(254)), None, None, Some(Ok(3)), None];
        let table = build(int(false, Integer::I8), &variants);
        assert_eq!(values(&table, &variants), [Ok(254), Ok(255), Err(overflow()), Ok(3), Ok(4)]);
        assert_eq!(table.overflowed(), [2]);
        assert_eq!(table.variant_of(4), Some(4));
    }

    #[test]
    fn the_widest_types_count_to_their_ends() {
        let variants = [Some(Ok(u128::MAX as i128 - 1)), None, None];
        let table = build(int(false, Integer::I128), &variants);
        assert_eq!(values(&table, &variants), [Ok(-2), Ok(-1), Err(overflow())]);
        assert_eq!(table.variant_of(-1), Some(1));
        let variants = [Some(Ok(i128::MAX)), None];
        let table = build(int(true, Integer::I128), &variants);
        assert_eq!(values(&table, &variants), [Ok(i128::MAX), Err(overflow())]);
        assert_eq!(table.value(1), i128::MIN);
        let variants = [Some(Ok(i128::MIN)), None];
        let table = build(int(true, Integer::I128), &variants);
        assert_eq!(values(&table, &variants), [Ok(i128::MIN), Ok(i128::MIN + 1)]);
    }

    #[test]
    fn explicit_values_are_read_in_the_repr_type() {
        // `#[repr(u8)] P = 257` is 1, and `#[repr(i8)] A = 255` is -1.
        let table = build(int(false, Integer::I8), &[Some(Ok(257))]);
        assert_eq!(table.discriminant(0), Ok(1));
        let table = build(int(true, Integer::I8), &[Some(Ok(255)), None]);
        assert_eq!(table.discriminant(1), Ok(0));
        assert_eq!(table.variant_of(-1), Some(0));
        assert_eq!(table.variant_of(255), None);
    }

    #[test]
    fn a_failed_explicit_discriminant_fails_its_run_and_counts_on_as_rustc() {
        let variants = [None, Some(Err(failed())), None, Some(Ok(7)), None];
        let table = build(int(true, Integer::I32), &variants);
        assert_eq!(values(&table, &variants), [Ok(0), Err(failed()), Err(failed()), Ok(7), Ok(8)]);
        assert_eq!((table.value(1), table.value(2)), (1, 2));
        assert_eq!(table.variant_of(1), None);
        assert_eq!(table.variant_of(8), Some(4));
        // One that failed after the greatest value counts on from the least, as rustc's does.
        let variants = [Some(Ok(255)), Some(Err(failed())), None];
        let table = build(int(false, Integer::I8), &variants);
        assert_eq!(values(&table, &variants), [Ok(255), Err(failed()), Err(failed())]);
        assert!(table.overflowed().is_empty());
        assert_eq!((table.value(1), table.value(2)), (0, 1));
        // An overflow in a failed run is E0370 and keeps the run's error.
        let variants = [Some(Ok(254)), Some(Err(failed())), None];
        let table = build(int(false, Integer::I8), &variants);
        assert_eq!(values(&table, &variants), [Ok(254), Err(failed()), Err(failed())]);
        assert_eq!(table.overflowed(), [2]);
        assert_eq!((table.value(1), table.value(2)), (255, 0));
    }

    #[test]
    fn duplicates_are_grouped_as_e0081_names_them() {
        // tests/ui/error-codes/E0081.rs, `MultipleDuplicates`.
        let variants = [
            None,
            Some(Ok(0)),
            None,
            None,
            Some(Ok(0)),
            Some(Ok(-2)),
            None,
            None,
            Some(Ok(-3)),
            None,
        ];
        let table = build(int(true, Integer::I64), &variants);
        values(&table, &variants);
        assert!(table.has_duplicates());
        let groups: Vec<Vec<u32>> = table.duplicates().into_iter().map(|it| it.to_vec()).collect();
        assert_eq!(groups, [vec![0, 1, 4, 7], vec![5, 9]]);
        assert_eq!(table.variant_of(0), Some(0));
        assert_eq!(table.variant_of(-2), Some(5));
        assert_eq!(table.variant_of(2), Some(3));
        assert_eq!(table.variant_of(-1), Some(6));
        // `TooManyVariants`: 257 variants of `repr(u8)`, the last overflowed to `0`.
        let variants = vec![None; 257];
        let table = build(int(false, Integer::I8), &variants);
        assert_eq!(table.overflowed(), [256]);
        let groups: Vec<Vec<u32>> = table.duplicates().into_iter().map(|it| it.to_vec()).collect();
        assert_eq!(groups, [vec![0, 256]]);
        assert_eq!(table.variant_of(0), Some(0));
    }

    #[test]
    fn values_with_no_variant_or_outside_the_type_find_none() {
        let table = build(int(false, Integer::I8), &[None, None]);
        assert_eq!(table.variant_of(-1), None);
        assert_eq!(table.variant_of(2), None);
        assert_eq!(table.variant_of(1 << 8), None);
        let empty = build(int(true, Integer::I32), &[]);
        assert!(empty.is_empty());
        assert_eq!(empty.variant_of(0), None);
        assert_eq!(empty.iter().count(), 0);
    }

    #[test]
    fn an_enum_of_implicit_discriminants_is_one_run_held_inline() {
        let variants = vec![None; 20_000];
        let table = blaze_alloc_count::assert_at_most(0, "an enum of one run", || {
            build(int(true, Integer::I64), &variants)
        });
        assert_eq!(table.starts.len(), 1);
        blaze_alloc_count::assert_at_most(0, "lookups", || {
            for index in 0..variants.len() {
                assert_eq!(table.discriminant(index), Ok(index as i128));
                assert_eq!(table.variant_of(index as i128), Some(index));
            }
            assert_eq!(table.iter().count(), variants.len());
        });
    }
}
