//! Sorting by name for determinism (docs/v2/plan/goals/principles.md, "Sorting and ordering"):
//! a radix sort of each name's [`order_key`], then the bytes only within a run of one key.
//!
//! Names that are interned symbols carry their order key already (`intern::Symbol::cmp_order`);
//! this is for byte strings that aren't: linker symbol names, paths, test names.
//!
//! Cost: one 8-byte load per name per pass of [`radix::sort_by_key`], then a comparison sort of
//! each run of names sharing their first 8 bytes.

use std::cmp::Ordering;

use crate::radix;

/// The order key of a name: its first 8 bytes big-endian, zero-padded. Two keys compare as the
/// names' first 8 bytes do, so `order_key(a) < order_key(b)` implies `a < b` and only equal
/// keys need the bytes compared. `intern` re-exports it as `intern::order_key`.
#[inline]
pub const fn order_key(bytes: &[u8]) -> u64 {
    let mut key = [0u8; 8];
    let mut i = 0;
    while i < 8 && i < bytes.len() {
        key[i] = bytes[i];
        i += 1;
    }
    u64::from_be_bytes(key)
}

/// Sorts `items` by the bytes of `name`, stably: the order of
/// `items.sort_by(|a, b| name(a).cmp(name(b)))`. At most 2^32 items.
pub fn sort_by_name<T>(items: &mut [T], name: impl Fn(&T) -> &[u8]) {
    sort_by_key_then(items, |it| order_key(name(it)), |a, b| name(a).cmp(name(b)));
}

/// [`sort_by_name`] for names kept outside the items, such as indices into a table of names.
pub fn sort_by_name_in<'n, T>(items: &mut [T], name: impl Fn(&T) -> &'n [u8]) {
    sort_by_key_then(items, |it| order_key(name(it)), |a, b| name(a).cmp(name(b)));
}

/// Sorts `items` stably by `cmp`, of which `key` is a coarser order: `key(a) < key(b)` implies
/// `cmp(a, b)` is `Less`.
fn sort_by_key_then<T>(items: &mut [T], key: impl Fn(&T) -> u64, cmp: impl Fn(&T, &T) -> Ordering) {
    if items.len() <= radix::SMALL {
        items.sort_by(&cmp);
        return;
    }
    radix::sort_by_key(items, &key);
    let mut start = 0;
    while start < items.len() {
        let run = key(&items[start]);
        let mut end = start + 1;
        while end < items.len() && key(&items[end]) == run {
            end += 1;
        }
        if end - start > 1 {
            items[start..end].sort_by(&cmp);
        }
        start = end;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rng::SplitMix64;

    /// `n` names over few bytes and lengths around 8, so that many share an order key and
    /// many are equal, each with its place in the input to check stability.
    fn names(seed: u64, n: usize) -> Vec<(Vec<u8>, usize)> {
        let mut rng = SplitMix64::new(seed);
        let alphabet = [0u8, b'a', b'b', 0x7f, 0xff];
        (0..n)
            .map(|at| {
                let len = rng.index(13);
                ((0..len).map(|_| alphabet[rng.index(alphabet.len())]).collect(), at)
            })
            .collect()
    }

    #[test]
    fn order_keys() {
        assert_eq!(order_key(b""), 0);
        assert_eq!(order_key(b"a"), 0x61 << 56);
        assert_eq!(order_key(b"abcdefgh"), u64::from_be_bytes(*b"abcdefgh"));
        assert_eq!(order_key(b"abcdefghij"), u64::from_be_bytes(*b"abcdefgh"));
        assert_eq!(order_key(b"a\0"), order_key(b"a"));
    }

    #[test]
    fn order_keys_agree_with_bytes() {
        let names = names(1, 2_000);
        for (a, _) in &names {
            for (b, _) in names.iter().step_by(7) {
                let (ka, kb) = (order_key(a), order_key(b));
                if ka != kb {
                    assert_eq!(ka.cmp(&kb), a.cmp(b), "{a:?} against {b:?}");
                }
            }
        }
    }

    #[test]
    fn sorts_as_a_stable_byte_sort() {
        let sizes = [0, 1, 2, 40, radix::SMALL, radix::SMALL + 1, 3_000, 70_000];
        for (seed, n) in (2..).zip(sizes) {
            let mut ours = names(seed, n);
            let mut reference = ours.clone();
            sort_by_name(&mut ours, |it| it.0.as_slice());
            reference.sort_by(|a, b| a.0.cmp(&b.0));
            assert_eq!(ours, reference, "{n} names");
        }
    }

    #[test]
    fn sorts_indices_by_the_names_they_point_at() {
        for n in [10, 500] {
            let names = names(10, n);
            let mut ours: Vec<u32> = (0..names.len() as u32).rev().collect();
            let mut reference = ours.clone();
            sort_by_name_in(&mut ours, |&it| names[it as usize].0.as_slice());
            reference.sort_by(|&a, &b| names[a as usize].0.cmp(&names[b as usize].0));
            assert_eq!(ours, reference, "{n} names");
        }
    }
}
