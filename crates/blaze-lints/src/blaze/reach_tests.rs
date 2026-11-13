//! The proved-unreachable checker (docs/v2/plan/design/proved-unreachable.md): per pattern the
//! prover discharges, a test that it passes and a near miss that is still reported.

use crate::tests::lint;

const MINICORE: &str = "//- minicore: sized, copy, clone, drop, option, result, fn, panic, index, slice, builtin_impls, allocator_api, range, iterator, non_zero, reach, blaze_unreachable, todo\n";

/// `alloc` with a `Vec` whose index checks its length and panics, as the real one does.
const TOOLCHAIN: &str = r#"
//- /alloc.rs crate:alloc
pub mod vec {
    pub struct Vec<T> {
        ptr: *mut T,
        len: usize,
    }
    impl<T> Vec<T> {
        pub fn len(&self) -> usize {
            self.len
        }
        pub fn is_empty(&self) -> bool {
            self.len == 0
        }
        pub fn push(&mut self, _value: T) {
            self.len = self.len + 1;
        }
        pub fn clear(&mut self) {
            self.len = 0;
        }
    }
    impl<T> core::ops::Index<usize> for Vec<T> {
        type Output = T;
        fn index(&self, index: usize) -> &T {
            if index >= self.len {
                core::panicking::panic("index out of bounds");
            }
            unsafe { &*self.ptr }
        }
    }
}
//- /std.rs crate:std deps:alloc
pub mod prelude {
    pub mod rust_2015 { pub use core::prelude::rust_2015::*; }
    pub mod rust_2018 { pub use core::prelude::rust_2018::*; }
    pub mod rust_2021 { pub use core::prelude::rust_2021::*; }
    pub mod rust_2024 { pub use core::prelude::rust_2024::*; }
}
pub mod vec { pub use alloc::vec::Vec; }
"#;

fn linted(main: &str, flags: &str) -> String {
    let fixture = format!(
        "{MINICORE}{TOOLCHAIN}//- /src/lib.rs crate:main deps:alloc,std\n{}",
        main.trim_start_matches('\n')
    );
    lint(&fixture, flags)
}

/// The names of the `#[blaze::nopanic]` functions of `main` the lint refuses, in order.
fn refused(main: &str) -> Vec<String> {
    let out = linted(main, "-A clippy::all --deny=blaze::nopanic");
    let mut names: Vec<String> = out
        .lines()
        .filter(|line| line.contains("is `#[blaze::nopanic]`"))
        .filter_map(|line| line.split('`').nth(1))
        .map(|name| name.rsplit("::").next().unwrap_or(name).to_owned())
        .collect();
    names.dedup();
    names
}

/// `v[i]` under `if i < v.len()` cannot fail. Near misses: the check is of another index;
/// the index is reassigned between the check and the use; the check is on one path only.
#[test]
fn a_slice_index_under_its_bound_check_is_no_panic() {
    let names = refused(
        r#"
#[blaze::nopanic]
pub fn guarded(v: &[u32], i: usize) -> u32 {
    if i < v.len() { v[i] } else { 0 }
}
#[blaze::nopanic]
pub fn early_return(v: &[u32], i: usize) -> u32 {
    if i >= v.len() {
        return 0;
    }
    v[i]
}
#[blaze::nopanic]
pub fn both(v: &[u32], i: usize, j: usize) -> u32 {
    if i < v.len() && j < v.len() { v[i] ^ v[j] } else { 0 }
}
#[blaze::nopanic]
pub fn other_index(v: &[u32], i: usize, j: usize) -> u32 {
    if i < v.len() { v[j] } else { 0 }
}
#[blaze::nopanic]
pub fn reassigned(v: &[u32], mut i: usize, j: usize) -> u32 {
    if i < v.len() {
        i = j;
        v[i]
    } else {
        0
    }
}
#[blaze::nopanic]
pub fn not_dominating(v: &[u32], i: usize, flag: bool) -> u32 {
    if flag {
        if i < v.len() {
            return 1;
        }
    }
    v[i]
}
#[blaze::nopanic]
pub fn other_slice(v: &[u32], w: &[u32], i: usize) -> u32 {
    if i < w.len() { v[i] } else { 0 }
}
"#,
    );
    assert_eq!(names, ["other_index", "reassigned", "not_dominating", "other_slice"]);
}

