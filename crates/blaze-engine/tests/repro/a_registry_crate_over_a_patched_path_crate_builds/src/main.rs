use indexmap::IndexMap;

fn main() {
    let mut map: IndexMap<String, u32> = IndexMap::new();
    map.insert("a".to_owned(), 1);
    map.insert("b".to_owned(), 2);
    println!("{:?} {:?}", map.get_index_of("b"), map.get_index_of("c"));
}
