//@ expect: accept
//@ edition: 2021
//@ aux-crate: sorting=sorting.rs
use sorting::Person;
fn main() {
    println!("{:?} {:?} {:?}", sorting::sorted_desc(vec![3, 1, 2]), sorting::find(&[1, 3, 5], 3), sorting::find(&[1, 3, 5], 4));
    let p = sorting::oldest(vec![Person { age: 30, name: "a".into() }, Person { age: 41, name: "b".into() }]);
    println!("{:?}", p);
}