/// `o.unwrap()` under `if o.is_some()`, under `if let Some(_) = o` and in a `match` arm that
/// knows the variant cannot fail. Near misses: the check is of another option; the option is
/// reassigned between; the check is `is_none`; the value went through a function no model
/// names (`Result::ok`).
#[test]
fn an_unwrap_of_a_known_variant_is_no_panic() {
    let names = refused(
        r#"
#[blaze::nopanic]
pub fn is_some(o: Option<u32>) -> u32 {
    if o.is_some() { o.unwrap() } else { 0 }
}
#[blaze::nopanic]
pub fn not_none(o: Option<u32>) -> u32 {
    if o.is_none() {
        return 0;
    }
    o.unwrap()
}
#[blaze::nopanic]
pub fn if_let(o: Option<u32>) -> u32 {
    if let Some(_) = o { o.unwrap() } else { 0 }
}
#[blaze::nopanic]
pub fn in_match(o: Option<u32>) -> u32 {
    match o {
        Some(_) => o.unwrap(),
        None => 0,
    }
}
#[blaze::nopanic]
pub fn by_ref(o: &Option<u32>) -> u32 {
    if o.is_some() { o.unwrap() } else { 0 }
}
#[blaze::nopanic]
pub fn is_ok(r: Result<u32, u8>) -> u32 {
    if r.is_ok() { r.ok().unwrap() } else { 0 }
}
#[blaze::nopanic]
pub fn other_option(o: Option<u32>, p: Option<u32>) -> u32 {
    if o.is_some() { p.unwrap() } else { 0 }
}
#[blaze::nopanic]
pub fn reassigned(mut o: Option<u32>, p: Option<u32>) -> u32 {
    if o.is_some() {
        o = p;
        o.unwrap()
    } else {
        0
    }
}
#[blaze::nopanic]
pub fn wrong_way(o: Option<u32>) -> u32 {
    if o.is_none() { o.unwrap() } else { 0 }
}
#[blaze::nopanic]
pub fn unchecked(o: Option<u32>) -> u32 {
    o.unwrap()
}
"#,
    );
    assert_eq!(names, ["is_ok", "other_option", "reassigned", "wrong_way", "unchecked"]);
}

/// `a / b` and `a % b` under `if b != 0`, and by a `NonZero`'s value, cannot fail. Near
/// misses: the check is of the dividend; the divisor is reassigned between; no check.
#[test]
fn a_division_by_a_checked_divisor_is_no_panic() {
    let names = refused(
        r#"
use core::num::NonZero;
#[blaze::nopanic]
pub fn checked(a: u32, b: u32) -> u32 {
    if b != 0 { a / b } else { 0 }
}
#[blaze::nopanic]
pub fn early(a: u32, b: u32) -> u32 {
    if b == 0 {
        return 0;
    }
    a % b
}
#[blaze::nopanic]
pub fn positive(a: u32, b: u32) -> u32 {
    if b > 0 { a / b } else { 0 }
}
#[blaze::nopanic]
pub fn nonzero(a: u32, b: NonZero<u32>) -> u32 {
    a / b.get()
}
#[blaze::nopanic]
pub fn dividend(a: u32, b: u32) -> u32 {
    if a != 0 { a / b } else { 0 }
}
#[blaze::nopanic]
pub fn reassigned(a: u32, mut b: u32, c: u32) -> u32 {
    if b != 0 {
        b = c;
        a / b
    } else {
        0
    }
}
#[blaze::nopanic]
pub fn unchecked(a: u32, b: u32) -> u32 {
    a / b
}
"#,
    );
    assert_eq!(names, ["dividend", "reassigned", "unchecked"]);
}

