//! x-match-10: `while let` runs its body once per match and stops at the first non-match.
//!
//! The Reference, "Predicate pattern loops": "A `while let` loop ... If the value of the
//! scrutinee matches the pattern, the loop body is executed, then control returns to the
//! pattern matching expression. Otherwise, the while expression completes."
//! `next(k)` gives Some(k + 1) while k < 5, then None.
//! 1. From k = 0: matches for 0, 1, 2, 3, 4 then None: the body runs 5 times.
//! 2. The values seen are 1, 2, 3, 4, 5; their sum is 15.
//! 3. A `while let` whose first test fails (k = 9) runs its body 0 times.
//! 4. A pattern with a guard inside the body: `while let Some(v) = next(k) { if v == 3 { break } }`
//!    stops with k = 2 (the body ran for v = 1 and v = 2 setting k to v, then broke at 3).
//! A wrong builder that tests the condition after the body exits 1 (6 runs) or 3 (1 run).

fn next(k: u32) -> Option<u32> {
    if k < 5 { Some(k + 1) } else { None }
}

fn main() -> std::entry::Status {
    let (mut k, mut runs, mut sum) = (0, 0, 0);
    while let Some(v) = next(k) {
        runs += 1;
        sum += v;
        k = v;
    }
    let mut late = 9;
    let mut late_runs = 0;
    while let Some(v) = next(late) {
        late_runs += 1;
        late = v;
    }
    let mut at = 0;
    while let Some(v) = next(at) {
        if v == 3 {
            break;
        }
        at = v;
    }
    let status = if runs != 5 {
        1
    } else if sum != 15 {
        2
    } else if late_runs != 0 || late != 9 {
        3
    } else if at != 2 {
        4
    } else {
        0
    };
    std::entry::Status(status)
}
