use bits::BitIndex;

fn main() {
    let n = std::env::args().count() as u64;
    let words = [0, 1 << 63];
    let bits = || BitIndex::new(Some(0b1011 * n), &words, None);
    let sum = bits().fold(0usize, |acc, i| acc + i);
    let names: Vec<String> = bits().map(|i| i.to_string()).collect();
    let small = bits().try_fold(0usize, |acc, i| if i > 100 { None } else { Some(acc + i) });
    println!("{sum} {} {small:?}", names.len());
}