/// A `Vec`'s index, whose panic is in the callee: `v[i]` under `if i < v.len()`, `v[0]` under
/// `if !v.is_empty()`, and `v[i]` in `for i in 0..v.len()`. Near misses: the vector is pushed
/// to or cleared between the check and the use (through `&mut`); another vector's length;
/// a loop over another bound.
#[test]
fn a_vec_index_under_its_length_check_is_no_panic() {
    let names = refused(
        r#"
use std::vec::Vec;
#[blaze::nopanic]
pub fn guarded(v: &Vec<u32>, i: usize) -> u32 {
    if i < v.len() { v[i] } else { 0 }
}
#[blaze::nopanic]
pub fn not_empty(v: &Vec<u32>) -> u32 {
    if !v.is_empty() { v[0] } else { 0 }
}
#[blaze::nopanic]
pub fn each(v: &Vec<u32>) -> u32 {
    let mut sum = 0;
    for i in 0..v.len() {
        sum ^= v[i];
    }
    sum
}
#[blaze::nopanic]
pub fn counted(v: &[u32]) -> u32 {
    let mut sum = 0;
    let mut i = 0;
    while i < v.len() {
        sum ^= v[i];
        i += 1;
    }
    sum
}
#[blaze::nopanic]
pub fn owned(v: Vec<u32>, i: usize) -> u32 {
    if i < v.len() { v[i] } else { 0 }
}
#[blaze::nopanic]
pub fn cleared(v: &mut Vec<u32>, i: usize) -> u32 {
    if i < v.len() {
        v.clear();
        v[i]
    } else {
        0
    }
}
#[blaze::nopanic]
pub fn other_vec(v: &Vec<u32>, w: &Vec<u32>, i: usize) -> u32 {
    if i < w.len() { v[i] } else { 0 }
}
#[blaze::nopanic]
pub fn other_bound(v: &Vec<u32>, n: usize) -> u32 {
    let mut sum = 0;
    for i in 0..n {
        sum ^= v[i];
    }
    sum
}
#[blaze::nopanic]
pub fn empty_unchecked(v: &Vec<u32>) -> u32 {
    v[0]
}
"#,
    );
    assert_eq!(names, ["cleared", "other_vec", "other_bound", "empty_unchecked"]);
}

/// `v.first().unwrap()` under `if !v.is_empty()`. Near miss: no check.
#[test]
fn first_of_a_slice_known_not_empty_is_some() {
    let names = refused(
        r#"
#[blaze::nopanic]
pub fn first(v: &[u32]) -> u32 {
    if !v.is_empty() { *v.first().unwrap() } else { 0 }
}
#[blaze::nopanic]
pub fn by_len(v: &[u32]) -> u32 {
    if v.len() > 0 { v[0] } else { 0 }
}
#[blaze::nopanic]
pub fn unchecked(v: &[u32]) -> u32 {
    *v.first().unwrap()
}
"#,
    );
    assert_eq!(names, ["unchecked"]);
}

/// The first line of each finding of `lint_name` in `out`.
fn findings<'o>(out: &'o str, lint_name: &str) -> Vec<&'o str> {
    let marker = format!("docs/lints.md#{lint_name}");
    let mut found = Vec::new();
    let mut head = None;
    for line in out.lines() {
        if line.starts_with("error") || line.starts_with("warning") {
            head = Some(line);
        }
        if line.trim_end().ends_with(&marker)
            && let Some(head) = head.take()
        {
            found.push(head);
        }
    }
    found
}

