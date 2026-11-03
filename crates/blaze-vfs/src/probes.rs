//! A count of the places the tables of names look at ([`crate::store::Names::find_base`], the
//! tree's and the watcher's indexes), kept only under `cfg(test)`: what the tests that bound a
//! lookup's probes read. A no-op in every other build.

#[cfg(test)]
thread_local! {
    static PROBES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// One more place looked at.
#[inline(always)]
pub(crate) fn count() {
    #[cfg(test)]
    PROBES.with(|it| it.set(it.get() + 1));
}

/// The places this thread looked at since the last take.
#[cfg(test)]
pub(crate) fn take() -> u64 {
    PROBES.with(|it| it.replace(0))
}

/// `n` distinct names of 24 ASCII bytes (no NUL, no `/`), every one of one
/// [`crate::paths::fixed_hash`] (the hash the VFS's tables of names were all placed by until
/// 2026-11-03, and the watcher's names still are until a flood): a fixed first word, a count
/// spelled in letters, and a last word that undoes the count, found by trying counts until it
/// is ASCII (about one in 300).
#[cfg(test)]
pub(crate) fn one_old_hash(n: usize) -> Vec<String> {
    const K: u64 = 0xf135_7aea_2e62_a9c5;
    const TARGET: u64 = 0x5eed_f100_d000_0001;
    let first = u64::from_le_bytes(*b"floodnam");
    let mut names = Vec::with_capacity(n); // alloc: a test's names
    let mut count = 0u64;
    while names.len() < n {
        count += 1;
        let middle: [u8; 8] = std::array::from_fn(|at| b'a' + ((count >> (4 * at)) & 15) as u8);
        let mut hash = 24u64;
        for word in [first, u64::from_le_bytes(middle)] {
            hash = (hash.rotate_left(26) ^ word).wrapping_mul(K);
        }
        let last = (hash.rotate_left(26) ^ TARGET).to_le_bytes();
        if last.iter().all(|&byte| (1..0x80).contains(&byte) && byte != b'/') {
            let mut name = b"floodnam".to_vec(); // alloc: a test's name
            name.extend_from_slice(&middle);
            name.extend_from_slice(&last);
            names.push(String::from_utf8(name).expect("ASCII"));
        }
    }
    let hash = crate::paths::fixed_hash(names[0].as_bytes());
    assert!(names.iter().all(|it| crate::paths::fixed_hash(it.as_bytes()) == hash), "one old hash");
    names
}
