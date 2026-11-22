//! s1-coll-01: `Vec<T>` and slices of the real `alloc`: growth through the global allocator
//! (`__rust_alloc`, `__rust_realloc`), `insert`/`remove`/`retain`/`dedup`/`drain`, the stable
//! sort (`sort`, `sort_by_key`, `sort_by`: "This sort is stable", alloc/src/slice.rs), the
//! `binary_search` contract ("If the value is not found then Result::Err is returned, containing
//! the index where a matching element could be inserted while maintaining sorted order"),
//! `windows`, `chunks`, `split_at`, `concat`, `rotate_left`.

fn main() {
    let mut v = vec![5, 3, 8, 1, 9, 2];
    v.push(7);
    let last = v.pop();
    v.insert(1, 4);
    let removed = v.remove(3);
    println!("{:?} {:?} {}", v, last, removed);
    v.sort();
    println!("{:?} {:?} {:?}", v, v.binary_search(&4), v.binary_search(&6));
    v.retain(|&x| x % 2 == 1);
    v.extend([9, 9, 11]);
    v.dedup();
    println!("{:?} len {}", v, v.len());
    let drained: Vec<i32> = v.drain(1..3).collect();
    println!("{:?} {:?}", drained, v);
    let w: Vec<i32> = (1..=6).collect();
    println!("{:?}", w.windows(2).map(|p| p[0] * p[1]).collect::<Vec<_>>());
    println!("{:?}", w.chunks(4).collect::<Vec<_>>());
    let (a, b) = w.split_at(2);
    println!("{:?} {:?}", a, b);
    let mut people = vec![("bo", 30), ("al", 25), ("cy", 30)];
    people.sort_by_key(|&(_, age)| age);
    println!("{:?}", people);
    people.sort_by(|x, y| y.1.cmp(&x.1).then(x.0.cmp(y.0)));
    println!("{:?}", people);
    let nested = vec![vec![1, 2], vec![], vec![3]];
    println!("{:?} {}", nested.concat(), w.iter().sum::<i32>());
    let mut z = vec![0u8; 3];
    z.truncate(1);
    z.resize(4, 7);
    println!("{:?} {}", z, z.contains(&7));
    let mut r = vec![1, 2, 3, 4, 5];
    r.reverse();
    r.rotate_left(2);
    println!("{:?}", r);
    println!("{:?} {:?}", r.first(), r.iter().position(|&x| x == 5));
    let mut big = Vec::new();
    for i in 0..10_000u32 {
        big.push(i);
    }
    println!("{} {}", big.len(), big.iter().map(|&x| x as u64).sum::<u64>());
}
