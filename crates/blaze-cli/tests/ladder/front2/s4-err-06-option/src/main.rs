//! s4-err-06: `Option` and `Result` combinators of core/src/option.rs and result.rs, each as its
//! doc states (`xor`: "Returns Some if exactly one of self, optb is Some"; `zip`; `take`;
//! `get_or_insert`; `is_some_and`; `flatten`; `map_or`; `unwrap_or_default`).

fn main() {
    let a: Option<i32> = Some(4);
    let n: Option<i32> = None;
    println!("{:?} {:?}", a.map(|x| x * 2), n.map(|x| x * 2));
    println!("{} {}", a.unwrap_or(0), n.unwrap_or(-1));
    println!("{:?} {:?}", a.and_then(|x| if x > 3 { Some(x) } else { None }), a.filter(|&x| x > 10));
    println!("{:?} {:?}", n.or(Some(9)), a.xor(n));
    println!("{:?} {:?}", a.ok_or("none"), n.ok_or("none"));
    println!("{:?}", a.zip(Some('z')));
    let mut slot = Some(3);
    if let Some(v) = slot.as_mut() {
        *v += 1;
    }
    let taken = slot.take();
    println!("{:?} {:?}", taken, slot);
    println!("{}", slot.get_or_insert(7));
    let words = ["apple", "bob"];
    println!("{:?}", words.iter().find(|w| w.starts_with('b')));
    let r: Result<i32, String> = Err("bad".into());
    println!("{} {:?} {:?}", r.clone().unwrap_or_default(), r.clone().ok(), r.as_ref().err());
    let good: Result<i32, String> = Ok(2);
    println!("{:?}", good.map(|x| x + 1).and_then(|x| if x == 3 { Ok(x * 10) } else { Err("no".to_string()) }));
    println!("{}", Some(5).map_or(0, |v| v * 3));
    println!("{:?}", Some(Some(1)).flatten());
    println!("{}", n.is_none() && a.is_some_and(|x| x % 2 == 0));
}