/// A `blaze::unreachable!()` compiles where the checks that dominate it rule every path out:
/// a variant already matched, a bound already checked, a divisor already known, and code
/// after an expression that never returns (an uninhabited value, a `return`).
#[test]
fn blaze_unreachable_is_accepted_where_no_path_reaches_it() {
    let out = linted(
        r#"
pub enum Never {}
pub fn variant(o: Option<u32>) -> u32 {
    match o {
        Some(x) => x,
        None => match o {
            Some(_) => blaze::unreachable!(),
            None => 0,
        },
    }
}
pub fn after_is_some(o: Option<u32>) -> u32 {
    if o.is_some() {
        match o {
            Some(x) => x,
            None => blaze::unreachable!(),
        }
    } else {
        0
    }
}
pub fn bound(v: &[u32], i: usize) -> u32 {
    if i < v.len() {
        if i >= v.len() {
            blaze::unreachable!();
        }
        v[i]
    } else {
        0
    }
}
pub fn divisor(a: u32, b: u32) -> u32 {
    if b == 0 {
        return 0;
    }
    if b == 0 {
        blaze::unreachable!();
    }
    a / b
}
pub fn uninhabited(never: Never) -> u32 {
    match never {}
}
pub fn after_never(make: fn() -> Never) -> u32 {
    let _never = make();
    blaze::unreachable!()
}
pub fn three(e: u8) -> u32 {
    if e < 3 {
        match e {
            0 => 10,
            1 => 11,
            2 => 12,
            _ => blaze::unreachable!(),
        }
    } else {
        0
    }
}
"#,
        "-A clippy::all -A unreachable_code",
    );
    assert_eq!(findings(&out, "unreachable"), Vec::<&str>::new(), "{out}");
}

/// A `blaze::unreachable!()` a path reaches is an error at the site, naming the function and
/// what could not be shown: nothing guards it; the guard is of another value; the guard does
/// not dominate it; the value is reassigned between the guard and the site.
#[test]
fn blaze_unreachable_is_refused_where_a_path_reaches_it() {
    let out = linted(
        r#"
pub fn bare() -> u32 {
    blaze::unreachable!()
}
pub fn index(v: &[u32], i: usize) -> u32 {
    if i < v.len() { v[i] } else { blaze::unreachable!() }
}
pub fn other_value(o: Option<u32>, p: Option<u32>) -> u32 {
    match o {
        Some(x) => x,
        None => match p {
            Some(_) => blaze::unreachable!(),
            None => 0,
        },
    }
}
pub fn not_dominating(o: Option<u32>, flag: bool) -> u32 {
    if flag {
        if o.is_none() {
            return 0;
        }
    }
    match o {
        Some(x) => x,
        None => blaze::unreachable!(),
    }
}
pub fn reassigned(mut o: Option<u32>, p: Option<u32>) -> u32 {
    if o.is_some() {
        o = p;
        match o {
            Some(x) => x,
            None => blaze::unreachable!(),
        }
    } else {
        0
    }
}
"#,
        "-A clippy::all",
    );
    let found = findings(&out, "unreachable");
    assert_eq!(found.len(), 5, "{out}");
    assert!(found.iter().all(|it| it.starts_with("error")), "{out}");
    assert!(
        found[0].contains("`bare`: can't prove this `blaze::unreachable!()` is never reached"),
        "{out}"
    );
    assert!(found[1].contains("`index`") && found[1].contains("can't show `i < v.len()` here"), "{out}");
    assert!(found[2].contains("`other_value`"), "{out}");
    assert!(
        found[3].contains("`not_dominating`") && found[3].contains("can't show `o is not None` here"),
        "{out}"
    );
    assert!(found[4].contains("`reassigned`"), "{out}");
}

/// A crate's own `blaze::unreachable!` is the crate's: the fallback to blaze's macro is taken
/// only where the path names nothing, so no program that does not use the extension changes.
#[test]
fn a_crates_own_blaze_unreachable_macro_wins() {
    let out = linted(
        r#"
pub mod blaze {
    #[macro_export]
    macro_rules! own_unreachable {
        () => {
            0
        };
    }
    pub use own_unreachable as unreachable;
}
pub fn own() -> u32 {
    blaze::unreachable!()
}
"#,
        "-A clippy::all",
    );
    assert_eq!(findings(&out, "unreachable"), Vec::<&str>::new(), "{out}");
}

