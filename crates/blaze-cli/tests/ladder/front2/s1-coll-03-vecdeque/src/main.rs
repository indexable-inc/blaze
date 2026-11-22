//! s1-coll-03: `VecDeque<T>`: a ring buffer that wraps (push_front on an empty deque, rotation,
//! `make_contiguous`), iterated front to back, and a round-robin queue.

use std::collections::VecDeque;

fn main() {
    let mut d: VecDeque<i32> = VecDeque::new();
    d.push_back(1);
    d.push_back(2);
    d.push_front(0);
    d.push_front(-1);
    println!("{:?}", d);
    println!("{:?} {:?}", d.front(), d.back());
    d.rotate_left(1);
    println!("{:?}", d);
    let a = d.pop_front();
    let b = d.pop_back();
    println!("{:?} {:?} {:?}", a, b, d);
    for i in 3..8 {
        d.push_back(i);
    }
    while d.len() > 3 {
        d.pop_front();
    }
    d.push_front(4);
    let s = d.make_contiguous();
    s.sort_by(|x, y| y.cmp(x));
    println!("{:?}", d);
    println!("{}", d.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(" "));
    let v: Vec<i32> = d.into_iter().collect();
    println!("{:?}", v);
    let mut q: VecDeque<(char, u32)> = [('a', 2), ('b', 1), ('c', 3)].into_iter().collect();
    let mut order = String::new();
    while let Some((name, left)) = q.pop_front() {
        order.push(name);
        if left > 1 {
            q.push_back((name, left - 1));
        }
    }
    println!("{order}");
}
