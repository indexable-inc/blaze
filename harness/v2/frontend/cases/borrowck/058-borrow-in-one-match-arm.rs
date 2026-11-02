//@ expect: accept
use std::collections::HashMap;

fn count(map: &mut HashMap<u32, u32>, k: u32) {
    match map.get_mut(&k) {
        Some(v) => *v += 1,
        None => {
            map.insert(k, 1);
        }
    }
}

fn main() {
    let mut map = HashMap::new();
    for k in [1, 2, 1, 3, 1] {
        count(&mut map, k);
    }
    let mut pairs: Vec<_> = map.into_iter().collect();
    pairs.sort();
    println!("{:?}", pairs);
    let mut opt = Some(5);
    match opt {
        Some(ref mut x) => *x += 1,
        None => opt = Some(0),
    }
    println!("{:?}", opt);
}
