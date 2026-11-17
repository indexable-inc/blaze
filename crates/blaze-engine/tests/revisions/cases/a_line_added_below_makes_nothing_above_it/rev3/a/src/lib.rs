// A comment on a line of its own, and a blank line: every line below moves.

pub fn base(x: u32) -> u32 {
    let more = 6;
    x + 1 + more
}

pub fn beside(x: u32) -> u32 {
    x * 5
}

pub fn twice<T: Copy + core::ops::Add<Output = T>>(t: T) -> T {
    t + t
}
