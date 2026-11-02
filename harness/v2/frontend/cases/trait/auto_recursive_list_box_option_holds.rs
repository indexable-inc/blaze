//@ expect: accept
//@ edition: 2021
// A linked list through `Option<Box<_>>`: the cycle `Node: Send` -> ... -> `Node: Send` is
// coinductive and holds, as do Sync and Unpin.
struct Node {
    value: i32,
    next: Option<Box<Node>>,
}

fn is_send<T: Send>() -> &'static str {
    "send"
}
fn is_sync<T: Sync>() -> &'static str {
    "sync"
}
fn is_unpin<T: Unpin>() -> &'static str {
    "unpin"
}

fn sum(mut n: Option<&Node>) -> i32 {
    let mut total = 0;
    while let Some(node) = n {
        total += node.value;
        n = node.next.as_deref();
    }
    total
}

fn main() {
    let list = Node { value: 1, next: Some(Box::new(Node { value: 2, next: None })) };
    println!("{} {} {}", is_send::<Node>(), is_sync::<Node>(), is_unpin::<Node>());
    println!("{}", sum(Some(&list)));
}