/// The lints that count a body's panics (`panic_in_drop` here) stop counting a site the
/// prover discharges. Near miss: the same index with no check.
#[test]
fn a_discharged_site_is_not_a_panic_in_drop() {
    let out = linted(
        r#"
pub struct Guarded(pub [u32; 4], pub usize, pub Option<u32>, pub u32);
impl Drop for Guarded {
    fn drop(&mut self) {
        let at = self.1;
        let slots: &[u32] = &self.0;
        if at < slots.len() {
            let _slot = slots[at];
        }
        let held = self.2;
        if held.is_some() {
            let _value = held.unwrap();
        }
        let by = self.3;
        if by != 0 {
            let _part = 100 / by;
        }
    }
}
pub struct Unguarded(pub [u32; 4], pub usize);
impl Drop for Unguarded {
    fn drop(&mut self) {
        let at = self.1;
        let slots: &[u32] = &self.0;
        let _slot = slots[at];
    }
}
"#,
        "-A clippy::all",
    );
    let found = findings(&out, "panic_in_drop");
    assert_eq!(found.len(), 1, "{out}");
    assert!(found[0].contains("`Unguarded`"), "{out}");
}

const HOLE: &str = r#"
pub struct Dir;
pub struct FsError;
pub fn open(cached: Result<Dir, FsError>, depth: u32) -> Result<Dir, FsError> {
    if depth > 3 {
        return cached;
    }
    todo!()
}
pub fn later() {
    todo!();
}
#[cfg(test)]
mod tests {
    #[test]
    fn pending() {
        todo!()
    }
}
#[test]
fn also_pending() {
    todo!()
}
"#;

/// A `todo!()` is a typed hole: a warning in every local build, whatever the profile, that
/// names the function, the type the hole must have and what in scope has it. Test code may
/// keep one.
#[test]
fn todo_warns_locally_with_the_holes_type() {
    for profile in ["", " cfg:debug_assertions", " cfg:test", " cfg:test,debug_assertions"] {
        let fixture = format!(
            "{MINICORE}{TOOLCHAIN}//- /src/lib.rs crate:main deps:alloc,std{profile}\n{}",
            HOLE.trim_start_matches('\n')
        );
        let out = lint(&fixture, "-A clippy::all");
        let found = findings(&out, "todo");
        assert_eq!(found.len(), 2, "{profile}: {out}");
        assert!(found.iter().all(|it| it.starts_with("warning")), "{profile}: {out}");
        assert!(
            found[0].contains(
                "`todo!()` in `open`: needs `Result<Dir, FsError>`; in scope: `cached`"
            ),
            "{profile}: {out}"
        );
        assert!(found[1].contains("`todo!()` in `later`: its value is not used"), "{profile}: {out}");
    }
}

/// Under the gate's level (`--deny blaze::todo`, as the merge gate and a build for a release
/// set it) the same holes are errors; test code is still allowed.
#[test]
fn todo_is_an_error_under_the_gates_level() {
    let out = linted(HOLE, "-A clippy::all --deny=blaze::todo");
    let found = findings(&out, "todo");
    assert_eq!(found.len(), 2, "{out}");
    assert!(found.iter().all(|it| it.starts_with("error")), "{out}");
}

/// The prover never discharges a `todo!()`: one in code no path reaches is still a hole.
#[test]
fn todo_in_dead_code_is_still_reported() {
    let out = linted(
        r#"
pub fn dead(o: Option<u32>) -> u32 {
    match o {
        Some(x) => x,
        None => match o {
            Some(_) => todo!(),
            None => 0,
        },
    }
}
"#,
        "-A clippy::all",
    );
    assert_eq!(findings(&out, "todo").len(), 1, "{out}");
}
