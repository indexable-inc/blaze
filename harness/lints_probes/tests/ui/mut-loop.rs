#![deny(unused_mut)]
#![allow(dead_code, unused_variables, unused_assignments)]
pub fn f(v: Vec<Vec<u8>>) -> u8 {
    let mut total = 0u8;
    for mut i in 0..3u8 {
        i += 1;
        total += i;
    }
    for mut j in 0..3u8 {
        total += j;
    }
    let mut it = v.into_iter();
    while let Some(mut t) = it.next() {
        t.push(1);
    }
    let mut it2 = vec![1u8].into_iter();
    while let Some(mut t) = it2.next() {
        total += t;
    }
    let mut n = 0u8;
    loop {
        n += 1;
        if n > 3 { break; }
    }
    for (mut a, b) in [(1u8, 2u8)] {
        a += b;
    }
    total + n
}
