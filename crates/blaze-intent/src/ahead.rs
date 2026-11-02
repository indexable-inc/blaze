//! What a [`blaze_api::Request::WillTest`] builds ahead: the test binaries holding a test its
//! filters select, and the functions those tests reached when they last ran, the ones most of
//! them reached first. The daemon builds those units at low priority and, with lazy codegen,
//! compiles those functions first; it runs nothing.
//!
//! Selection is libtest's: a test is selected when its name contains any of the filters, and
//! every test when there are none.

use blaze_hash::ByHash;
use blaze_ids::radix;
use blaze_items::CodeHash;
use memchr::memmem::Finder;

/// A test as its binary's last build and run left it.
pub struct Test<'a> {
    pub name: &'a str,
    /// The code hashes of what it reached when it last ran.
    pub reached: &'a [CodeHash],
}

/// A test binary's last build: the caller's number for its unit, and its tests.
pub struct Binary<'a> {
    pub unit: u32,
    pub tests: &'a [Test<'a>],
}

/// What to build ahead.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Plan {
    /// The units of the binaries holding a selected test, in the order given.
    pub units: Vec<u32>,
    /// The functions the selected tests reached, those more of them reached first, then by
    /// hash.
    pub hot: Vec<CodeHash>,
    /// The filters that select no test of any binary: tests the last builds don't have, which
    /// building ahead can't serve.
    pub unmatched: Vec<String>,
}

/// The plan for `filters` over the binaries' last builds.
pub fn plan(binaries: &[Binary<'_>], filters: &[String]) -> Plan {
    let mut plan = Plan::default();
    let mut matched = vec![false; filters.len()];
    // Each filter's searcher is made once, not once per test name as `str::contains` makes it.
    let finders: Vec<Finder<'_>> = filters.iter().map(|it| Finder::new(it.as_bytes())).collect();
    let mut count: ByHash<CodeHash, u32> = ByHash::default();
    for binary in binaries {
        let mut selected = false;
        for test in binary.tests {
            let mut hit = filters.is_empty();
            for (at, finder) in finders.iter().enumerate() {
                if finder.find(test.name.as_bytes()).is_some() {
                    matched[at] = true;
                    hit = true;
                }
            }
            if !hit {
                continue;
            }
            selected = true;
            count.reserve(test.reached.len());
            for hash in test.reached {
                *count.entry(*hash).or_insert(0) += 1;
            }
        }
        if selected {
            plan.units.push(binary.unit);
        }
    }
    plan.hot = hottest_first(count);
    plan.unmatched = filters.iter().zip(&matched).filter(|(_, hit)| !**hit).map(|(it, _)| it.clone()).collect();
    plan
}

/// The hashes by how many tests reached each, most first, then by hash: a radix sort on the
/// count and the hash's first four bytes, then a compare of the whole hash within each run that
/// key leaves tied (a run of more than one is rare: two of `n` hashes share four bytes with
/// probability about \(n^2 / 2^{33}\)).
fn hottest_first(count: ByHash<CodeHash, u32>) -> Vec<CodeHash> {
    let key = |n: u32, hash: &CodeHash| {
        let [a, b, c, d, ..] = hash.0.0;
        radix::pack_32(u32::MAX - n, u32::from_be_bytes([a, b, c, d]))
    };
    let mut hot: Vec<(u64, CodeHash)> = Vec::with_capacity(count.len());
    hot.extend(count.into_iter().map(|(hash, n)| (key(n, &hash), hash)));
    radix::sort_copy_by_key(&mut hot, |it| it.0);
    for run in hot.chunk_by_mut(|a, b| a.0 == b.0) {
        if run.len() > 1 {
            run.sort_unstable_by(|a, b| a.1.cmp(&b.1));
        }
    }
    hot.into_iter().map(|(_, hash)| hash).collect()
}

#[cfg(test)]
mod tests {
    use blaze_ids::ContentId;

    use super::*;

    fn hash(n: u8) -> CodeHash {
        CodeHash(ContentId::of("blaze-intent test", &[n]))
    }

    #[test]
    fn selects_binaries_and_orders_what_more_tests_reached_first() {
        let (a, b, c) = (hash(1), hash(2), hash(3));
        let first = [
            Test { name: "parse::empty", reached: &[a, b] },
            Test { name: "parse::long", reached: &[b] },
            Test { name: "eval::adds", reached: &[c] },
        ];
        let second = [Test { name: "io::reads", reached: &[c] }];
        let binaries = [Binary { unit: 7, tests: &first }, Binary { unit: 9, tests: &second }];
        let plan = plan(&binaries, &["parse".to_owned(), "nothing".to_owned()]);
        assert_eq!(plan.units, [7]);
        assert_eq!(plan.hot, [b, a]);
        assert_eq!(plan.unmatched, ["nothing"]);
    }

    #[test]
    fn the_radix_order_is_the_compare_order_ties_and_all() {
        let mut count: ByHash<CodeHash, u32> = ByHash::default();
        for n in 0..2000u32 {
            let mut bytes = ContentId::of("blaze-intent test", &n.to_le_bytes()).0;
            // A tenth share their first four bytes, so the key leaves runs tied.
            if n % 10 == 0 {
                bytes[..4].copy_from_slice(&[7, 7, 7, 7]);
            }
            count.insert(CodeHash(ContentId(bytes)), n % 5);
        }
        let mut reference: Vec<(u32, CodeHash)> = count.iter().map(|(hash, n)| (*n, *hash)).collect();
        reference.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        let reference: Vec<CodeHash> = reference.into_iter().map(|(_, hash)| hash).collect();
        assert_eq!(hottest_first(count), reference);
    }

    #[test]
    fn filters_match_as_str_contains_does() {
        let names = ["parse::empty", "", "ünï::cödé", "a::b::c"];
        let filters = ["", "::", "cödé", "b::c", "zz", "parse::empty::more"];
        for filter in filters {
            let finder = Finder::new(filter.as_bytes());
            for name in names {
                assert_eq!(finder.find(name.as_bytes()).is_some(), name.contains(filter), "{name:?} {filter:?}");
            }
        }
    }

    #[test]
    fn no_filter_selects_every_test_as_libtest_does() {
        let (a, b) = (hash(1), hash(2));
        let first = [Test { name: "t", reached: &[a] }];
        let second = [Test { name: "u", reached: &[a, b] }];
        let binaries = [Binary { unit: 0, tests: &first }, Binary { unit: 1, tests: &second }];
        let plan = plan(&binaries, &[]);
        assert_eq!((plan.units, plan.hot, plan.unmatched), (vec![0, 1], vec![a, b], Vec::<String>::new()));
    }
}
