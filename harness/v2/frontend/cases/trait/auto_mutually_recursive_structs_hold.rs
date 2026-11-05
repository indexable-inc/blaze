//@ expect: accept
//@ edition: 2021
// Two structs that reach each other (`Tree` through `Vec<Branch>`, `Branch` through
// `Option<Box<Tree>>`): Send and Sync hold for both.
struct Tree {
    label: u8,
    branches: Vec<Branch>,
}

struct Branch {
    weight: u32,
    sub: Option<Box<Tree>>,
}

fn is_send<T: Send>() -> u32 {
    1
}
fn is_sync<T: Sync>() -> u32 {
    1
}

fn weight(t: &Tree) -> u32 {
    t.label as u32 + t.branches.iter().map(|b| b.weight + b.sub.as_deref().map_or(0, weight)).sum::<u32>()
}

fn main() {
    let t = Tree {
        label: 1,
        branches: vec![Branch { weight: 10, sub: Some(Box::new(Tree { label: 2, branches: vec![] })) }],
    };
    let n = is_send::<Tree>() + is_sync::<Tree>() + is_send::<Branch>() + is_sync::<Branch>();
    println!("{} {}", n, weight(&t));
}
