use std::collections::{HashMap, HashSet};

fn squares(n: u64) -> HashMap<u64, u64> {
    let mut map = HashMap::new();
    for i in 1..=n {
        map.insert(i, i * i);
    }
    map
}

fn main() {
    let map = squares(100);
    let sum: u64 = map.values().sum();
    let mut keys: Vec<u64> = map.keys().copied().filter(|k| k % 25 == 0).collect();
    keys.sort();
    println!("len {}", map.len());
    println!("sum {sum}");
    println!("keys {keys:?}");
    println!("get 7 {:?}, get 101 {:?}", map.get(&7), map.get(&101));

    let mut words: HashMap<&str, usize> = HashMap::new();
    for word in "the cat and the hat and the bat".split(' ') {
        *words.entry(word).or_default() += 1;
    }
    let mut counts: Vec<(&str, usize)> = words.into_iter().collect();
    counts.sort();
    println!("words {counts:?}");

    let set: HashSet<u64> = (0..1000).map(|i| i % 7).collect();
    println!("set {}", set.len());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn has_every_key() {
        let map = squares(10);
        assert_eq!(map.len(), 10);
        assert_eq!(map[&3], 9);
    }

    #[test]
    fn removes() {
        let mut map = squares(5);
        assert_eq!(map.remove(&2), Some(4));
        assert!(!map.contains_key(&2));
    }
}
