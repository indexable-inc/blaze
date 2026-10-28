fn main() {
    let values = [dep::selected(), dep::renamed(), dep::local_alias(), dep::global()];
    assert_eq!(values, [37, 37, 37, 91]);
    println!("{values:?}");
}
