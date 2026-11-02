//! The builtin parsers by number, a parser's number its place in [`BUILTINS`]: a perfect hash
//! of a path's first segment gives the run of [`BUILTINS`] whose paths start with it, and the
//! path is one of that run. A run is one because the table is sorted by path; a first segment
//! of two runs would be a key twice, which does not compile.

use super::BUILTINS;

/// How many builtin parsers there are.
pub(crate) const COUNT: usize = BUILTINS.len();

const _: () = assert!(COUNT <= u16::MAX as usize, "a parser's number is a u16");

/// The distinct first segments of [`BUILTINS`]' paths, in order, the end of each one's run, and
/// how many there are.
const RUNS: ([&str; COUNT], [u16; COUNT], usize) = {
    let mut firsts = [""; COUNT];
    let mut ends = [0; COUNT];
    let mut len = 0;
    let mut at = 0;
    while at < COUNT {
        let path = BUILTINS[at].path;
        assert!(!path.is_empty(), "a builtin parser has an empty path");
        if len == 0 || !blaze_phf::bytes_eq(firsts[len - 1].as_bytes(), path[0].as_bytes()) {
            firsts[len] = path[0];
            len += 1;
        }
        ends[len - 1] = (at + 1) as u16;
        at += 1;
    }
    (firsts, ends, len)
};

const FIRSTS: [&str; RUNS.2] = {
    let mut out = [""; RUNS.2];
    let mut at = 0;
    while at < RUNS.2 {
        out[at] = RUNS.0[at];
        at += 1;
    }
    out
};

const FIRST_KEYS: &[&str] = &FIRSTS;

/// The end of each run of [`FIRSTS`] in [`BUILTINS`]; a run starts where the one before ends.
static ENDS: [u16; RUNS.2] = {
    let mut out = [0; RUNS.2];
    let mut at = 0;
    while at < RUNS.2 {
        out[at] = RUNS.1[at];
        at += 1;
    }
    out
};

blaze_phf::phf_table!(
    /// [`FIRSTS`] to the index of their run.
    static RUN_OF = FIRST_KEYS;
);

/// The number of the parser of `path`, if rustc has one.
pub(crate) fn of(path: &[&str]) -> Option<usize> {
    let run = RUN_OF.get(path.first()?)?;
    let start = match run {
        0 => 0,
        run => usize::from(ENDS[run - 1]),
    };
    (start..usize::from(ENDS[run])).find(|&at| BUILTINS[at].path == path)
}

/// The path of the parser numbered `at`.
pub(crate) const fn path(at: usize) -> &'static [&'static str] {
    BUILTINS[at].path
}

/// Whether two paths are the same, in a `const`.
pub(crate) const fn same_path(a: &[&str], b: &[&str]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut at = 0;
    while at < a.len() {
        if !blaze_phf::bytes_eq(a[at].as_bytes(), b[at].as_bytes()) {
            return false;
        }
        at += 1;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn by_search(path: &[&str]) -> Option<usize> {
        BUILTINS.binary_search_by(|it| it.path.cmp(path)).ok()
    }

    /// Paths that are near a parser's without being one: each segment cut short or lengthened,
    /// a segment more or less, and the segments of a two-segment path alone.
    fn near_misses(path: &[&str]) -> Vec<Vec<String>> {
        let owned: Vec<String> = path.iter().map(|it| it.to_string()).collect();
        let mut out = Vec::new();
        for at in 0..owned.len() {
            let mut longer = owned.clone();
            longer[at].push('_');
            out.push(longer);
            let mut shorter = owned.clone();
            shorter[at].pop();
            out.push(shorter);
            let mut cased = owned.clone();
            cased[at] = cased[at].to_uppercase();
            out.push(cased);
            out.push(vec![owned[at].clone()]);
        }
        let mut more = owned.clone();
        more.push("x".to_owned());
        out.push(more);
        out.push(owned[..owned.len() - 1].to_vec());
        out.push(owned.iter().rev().cloned().collect());
        out
    }

    #[test]
    fn every_parser_is_found_at_its_number() {
        assert_eq!(COUNT, BUILTINS.len());
        for (at, builtin) in BUILTINS.iter().enumerate() {
            assert_eq!(of(builtin.path), Some(at), "{:?}", builtin.path);
            assert_eq!(path(at), builtin.path);
            assert!(same_path(path(at), builtin.path));
        }
    }

    #[test]
    fn paths_are_found_as_by_a_search() {
        let mut misses = 0;
        for builtin in BUILTINS {
            for miss in near_misses(builtin.path) {
                let miss: Vec<&str> = miss.iter().map(String::as_str).collect();
                assert_eq!(of(&miss), by_search(&miss), "{miss:?}");
                misses += usize::from(by_search(&miss).is_none());
            }
        }
        assert!(misses > BUILTINS.len());
        let none: [&[&str]; 6] = [&[], &[""], &["diagnostic"], &["diagnostic", ""], &["no_such_attribute"], &["inline", "inline"]];
        for path in none {
            assert_eq!(of(path), None, "{path:?}");
            assert_eq!(by_search(path), None, "{path:?}");
        }
    }

    #[test]
    fn same_path_is_equality() {
        let extra: [&[&str]; 2] = [&[], &["diagnostic"]];
        let paths: Vec<&[&str]> = BUILTINS.iter().map(|it| it.path).chain(extra).collect();
        for a in &paths {
            for b in &paths {
                assert_eq!(same_path(a, b), a == b, "{a:?} {b:?}");
            }
        }
    }
}
