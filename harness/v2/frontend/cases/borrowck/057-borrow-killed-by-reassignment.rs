//@ expect: accept
struct List {
    value: i32,
    next: Option<Box<List>>,
}

fn to_refs(mut list: &mut List) -> Vec<&mut i32> {
    let mut result = vec![];
    loop {
        result.push(&mut list.value);
        if let Some(n) = list.next.as_mut() {
            list = n;
        } else {
            return result;
        }
    }
}

fn main() {
    let mut a = 1;
    let mut b = 2;
    let mut r = &mut a;
    let s = &mut *r;
    r = &mut b;
    *r += 1;
    *s += 1;
    println!("{a} {b}");
    let mut list = List { value: 1, next: Some(Box::new(List { value: 2, next: None })) };
    for v in to_refs(&mut list) {
        *v *= 10;
    }
    println!("{} {}", list.value, list.next.as_ref().unwrap().value);
}
