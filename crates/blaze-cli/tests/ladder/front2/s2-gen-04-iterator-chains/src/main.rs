//! s2-gen-04: iterator chains over the real `core::iter`: a user `Iterator` impl, adapters
//! (`take_while`, `step_by`, `flat_map`, `filter_map`, `scan`, `zip`, `peekable`, `chain` with
//! `rev`, `partition`, `cycle`, `flatten`), consumers (`fold`, `sum`, `product`, `max_by_key`,
//! `position`, `any`, `all`, `last`). Each adapter's order is its doc's ("`max_by_key`: If several
//! elements are equally maximum, the last element is returned"; none ties here).

struct Fib {
    a: u64,
    b: u64,
}

impl Iterator for Fib {
    type Item = u64;
    fn next(&mut self) -> Option<u64> {
        let r = self.a;
        self.a = self.b;
        self.b += r;
        Some(r)
    }
}

fn main() {
    println!("{:?}", Fib { a: 0, b: 1 }.take(10).collect::<Vec<_>>());
    let evens: u64 = Fib { a: 0, b: 1 }.take_while(|&x| x < 100).filter(|x| x % 2 == 0).sum();
    println!("{evens}");
    println!("{:?}", (1..=10).step_by(3).collect::<Vec<_>>());
    println!("{:?}", (1..4).flat_map(|i| (0..i).map(move |j| i * 10 + j)).collect::<Vec<_>>());
    println!("{:?}", ["1", "x", "3"].iter().filter_map(|s| s.parse::<i32>().ok()).collect::<Vec<_>>());
    println!("{}", (1..=5).fold(String::new(), |acc, n| acc + &n.to_string()));
    println!(
        "{:?}",
        (1..=5)
            .scan(0, |s, x| {
                *s += x;
                Some(*s)
            })
            .collect::<Vec<_>>()
    );
    println!("{:?}", "abc".chars().zip(1..).collect::<Vec<_>>());
    println!("{:?}", [5, 3, 8].iter().enumerate().max_by_key(|&(_, v)| *v));
    println!("{} {}", (1..=5).product::<u32>(), (1..=4).map(|x| x * x).sum::<u32>());
    println!("{} {} {:?}", (1..10).any(|x| x == 7), (1..10).all(|x| x > 0), (1..10).position(|x| x * x > 20));
    let mut it = [1, 2, 3].into_iter().peekable();
    let mut seen = Vec::new();
    while let Some(x) = it.next() {
        if let Some(&n) = it.peek() {
            seen.push(x * 10 + n);
        }
    }
    println!("{:?}", seen);
    println!("{:?}", (0..3).chain(7..9).rev().collect::<Vec<_>>());
    println!("{:?}", [3, 1, 2].iter().copied().min());
    let (small, big): (Vec<i32>, Vec<i32>) = (1..=6).partition(|&x| x <= 3);
    println!("{:?} {:?}", small, big);
    println!("{}", ["a", "b", "c"].iter().skip(1).cloned().collect::<String>());
    println!("{:?}", (1..=3).cycle().take(7).collect::<Vec<_>>());
    println!("{:?}", [[1, 2], [3, 4]].iter().flatten().count());
    println!("{}", (1..=100).filter(|n| n % 3 == 0 || n % 5 == 0).sum::<u32>());
    println!("{:?}", vec![1, 2, 3, 4].chunks(2).map(|c| c.iter().sum::<i32>()).collect::<Vec<_>>());
    println!("{:?}", (1..=4).map(|x| x as f64 / 2.0).collect::<Vec<_>>());
    println!("{:?}", "a-b-c".rsplit('-').last());
}
