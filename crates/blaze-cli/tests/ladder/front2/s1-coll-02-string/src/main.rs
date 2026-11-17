//! s1-coll-02: `String` and `str`: UTF-8 bytes and chars, search, split, replace, case, parse.
//!
//! `str::len` is the byte length ("the length of self in bytes", core/src/str/mod.rs); `é` is two
//! bytes (0xC3 0xA9); `char_indices` yields byte offsets; `split(',')` keeps empty pieces;
//! `split_whitespace` drops them; `str` ordering is byte-wise (`'A'` 65 < `'a'` 97).

fn main() {
    let mut s = String::from("hello");
    s.push(',');
    s.push_str(" world");
    println!("{} {}", s, s.len());
    println!("{}", s.to_uppercase());
    println!("{:?}", s.find("wor"));
    println!("{}", s.replace("l", "L"));
    let words: Vec<&str> = "  a bb   ccc ".split_whitespace().collect();
    println!("{:?}", words);
    println!("{:?}", "a,b,,c".split(',').collect::<Vec<_>>());
    println!("[{}]", "  pad  ".trim());
    let rev: String = "stressed".chars().rev().collect();
    println!("{}", rev);
    let n: i32 = "-123".parse().unwrap();
    println!("{}", n * 2);
    let u = "héllo";
    println!("{} {} {:?}", u.len(), u.chars().count(), u.char_indices().nth(2));
    println!("{}", &u[0..3]);
    println!("{:?}", u.bytes().take(3).collect::<Vec<u8>>());
    println!("{}", "ab".repeat(3));
    println!("{:?}", "key=value=x".split_once('='));
    println!("{} {}", "Apple" < "apple", "abc".cmp("abd") == std::cmp::Ordering::Less);
    let mut t = String::with_capacity(4);
    for c in ['x', 'y'] {
        t.insert(0, c);
    }
    println!("{t}");
    println!("{:?}", String::from_utf8(vec![0xff]).is_err());
    println!("{}", format!("{}{}", "con", "cat") + "!");
    println!("{:?}", "Mary had".char_indices().filter(|(_, c)| c.is_uppercase()).map(|(i, _)| i).collect::<Vec<_>>());
}
