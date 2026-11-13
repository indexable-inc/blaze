use std::panic::{self, AssertUnwindSafe};

bitflags::bitflags! {
    struct Flags: u8 {
        const A = 1;
        const B = 2;
    }
}

fn value() -> u32 {
    (1..=10).sum::<u32>() + 1
}

// One call into each dependency, so each is linked: 1 + 1 + 2 + 1 + 3.
fn others() -> usize {
    let digits = regex::Regex::new(r"^[0-9]+$").unwrap();
    let mut map: hashbrown::HashMap<u32, u32> = hashbrown::HashMap::new();
    map.insert(1, 2);
    let mut buffer = itoa::Buffer::new();
    let text = buffer.format(42u32);
    let mut small: smallvec::SmallVec<[u8; 4]> = smallvec::SmallVec::new();
    small.push(1);
    small.push(2);
    let at = memchr::memchr(b'2', text.as_bytes()).unwrap_or(9);
    usize::from(digits.is_match(text)) + map.len() + small.len() + at + usize::from((Flags::A | Flags::B).bits())
}

// A panic in hashbrown's own code: its location is a file of the registry crate, named under
// `$CARGO_HOME`. Printed is the directory of that path that is a cargo home of this test.
fn missing() -> bool {
    panic::set_hook(Box::new(|info| {
        let file = info.location().map_or("no location", |it| it.file());
        let home = file.split('/').find(|it| it.starts_with("cargo-")).unwrap_or("no cargo home");
        println!("a lookup of a missing key panicked in {home}");
    }));
    let map: hashbrown::HashMap<u32, u32> = hashbrown::HashMap::new();
    let caught = panic::catch_unwind(AssertUnwindSafe(|| map[&7])).is_err();
    let _ = panic::take_hook();
    caught
}

fn main() {
    let caught = missing();
    println!("six: {} {} {caught}", value(), others());
}
