//@ expect: reject E0499
use std::collections::HashMap;

fn get_default(map: &mut HashMap<u32, String>, k: u32) -> &mut String {
    match map.get_mut(&k) {
        Some(v) => v,
        None => {
            map.insert(k, String::new()); //~ ERROR E0499
            panic!("inserted {k}")
        }
    }
}

fn main() {
    let mut m = HashMap::new();
    m.insert(1, String::new());
    get_default(&mut m, 1).push('x');
    println!("{}", m[&1]);
}
