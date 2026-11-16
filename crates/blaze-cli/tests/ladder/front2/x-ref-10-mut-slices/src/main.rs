//! x-ref-10: writing through `&mut [T]`: by index, `swap`, and a hand-written reverse.
//!
//! core's `slice::swap(a, b)`: "Swaps two elements in the slice"; a write `s[i] = v` through a
//! `&mut [T]` changes the caller's array (the slice points into it).
//! v = [1, 2, 3, 4, 5]:
//! 1. reverse by swapping i and len - 1 - i for i < len / 2 (= 2): [5, 4, 3, 2, 1]: v[0] = 5,
//!    v[4] = 1, v[2] = 3 (the middle stays).
//! 2. as a number in base 10 by index: 54321.
//! 3. `v.swap(0, 4)` then: [1, 4, 3, 2, 5]: 14325.
//! 4. through a sub-slice `&mut v[1..4]` set every element to 0: [1, 0, 0, 0, 5]: 10005.
//! A wrong builder whose `&mut [T]` argument is a copy exits 1 (v unchanged: 12345).

fn reverse(s: &mut [u32]) {
    let n = s.len();
    let mut i = 0;
    while i < n / 2 {
        let t = s[i];
        s[i] = s[n - 1 - i];
        s[n - 1 - i] = t;
        i += 1;
    }
}

fn number(s: &[u32]) -> u32 {
    let (mut v, mut i) = (0, 0);
    while i < s.len() {
        v = v * 10 + s[i];
        i += 1;
    }
    v
}

fn zero(s: &mut [u32]) {
    let mut i = 0;
    while i < s.len() {
        s[i] = 0;
        i += 1;
    }
}

fn main() -> std::entry::Status {
    let mut v = [1u32, 2, 3, 4, 5];
    reverse(&mut v);
    let one = v[0] == 5 && v[4] == 1 && v[2] == 3;
    let two = number(&v);
    v.swap(0, 4);
    let three = number(&v);
    zero(&mut v[1..4]);
    let status = if !one {
        1
    } else if two != 54321 {
        2
    } else if three != 14325 {
        3
    } else if number(&v) != 10005 {
        4
    } else {
        0
    };
    std::entry::Status(status)
}
